// Pico Link firmware -- M4 S1: A2DP source (bead pico-link-cz0.5.2).
// .planning/design/2026-08-29-a2dp-source-pipeline.md is the design of
// record; this file implements its sec 4 (codec table wiring), sec 4.3
// (Stage 1 implicit negotiation flow), sec 5 (media timer, IRQ-context
// contract), and sec 7 (counters/report line).
//
// Provenance (design sec 11.1): the overall shape -- SDP/AVRCP/A2DP
// service init, the a2dp_source_packet_handler event switch, the SBC
// storage-buffer accumulate-then-send pattern -- mirrors BlueKitchen's own
// ${PICO_SDK_PATH}/lib/btstack/example/a2dp_source_demo.c, read directly
// from the pico-sdk's vendored btstack submodule (never USBPods -- GPL-3
// would taint this whole binary, including core/ui-ffi).
//
// The divergence from the demo (sec 11.2), NARROWED after bead
// pico-link-pbv -- the original wording here ("encode work is capped at
// PL_A2DP_MAX_FRAMES_PER_TICK per timer tick") is what let a real bug
// hide behind a plausible-sounding sentence for one whole milestone, so
// stated precisely this time: the demo's a2dp_demo_audio_timeout_handler
// derives elapsed_time * sample_rate to decide how many SAMPLES its
// SYNTHETIC source produced this tick -- it doesn't need a per-tick frame
// cap because a synthetic source can never build an unbounded backlog.
// Our audio is real, ring-fed PCM (pcm_ring.h, from usb_audio.c's real USB
// arrival), which CAN build a backlog (a stall, a late tick) -- so unlike
// the demo, this file's drain genuinely needs both things design sec 1
// requires: a real clock (samples_owed, credit-pacing accrued from
// time_us_64() deltas at microsecond resolution -- pl_a2dp_ctx_t's doc
// comment below) so steady-state throughput is exactly the RP2350
// crystal's 375 SBC-frames/s regardless of tick jitter or MTU alignment,
// AND a bounded per-tick dwell cap so a real backlog can never make one
// IRQ dwell arbitrarily long (PL_A2DP_MAX_ENCODE_DWELL_US, enforced as a
// real TIME check inside pl_a2dp_fill_sbc_buffer's loop as of round 2 --
// round 1's frame-count proxy for this, frames_per_tick_cap, turned out to
// be indistinguishable from healthy in steady state and was retired; see
// that constant's own doc comment). Before pico-link-pbv this file had
// ONLY the second half (a fixed PL_A2DP_MAX_FRAMES_PER_TICK=5) and NO
// clock at all -- pl_a2dp_fill_sbc_buffer was a pure data-driven drain,
// exactly what design sec 1 (lines 44-48) names and forbids, and it only
// looked stable because the accidental rate limiter (cap 5 x the
// negotiated payload's frame-boundary interaction x the real ~91Hz tick
// rate = ~319 frames/s) happened to sit below the 375 frames/s real-time
// demand. Raising the cap alone (to e.g. 7) would have removed that
// accidental limiter without replacing it with a real one -- the ring
// would drain faster than the host supplies, empty out, and look "fixed"
// for about ten seconds before failing the other direction. ROUND 1's OWN
// FIX THEN REPRODUCED A DIFFERENT LATENT BUG (unclamped credit windup --
// see samples_owed's doc comment and credit_clamped_samples's doc comment on
// pl_a2dp_ctx_t): the drain oscillated between catch-up bursts above 375
// frames/s and real starvation instead of sitting steadily below it. See
// bead pico-link-pbv's comment history for both rounds' measurements and
// Ada's two design passes.
//
// IRQ-context contract (design sec 5): pl_a2dp_media_timer_handler,
// pl_a2dp_fill_sbc_buffer, and pl_a2dp_send_media_packet run in the
// cyw43/BTstack background IRQ (PICO_LOWEST_IRQ_PRIORITY, 0xFF) -- the
// media hot path. NEVER call pl_log from those three functions: pl_log
// takes pl_usb_mutex, and the 0xC0 USB worker's mutex_try_enter then skips
// its tick entirely, and enough skipped ticks kills the ISO-OUT endpoint
// permanently (bead pico-link-0d2). Counters only there; pl_a2dp_report
// (thread context, superloop) is where they get printed. One-off
// signalling transitions (connect/codec-configured/stream-started/etc, all
// in pl_a2dp_packet_handler) are NOT the hot path -- bt.c's own packet
// handler already calls pl_log freely from this same IRQ context for
// exactly this class of low-frequency event, so this file does too.
//
// Bead pico-link-pbv ROUND 3 (R3-5): the media timer's REAL cadence is
// ~11.3ms, not the nominal PL_A2DP_AUDIO_TIMEOUT_MS=10. Cause, read
// directly from pico-sdk 2.1.1's
// src/rp2_common/pico_btstack/btstack_run_loop_async_context.c:
// btstack_run_loop_set_timer (line 51) adds a whole extra millisecond
// (`timeout_in_ms + 1`) on top of to_ms_since_boot's own truncation to
// whole milliseconds, and btstack_work_pending (lines 144-153) re-derives
// the next deadline from that already-truncated ms value, re-inflating it
// by up to another 1ms. This is expected pico-sdk behaviour, not a bug in
// this file, and credit-pacing accrual (samples_owed) is IMMUNE to it --
// accrual is driven by a real time_us_64() delta every tick, never by the
// nominal timer period. But it is an AMPLIFIER for anything that assumes
// a fixed tick period: NO DERIVATION IN THIS FILE MAY ASSUME 100 ticks/s.
// Both prior design rounds made that assumption silently, and round 3's
// root cause (the credit clamp bound sitting on the drain's natural
// operating point, see samples_owed's and credit_clamped_samples's doc
// comments) is exactly what that silent assumption produced. Where a real
// elapsed period is needed, derive it from the measured
// worst_tick_interval_us or (preferably, since that field never resets)
// the current tick's own elapsed_us -- never from
// PL_A2DP_AUDIO_TIMEOUT_MS.
#include "a2dp.h"

#include <string.h>

#include "pico/time.h"

#include "btstack.h"

#include "bt.h"
#include "codec_sbc.h"
#include "codec_table.h"
#include "pcm_ring.h"
#include "usb_audio.h"
#include "usb_pump.h"
#include "watchdog_sup.h"

// Matches a2dp_source_demo.c's SBC_STORAGE_SIZE -- generous headroom over
// any real AVDTP media MTU (~650-1013B typical), -1 reserved for the SBC
// media payload header byte (num_frames), see pl_a2dp_send_media_packet.
#define SBC_STORAGE_SIZE 1030

// design sec 1: our own crystal, via btstack_run_loop timers, paces the
// A2DP media stream -- matches a2dp_source_demo.c's own AUDIO_TIMEOUT_MS.
#define PL_A2DP_AUDIO_TIMEOUT_MS 10

// Bead pico-link-pbv: PL_A2DP_MAX_FRAMES_PER_TICK (a fixed constant, 5) is
// GONE. Round 1 replaced it with s_ctx.frames_per_tick_cap, a per-stream
// FRAME-COUNT cap computed from the negotiated payload size and the
// codec's worst_case_encode_us. Round 2 found that frame-count proxy
// arithmetically indistinguishable from a healthy steady state whenever it
// equals frames_per_packet (its own falsifier instrumentation could not
// tell "the dwell cap tripped" from "a packet legitimately filled") and
// retired it: pl_a2dp_fill_sbc_buffer now enforces the same "one IRQ dwell
// must stay bounded" property (design sec 3.5/5) directly, as a real TIME
// check against this constant (accumulating the already-measured
// per-encode dt), with stop_dwell as the resulting true safety-trip
// counter. This is a SAFETY bound on top of, not instead of, the real
// clock (samples_owed credit-pacing below, now windup-clamped -- C2-1) --
// see the module doc's sec 11.2 rewrite for why both are required
// together.
#define PL_A2DP_MAX_ENCODE_DWELL_US 6000u

// design sec 3.5 case 2 ("host silent"): no PCM available for >200ms while
// streaming is not a fault (the user paused) -- auto-pause and re-prime
// rather than starving the link forever. 200ms / 10ms tick = 20 ticks.
#define PL_A2DP_HOST_SILENT_TICKS 20

