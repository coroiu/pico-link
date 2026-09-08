// Pico Link firmware -- the audio fault evaluator. See fault.h's module
// doc for ownership/context; this file is the implementation of design
// .planning/design/2026-09-07-audio-fault-model.md secs 5-7 (bead
// pico-link-9eq2.3.2).
//
// Doctrine (design sec 2, "faults are a VIEW, never a second source of
// truth"): every fault below is derived from a monotonic counter or a
// sticky witness some OTHER module already maintains -- this file adds no
// new IRQ-context state of its own besides the two counters design sec 6
// asks for (fb_rail_ticks in usb_audio.c, link_lost_events in a2dp.c, both
// added by this same bead but owned by those files, not this one).

#include "fault.h"

#include <stdbool.h>

#include "a2dp.h"
#include "pcm_ring.h"
#include "usb_audio.h"

// --- Constants (design sec 5.7) ---
#define PL_FAULT_WINDOW_US 1000000u
// One decision per key per second -- documents the cadence this file's
// caller (main.c) is required to hold; nothing here re-derives it from a
// clock of its own (design sec 5.1: "one evaluator, one cadence").
#define PL_FAULT_CLEAR_WINDOWS 3u
// Slow to declare a fault over; asymmetric with the 1-window raise below.
#define PL_FAULT_LEVEL_ENTER_WINDOWS 3u
// Applied by ord 2 (USB SUPPLY LOW), the one level fault -- it must be
// under the band for three consecutive windows before it is believed.
#define PL_FAULT_REFRESH_US 10000000u
#define PL_FAULT_CONGEST_MIN 2u
// Measured by Tess on pico-link-9eq2.4 -- NOT Ada's provisional 8 (design
// sec 5.7's own flagged "no measurement behind it" caveat is now resolved).
#define PL_FAULT_SUPPLY_LO 250u
#define PL_FAULT_SUPPLY_HI 254u
// q8 hysteresis band for ord 2's LEVEL half, live as of bead
// pico-link-47us: raise below LO, clear above HI, hold in between. 250/256
// == 0.977x nominal, i.e. a sustained ~2.3% USB under-delivery.

// Wire ordinals -- MUST match ui-ffi's PlFaultKey (ui-ffi/src/lib.rs)
// exactly; append-only forever (design sec 7.3).
typedef enum {
    PL_FAULT_KEY_BUF_STARVED = 0,
    PL_FAULT_KEY_BUF_OVERFLOW = 1,
    PL_FAULT_KEY_USB_SUPPLY_LOW = 2,
    PL_FAULT_KEY_AIR_CONGESTED = 3,
    PL_FAULT_KEY_AIR_LINK_LOST = 4,
    PL_FAULT_KEY_ENC_RESYNC = 5,
    PL_FAULT_KEY_COUNT,
} pl_fault_key_t;

// Wire ordinals -- MUST match ui-ffi's PlFaultSeverity/PlFaultGlyph/
// PlFaultValueKind exactly (design sec 7.3). Not exported into
// pico_link_ui.h by cbindgen (every PlAudioFaultPayload field is a plain
// u8/u16, see that struct's doc comment) -- same convention as a2dp.h's
// PL_CONNECT_STEP_* constants, which exist for the identical reason.
#define PL_FAULT_SEVERITY_CONCEALED 0u
#define PL_FAULT_SEVERITY_AUDIBLE 1u
#define PL_FAULT_GLYPH_NEUTRAL 0u
#define PL_FAULT_GLYPH_FILLED 1u
#define PL_FAULT_GLYPH_STARVED 2u
#define PL_FAULT_VALUE_KIND_NONE 0u
#define PL_FAULT_VALUE_KIND_RATIO 1u
#define PL_FAULT_VALUE_KIND_COUNT 2u
#define PL_FAULT_VALUE_KIND_MILLIS 3u