typedef enum {
    PL_A2DP_MEDIA_IDLE,
    // Holding the media stream until pl_pcm_fill_bytes() reaches
    // PL_PCM_TARGET_FILL_BYTES before calling a2dp_source_start_stream --
    // design sec 3.5 case 3, avoids a guaranteed underrun burst in the
    // first second of a fresh connection or an auto-resume.
    PL_A2DP_MEDIA_PRIMING,
    PL_A2DP_MEDIA_STREAMING,
} pl_a2dp_media_state_t;

typedef struct {
    uint16_t a2dp_cid;
    uint8_t local_seid;
    uint8_t remote_seid;
    bd_addr_t connect_addr; // for ConnectFailed's addr payload

    pl_codec_t *codec; // NULL until A2DP_SUBEVENT_SIGNALING_MEDIA_CODEC_SBC_CONFIGURATION
    pl_codec_format_t format;
    pl_codec_frame_info_t frame;

    pl_a2dp_media_state_t state;
    btstack_timer_source_t media_timer;
    bool timer_armed;

    int max_media_payload_size;
    uint32_t rtp_timestamp;
    uint8_t sbc_storage[SBC_STORAGE_SIZE];
    uint16_t sbc_storage_count;
    bool sbc_ready_to_send;

    uint32_t silent_ticks;
    bool pause_requested;
    bool auto_resume;

    // --- bead pico-link-pbv round 2 (C2-5): host-silent detection. The
    // auto-pause below must fire only on a REAL host-silent signal, never
    // on our own drain outrunning the ring (that is a counted bug signal --
    // underrun_events/silent_ticks above -- not a state transition). Tracks
    // the last-seen USB packet count and when it last changed; "unchanged
    // for >200ms while pl_usb_audio_streaming() is true" is host-silent,
    // same as "streaming went false" outright.
    uint32_t last_packet_count;
    uint64_t last_packet_change_us;

    // --- design sec 7 counters -- IRQ-context producer, thread-context
    // (pl_a2dp_report) consumer. Plain volatile, no formatting here. ---
    volatile uint32_t underrun_events;
    volatile uint32_t enc_max_us;
    volatile uint32_t pkt_sent;
    volatile uint32_t pkt_fail;

    // --- bead pico-link-pbv tick-cadence instrumentation: is the media
    // timer firing at its intended ~10ms cadence? Counters only, updated
    // from IRQ context (same producer as the block above), read from
    // pl_a2dp_report (thread context) -- never pl_log from the hot path
    // itself. last_tick_us doubles as the credit-pacing clock's own
    // elapsed-time source below -- one time_us_64() delta serves both.
    volatile uint64_t last_tick_us;
    volatile uint32_t worst_tick_interval_us;
    volatile uint32_t tick_count;
    // Cumulative frames actually encoded -- the RATE (delta between two
    // pl_a2dp_report samples, ~1s apart) is what the pbv acceptance
    // criterion calls "enc_frames": must read 375 +/- 2 per second in
    // steady state once credit-pacing is correct. Renamed from
    // frames_filled_total (pbv's original diagnostic-only name) to match
    // the design's own vocabulary now that it is a real pass/fail signal,
    // not just a diagnostic.
    volatile uint32_t enc_frames_total;
    // Tests a specific hypothesis: does the fill loop stall waiting on
    // BTstack's async A2DP_SUBEVENT_STREAMING_CAN_SEND_MEDIA_PACKET_NOW
    // grant (the sbc_ready_to_send handoff below) for a meaningful
    // fraction of ticks? Counter only. Measured 0 for the entire pbv
    // investigation run -- kept as an ongoing regression check.
    volatile uint32_t ticks_send_pending;

    // --- bead pico-link-pbv FIX (Ada's design, accepted 2026-08-29):
    // credit-pacing gives pl_a2dp_fill_sbc_buffer a real clock instead of
    // being a pure data-driven drain (design sec 1, forbidden verbatim at
    // .planning/design/2026-08-29-a2dp-source-pipeline.md lines 44-48).
    // samples_owed is PCM SAMPLE-FRAMES (not SBC-encoded frames -- stays
    // correct across codecs with a different pcm_frames_per_encoded_frame,
    // e.g. a future LDAC row) owed and not yet encoded, accrued every
    // media-timer tick from a REAL time_us_64() delta at microsecond
    // resolution: owed_us_hz = elapsed_us * sample_rate_hz +
    // samples_owed_rem_us (the sub-sample-frame remainder carried forward
    // so long-run accrual has ZERO systematic drift from truncation --
    // same idea as a2dp_source_demo.c's own acc_num_missed_samples, just
    // at us instead of ms resolution). The fill loop may encode a frame
    // only while samples_owed >= pcm_frames_per_encoded_frame, decrementing
    // it by that amount each time -- so steady-state throughput is exactly
    // sample_rate_hz/pcm_frames_per_encoded_frame (375/s at 48kHz/128),
    // pinned to the RP2350 crystal, independent of tick cadence, MTU, or
    // any per-tick cap. Both fields are RESET to 0 at STREAM_STARTED (not
    // before -- accrual during IDLE/PRIMING is harmless and discarded
    // there) so the credit clock always starts fresh exactly when real
    // streaming begins, never carrying a fake backlog from however long
    // priming took.
    volatile uint32_t samples_owed;
    volatile uint32_t samples_owed_rem_us;

    // --- bead pico-link-pbv ROUND 2 (Ada's design, accepted 2026-08-29):
    // round 1's windup bug. samples_owed had no upper clamp, so any tick
    // where the drain fell behind real time (a late tick, a starved tick)
    // converted that lag PERMANENTLY into stored credit that never expired
    // -- once it exceeded one packet's worth, the credit-clock stop
    // condition (see pl_a2dp_fill_sbc_buffer) stopped firing and the drain
    // degenerated back into the pure data-driven form design sec 1 forbids.
    // C2-1: samples_owed is now clamped every tick. ROUND 3 (R3-1)
    // corrects the clamp BOUND itself: round 2's bound (exactly one media
    // packet's worth) sits precisely on the drain's natural steady-state
    // operating point (measured ~7.2 frames against a 7-frame bound), so
    // it stopped being a windup guard and became an in-band regulator that
    // converts ordinary tick jitter into permanently destroyed credit --
    // see pl_a2dp_media_timer_handler's clamp site for the corrected
    // derivation (one packet + this tick's real accrual + one frame,
    // still self-scaling, still incapable of accumulating across ticks).
    //
    // R3-2: credit_clamped (encoded frames, ROUNDED DOWN via integer
    // division) is RETIRED -- round 3's measurement showed that rounding
    // silently undercounts roughly half of the real loss (every sub-frame
    // clamp event was invisible; every super-frame event lost its
    // fractional part), which is exactly why round 2's conservation
    // check could not close. Replaced with two exact counters:
    // credit_clamped_samples (cumulative PCM SAMPLES discarded by the
    // clamp, no division, no truncation -- feeds the conservation
    // identity directly) and credit_clamp_events (ticks on which the
    // clamp bound; expected ~0/s once R3-1 lands, see acceptance A3-4).
    volatile uint32_t credit_clamped_samples;
    volatile uint32_t credit_clamp_events;

    // Computed once per stream at STREAM_ESTABLISHED. How many
    // frame_bytes-sized encoded frames fit in one negotiated AVDTP media
    // payload -- used both by C2-1's clamp above and C2-4's priming-cushion
    // derivation below. NOT a per-tick loop bound any more (round 1's
    // frames_per_tick_cap/ticks_cap_bound are GONE -- round 2 replaced the
    // frame-count dwell cap with a real TIME bound inside the fill loop
    // itself, see PL_A2DP_MAX_ENCODE_DWELL_US and stop_dwell below).
    uint32_t frames_per_packet;

    // --- bead pico-link-pbv round 2 (C2-2): stop-REASON instrumentation,
    // replacing the single (and, round 2 found, arithmetically
    // indistinguishable-from-healthy) ticks_cap_bound. Each counts the
    // actual break site reached in pl_a2dp_fill_sbc_buffer's loop -- see
    // that function's doc comment for what each one means and why
    // stop_dwell, not stop_credit/stop_packet_full/stop_ring_empty, is the
    // one true safety-trip signal that must read 0 in a healthy run.
    volatile uint32_t stop_credit;
    volatile uint32_t stop_packet_full;
    volatile uint32_t stop_ring_empty;
    volatile uint32_t stop_dwell;

    // Bead pico-link-okx D10: the uncounted break below (pl_a2dp_fill_sbc_buffer,
    // "got != pcm_bytes_needed") -- the ring's fill_bytes() promised at
    // least pcm_bytes_needed but pl_pcm_read() returned less. Shouldn't
    // happen; counted so a conservation check can tell "never happens"
    // from "silently happens sometimes" instead of assuming the former.
    volatile uint32_t fill_short_read;

    // Bead pico-link-pbv ROUND 3 (R3-3): tripwire for the one-packet-per-
    // tick output ceiling (626 frames/s at today's MTU, 1.67x the 375
    // frames/s requirement) that round 3's design deliberately does NOT
    // build a multi-packet-per-tick queue for -- see the design's sec 8
    // sustainability note. Incremented at the packet-full break in
    // pl_a2dp_fill_sbc_buffer ONLY when a WHOLE packet's worth of credit
    // was already owed and stranded by lack of room (samples_owed >=
    // frames_per_packet * pcm_frames_per_encoded_frame at that break).
    // Predicted 0 today. If this ever reads persistently nonzero, the
    // ceiling is genuinely binding and the deferred multi-packet queue
    // (bead pico-link-85v) becomes required work, not speculation.
    volatile uint32_t stop_packet_full_hot;

    // Bead pico-link-pbv round 2 (C2-6): cumulative whole frames dropped by
    // pl_pcm_reset() (a2dp.c's STREAM_ESTABLISHED/SUSPENDED/RELEASED
    // handlers) -- an uncounted route PCM leaves the ring by, outside
    // pl_a2dp_fill_sbc_buffer's own accounting. Without this the
    // drain-vs-supply books cannot be balanced (acceptance A1,
    // conservation) -- this is exactly why round 1's enc_frames rate
    // looked arithmetically impossible against ovr_frames=0.
    volatile uint32_t flush_frames;

    // Bead pico-link-pbv round 2 (C2-3/C2-4): round 1 trimmed the ring down
    // to exactly PL_PCM_TARGET_FILL_BYTES at STREAM_STARTED, which discards
    // the very cushion that keeps one late tick from reaching zero -- C2-3
    // deletes that trim outright. C2-4 replaces the priming wait condition
    // (pl_pcm_fill_bytes() >= this) with a value DERIVED at
    // STREAM_ESTABLISHED from what was actually negotiated (one media
    // packet's worth of PCM bytes) plus one ISO packet plus the measured
    // tick jitter, floored at PL_PCM_TARGET_FILL_BYTES so a fresh
    // connection (no jitter measurement yet) still primes to a sane
    // minimum. See the STREAM_ESTABLISHED handler for the derivation.
    uint32_t priming_target_bytes;

    // Bead pico-link-pbv (C5), UNCHANGED by round 2's C2-3 (which deletes
    // the call site that used to increment this, not the field itself --
    // acceptance's conservation check (A1) still sums this term). Always 0
    // now that nothing calls pl_pcm_trim_to() any more.
    volatile uint32_t resync_drops;
} pl_a2dp_ctx_t;

static pl_a2dp_ctx_t s_ctx;

// SDP service record buffers -- sized exactly like a2dp_source_demo.c's
// own (sdp_a2dp_source_service_buffer[150] / sdp_avrcp_target_service_buffer[200] /
// sdp_avrcp_controller_service_buffer[200] / device_id_sdp_service_buffer[100]).
static uint8_t s_sdp_a2dp_source_buf[150];
static uint8_t s_sdp_avrcp_target_buf[200];
static uint8_t s_sdp_avrcp_controller_buf[200];
static uint8_t s_sdp_device_id_buf[100];

// Encode scratch -- file-scope static (not a stack array) matching
// usb_audio.c's pl_usb_audio_task's own `static uint8_t scratch[256]`
// convention for IRQ-context buffers. 256 stereo int16 frames is
// generously above SBC's 128 (8 subbands x 16 blocks); a future codec
// needing more would need this to grow, guarded below.
static int16_t s_pcm_scratch[256 * 2];

static void pl_a2dp_avrcp_packet_handler(uint8_t packet_type, uint16_t channel, uint8_t *packet, uint16_t size) {
    (void)channel;
    (void)size;
    if (packet_type != HCI_EVENT_PACKET) {
        return;
    }
    if (hci_event_packet_get_type(packet) != HCI_EVENT_AVRCP_META) {
        return;
    }
    switch (packet[2]) {
        case AVRCP_SUBEVENT_CONNECTION_ESTABLISHED:
            pl_log("avrcp: connection established\r\n");
            break;
        case AVRCP_SUBEVENT_CONNECTION_RELEASED:
            pl_log("avrcp: connection released\r\n");
            break;
        default:
            break;
    }
}

// S1 registers AVRCP Target/Controller purely so a sink that opens an
// AVRCP channel unprompted (common real-hardware behaviour right after
// A2DP connects) gets a clean accept against a registered service instead
// of failing against an unregistered PSM -- design sec 9's rationale for
// sizing L2CAP channels/services around AVRCP even though S1 doesn't act
// on transport controls. Acting on play/pause/volume/metadata queries is
// out of scope here -- deliberately deferred, not implemented.
static void pl_a2dp_avrcp_target_packet_handler(uint8_t packet_type, uint16_t channel, uint8_t *packet, uint16_t size) {
    (void)packet_type;
    (void)channel;
    (void)packet;
    (void)size;
}

static void pl_a2dp_avrcp_controller_packet_handler(uint8_t packet_type, uint16_t channel, uint8_t *packet, uint16_t size) {
    (void)packet_type;
    (void)channel;
    (void)packet;
    (void)size;
}

// Bead pico-link-pbv ROUND 3 (R3-4): the one true usable-payload
// computation. C2-10 established that the packet actually sent is
// sbc_storage_count + 1 bytes (the SBC media header byte), so the usable
// capacity for encoded-frame DATA is max_media_payload_size - 1, not
// max_media_payload_size -- but before round 3 that correction had been
// applied at the fill loop's packet-full check and the send-now trigger
// (both call sites below) while frames_per_packet's own computation at
// STREAM_ESTABLISHED still divided by the UNCORRECTED
// max_media_payload_size, a one-frame-too-generous off-by-one that was
// LATENT (today's MTU isn't an exact multiple of encoded_frame_bytes, so
// it never actually claimed a 7th frame that didn't fit) but stops being
// latent the moment R3-1 makes the credit clamp bound depend on
// frames_per_packet. One function, used at all three sites, so the
// correction can never drift out of sync again.
static inline uint32_t pl_a2dp_usable_payload(int max_media_payload_size) {
    return max_media_payload_size > 0 ? (uint32_t)(max_media_payload_size - 1) : 0u;
}