// The one static, per-key property this file owns: the glyph class
// (design sec 3.1). Severity is passed explicitly at each call site below
// instead of living here too -- AIR_CONGESTED's is dynamic (sec 7.3's
// escalation), and giving the other five keys a table entry that would
// just be echoed back added a second place to keep in sync for no benefit.
// `wakes_display` deliberately has NO entry here: design sec 7.5 makes
// that entirely a Rust-side decision from key+severity (see ui-ffi's
// fault_key_wakes_display) -- C's only job is severity/glyph, both on the
// wire.
static const uint8_t PL_FAULT_KEY_GLYPH[PL_FAULT_KEY_COUNT] = {
    [PL_FAULT_KEY_BUF_STARVED] = PL_FAULT_GLYPH_STARVED,     [PL_FAULT_KEY_BUF_OVERFLOW] = PL_FAULT_GLYPH_FILLED,
    [PL_FAULT_KEY_USB_SUPPLY_LOW] = PL_FAULT_GLYPH_STARVED,  [PL_FAULT_KEY_AIR_CONGESTED] = PL_FAULT_GLYPH_NEUTRAL,
    [PL_FAULT_KEY_AIR_LINK_LOST] = PL_FAULT_GLYPH_NEUTRAL,   [PL_FAULT_KEY_ENC_RESYNC] = PL_FAULT_GLYPH_NEUTRAL,
};

// Per-key derived-edge state (design sec 5.2/5.3). `snapshot`/
// `have_snapshot` are the previous window's counter value for the delta
// computation; `active`/`quiet_windows` implement the raise-on-1/clear-
// on-3 asymmetry; `last_emit_us` gates the 10s refresh.
typedef struct {
    uint32_t snapshot;
    bool have_snapshot;
    bool active;
    uint32_t quiet_windows;
    uint64_t last_emit_us;
} pl_fault_key_state_t;

static pl_fault_key_state_t s_keys[PL_FAULT_KEY_COUNT];

// resync_drops (design sec 3.1 ord 5's VALUE, "frames dropped by the
// trim") is windowed the same way every raise-condition counter is, but it
// never gates a raise itself (resync_events does -- the discrete-trim
// COUNT, design sec 4 Ruling 2's fhf provenance) -- so it gets its own
// tiny snapshot pair rather than a full pl_fault_key_state_t entry.
static uint32_t s_resync_drops_snapshot;
static bool s_resync_drops_have_snapshot;

// --- ord 2 (USB SUPPLY LOW) level state. Bead pico-link-47us. ---
// The design's LEVEL half (sec 6.3), built at last. What shipped before was
// the RATE half alone: raise on any ISO-OUT packet whose size differed from
// 192 bytes. That is not an under-supply signal at all -- we run EXPLICIT
// feedback (usb_audio.c's tud_audio_feedback_params_cb selects
// AUDIO_FEEDBACK_METHOD_DISABLED and pl_usb_audio_feedback_task drives
// tud_audio_fb_set), so the host varying its send size IS the control loop
// working. One trimmed packet in 48000 raised the fault, and a 196-byte
// packet (OVER-supply) raised "SUPPLY LOW" too.
//
// The level signal is delivered bytes over the 192 B/ms nominal for the
// MEASURED window -- not an assumed 1s, because the superloop stretches.
// rx_bytes_total is usb_audio.c's since-boot sum, so it gets the same
// reset-safe snapshot treatment as every other cumulative counter here.
static uint32_t s_rx_bytes_snapshot;
static bool s_rx_bytes_have_snapshot;
static uint64_t s_last_eval_us;
static bool s_have_last_eval;
// Consecutive windows below PL_FAULT_SUPPLY_LO (the sec 5.7 enter count),
// and the cumulative seconds spent raised -- the latter is this key's
// absolute `count`, i.e. the strip's "xN", and is deliberately NOT cleared
// by pl_fault_reset_all: like every other key's count it is a since-boot
// total, not a per-stream one.
static uint8_t s_supply_low_windows;
static uint32_t s_supply_low_seconds;

// Bead pico-link-9eq2.3.2, design sec 5.6.5: the ONLY cache in this file
// that survives a non-evaluated window (host-silent / not-streaming) --
// pl_a2dp_report needs a fresh reading every second regardless of A2DP
// stream state, matching what it read directly before this bead moved the
// call here.
static uint32_t s_cached_fill_min_bytes;

uint32_t pl_fault_last_fill_min(void) {
    return s_cached_fill_min_bytes;
}