// Fills s_ctx.sbc_storage from the PCM ring under CREDIT PACING (bead
// pico-link-pbv fix -- see the module doc's sec 11.2 rewrite and
// s_ctx.samples_owed's doc comment for why this replaced a pure
// data-driven drain). IRQ context (0xFF) -- see this file's module doc.
// No allocation (s_pcm_scratch is static), no logging, no blocking;
// codec->encode() carries the same contract (codec_table.h).
//
// Bead pico-link-pbv ROUND 2 (C2-2): the loop is no longer bounded by a
// per-tick FRAME-COUNT cap (round 1's frames_per_tick_cap/ticks_cap_bound
// are gone -- round 2 found ticks_cap_bound arithmetically indistinguishable
// from a healthy steady state whenever the cap equals frames_per_packet).
// Instead it is bounded by a real TIME budget: dwell_us accumulates the
// already-measured per-encode dt below and the loop stops once it reaches
// PL_A2DP_MAX_ENCODE_DWELL_US, same safety property (bounded worst-case IRQ
// dwell), now correctly a wall-clock bound instead of a frame-count proxy
// for one. stop_dwell is the true safety-trip counter this produces --
// nonzero in steady state means the real dwell-safety bound (not the
// credit clock or the packet size) is the actual limiter, which must never
// happen with C2-1's clamp in place.
//
// The four stop conditions are checked IN THIS ORDER -- dwell, credit,
// packet-full, ring-empty -- because the order is what makes `starved`
// (and therefore underrun_events/silent_ticks) mean anything. Under credit
// pacing, frames_this_tick == 0 ROUTINELY means "the clock owed less than
// one whole frame this tick" (normal -- most ticks fire faster than one
// SBC frame's worth of real time, ~2.67ms at 48kHz/128), NOT "the ring is
// empty". Only the ring-empty check, reached with the clock AND packet
// room both still willing, is a real starvation signal -- conflating the
// two would fire underrun_events on ordinary ticks, mid-music.
static void pl_a2dp_fill_sbc_buffer(void) {
    uint16_t frame_bytes = s_ctx.frame.encoded_frame_bytes;
    uint16_t pcm_frame_count = s_ctx.frame.pcm_frames_per_encoded_frame;
    uint32_t pcm_bytes_needed = (uint32_t)pcm_frame_count * PL_PCM_FRAME_BYTES;

    if (pcm_bytes_needed == 0 || pcm_bytes_needed > sizeof(s_pcm_scratch)) {
        // Would overflow s_pcm_scratch -- codec_table.h's frame_info
        // contract was violated (S1's only row, SBC at 128 PCM
        // frames/encoded frame, never hits this). Defensive only.
        s_ctx.pkt_fail++;
        return;
    }

    // Bead pico-link-pbv round 2 (C2-10), round 3 (R3-4): see
    // pl_a2dp_usable_payload's doc comment for why this must be the one
    // shared computation.
    uint32_t usable_payload = pl_a2dp_usable_payload(s_ctx.max_media_payload_size);

    uint8_t frames_this_tick = 0;
    bool starved = false;
    uint32_t dwell_us = 0;
    for (;;) {
        // 0. Dwell safety: has this tick's fill loop already consumed the
        // IRQ-dwell safety budget? The one true safety-trip stop reason --
        // see this function's doc comment.
        if (dwell_us >= PL_A2DP_MAX_ENCODE_DWELL_US) {
            s_ctx.stop_dwell++;
            break;
        }
        // 1. Credit: has the real-time clock actually owed us a whole
        // encoded frame's worth of samples yet? Most-common stop reason
        // by far in steady state -- not a fault.
        if (s_ctx.samples_owed < pcm_frame_count) {
            s_ctx.stop_credit++;
            break;
        }
        // 2. Packet-full: is there room for one more frame in the current
        // AVDTP payload? Also not a fault -- just means it's time to send.
        if ((uint32_t)(s_ctx.sbc_storage_count + frame_bytes) > usable_payload) {
            s_ctx.stop_packet_full++;
            // Bead pico-link-pbv ROUND 3 (R3-3): tripwire for the
            // deliberately-not-built-for one-packet-per-tick ceiling --
            // see stop_packet_full_hot's doc comment on pl_a2dp_ctx_t.
            // Only "hot" when a WHOLE packet's worth of credit is already
            // owed and stranded by lack of room, not merely a partial one.
            if (s_ctx.frames_per_packet > 0 && pcm_frame_count > 0 &&
                s_ctx.samples_owed >= s_ctx.frames_per_packet * (uint32_t)pcm_frame_count) {
                s_ctx.stop_packet_full_hot++;
            }
            break;
        }
        // 3. Ring-empty: the clock says we should encode and there is
        // room to, but the ring genuinely has nothing to give us. This is
        // the ONE true starvation signal.
        if (pl_pcm_fill_bytes() < pcm_bytes_needed) {
            starved = true;
            s_ctx.stop_ring_empty++;
            break;
        }

        uint32_t got = pl_pcm_read((uint8_t *)s_pcm_scratch, pcm_bytes_needed);
        if (got != pcm_bytes_needed) {
            // Bead pico-link-okx D10: was an uncounted break. Ring gave
            // less than its own fill_bytes() promised -- shouldn't happen,
            // defend anyway, but now visible if it does.
            s_ctx.fill_short_read++;
            break;
        }

        uint64_t t0 = time_us_64();
        uint16_t written = s_ctx.codec->encode(
            s_ctx.codec->state, s_pcm_scratch, &s_ctx.sbc_storage[1 + s_ctx.sbc_storage_count],
            (uint16_t)(sizeof(s_ctx.sbc_storage) - 1 - s_ctx.sbc_storage_count)
        );
        uint32_t dt = (uint32_t)(time_us_64() - t0);
        if (dt > s_ctx.enc_max_us) {
            s_ctx.enc_max_us = dt;
        }
        dwell_us += dt;
        if (written == 0) {
            s_ctx.pkt_fail++;
            break;
        }

        s_ctx.sbc_storage_count = (uint16_t)(s_ctx.sbc_storage_count + written);
        s_ctx.samples_owed -= pcm_frame_count;
        frames_this_tick++;
    }

    // Cumulative -- see enc_frames_total's doc comment on pl_a2dp_ctx_t
    // for the acceptance-criterion rate this feeds.
    s_ctx.enc_frames_total += frames_this_tick;

    if (starved) {
        s_ctx.underrun_events++;
        s_ctx.silent_ticks++;
    } else {
        s_ctx.silent_ticks = 0;
    }
}

// Sends whatever is currently queued in s_ctx.sbc_storage as one AVDTP
// media packet -- mirrors a2dp_demo_send_media_packet exactly (SBC media
// header byte = frame count, RTP timestamp advances by frames *
// pcm_frames_per_encoded_frame). Called from
// A2DP_SUBEVENT_STREAMING_CAN_SEND_MEDIA_PACKET_NOW, IRQ context.
static void pl_a2dp_send_media_packet(void) {
    uint16_t frame_bytes = s_ctx.frame.encoded_frame_bytes;
    uint16_t bytes_in_storage = s_ctx.sbc_storage_count;
    uint8_t num_frames = frame_bytes > 0 ? (uint8_t)(bytes_in_storage / frame_bytes) : 0;

    s_ctx.sbc_storage[0] = num_frames; // (fragmentation<<7)|(start<<6)|(last<<5)|num_frames -- no fragmentation here
    uint8_t status = a2dp_source_stream_send_media_payload_rtp(
        s_ctx.a2dp_cid, s_ctx.local_seid, 0, s_ctx.rtp_timestamp, s_ctx.sbc_storage, (uint16_t)(bytes_in_storage + 1)
    );
    if (status == ERROR_CODE_SUCCESS) {
        s_ctx.pkt_sent++;
    } else {
        s_ctx.pkt_fail++;
    }

    s_ctx.rtp_timestamp += (uint32_t)num_frames * s_ctx.frame.pcm_frames_per_encoded_frame;
    s_ctx.sbc_storage_count = 0;
    s_ctx.sbc_ready_to_send = false;
}