// Design sec 5.6.3: "re-snapshot and clear all fault state at every stream
// transition." Applied whenever the current window is not eligible to
// raise (state != STREAMING, or host_silent) rather than hooked into each
// individual BTstack subevent handler -- at this file's 1Hz cadence the
// two are equivalent (a transition and the next ineligible window are
// indistinguishable to a once-a-second sampler), and this is what
// structurally closes THE CRY-WOLF GATE (pico-link-9odv): ovr_frames
// climbs by hundreds of thousands before A2DP ever connects, but every one
// of those windows has state != STREAMING, so every one of them
// re-baselines the snapshot to the (already huge) current value. The
// first real STREAMING window therefore always sees delta == 0 for it,
// regardless of how large the absolute counter has grown.
static void pl_fault_reset_all(void) {
    for (int i = 0; i < (int)PL_FAULT_KEY_COUNT; i++) {
        s_keys[i].have_snapshot = false;
        s_keys[i].active = false;
        s_keys[i].quiet_windows = 0;
        // snapshot/last_emit_us need no explicit clear: have_snapshot ==
        // false forces the next window to re-baseline unconditionally
        // (pl_fault_delta below), and active == false means last_emit_us
        // is never read until a fresh raise sets it again.
    }
    s_resync_drops_have_snapshot = false;
    s_rx_bytes_have_snapshot = false;
    s_have_last_eval = false;
    s_supply_low_windows = 0;
}

static uint16_t pl_fault_clamp_u16(uint32_t v) {
    return (uint16_t)(v > 0xFFFFu ? 0xFFFFu : v);
}

// Turns a since-boot cumulative counter into this window's delta. Design
// sec 5.6.4's trap, verbatim: "a NEGATIVE delta means the counter was
// reset, not that a fault occurred" (resync_events/tx_depth_max reset at
// STREAM_STARTED while resync_drops/ovr_frames do not) -- treated
// identically to "no snapshot yet" (the very first window after a reset):
// both re-baseline to `now_value` and report delta == 0, never a raise.
static uint32_t pl_fault_delta(pl_fault_key_state_t *state, uint32_t now_value) {
    uint32_t delta = 0;
    if (state->have_snapshot && now_value >= state->snapshot) {
        delta = now_value - state->snapshot;
    }
    state->snapshot = now_value;
    state->have_snapshot = true;
    return delta;
}

static void pl_fault_push(
    struct PlUi *ui, pl_fault_key_t key, uint8_t severity, uint8_t value_kind, uint32_t value_raw, uint32_t count_abs
) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_AUDIO_FAULT,
        .payload = {.audio_fault =
                        {
                            .key = (uint8_t)key,
                            .severity = severity,
                            .glyph = PL_FAULT_KEY_GLYPH[key],
                            .value_kind = value_kind,
                            .value = pl_fault_clamp_u16(value_raw),
                            .count = pl_fault_clamp_u16(count_abs),
                        }},
    };
    pl_ui_push_event(ui, event);
}

// Applies design sec 5.2's asymmetric raise/clear (raise on the FIRST
// window over threshold; clear only after PL_FAULT_CLEAR_WINDOWS
// consecutive quiet windows) and sec 5.3's periodic refresh (an active
// fault re-emits every PL_FAULT_REFRESH_US so a genuinely persistent
// condition cannot retire out from under itself at the UI's render-time
// retirement). `value_raw`/`count_abs` are always THIS window's figures,
// even on a refresh-only emit -- a refresh line is never stale. No clear
// event is ever sent (design sec 5.3: "the FFI carries raises only") --
// clearing this window just stops future refreshes.
static void pl_fault_emit_if_due(
    struct PlUi *ui, pl_fault_key_t key, uint64_t now_us, bool raised_now, uint8_t severity, uint8_t value_kind,
    uint32_t value_raw, uint32_t count_abs
) {
    pl_fault_key_state_t *state = &s_keys[key];
    bool emit = false;

    if (raised_now) {
        state->active = true;
        state->quiet_windows = 0;
        emit = true;
    } else if (state->active) {
        state->quiet_windows++;
        if (state->quiet_windows >= PL_FAULT_CLEAR_WINDOWS) {
            state->active = false;
        } else if (now_us - state->last_emit_us >= PL_FAULT_REFRESH_US) {
            emit = true;
        }
    }

    if (!emit) {
        return;
    }
    state->last_emit_us = now_us;
    pl_fault_push(ui, key, severity, value_kind, value_raw, count_abs);
}