static void pl_a2dp_media_timer_handler(btstack_timer_source_t *ts) {
    btstack_run_loop_set_timer(ts, PL_A2DP_AUDIO_TIMEOUT_MS);
    btstack_run_loop_add_timer(ts);

    // Bead pico-link-ufh: proves the A2DP media timer is ticking. Enabled
    // only while streaming (see the STREAM_STARTED/SUSPENDED/RELEASED/
    // SIGNALING_CONNECTION_RELEASED handlers below) -- a single volatile
    // increment, not a pl_log call, so this does not violate this file's
    // IRQ-context contract above.
    pl_wdt_kick(PL_WDT_MEDIA);

    // Bead pico-link-pbv: measure the REAL interval between calls to this
    // handler, not the nominal PL_A2DP_AUDIO_TIMEOUT_MS -- no pl_log,
    // matches usb_pump.c's own worst_interval_us pattern for exactly this
    // class of measurement. The SAME elapsed_us also drives credit-pacing
    // accrual below (s_ctx.samples_owed's doc comment) -- one
    // time_us_64() delta, two consumers.
    uint64_t pbv_now_us = time_us_64();
    if (s_ctx.last_tick_us != 0) {
        uint32_t elapsed_us = (uint32_t)(pbv_now_us - s_ctx.last_tick_us);
        if (elapsed_us > s_ctx.worst_tick_interval_us) {
            s_ctx.worst_tick_interval_us = elapsed_us;
        }

        // Credit-pacing accrual (bead pico-link-pbv fix). Accrues
        // unconditionally every tick, including IDLE/PRIMING -- harmless,
        // since STREAM_STARTED resets samples_owed/samples_owed_rem_us to
        // 0, so accrual only ever matters from the instant real streaming
        // begins. sample_rate_hz falls back to 48000 before the first
        // codec negotiation has populated s_ctx.format (a2dp.c never
        // reaches STREAMING before that anyway, so this only affects
        // idle-tick bookkeeping that gets discarded regardless).
        uint32_t sample_rate = s_ctx.format.sample_rate_hz != 0 ? s_ctx.format.sample_rate_hz : 48000u;
        uint64_t owed_us_hz = (uint64_t)elapsed_us * sample_rate + s_ctx.samples_owed_rem_us;
        // R3-1 reuses this exact value (the real PCM-sample-frames accrued
        // THIS tick, not a historical worst-case) as one term of the
        // corrected clamp bound below -- see that comment for why.
        uint32_t accrued_this_tick = (uint32_t)(owed_us_hz / 1000000u);
        s_ctx.samples_owed += accrued_this_tick;
        s_ctx.samples_owed_rem_us = (uint32_t)(owed_us_hz % 1000000u);

        // Bead pico-link-pbv ROUND 3 (R3-1): clamp the credit to the
        // largest backlog the drain can genuinely retire, not to the size
        // of one packet. Round 2's bound (exactly one packet) sits
        // precisely on the drain's natural steady-state operating point
        // (measured ~7.2 frames against a 7-frame bound), so it stopped
        // being a windup guard and became an in-band regulator that
        // converts ordinary tick jitter into permanently destroyed credit
        // -- see credit_clamped_samples's doc comment on pl_a2dp_ctx_t for
        // the full mechanism and the design-round-3 bead comment for the
        // hand simulation that shows the cycle pressing against the old
        // barrier every cycle.
        //
        // The corrected bound is three DERIVED terms, no history, no
        // stored worst-case: one packet (with an empty buffer the loop can
        // retire frames_per_packet frames in a single tick, so credit up
        // to that is immediately usable -- never windup); plus THIS TICK'S
        // real accrual (accrued_this_tick above -- the debt that
        // legitimately arrived while the drain was blocked; deliberately
        // NOT worst_tick_interval_us, which is never reset and would let
        // one historical spike inflate the bound forever); plus one frame
        // (the sub-frame remainder pcm_frame_count granularity forces).
        // This is NOT a return to round 1: round 1 had no bound at all and
        // accumulated across arbitrarily many ticks, whereas this bound is
        // one packet plus exactly one tick of real elapsed time --
        // self-scaling and structurally incapable of accumulating.
        //
        // pcm_frame_count_for_clamp guards against clamping before a codec
        // has negotiated (frames_per_packet/pcm_frames_per_encoded_frame
        // both 0 pre-negotiation) -- accrual during IDLE/PRIMING is
        // discarded at STREAM_STARTED anyway (see samples_owed's doc
        // comment), so skipping the clamp there is harmless.
        uint32_t pcm_frame_count_for_clamp = s_ctx.frame.pcm_frames_per_encoded_frame;
        if (s_ctx.frames_per_packet > 0 && pcm_frame_count_for_clamp > 0) {
            uint32_t max_samples_owed = s_ctx.frames_per_packet * pcm_frame_count_for_clamp + accrued_this_tick +
                                         pcm_frame_count_for_clamp;
            if (s_ctx.samples_owed > max_samples_owed) {
                uint32_t excess_samples = s_ctx.samples_owed - max_samples_owed;
                s_ctx.samples_owed = max_samples_owed;
                // R3-2: exact accounting, no division, no truncation --
                // see credit_clamped_samples's doc comment for why round
                // 2's floor-divided credit_clamped could not close the
                // conservation identity.
                s_ctx.credit_clamped_samples += excess_samples;
                s_ctx.credit_clamp_events++;
            }
        }
    }
    s_ctx.last_tick_us = pbv_now_us;
    s_ctx.tick_count++;

    // Bead pico-link-pbv round 2 (C2-5): track whether the HOST is actually
    // still sending PCM, independent of our own drain state -- the only
    // input the auto-pause below may act on. A tick where the packet count
    // hasn't moved doesn't by itself mean silence (packets arrive faster
    // than 10ms ticks); host_silent only latches true once nothing has
    // arrived for over 200ms, or the streaming alt-setting itself dropped.
    uint32_t current_packet_count = pl_usb_audio_packet_count();
    if (current_packet_count != s_ctx.last_packet_count) {
        s_ctx.last_packet_count = current_packet_count;
        s_ctx.last_packet_change_us = pbv_now_us;
    }
    bool host_silent =
        !pl_usb_audio_streaming() || (s_ctx.last_packet_change_us != 0 && (pbv_now_us - s_ctx.last_packet_change_us) > 200000u);

    if (s_ctx.state == PL_A2DP_MEDIA_PRIMING) {
        // Bead pico-link-pbv round 2 (C2-4): prime to the structurally
        // derived cushion (s_ctx.priming_target_bytes, computed at
        // STREAM_ESTABLISHED), not the bare setpoint -- see that field's
        // doc comment.
        if (pl_pcm_fill_bytes() >= s_ctx.priming_target_bytes) {
            a2dp_source_start_stream(s_ctx.a2dp_cid, s_ctx.local_seid);
            // A2DP_SUBEVENT_STREAM_STARTED flips state to STREAMING.
        }
        return;
    }
    if (s_ctx.state != PL_A2DP_MEDIA_STREAMING) {
        return;
    }

    if (!s_ctx.sbc_ready_to_send) {
        pl_a2dp_fill_sbc_buffer();
        // Bead pico-link-pbv round 2 (C2-10), round 3 (R3-4): see
        // pl_a2dp_usable_payload's doc comment -- must match
        // pl_a2dp_fill_sbc_buffer's own usable_payload exactly.
        uint32_t usable_payload = pl_a2dp_usable_payload(s_ctx.max_media_payload_size);
        if ((uint32_t)(s_ctx.sbc_storage_count + s_ctx.frame.encoded_frame_bytes) > usable_payload) {
            s_ctx.sbc_ready_to_send = true;
            a2dp_source_stream_endpoint_request_can_send_now(s_ctx.a2dp_cid, s_ctx.local_seid);
        }
    } else {
        s_ctx.ticks_send_pending++;
    }

    // design sec 3.5 case 2: host silent -> not a fault. Auto-pause once,
    // wait for SUSPENDED, then re-prime (see the SUSPENDED case below).
    //
    // Bead pico-link-pbv round 2 (C2-5): gated on host_silent (computed
    // above from the actual USB packet-arrival signal), NOT merely on our
    // own silent_ticks/starved bookkeeping -- round 1 conflated "the host
    // stopped sending" (a real pause) with "our drain outran the ring" (an
    // internal fault) and its remedy (suspend, silent flush, re-prime) was
    // a ~300ms audible dropout that MANUFACTURED the oscillation this bead
    // exists to fix. Starvation while the host is still streaming is
    // already a counted bug signal (underrun_events, incremented in
    // pl_a2dp_fill_sbc_buffer on every starved tick regardless of this
    // branch) -- it must never become a state transition.
    if (s_ctx.silent_ticks >= PL_A2DP_HOST_SILENT_TICKS && !s_ctx.pause_requested && host_silent) {
        s_ctx.pause_requested = true;
        s_ctx.auto_resume = true;
        a2dp_source_pause_stream(s_ctx.a2dp_cid, s_ctx.local_seid);
    }
}

static void pl_a2dp_media_timer_arm(void) {
    btstack_run_loop_remove_timer(&s_ctx.media_timer); // safe even if not currently added
    btstack_run_loop_set_timer_handler(&s_ctx.media_timer, pl_a2dp_media_timer_handler);
    btstack_run_loop_set_timer(&s_ctx.media_timer, PL_A2DP_AUDIO_TIMEOUT_MS);
    btstack_run_loop_add_timer(&s_ctx.media_timer);
    s_ctx.timer_armed = true;
}

static void pl_a2dp_packet_handler(uint8_t packet_type, uint16_t channel, uint8_t *packet, uint16_t size) {
    (void)channel;
    (void)size;
    if (packet_type != HCI_EVENT_PACKET) {
        return;
    }
    if (hci_event_packet_get_type(packet) != HCI_EVENT_A2DP_META) {
        return;
    }

    switch (hci_event_a2dp_meta_get_subevent_code(packet)) {
        case A2DP_SUBEVENT_SIGNALING_CONNECTION_ESTABLISHED: {
            uint8_t status = a2dp_subevent_signaling_connection_established_get_status(packet);
            uint16_t cid = a2dp_subevent_signaling_connection_established_get_a2dp_cid(packet);
            if (status != ERROR_CODE_SUCCESS) {
                pl_log("a2dp: signaling connection FAILED status=0x%02x\r\n", status);
                s_ctx.a2dp_cid = 0;
                pl_bt_push_connect_failed(s_ctx.connect_addr, PL_FAILURE_REASON_REJECTED);
                break;
            }
            s_ctx.a2dp_cid = cid;
            pl_log("a2dp: signaling connected, cid=0x%02x\r\n", cid);
            break;
        }

        case A2DP_SUBEVENT_SIGNALING_MEDIA_CODEC_SBC_CONFIGURATION: {
            uint16_t cid = a2dp_subevent_signaling_media_codec_sbc_configuration_get_a2dp_cid(packet);
            if (cid != s_ctx.a2dp_cid) {
                break;
            }
            uint8_t local_seid = a2dp_subevent_signaling_media_codec_sbc_configuration_get_local_seid(packet);
            s_ctx.remote_seid = a2dp_subevent_signaling_media_codec_sbc_configuration_get_remote_seid(packet);

            // Decode into this row's own private negotiated-config shape
            // (codec_sbc.h's pl_codec_sbc_negotiated_t) -- SBC-subevent-
            // specific decode logic belongs here (this IS the one SBC
            // subevent handler S1 registers), not a codec-identity switch
            // in the sense design sec 4.2 forbids (that rule is about
            // a2dp.c's dispatch/table logic, not this necessarily-SBC-
            // shaped event decode). Field mapping mirrors
            // a2dp_source_demo.c's identical decode exactly, including
            // allocation_method's -1 adjustment (AVDTP wire value ->
            // btstack_sbc_allocation_method_t).
            pl_codec_sbc_negotiated_t cfg;
            cfg.num_channels = a2dp_subevent_signaling_media_codec_sbc_configuration_get_num_channels(packet);
            cfg.sampling_frequency = (int)a2dp_subevent_signaling_media_codec_sbc_configuration_get_sampling_frequency(packet);
            cfg.block_length = a2dp_subevent_signaling_media_codec_sbc_configuration_get_block_length(packet);
            cfg.subbands = a2dp_subevent_signaling_media_codec_sbc_configuration_get_subbands(packet);
            cfg.min_bitpool_value = a2dp_subevent_signaling_media_codec_sbc_configuration_get_min_bitpool_value(packet);
            cfg.max_bitpool_value = a2dp_subevent_signaling_media_codec_sbc_configuration_get_max_bitpool_value(packet);

            avdtp_channel_mode_t channel_mode =
                (avdtp_channel_mode_t)a2dp_subevent_signaling_media_codec_sbc_configuration_get_channel_mode(packet);
            uint8_t allocation_method = a2dp_subevent_signaling_media_codec_sbc_configuration_get_allocation_method(packet);
            cfg.allocation_method = (uint8_t)(allocation_method - 1);
            switch (channel_mode) {
                case AVDTP_CHANNEL_MODE_JOINT_STEREO:
                    cfg.channel_mode = SBC_CHANNEL_MODE_JOINT_STEREO;
                    break;
                case AVDTP_CHANNEL_MODE_STEREO:
                    cfg.channel_mode = SBC_CHANNEL_MODE_STEREO;
                    break;
                case AVDTP_CHANNEL_MODE_DUAL_CHANNEL:
                    cfg.channel_mode = SBC_CHANNEL_MODE_DUAL_CHANNEL;
                    break;
                case AVDTP_CHANNEL_MODE_MONO:
                    cfg.channel_mode = SBC_CHANNEL_MODE_MONO;
                    break;
                default:
                    cfg.channel_mode = SBC_CHANNEL_MODE_STEREO;
                    break;
            }

            // Table lookup by local_seid -- written generically (S1 has
            // one row; S4 adds a second). No call site here assumes SBC is
            // the only possible row.
            pl_codec_t *row = NULL;
            for (size_t i = 0; i < PL_CODEC_COUNT; i++) {
                if (PL_CODECS[i]->local_seid == local_seid) {
                    row = PL_CODECS[i];
                    break;
                }
            }
            if (row == NULL || !row->init(row->state, (const uint8_t *)&cfg, sizeof(cfg), &s_ctx.format, &s_ctx.frame)) {
                pl_log("a2dp: codec init FAILED for local_seid %u\r\n", local_seid);
                pl_bt_push_connect_failed(s_ctx.connect_addr, PL_FAILURE_REASON_NO_A2DP_SINK);
                break;
            }
            s_ctx.codec = row;
            s_ctx.local_seid = local_seid;
            pl_log(
                "a2dp: codec=%s sample_rate=%lu pcm_frames_per_frame=%u frame_bytes=%u nominal_bitrate=%lu\r\n",
                row->display_name, (unsigned long)s_ctx.format.sample_rate_hz, s_ctx.frame.pcm_frames_per_encoded_frame,
                s_ctx.frame.encoded_frame_bytes, (unsigned long)s_ctx.frame.nominal_bitrate_bps
            );
            pl_bt_push_connect_step(PL_CONNECT_STEP_NEGOTIATING_CODEC);

            // Bead pico-link-1v5: tells the Home hero which codec is now
            // live and at what nominal bitrate, so it stops reading
            // "NO LINK" once a device is actually connected. Narrow
            // addition at the one place codec negotiation completes --
            // never called from the media timer path (s_ctx.frame is
            // already fully populated by row->init above, so this reads
            // only settled state).
            pl_bt_push_codec_changed(
                s_ctx.connect_addr, row->display_name, (uint8_t)strlen(row->display_name),
                s_ctx.frame.nominal_bitrate_bps
            );
            break;
        }

        case A2DP_SUBEVENT_SIGNALING_CAPABILITIES_DONE:
        case A2DP_SUBEVENT_SIGNALING_DELAY_REPORTING_CAPABILITY:
        case A2DP_SUBEVENT_SIGNALING_DELAY_REPORT:
            // Not acted on in S1 (no explicit negotiation -- design sec
            // 4.1) but harmless to receive; no logging (would flood at
            // signalling time) beyond what BTstack's own ENABLE_LOG_INFO
            // already does.
            break;

        case A2DP_SUBEVENT_STREAM_ESTABLISHED: {
            uint8_t status = a2dp_subevent_stream_established_get_status(packet);
            if (status != ERROR_CODE_SUCCESS) {
                pl_log("a2dp: stream establish FAILED status=0x%02x\r\n", status);
                pl_bt_push_connect_failed(s_ctx.connect_addr, PL_FAILURE_REASON_REJECTED);
                break;
            }
            s_ctx.local_seid = a2dp_subevent_stream_established_get_local_seid(packet);
            int mtu = a2dp_max_media_payload_size(s_ctx.a2dp_cid, s_ctx.local_seid);
            s_ctx.max_media_payload_size = btstack_min(mtu, (int)sizeof(s_ctx.sbc_storage) - 1);
            s_ctx.sbc_storage_count = 0;
            s_ctx.sbc_ready_to_send = false;
            s_ctx.rtp_timestamp = 0;
            s_ctx.silent_ticks = 0;
            s_ctx.pause_requested = false;

            // Bead pico-link-pbv (C3, round 1): frames_per_packet --
            // computed from what was ACTUALLY negotiated (this
            // connection's real payload size), not a guessed-at fixed
            // number. Round 2 (C2-1/C2-4) reuses this same value for the
            // credit clamp and the priming-cushion derivation below;
            // round 2 also RETIRES the dwell_cap/frames_per_tick_cap half
            // of round 1's computation -- pl_a2dp_fill_sbc_buffer now
            // bounds dwell with a real TIME check against
            // PL_A2DP_MAX_ENCODE_DWELL_US directly, not a frame-count
            // proxy for one. Round 3 (R3-4): divide by
            // pl_a2dp_usable_payload's corrected capacity, not the raw
            // negotiated size -- see that helper's doc comment for why
            // using the uncorrected size here was a latent off-by-one that
            // R3-1 would have turned active.
            s_ctx.frames_per_packet = s_ctx.frame.encoded_frame_bytes > 0
                                           ? btstack_max(
                                                 1u,
                                                 pl_a2dp_usable_payload(s_ctx.max_media_payload_size) /
                                                     s_ctx.frame.encoded_frame_bytes
                                             )
                                           : 1u;

            // Bead pico-link-pbv round 2 (C2-4): the priming cushion is
            // DERIVED, not hand-picked -- one media packet's worth of PCM
            // bytes (the consumer's own excursion size) plus one ISO
            // packet (192B, the ~1ms USB audio rate) plus the measured
            // tick jitter (worst_tick_interval_us, in bytes at 192B/ms).
            // Floored at PL_PCM_TARGET_FILL_BYTES: on a FRESH connection
            // (this stream's own media timer hasn't ticked yet, so
            // worst_tick_interval_us may still read whatever a PRIOR
            // stream left behind, or 0) the derived value could otherwise
            // be too small -- the macro is the floor that guarantees a
            // sane minimum regardless.
            uint32_t one_packet_bytes =
                s_ctx.frames_per_packet * (uint32_t)s_ctx.frame.pcm_frames_per_encoded_frame * PL_PCM_FRAME_BYTES;
            uint32_t one_iso_packet_bytes = 192u;
            uint32_t jitter_bytes = (s_ctx.worst_tick_interval_us / 1000u) * 192u;
            uint32_t derived_cushion = one_packet_bytes + one_iso_packet_bytes + jitter_bytes;
            s_ctx.priming_target_bytes = btstack_max(derived_cushion, PL_PCM_TARGET_FILL_BYTES);

            // Signalling context, not the media hot path -- pl_log is
            // fine here (see this file's module doc). Startup-only line,
            // not rate-limited: fires once per stream, same as the
            // "stream established"/"codec=" lines already here.
            pl_log(
                "a2dp: frames_per_packet=%lu priming_target_bytes=%lu (one_packet_bytes=%lu jitter_bytes=%lu)\r\n",
                (unsigned long)s_ctx.frames_per_packet, (unsigned long)s_ctx.priming_target_bytes,
                (unsigned long)one_packet_bytes, (unsigned long)jitter_bytes
            );

            // Bead pico-link-pbv (C1): without this, the ~31KB already
            // sitting in the ring from before this connection (or from a
            // reflash-then-reconnect cycle during testing) never drains --
            // once the credit clock makes drain rate exactly equal supply
            // rate by construction, the controller's own +/-500ppm
            // authority (design sec 2.1) is the ONLY thing that can ever
            // reduce fill, and 500ppm of 192 B/ms is ~0.096 B/ms: clearing
            // a 31KB head start would take on the order of 280 SECONDS.
            // pl_pcm_reset() before PRIMING starts is the fix -- see
            // pcm_ring.h's doc comment; consumer-side, safe to call here.
            // Bead pico-link-pbv round 2 (C2-6): count the discard.
            s_ctx.flush_frames += pl_pcm_reset();

            pl_log("a2dp: stream established, max_media_payload_size=%d\r\n", s_ctx.max_media_payload_size);
            // design sec 3.5 case 3: prime before starting -- see this
            // file's PL_A2DP_MEDIA_PRIMING doc comment.
            s_ctx.state = PL_A2DP_MEDIA_PRIMING;
            pl_a2dp_media_timer_arm();
            break;
        }

        case A2DP_SUBEVENT_STREAM_STARTED:
            // Bead pico-link-ufh: PL_WDT_MEDIA is only meaningful while
            // actually streaming -- enabling it re-bases its deadline, so a
            // stream that was idle/priming beforehand is never judged stale
            // against history from before real streaming began.
            pl_wdt_set_enabled(PL_WDT_MEDIA, true);
            s_ctx.state = PL_A2DP_MEDIA_STREAMING;
            s_ctx.silent_ticks = 0;
            s_ctx.pause_requested = false;
            // Bead pico-link-pbv (C4): the credit-pacing clock starts
            // fresh exactly at the instant real streaming begins -- any
            // accrual from IDLE/PRIMING ticks (harmless, unconditional --
            // see the media timer handler's doc comment) is discarded
            // here rather than carried forward as a false backlog that
            // would let the very first ticks of streaming burst ahead of
            // real time.
            s_ctx.samples_owed = 0;
            s_ctx.samples_owed_rem_us = 0;
            // Bead pico-link-pbv round 2 (C2-3): round 1's trim-to-target
            // here is DELETED -- it discarded exactly the cushion that
            // keeps one late tick from reaching zero (PRIMING now waits
            // for the derived priming_target_bytes cushion instead, C2-4
            // above; there is no overshoot left worth trimming away, and
            // trimming it destroyed the margin the fix depends on). Prime
            // to the target and start; do not trim. resync_drops (see its
            // doc comment on pl_a2dp_ctx_t) stays 0 now that nothing calls
            // pl_pcm_trim_to() any more.
            //
            // Bead pico-link-pbv round 2 (C2-5): (re)establish the
            // host-silence baseline exactly at the instant real streaming
            // begins, same reasoning as the credit-clock reset above --
            // any packet-count/timestamp state from before this stream
            // (or from PRIMING) must not be read as "the host just went
            // silent".
            s_ctx.last_packet_count = pl_usb_audio_packet_count();
            s_ctx.last_packet_change_us = time_us_64();
            // Bead pico-link-pbv round 2 (C2-9): seed the feedback loop's
            // EMA to the actual current fill and reset the windowed
            // fill_min -- see pl_usb_audio_fb_reset's doc comment.
            pl_usb_audio_fb_reset();
            pl_log("a2dp: stream started\r\n");
            pl_bt_push_link_state_connected();
            // S1's table has exactly one row -- SBC is never "not the
            // first row we'd have accepted", so degraded is always false
            // here (design sec 4.3). S4 computes this for real once a
            // second row exists.
            pl_bt_push_connect_succeeded(false);
            break;

        case A2DP_SUBEVENT_STREAM_SUSPENDED:
            pl_wdt_set_enabled(PL_WDT_MEDIA, false);
            pl_log("a2dp: stream suspended (auto_resume=%d)\r\n", s_ctx.auto_resume ? 1 : 0);
            s_ctx.sbc_storage_count = 0;
            s_ctx.sbc_ready_to_send = false;
            s_ctx.silent_ticks = 0;
            s_ctx.pause_requested = false;
            // Bead pico-link-pbv round 2 (C2-6): count the discard.
            s_ctx.flush_frames += pl_pcm_reset();
            s_ctx.state = s_ctx.auto_resume ? PL_A2DP_MEDIA_PRIMING : PL_A2DP_MEDIA_IDLE;
            s_ctx.auto_resume = false;
            break;

        case A2DP_SUBEVENT_STREAM_RELEASED:
            pl_wdt_set_enabled(PL_WDT_MEDIA, false);
            pl_log("a2dp: stream released\r\n");
            s_ctx.state = PL_A2DP_MEDIA_IDLE;
            s_ctx.codec = NULL;
            if (s_ctx.timer_armed) {
                btstack_run_loop_remove_timer(&s_ctx.media_timer);
                s_ctx.timer_armed = false;
            }
            // Bead pico-link-pbv round 2 (C2-6): count the discard.
            s_ctx.flush_frames += pl_pcm_reset();
            break;

        case A2DP_SUBEVENT_SIGNALING_CONNECTION_RELEASED:
            pl_wdt_set_enabled(PL_WDT_MEDIA, false);
            pl_log("a2dp: signaling connection released\r\n");
            s_ctx.a2dp_cid = 0;
            s_ctx.codec = NULL;
            s_ctx.state = PL_A2DP_MEDIA_IDLE;
            if (s_ctx.timer_armed) {
                btstack_run_loop_remove_timer(&s_ctx.media_timer);
                s_ctx.timer_armed = false;
            }
            break;

        case A2DP_SUBEVENT_STREAMING_CAN_SEND_MEDIA_PACKET_NOW:
            pl_a2dp_send_media_packet();
            break;

        default:
            break;
    }
}