void pl_fault_evaluate(struct PlUi *ui, uint64_t now_us) {
    // Design sec 5.6.5: this file is usb_audio.c's pl_usb_audio_fill_min()
    // SOLE caller. Read unconditionally, every call, regardless of A2DP
    // stream state below -- pl_a2dp_report's own report line used to call
    // this directly, every second, unconditionally, and this keeps that
    // behaviour identical from its point of view (it now reads the cached
    // value via pl_fault_last_fill_min() instead).
    s_cached_fill_min_bytes = pl_usb_audio_fill_min();

    // Design sec 5.6.1/5.6.2: evaluate (and therefore raise) only while
    // genuinely streaming and the host is not silent -- see
    // pl_fault_reset_all's doc comment for why resetting here also
    // satisfies sec 5.6.3's re-snapshot-at-every-transition requirement
    // and neutralizes the cry-wolf gate.
    if (!pl_a2dp_media_streaming() || pl_a2dp_host_silent()) {
        pl_fault_reset_all();
        return;
    }

    uint32_t ovr_frames = pl_pcm_overrun_frames();
    uint32_t underrun_events = pl_a2dp_underrun_events();
    uint32_t stop_queue_full = pl_a2dp_stop_queue_full();
    uint32_t link_lost_events = pl_a2dp_link_lost_events();
    uint32_t resync_events = pl_a2dp_resync_events();
    uint32_t resync_drops = pl_a2dp_resync_drops();

    uint32_t d_underrun = pl_fault_delta(&s_keys[PL_FAULT_KEY_BUF_STARVED], underrun_events);
    uint32_t d_ovr = pl_fault_delta(&s_keys[PL_FAULT_KEY_BUF_OVERFLOW], ovr_frames);
    uint32_t d_queue_full = pl_fault_delta(&s_keys[PL_FAULT_KEY_AIR_CONGESTED], stop_queue_full);
    uint32_t d_link_lost = pl_fault_delta(&s_keys[PL_FAULT_KEY_AIR_LINK_LOST], link_lost_events);
    uint32_t d_resync_events = pl_fault_delta(&s_keys[PL_FAULT_KEY_ENC_RESYNC], resync_events);

    // resync_drops backs ord 5's VALUE only (not its raise condition) --
    // see s_resync_drops_snapshot's doc comment above for why it gets its
    // own pair instead of a full key-state entry. Same reset-safe delta
    // rule as pl_fault_delta, inlined (one caller, not worth a second
    // helper signature just to share four lines).
    uint32_t d_resync_drops = 0;
    if (s_resync_drops_have_snapshot && resync_drops >= s_resync_drops_snapshot) {
        d_resync_drops = resync_drops - s_resync_drops_snapshot;
    }
    s_resync_drops_snapshot = resync_drops;
    s_resync_drops_have_snapshot = true;

    // ord 2's level signal (see s_rx_bytes_snapshot's doc comment above).
    // The window is the elapsed time since the previous EVALUATED window,
    // so a stretched superloop scales the nominal budget instead of faking
    // a deficit.
    uint64_t d_us = (s_have_last_eval && now_us > s_last_eval_us) ? (now_us - s_last_eval_us) : 0;
    s_last_eval_us = now_us;
    s_have_last_eval = true;

    uint32_t rx_bytes_total = pl_usb_audio_rx_bytes_total();
    uint32_t d_bytes = 0;
    if (s_rx_bytes_have_snapshot && rx_bytes_total >= s_rx_bytes_snapshot) {
        d_bytes = rx_bytes_total - s_rx_bytes_snapshot;
    }
    s_rx_bytes_snapshot = rx_bytes_total;
    s_rx_bytes_have_snapshot = true;

    // A window outside this range is not measurable (the first window
    // after a reset has d_us == 0; a multi-second one means the evaluator
    // itself was starved, which says nothing about the host). Both report
    // nominal and clear the enter counter -- never a fault.
    bool supply_valid = d_us >= 250000u && d_us <= 4000000u;
    uint32_t supply_q8 = 256u;
    if (supply_valid) {
        uint32_t nominal = (uint32_t)((d_us * 192u) / 1000u);
        uint64_t ratio = nominal > 0u ? ((uint64_t)d_bytes * 256u) / nominal : 256u;
        supply_q8 = ratio > 0xFFFFu ? 0xFFFFu : (uint32_t)ratio;
    }
    if (supply_valid && supply_q8 < PL_FAULT_SUPPLY_LO) {
        if (s_supply_low_windows < 0xFFu) {
            s_supply_low_windows++;
        }
    } else if (!supply_valid || supply_q8 > PL_FAULT_SUPPLY_HI) {
        s_supply_low_windows = 0;
    }
    bool supply_low_raised = s_supply_low_windows >= PL_FAULT_LEVEL_ENTER_WINDOWS;
    if (supply_low_raised) {
        s_supply_low_seconds++;
    }

    // Evaluated BEFORE ord 3 (AIR_CONGESTED) below, in the same window --
    // design sec 7.3/3.4's dynamic-severity escalation reads this.
    bool buf_overflow_raised_now = d_ovr >= 1;

    // ord 0: BUF STARVED -- design sec 3.1/4. Value is the windowed
    // minimum ring fill in milliseconds (192 bytes/ms at 48kHz/16-bit/
    // stereo) -- naturally near 0 whenever the fault is real, no override
    // needed. Count is underrun_events' own absolute value (matches the
    // design sec 8.1 worked example: "underrun_events reached 13" ->
    // "BUF STARVED x13").
    pl_fault_emit_if_due(
        ui, PL_FAULT_KEY_BUF_STARVED, now_us, d_underrun >= 1, PL_FAULT_SEVERITY_AUDIBLE, PL_FAULT_VALUE_KIND_MILLIS,
        s_cached_fill_min_bytes / 192u, underrun_events
    );

    // ord 1: BUF OVERFLOW -- value is frames dropped THIS window; count is
    // ovr_frames' absolute value.
    pl_fault_emit_if_due(
        ui, PL_FAULT_KEY_BUF_OVERFLOW, now_us, buf_overflow_raised_now, PL_FAULT_SEVERITY_AUDIBLE,
        PL_FAULT_VALUE_KIND_COUNT, d_ovr, ovr_frames
    );

    // ord 2: USB SUPPLY LOW -- the LEVEL fault (bead pico-link-47us).
    // Raised only after PL_FAULT_LEVEL_ENTER_WINDOWS consecutive windows
    // below the q8 band, i.e. a sustained, genuine under-delivery; the
    // value is the real supply ratio, which is what line 3 renders as
    // "supply 0.9Nx nominal", and the count is seconds spent raised.
    pl_fault_emit_if_due(
        ui, PL_FAULT_KEY_USB_SUPPLY_LOW, now_us, supply_low_raised, PL_FAULT_SEVERITY_CONCEALED,
        PL_FAULT_VALUE_KIND_RATIO, supply_q8, s_supply_low_seconds
    );

    // ord 3: AIR CONGESTED -- design sec 7.3: dynamic severity, escalating
    // Concealed -> Audible only in a window where BUF OVERFLOW also
    // raised (the q4tq co-occurrence signature, design sec 8.2).
    // wakes_display stays false either way -- that is a Rust-side static
    // property this file never touches (see PL_FAULT_KEY_GLYPH's doc
    // comment above).
    uint8_t congested_severity = buf_overflow_raised_now ? PL_FAULT_SEVERITY_AUDIBLE : PL_FAULT_SEVERITY_CONCEALED;
    pl_fault_emit_if_due(
        ui, PL_FAULT_KEY_AIR_CONGESTED, now_us, d_queue_full >= PL_FAULT_CONGEST_MIN, congested_severity,
        PL_FAULT_VALUE_KIND_COUNT, d_queue_full, stop_queue_full
    );

    // ord 4: AIR LINK LOST -- state->rate (design sec 5.2(c)): a2dp.c's
    // handlers already edge-detected and latched this into
    // link_lost_events; evaluated identically to every other rate fault
    // here, so a slow superloop can never miss it (design sec 7.1).
    pl_fault_emit_if_due(
        ui, PL_FAULT_KEY_AIR_LINK_LOST, now_us, d_link_lost >= 1, PL_FAULT_SEVERITY_AUDIBLE, PL_FAULT_VALUE_KIND_COUNT,
        d_link_lost, link_lost_events
    );

    // ord 5: ENC RESYNC -- raised on resync_events (the discrete-trim
    // COUNT); value is resync_drops' delta (frames the trim actually
    // dropped this window) -- design sec 4 Ruling 2's fhf provenance is
    // exactly why these two counters, not one, are needed to tell a
    // correct single cut from a double-cut bug.
    pl_fault_emit_if_due(
        ui, PL_FAULT_KEY_ENC_RESYNC, now_us, d_resync_events >= 1, PL_FAULT_SEVERITY_CONCEALED,
        PL_FAULT_VALUE_KIND_COUNT, d_resync_drops, resync_events
    );
}