void pl_a2dp_init(struct PlUi *ui) {
    // Events flow to Rust through bt.c's existing MPSC ring
    // (pl_bt_push_connect_step/_succeeded/_failed/_link_state_connected,
    // see bt.h) -- `ui` isn't touched directly here. Accepted anyway for
    // signature symmetry with pl_bt_init and in case a future stage (S2's
    // CodecChanged, design sec 6.1) needs it.
    (void)ui;

    l2cap_init();

    a2dp_source_init();
    a2dp_source_register_packet_handler(&pl_a2dp_packet_handler);

    for (size_t i = 0; i < PL_CODEC_COUNT; i++) {
        pl_codec_t *row = PL_CODECS[i];
        avdtp_stream_endpoint_t *ep = a2dp_source_create_stream_endpoint(
            AVDTP_AUDIO, row->avdtp_codec_type, row->capabilities, row->capabilities_len, row->configuration,
            row->configuration_len
        );
        if (ep == NULL) {
            pl_log("a2dp: FAILED to create stream endpoint for %s\r\n", row->display_name);
            continue;
        }
        // 48000 to match usb_audio.c's only offered UAC2 rate (M3 scope) --
        // avoids a resample step neither side of this pipeline implements.
        avdtp_set_preferred_sampling_frequency(ep, 48000);
        row->local_seid = avdtp_local_seid(ep);
        avdtp_source_register_delay_reporting_category(row->local_seid);
    }

    avrcp_init();
    avrcp_register_packet_handler(&pl_a2dp_avrcp_packet_handler);
    avrcp_target_init();
    avrcp_target_register_packet_handler(&pl_a2dp_avrcp_target_packet_handler);
    avrcp_controller_init();
    avrcp_controller_register_packet_handler(&pl_a2dp_avrcp_controller_packet_handler);

    sdp_init();

    memset(s_sdp_a2dp_source_buf, 0, sizeof(s_sdp_a2dp_source_buf));
    a2dp_source_create_sdp_record(
        s_sdp_a2dp_source_buf, sdp_create_service_record_handle(), AVDTP_SOURCE_FEATURE_MASK_PLAYER, NULL, NULL
    );
    sdp_register_service(s_sdp_a2dp_source_buf);

    memset(s_sdp_avrcp_target_buf, 0, sizeof(s_sdp_avrcp_target_buf));
    avrcp_target_create_sdp_record(
        s_sdp_avrcp_target_buf, sdp_create_service_record_handle(), AVRCP_FEATURE_MASK_CATEGORY_PLAYER_OR_RECORDER, NULL,
        NULL
    );
    sdp_register_service(s_sdp_avrcp_target_buf);

    memset(s_sdp_avrcp_controller_buf, 0, sizeof(s_sdp_avrcp_controller_buf));
    avrcp_controller_create_sdp_record(
        s_sdp_avrcp_controller_buf, sdp_create_service_record_handle(), AVRCP_FEATURE_MASK_CATEGORY_MONITOR_OR_AMPLIFIER,
        NULL, NULL
    );
    sdp_register_service(s_sdp_avrcp_controller_buf);

    memset(s_sdp_device_id_buf, 0, sizeof(s_sdp_device_id_buf));
    device_id_create_sdp_record(
        s_sdp_device_id_buf, sdp_create_service_record_handle(), DEVICE_ID_VENDOR_ID_SOURCE_BLUETOOTH,
        BLUETOOTH_COMPANY_ID_BLUEKITCHEN_GMBH, 1, 1
    );
    sdp_register_service(s_sdp_device_id_buf);

    gap_set_local_name("Pico Link 00:00:00:00:00:00");
    gap_discoverable_control(1);
    // Audio/Video, Rendering -- matches a2dp_source_demo.c's own
    // gap_set_class_of_device(0x200408) exactly (design sec 9/bead spec).
    gap_set_class_of_device(0x200408);

    pl_log("a2dp: init OK, %u codec row(s) registered\r\n", (unsigned)PL_CODEC_COUNT);
}

void pl_a2dp_connect(const uint8_t *addr) {
    bd_addr_t local_addr;
    memcpy(local_addr, addr, 6);
    memcpy(s_ctx.connect_addr, addr, 6);

    uint8_t status = a2dp_source_establish_stream(local_addr, &s_ctx.a2dp_cid);
    if (status != ERROR_CODE_SUCCESS) {
        pl_log("a2dp: establish_stream rejected, status=0x%02x\r\n", status);
        pl_bt_push_connect_failed(addr, PL_FAILURE_REASON_RADIO_ERROR);
        return;
    }
    pl_bt_push_connect_step(PL_CONNECT_STEP_SETTING_UP_AUDIO);
}

void pl_a2dp_report(uint32_t report_dt_us) {
    // Bead pico-link-pbv round 2 (C2-11): every rate the reader computes
    // from two report lines (enc_frames/s, tick rate, etc) MUST divide by
    // the real interval since the last report, not an assumed 1s -- round
    // 1's "354/s" and "375 +/- 2" were compared across an interval nobody
    // had actually measured.
    //
    // Bead pico-link-okx (D11): report_dt_us is now a PARAMETER, computed
    // ONCE in main.c's superloop and shared with pl_usb_pump_report --
    // this function used to run its own independent ~1s rate-limit clock,
    // so every sof_isr/s-vs-packets/s-vs-enc_frames_total/s comparison this
    // bead's discriminator needs divided two different unsynchronized
    // ~1.02s windows. NOT cosmetic -- worth ~1 percent by construction. 0
    // on the very first call (no prior sample to diff against).
    const char *codec_name = s_ctx.codec != NULL ? s_ctx.codec->display_name : "none";
    pl_log(
        "a2dp: codec=%s bitrate=%lu fill=%lu/%lu ovr_frames=%lu und=%lu report_dt_us=%lu\r\n", codec_name,
        (unsigned long)s_ctx.frame.nominal_bitrate_bps, (unsigned long)pl_pcm_fill_bytes(),
        (unsigned long)PL_PCM_TARGET_FILL_BYTES, (unsigned long)pl_pcm_overrun_frames(),
        (unsigned long)s_ctx.underrun_events, (unsigned long)report_dt_us
    );
    pl_log(
        "a2dp: enc_max_us=%lu pkt_sent=%lu pkt_fail=%lu misaligned=%lu\r\n", (unsigned long)s_ctx.enc_max_us,
        (unsigned long)s_ctx.pkt_sent, (unsigned long)s_ctx.pkt_fail, (unsigned long)pl_pcm_misaligned()
    );
    // Cumulative, never reset -- compute deltas between two consecutive
    // report lines (report_dt_us apart) to get real tick rate (tick_count
    // delta) and enc_frames RATE (enc_frames_total delta, the pbv
    // acceptance criterion: must read 375 +/- 2 per second in steady
    // state). worst_tick_interval_us is the worst single interval seen
    // since streaming started (never reset, so a spike stays visible even
    // if the average recovers) -- also falsifier #1 (pico-link-pbv): must
    // stay below 2000us for the 0xC0 USB worker's own report
    // (pl_usb_pump_report), a DIFFERENT counter than this one; if IT rises
    // above 2000us after this fix ships, the priority-preemption model
    // this fix relies on (usb_pump.c:20-22, worker at 0xC0 preempts this
    // file's 0xFF encode loop) is wrong and the change should be reverted.
    pl_log(
        "a2dp: tick_count=%lu worst_tick_interval_us=%lu enc_frames_total=%lu ticks_send_pending=%lu\r\n",
        (unsigned long)s_ctx.tick_count, (unsigned long)s_ctx.worst_tick_interval_us,
        (unsigned long)s_ctx.enc_frames_total, (unsigned long)s_ctx.ticks_send_pending
    );
    // Bead pico-link-pbv round 2 instrumentation. fill_ema/fill_min come
    // from usb_audio.c's feedback task (its own ~1ms sampling of
    // pl_pcm_fill_bytes(), far finer-grained than this file's ~11ms tick)
    // -- see design sec 2.1 for why the EMA, not raw fill, is the correct
    // thing to evaluate the pass criterion against; fill_min is now a
    // WINDOWED minimum, reset by this very read (C2-9). Round 3 (R3-2):
    // credit_clamped_samples/credit_clamp_events replace round 2's
    // floor-divided credit_clamped -- the real "am I behind real time"
    // observable, now exact (feeds the A3-3 conservation identity
    // directly, no truncation). flush_frames (C2-6) and resync_drops
    // together with enc_frames_total/ovr_frames feed the conservation
    // check. See Ada's design-round-3 bead comment for the full
    // acceptance criteria (A3-1..A3-8) and falsifiers (F3-1..F3-5).
    pl_log(
        "a2dp: frames_per_packet=%lu credit_clamped_samples=%lu credit_clamp_events=%lu flush_frames=%lu "
        "resync_drops=%lu fill_ema=%ld fill_min=%lu\r\n",
        (unsigned long)s_ctx.frames_per_packet, (unsigned long)s_ctx.credit_clamped_samples,
        (unsigned long)s_ctx.credit_clamp_events, (unsigned long)s_ctx.flush_frames, (unsigned long)s_ctx.resync_drops,
        (long)pl_usb_audio_fb_fill_ema(), (unsigned long)pl_usb_audio_fill_min()
    );
    // Bead pico-link-pbv round 2 (C2-2): stop-reason breakdown for
    // pl_a2dp_fill_sbc_buffer's loop. stop_dwell must read 0 in a healthy
    // run (falsifier: the real dwell-safety bound, not the credit clock,
    // would be the actual limiter). stop_credit dominating is the expected
    // healthy reading (most ticks fire faster than one SBC frame's worth
    // of real time). Round 3 (R3-3): stop_packet_full_hot is the tripwire
    // for the one-packet-per-tick output ceiling -- must read 0 in a
    // healthy run; see that field's doc comment on pl_a2dp_ctx_t.
    pl_log(
        "a2dp: stop_credit=%lu stop_packet_full=%lu stop_packet_full_hot=%lu stop_ring_empty=%lu stop_dwell=%lu "
        "fill_short_read=%lu\r\n",
        (unsigned long)s_ctx.stop_credit, (unsigned long)s_ctx.stop_packet_full,
        (unsigned long)s_ctx.stop_packet_full_hot, (unsigned long)s_ctx.stop_ring_empty, (unsigned long)s_ctx.stop_dwell,
        (unsigned long)s_ctx.fill_short_read
    );
}
