// Pico Link firmware -- the LDAC codec table row implementation (M4 S4,
// bead pico-link-cz0.5.6). See codec_ldac.h's module doc and
// .planning/design/2026-08-30-ldac.md (design of record, stage L3).
//
// UNPROVEN ON HARDWARE as of this bead (Andreas is using the only
// available headset for work -- pico-link-371 runs the real link test in
// a later session). This file builds and cross-compiles cleanly and its
// host-independent logic (the 8-byte capability blob shape, the static-
// lifetime buffer, the vtable contract) is reasoned from BTstack's/
// libldac's own headers, but no AVDTP negotiation with a real LDAC sink
// has ever exercised it.
#include "codec_ldac.h"

#include <string.h>

#include "classic/avdtp.h"

#include "ldacBT.h"
#include "usb_pump.h" // pl_log

// AVDTP media codec capability bytes we advertise for LDAC -- the classic
// vendor-specific layout every open LDAC A2DP implementation uses
// (.planning/design/2026-08-30-ldac.md Q2): 4-byte vendor ID (Sony,
// 0x0000012D) + 2-byte vendor codec ID (LDAC, 0x00AA), both little-endian
// per the AVDTP vendor-specific media codec capabilities format, followed
// by a 1-byte sampling-frequency bitmap and a 1-byte channel-mode bitmap
// (bit layouts documented in ldacBT.h's LDACBT_SAMPLING_FREQ_*/
// LDACBT_CHANNEL_MODE_* macros -- these ARE the AVDTP wire bits, not a
// libldac-internal invention). Only 48kHz/stereo is ever advertised:
// usb_audio.c's UAC2 offers 48kHz only (M3 scope, no resample step exists
// on either side of this pipeline -- same reasoning as codec_sbc.c's
// avdtp_set_preferred_sampling_frequency(ep, 48000) call in a2dp.c, and
// the project is stereo-only throughout).
static uint8_t s_ldac_capabilities[] = {
    0x2D, 0x01, 0x00, 0x00, // vendor ID 0x0000012D, little-endian
    0xAA, 0x00,             // vendor codec ID 0x00AA, little-endian
    LDACBT_SAMPLING_FREQ_048000,
    LDACBT_CHANNEL_MODE_STEREO,
};

// Bead pico-link-cz0.5.6 (design doc trap #2): LDAC's media codec info is
// EXACTLY 8 bytes against avdtp_stream_endpoint_t::media_codec_info[8]
// (avdtp.h:634) -- no headroom. Static assert both buffers below (this one
// and pl_codec_ldac_negotiated_info) match that exactly, so a future edit
// that grows either one fails the BUILD, not a real headphone.
_Static_assert(sizeof(s_ldac_capabilities) == 8, "LDAC capabilities must be exactly 8 bytes -- avdtp.h:634 media_codec_info[8] has no headroom");

// a2dp_source_create_stream_endpoint's scratch "default configuration"
// buffer -- BTstack owns writes into this; this project never reads it
// back (same role as codec_sbc.c's s_sbc_configuration; see that file's
// doc comment for why that's fine).
static uint8_t s_ldac_configuration[8];

// Bead pico-link-cz0.5.6 (design doc trap #1): a2dp_source_set_config_other
// STORES THE POINTER to media_codec_information (BTstack's
// a2dp.c:1051, a2dp_config_process_set_other) -- it does not copy it. This
// buffer therefore MUST have static lifetime; a stack local here would be
// a use-after-free the instant a2dp.c's CAPABILITIES_COMPLETE handler
// returns. Non-static so a2dp.c can reference it directly (declared
// extern in codec_ldac.h) -- content is built once, at compile time,
// below, since the project only ever offers exactly one LDAC
// configuration (48kHz/stereo, same constraint as the capabilities
// above); a2dp.c's AVDTP_CODEC_NON_A2DP arm only needs to check the
// remote's discovered bitmaps accept this exact configuration before
// handing this buffer to a2dp_source_set_config_other.
const uint8_t pl_codec_ldac_negotiated_info[8] = {
    0x2D, 0x01, 0x00, 0x00,
    0xAA, 0x00,
    LDACBT_SAMPLING_FREQ_048000,
    LDACBT_CHANNEL_MODE_STEREO,
};
_Static_assert(sizeof(pl_codec_ldac_negotiated_info) == 8, "LDAC negotiated media_codec_information must be exactly 8 bytes -- avdtp.h:634 media_codec_info[8] has no headroom");

// Bead pico-link-cz0.5.6 / design doc Q4: libldac self-packetises to ITS
// OWN configured mtu -- it is not told a2dp.c's negotiated AVDTP payload
// size per call, so init() (which runs at CODEC CONFIGURATION time, before
// the media L2CAP channel that a2dp_max_media_payload_size() depends on
// even exists -- avdtp_source.c:257-261 returns 0 without it) cannot use
// the real negotiated MTU. This is libldac's own documented required
// minimum (LDACBT_MTU_REQUIRED, firmware/vendor/libldac/src/
// ldacBT_internal.h:56 -- kept internal to the library, not re-included
// here; ~679B, cited by .planning/design/2026-08-30-ldac.md Q4) -- a
// fixed, conservative choice that is always safely smaller than a2dp.c's
// generous PL_A2DP_PAYLOAD_SLOT_BYTES (1030) slot buffer regardless of
// what the remote actually negotiates, at the cost of under-using the real
// negotiated payload once STREAM_ESTABLISHED learns it (a pacing-budget
// efficiency question, not a correctness one -- follow-up once hardware
// (pico-link-371) proves the real negotiated MTU against a real sink; see
// this bead's completion comment).
#define PL_LDAC_INIT_MTU 679

// Bead pico-link-d42g (design sec 0/3): libldac's own per-packet transport
// header size, subtracted from PL_LDAC_INIT_MTU to get the real payload
// budget (ldacBT_api.c's tx.tx_size = mtu - pkt_hdr_sz) that
// nfrm_in_pkt = tx_size / frmlen_tx packs frames into -- same convention
// as PL_LDAC_INIT_MTU just above: restated locally, cited by name and
// value, not re-included from the internal header (firmware/vendor/
// libldac/src/ldacBT_internal.h:61, LDACBT_TX_HEADER_SIZE, currently 18).
#define PL_LDAC_TX_HEADER_SIZE 18

// The encoder instance -- statically allocated, never malloc'd as a
// pl_ldac_encoder_t (design sec 5); the libldac HANDLE_LDAC_BT it owns IS
// heap-allocated, but exactly once, lazily, the first time init() ever
// runs (see pl_codec_ldac_init) -- never from encode() (codec_table.h's
// IRQ-context contract), and never again on a reconnect (ldacBT_close_handle
// + ldacBT_init_handle_encode reconfigures the SAME handle, matching
// ldacBT.h's own documented reuse pattern -- "closed handle can be
// initialized and used again").
typedef struct {
    HANDLE_LDAC_BT handle;
} pl_ldac_encoder_t;

static pl_ldac_encoder_t s_ldac_encoder;

// Bead pico-link-7jol.3: the ABR ladder. PL_LDAC_ADAPTIVE_LADDER_RUNGS is
// declared in codec_ldac.h (a2dp.c's controller needs it too). See
// codec_ldac.h's doc comments on the public accessors this state backs,
// and design sec 0.1 for why the ladder is 5 rungs (not 3) and only rungs
// 0/2/4 are the public HQ/SQ/MQ constants.

// Set by pl_codec_ldac_set_quality(), consumed once by pl_codec_ldac_init()
// below. Plain (not volatile): both run on the same cyw43/BTstack
// background async_context, never concurrently.
static uint8_t s_pending_ldac_quality = 0;

// Controller state. s_ldac_adaptive/s_ldac_applied_rung are reset every
// pl_codec_ldac_init() call (design sec 5.2 -- STREAM_ESTABLISHED/codec
// renegotiation is one of the four reset events; "no carried-over
// controller state, ever"). s_ldac_target_rung is the ONE word core0's
// decide phase writes and the encoder-context apply phase reads (design
// sec 6.2) -- one aligned word, one writer, one reader, a target rather
// than a delta so a missed or duplicated observation converges.
// s_ldac_applied_rung is `volatile` for the same cross-core reason (code
// review, 2026-09-07): it is written from encoder context (core1 under
// PL_ENCODER_ON_CORE1) and read cross-core by a2dp.c's report/decide
// paths -- every other cross-core field in this file is explicitly
// volatile and this one should not be the sole exception, even though
// aligned-word access on Cortex-M33 makes the plain-int version benign
// today.
// Bead pico-link-7jol.5: promoted plain -> volatile. A manual quality pick
// (pl_codec_ldac_pin_now) now writes this from THREAD context (bt.c's
// command dispatch) while pl_codec_ldac_apply_pending_tuning reads it from
// encoder context (core0 IRQ legacy, core1 under PL_ENCODER_ON_CORE1) --
// the exact same cross-context read/write class s_ldac_applied_rung/
// s_ldac_target_rung already are, for the same reason: single aligned-word
// access is atomic on Cortex-M33, so volatile is sufficient here without a
// seqlock.
static volatile bool s_ldac_adaptive = false;
static volatile int32_t s_ldac_applied_rung = 0;
static volatile int32_t s_ldac_target_rung = 0;
// Bead pico-link-7jol.5: the live encoder's current effective bitrate, in
// kbps -- see codec_ldac.h's doc comment on pl_codec_ldac_current_kbps for
// why this is a cache updated only from encoder context, never a live
// re-query from elsewhere.
static volatile uint32_t s_ldac_current_kbps = 0;

// Lifetime observability counters (design sec 7 / bead trap 4) -- NOT
// reset by init(), same convention as this file's sibling lifetime
// counters elsewhere in the firmware (e.g. a2dp.c's resync_events).
static uint32_t s_ldac_abr_steps_down = 0;
static uint32_t s_ldac_abr_steps_up = 0;
static uint32_t s_ldac_abr_rail_hits = 0;
static uint32_t s_ldac_abr_apply_fail = 0;

// Bead pico-link-d42g (design sec 4): the global Adaptive floor, in rungs.
// Default rung 4 (330kbps, same as today's physical rail before this
// bead) means behaviour is IDENTICAL until a floor is explicitly set --
// pl_codec_ldac_set_floor is the only writer, from any context (same
// cross-context class as s_ldac_adaptive/s_ldac_target_rung). NOT reset
// by init() -- this is a global, persisted-across-reconnects setting
// (design sec 1/2), unlike the per-stream controller state above it.
static volatile int32_t s_ldac_floor_rung = 4;

// Andreas's ruling 2026-09-07 (design sec 5): a manual quality pick PINS
// the EQMID; the controller is only ever constructed for ldac_quality ==
// 4 (Adaptive). This is the ONE function that maps a persisted, 1-based
// ldac_quality byte to an initial EQMID and an is-adaptive flag -- design
// sec 5.4: "all of the pin-versus-ceiling policy therefore lives in the
// single function that maps ldac_quality to that pair. Do not scatter the
// policy anywhere else." If the ruling is ever reversed, this is the only
// function that changes.
static void pl_ldac_quality_to_initial_state(uint8_t ldac_quality_1based, int *out_eqmid, bool *out_adaptive) {
    switch (ldac_quality_1based) {
        case 2: // 660 kbps, pinned
            *out_eqmid = LDACBT_EQMID_SQ;
            *out_adaptive = false;
            break;
        case 3: // 330 kbps, pinned
            *out_eqmid = LDACBT_EQMID_MQ;
            *out_adaptive = false;
            break;
        case 4: // Adaptive -- starts at rung 0 (HQ, 990 kbps)
            *out_eqmid = LDACBT_EQMID_HQ;
            *out_adaptive = true;
            break;
        case 0: // never chosen -- firmware default (today: pinned HQ)
        case 1: // 990 kbps, pinned
        default:
            *out_eqmid = LDACBT_EQMID_HQ;
            *out_adaptive = false;
            break;
    }
}

void pl_codec_ldac_set_quality(uint8_t ldac_quality_1based) { s_pending_ldac_quality = ldac_quality_1based; }

bool pl_codec_ldac_is_adaptive(void) { return s_ldac_adaptive; }

void pl_codec_ldac_request_rung(int32_t rung) {
    if (rung < 0) {
        rung = 0;
    } else if (rung > PL_LDAC_ADAPTIVE_LADDER_RUNGS - 1) {
        rung = PL_LDAC_ADAPTIVE_LADDER_RUNGS - 1;
    }
    s_ldac_target_rung = rung;
}

int32_t pl_codec_ldac_requested_rung(void) { return s_ldac_target_rung; }
int32_t pl_codec_ldac_applied_rung(void) { return s_ldac_applied_rung; }
uint32_t pl_codec_ldac_abr_steps_down(void) { return s_ldac_abr_steps_down; }
uint32_t pl_codec_ldac_abr_steps_up(void) { return s_ldac_abr_steps_up; }
uint32_t pl_codec_ldac_abr_rail_hits(void) { return s_ldac_abr_rail_hits; }
uint32_t pl_codec_ldac_abr_apply_fail(void) { return s_ldac_abr_apply_fail; }

// The vtable's optional apply_pending_tuning slot (codec_table.h,
// design sec 6.2/6.3). Called once per pl_a2dp_fill() invocation, from
// whichever context owns the encoder under the live build
// (core0 IRQ legacy, core1 under PL_ENCODER_ON_CORE1) -- NEVER from
// core0's media-timer decide phase, which is what makes this safe under
// PL_ENCODER_ON_CORE1 without inheriting fhf's #ifndef gate: the decide
// phase writes only s_ldac_target_rung (one volatile word); this function
// is the sole reader AND the sole writer of every other piece of ladder
// state, so there is no cross-context tear.
//
// Steps the applied rung ONE STEP per call toward the target, so a
// multi-rung change converges over several fill() calls rather than all
// at once (design sec 6.2: ~40ms for a 4-rung walk at 100 fill()/s).
//
// Bead pico-link-7jol.5: the `!s_ldac_adaptive` half of this gate was
// REMOVED. A pinned stream still never has a reason to step on its own --
// nothing but pl_codec_ldac_pin_now (below) ever writes s_ldac_target_rung
// away from s_ldac_applied_rung while not adaptive, and it only does so as
// the ONE-TIME effect of a manual pick -- but this function must still be
// the thing that WALKS toward that one-time target once pin_now sets it,
// exactly like an ABR multi-rung walk already does. Gating on `handle`
// alone is behaviour-preserving for every pre-existing pinned stream
// (target == applied == 0 always, since nothing else moves it), and is
// what lets a manual pin-while-Adaptive or pin-to-a-different-fixed-rate
// converge the live encoder without a reconnect (design sec 5: "applies
// live, no confirm").
static void pl_codec_ldac_apply_pending_tuning(void *state) {
    pl_ldac_encoder_t *enc = (pl_ldac_encoder_t *)state;
    if (enc->handle == NULL) {
        return;
    }
    int32_t target = s_ldac_target_rung; // single volatile read
    // Bead pico-link-d42g (design sec 4): THE single enforcement point for
    // the Adaptive floor. Clamp here even though pl_codec_ldac_set_floor
    // already tries to pull the target in on its own write -- this
    // function is the sole reader AND writer of every other piece of
    // ladder state (this file's own module doc above), so re-deriving the
    // clamp from s_ldac_floor_rung right here, on every apply call, is
    // race-safe in ANY write order between the decide phase (writes
    // s_ldac_target_rung) and pl_codec_ldac_set_floor (writes
    // s_ldac_floor_rung, possibly from a different context): whichever
    // wrote last, the encoder never applies a rung past the floor, and
    // walks back up (fewer steps down than requested, or steps up) if it
    // is already beyond it. Non-Adaptive streams are untouched (a pin's
    // target is exempt by design -- floors only cap Adaptive).
    if (s_ldac_adaptive) {
        int32_t floor_rung = s_ldac_floor_rung;
        if (target > floor_rung) {
            target = floor_rung;
        }
    }
    if (target == s_ldac_applied_rung) {
        return;
    }
    // Rung increasing == moving AWAY from HQ (toward MQ) == "for better
    // connectivity" == LDACBT_EQMID_INC_CONNECTION (ldacBT.h's own naming
    // is relative to EQMID/HQ, not our rung counter -- this is the
    // correct match, not an inversion). Rung decreasing == toward HQ ==
    // LDACBT_EQMID_INC_QUALITY.
    bool stepping_down = target > s_ldac_applied_rung;
    bool at_rail = stepping_down ? (s_ldac_applied_rung >= PL_LDAC_ADAPTIVE_LADDER_RUNGS - 1) : (s_ldac_applied_rung <= 0);
    if (at_rail) {
        // design sec 3.5: already at the rail -- do not advance the rung
        // counter, count it, log nothing (IRQ/encoder context).
        s_ldac_abr_rail_hits++;
        return;
    }
    int direction = stepping_down ? LDACBT_EQMID_INC_CONNECTION : LDACBT_EQMID_INC_QUALITY;
    int status = ldacBT_alter_eqmid_priority(enc->handle, direction);
    if (status != 0) {
        // Code review, 2026-09-07 (bead pico-link-7jol.3): our own rung
        // counter already ruled the rail case out above, so per design
        // sec 11.1(b) ANY non-zero return here is, by definition, a
        // genuine fault -- never a rail. Do NOT re-classify via
        // ldacBT_get_error_code(): reading ldacBT_api.c:321-341 shows
        // EVERY reachable failure path inside ldacBT_alter_eqmid_priority
        // sets LDACBT_ERR_ALTER_EQMID_LIMITED, including the
        // pkt_type != _2_DH5 case sec 0.2 names as the one persistent-
        // failure mode that must be reported LOUDLY -- so that re-check
        // could only ever re-derive "rail", silently burying the exact
        // fault this counter exists to catch. abr_apply_fail must be
        // reachable; a rail hit is caught entirely by the at_rail branch
        // above.
        s_ldac_abr_apply_fail++;
        return;
    }
    if (stepping_down) {
        s_ldac_applied_rung++;
        s_ldac_abr_steps_down++;
    } else {
        s_ldac_applied_rung--;
        s_ldac_abr_steps_up++;
    }
    // Bead pico-link-dge6 (one-pick lag): do NOT read
    // ldacBT_get_bitrate() here. ldacBT_alter_eqmid_priority() only writes
    // hLdacBT->tgt_eqmid/tgt_frmlen (ldacBT_internal.c's
    // ldacBT_set_eqmid_core) -- the library's own hLdacBT->bitrate field
    // is not updated until a LATER ldacBT_encode() call notices the
    // target changed and adopts it at a frame boundary
    // (ldacBT_internal.c's ldacBT_update_frmlen, invoked from
    // ldacBT_api.c's ldacBT_encode). A synchronous read right here always
    // fetched the PRE-transition bitrate, which is why Home's live
    // readout lagged exactly one pick behind. pl_codec_ldac_encode() below
    // now re-reads ldacBT_get_bitrate() after every successful encode call
    // (same encoder-owning context) and republishes s_ldac_current_kbps
    // when the library's own value has actually changed -- that is the
    // only place the adoption is visible, so that is the only place that
    // should ask.
}

// Bead pico-link-7jol.5, design sec 5: maps a FIXED (non-Adaptive)
// ldac_quality to the ladder rung it corresponds to -- the ladder has 5
// rungs but only 0/2/4 are the public HQ/SQ/MQ constants (this file's own
// module doc, sec 0.1), same three rungs pl_ldac_quality_to_initial_state
// maps to EQMIDs for the init()-time path. Kept next to that function so
// the two mappings can never drift -- see design sec 5.4's "one function"
// rule (extended to this rung-flavoured sibling).
static int32_t pl_ldac_quality_to_rung(uint8_t ldac_quality_1based) {
    // Bead pico-link-dge6, secondary fix: the real libldac EQMID table
    // (ldacBT_internal.c's tbl_ldacbt_eqmid_property, ASK THE LIBRARY per
    // bead pico-link-qx8's doctrine) is HQ(0)=990, SQ(1)=660, Q0(2)=492,
    // Q1(3)=396, MQ(4)=330 -- exactly the PL_LDAC_ADAPTIVE_LADDER_RUNGS==5
    // rungs this ladder already has, one real alter_eqmid_priority step per
    // rung. SQ is table position 1, NOT 2 -- the old mapping (SQ->2) walked
    // one extra real step past SQ and landed on Q0 (492kbps) instead. MQ
    // was already correct at 4.
    switch (ldac_quality_1based) {
        case 2: // 660 kbps / SQ
            return 1;
        case 3: // 330 kbps / MQ
            return 4;
        case 0: // never chosen -- same default as pinned 990/HQ
        case 1: // 990 kbps / HQ
        default:
            return 0;
    }
}

// Bead pico-link-d42g (design sec 2/4): maps the PERSISTED wire byte
// (persist.h's abr_floor -- 0 unset, 1/2/3 = 330/246/198 kbps) to the
// ladder rung it caps at. Kept beside pl_ldac_quality_to_rung so the two
// "wire byte -> rung" mappings in this file can never drift independently
// (same "one function" discipline design sec 5.4 already applies to the
// quality mapping). Design sec 0: 330=rung4, 246=rung6, 198=rung8.
static int32_t pl_ldac_floor_wire_to_rung(uint8_t floor_wire) {
    switch (floor_wire) {
        case 2: // 246 kbps
            return 6;
        case 3: // 198 kbps
            return 8;
        case 0: // unset -- same default as wire value 1
        case 1: // 330 kbps
        default:
            return 4;
    }
}

void pl_codec_ldac_set_floor(uint8_t floor_wire) {
    int32_t rung = pl_ldac_floor_wire_to_rung(floor_wire);
    s_ldac_floor_rung = rung;
    // Bead pico-link-d42g: grants permission immediately -- if the current
    // target already sits past the new (tighter) floor, pull it in right
    // now rather than waiting for the next decide-phase write. This is a
    // convenience only; pl_codec_ldac_apply_pending_tuning's own clamp is
    // the single point that is actually race-safe against a concurrent
    // decide-phase write (design sec 4).
    if (s_ldac_adaptive && s_ldac_target_rung > rung) {
        s_ldac_target_rung = rung;
    }
}

int32_t pl_codec_ldac_floor_rung(void) { return s_ldac_floor_rung; }

// Bead pico-link-d42g, PL_DEBUG_REMOTE-only "LDAC RUNG n" command: same
// live-apply mechanics as pl_codec_ldac_pin_now's fixed-quality branch
// (adaptive = false, one-time target write, walked by
// pl_codec_ldac_apply_pending_tuning), but takes a raw rung directly so
// the by-ear round can reach 246/198 kbps without real queue congestion.
void pl_codec_ldac_debug_pin_rung(int32_t rung) {
    if (s_ldac_encoder.handle == NULL) {
        return; // no live LDAC stream to apply to right now
    }
    if (rung < 0) {
        rung = 0;
    } else if (rung > PL_LDAC_ADAPTIVE_LADDER_RUNGS - 1) {
        rung = PL_LDAC_ADAPTIVE_LADDER_RUNGS - 1;
    }
    s_ldac_adaptive = false;
    s_ldac_target_rung = rung;
}

void pl_codec_ldac_pin_now(uint8_t ldac_quality_1based) {
    if (s_ldac_encoder.handle == NULL) {
        // No live LDAC stream to apply to right now -- the persisted pick
        // still takes effect at the next connect via
        // pl_a2dp_finish_codec_negotiation's existing re-apply (design sec
        // 5, "a pin pins... re-applied on reconnect"). Not an error.
        return;
    }
    int unused_eqmid;
    bool adaptive;
    pl_ldac_quality_to_initial_state(ldac_quality_1based, &unused_eqmid, &adaptive);
    s_ldac_adaptive = adaptive;
    if (adaptive) {
        // Enter Adaptive from wherever the ladder currently sits -- no
        // audible jump; a2dp.c's decide loop picks up from here on its
        // next cycle (design sec 5.1: "the number walks... over the
        // following moment", not a snap).
        s_ldac_target_rung = s_ldac_applied_rung;
    } else {
        // A fixed pick pins the target; pl_codec_ldac_apply_pending_tuning
        // (encoder context) walks s_ldac_applied_rung to it one step per
        // fill() call, same primitive an ABR multi-rung walk already uses.
        s_ldac_target_rung = pl_ldac_quality_to_rung(ldac_quality_1based);
    }
}

uint32_t pl_codec_ldac_current_kbps(void) { return s_ldac_current_kbps; }

static bool pl_codec_ldac_init(
    void *state, const uint8_t *configuration, uint8_t configuration_len, pl_codec_format_t *out_format,
    pl_codec_frame_info_t *out_frame
) {
    pl_ldac_encoder_t *enc = (pl_ldac_encoder_t *)state;
    if (configuration_len != sizeof(pl_codec_ldac_negotiated_t)) {
        return false;
    }
    pl_codec_ldac_negotiated_t cfg;
    memcpy(&cfg, configuration, sizeof(cfg));

    // Defensive re-check: a2dp.c's AVDTP_CODEC_NON_A2DP arm (the
    // CAPABILITIES_COMPLETE walk) already refuses to call
    // a2dp_source_set_config_other unless the remote's discovered bitmaps
    // accept exactly this configuration, so this should never trip on a
    // real negotiation -- kept as a hard guard rather than trusting that
    // invariant silently, same spirit as codec_sbc.c's configuration_len
    // check above.
    if (cfg.sampling_frequency != LDACBT_SAMPLING_FREQ_048000 || cfg.channel_mode != LDACBT_CHANNEL_MODE_STEREO) {
        return false;
    }

    if (enc->handle == NULL) {
        enc->handle = ldacBT_get_handle();
        if (enc->handle == NULL) {
            return false;
        }
    } else {
        // Reconnect within the same boot -- reconfigure the existing
        // handle rather than allocating a new one (see this file's doc
        // comment on pl_ldac_encoder_t).
        ldacBT_close_handle(enc->handle);
    }

    // Bead pico-link-7jol.3 (design sec 5): map the persisted, per-device
    // ldac_quality byte (set by a2dp.c's pl_a2dp_finish_codec_negotiation
    // via pl_codec_ldac_set_quality, BEFORE this init() call) to the
    // initial EQMID and whether the controller is active for this stream.
    // Consuming s_pending_ldac_quality here, once, is what makes a pin
    // "structurally inert" (sec 5.1) rather than merely unused: for a
    // pinned device the controller is never told to step because
    // s_ldac_adaptive is false, not because it happens not to fire.
    int initial_eqmid;
    // Bead pico-link-7jol.5: s_ldac_adaptive is now `volatile` (cross-
    // context, see its own doc comment) -- write it through a plain local
    // so this helper's signature doesn't need a volatile-qualified
    // parameter for its only OTHER caller (pl_codec_ldac_pin_now), which
    // writes a plain local of its own.
    bool adaptive_out;
    pl_ldac_quality_to_initial_state(s_pending_ldac_quality, &initial_eqmid, &adaptive_out);
    s_ldac_adaptive = adaptive_out;
    // design sec 5.2: no carried-over controller state, ever -- this
    // init() call is one of the four reset events (codec re-negotiation).
    // Bead pico-link-dge6: "no carried-over state" means no state survives
    // from the PREVIOUS stream, not "always reset to rung 0" -- rung 0 is
    // only correct when initial_eqmid actually IS HQ. A pinned SQ/MQ device
    // starts the real encoder at that EQMID via initial_eqmid above, so the
    // rung bookkeeping must be seeded to match it, or a later pin to a
    // *different* fixed quality can compute target_rung == applied_rung by
    // coincidence and pl_codec_ldac_apply_pending_tuning's early-return
    // (line ~239) silently no-ops -- exactly the "raising quality does
    // nothing" bug this bead fixes. Adaptive streams still start correctly
    // at rung 0 (pl_ldac_quality_to_rung's Adaptive/HQ default), so this is
    // a pure correction of the seed value, not new carried-over state.
    s_ldac_applied_rung = pl_ldac_quality_to_rung(s_pending_ldac_quality);
    s_ldac_target_rung = s_ldac_applied_rung;
    // Bead pico-link-7jol.5: same "no carried-over state" rule extends to
    // the live-bitrate cache -- a stale reading from the PREVIOUS stream
    // must not survive into this one, however briefly (Home's live number
    // is read at any time via pl_codec_ldac_current_kbps). Set for real,
    // below, once ldacBT_get_bitrate has actually run for this stream.
    s_ldac_current_kbps = 0;

    int status = ldacBT_init_handle_encode(
        enc->handle, PL_LDAC_INIT_MTU, initial_eqmid, (int)cfg.channel_mode, LDACBT_SMPL_FMT_S16, 48000
    );
    if (status != 0) {
        // Bead pico-link-371 heap finding: this is negotiation-time
        // (thread context, not encode()'s IRQ hot path), so a synchronous
        // close on failure is fine -- do not leave a half-initialized
        // handle around for the next init() call to inherit.
        ldacBT_close_handle(enc->handle);
        return false;
    }

    out_format->sample_rate_hz = 48000;
    out_format->channels = 2;
    out_format->bits_per_sample = 16;

    // Bead pico-link-cz0.5.6: LDACBT_ENC_LSU (ldacBT.h) is libldac's fixed
    // PCM chunk size, 128 samples/channel regardless of sampling
    // frequency -- same 128 as SBC's own pcm_frames_per_encoded_frame
    // (codec_sbc.c), which is why the design doc could say "LDAC gets the
    // same 375 calls/s (128 samples/frame both sides)" without measuring
    // it separately.
    out_frame->pcm_frames_per_encoded_frame = LDACBT_ENC_LSU;
    // Self-packetising, variable-length output -- codec_table.h's
    // encoded_frame_bytes==0 convention, now actually implemented (it was
    // reserved-but-unbuilt before this bead).
    out_frame->encoded_frame_bytes = 0;
    out_frame->header_bytes = 1; // same 1-byte AVDTP media header as SBC -- see a2dp.c's send site comment
    // Bead pico-link-371 (hardware capture, 2026-08-30): measured HQ
    // ldacBT_encode cost on real target hardware, 200 iterations,
    // MTU=679: avg=1069us min=1046 max=1814. worst_case_encode_us is this
    // row's own honest worst-case declaration (codec_table.h's doc
    // comment) -- 2000 is the measured max (1814) plus margin, same
    // methodology as codec_sbc.c's row (measured 574-582, declared 800).
    // KNOWN GAP, not fixed by this bead: PL_A2DP_TX_QUEUE_SLOTS (a2dp.c)
    // was sized assuming ~1100us (the design's amended stopping-rule
    // budget), not this measured 1814us worst case -- a2dp.c's
    // STREAM_ESTABLISHED handler already has a loud (never silent, per
    // pico-link-r44) WARNING for exactly this "compile-time assumption
    // undersized" case; expect it to fire on a real LDAC HQ connection
    // until that queue depth is revisited. Flagged for the pico-link-371
    // hardware session, not resolved here.
    out_frame->worst_case_encode_us = 2000;
    // Bead pico-link-qx8: ASK THE LIBRARY, never restate its table. libldac
    // computes the real bitrate inside ldacBT_init_handle_encode
    // (ldacBT_api.c:281-282, via ldacBT_frmlen_to_bitrate) from the frame
    // length the EQMID actually produced, so it is already correct by the
    // time we get here -- no need to wait for a first encoded frame despite
    // what ldacBT.h's "previously processed frame" wording suggests.
    // ldacBT_frmlen_to_bitrate returns KILObits/s (ldacBT_internal.c:429
    // divides by 1000/8), hence the x1000.
    //
    // This used to be a literal 990000 restating ldacBT.h's HQ-at-48kHz row,
    // which meant the panel and console reported 990k for every stream
    // whatever quality the encoder ran -- the MVP's headline readout was a
    // constant, and it actively misled a listening test.
    int kbps = ldacBT_get_bitrate(enc->handle);
    if (kbps <= 0) {
        // LDACBT_E_FAIL. Report 0 rather than inventing a number: a wrong
        // bitrate on the panel is worse than an obviously absent one.
        pl_log("ldac: ldacBT_get_bitrate failed (%d), reporting 0\r\n", kbps);
        out_frame->nominal_bitrate_bps = 0;
        // s_ldac_current_kbps stays 0 (set above) -- no honest bitrate to
        // cache either.
        // Bead pico-link-i6zn: no honest bitrate means no honest
        // frames-per-packet either -- leave the hint at 0 (unknown) so
        // a2dp.c's fallback (with its own loud warning) governs instead of
        // a made-up number. See codec_table.h's doc comment on this field.
        out_frame->self_packetising_frames_per_packet = 0;
    } else {
        out_frame->nominal_bitrate_bps = (uint32_t)kbps * 1000u;
        pl_log("ldac: nominal bitrate %d kbps from ldacBT_get_bitrate\r\n", kbps);
        // Bead pico-link-7jol.5: this IS the first live reading for this
        // stream -- Home must not wait for the first ABR step (which may
        // never come, for a pinned device) to show a live number.
        s_ldac_current_kbps = (uint32_t)kbps;

        // Bead pico-link-i6zn (design .planning/design/2026-09-07-ldac-abr-
        // control-loop.md sec 4.2): the honest per-packet transport-frame
        // count, derived the same way libldac derives it internally --
        // ASK THE LIBRARY'S OWN MATHS, never restate a per-EQMID table
        // (same discipline as nominal_bitrate_bps just above, bead
        // pico-link-qx8). bytes_per_frame is libldac's encoded byte count
        // per 128-sample LDAC transport frame at 48kHz
        // (kbps*1000 bits/s / 8 bits/byte * (128/48000) s/frame reduces to
        // kbps*1000/3000); frmlen_tx adds LDAC's own 3-byte per-frame
        // transport header; frames_per_packet is how many of those fit in
        // the MTU libldac was ACTUALLY configured with (PL_LDAC_INIT_MTU,
        // not the real negotiated AVDTP payload -- see that macro's doc
        // comment for why libldac never sees the larger real MTU), clamped
        // to libldac's own documented packing range of 2..15 frames/packet.
        // This is computed once here, at configuration time, exactly like
        // every other frame_info field -- a2dp.c never recomputes it.
        // Bead pico-link-7jol.3 (design sec 4.2, "must not ship without
        // this"): size the hint for the FLOOR rung the controller may
        // reach -- MQ (330 kbps) if this stream is Adaptive, this row's
        // own (pinned, floor==ceiling) bitrate otherwise -- NOT the
        // current bitrate. a2dp.c's STREAM_ESTABLISHED handler derives its
        // priming cushion from this value ONCE, at connect time, and a
        // rung change must never recompute it (sec 4.3: that would move
        // the setpoint out from under a live fhf trim). Sizing for the
        // worst case up front is what makes that setpoint rung-invariant.
        // MQ's bitrate is a fixed ladder fact (design sec 0.1's table),
        // not something to ask the library for mid-negotiation (no handle
        // is at MQ yet to ask).
        // Bead pico-link-d42g (design sec 0/3): the OLD formula
        // (679/(kbps/3+3)) double-counted LDAC's own 3-byte per-frame
        // transport header AND used the raw MTU (679) instead of
        // libldac's real payload budget, tx_size = mtu -
        // LDACBT_TX_HEADER_SIZE (18) -- ldacBT_api.c's own
        // nfrm_in_pkt = tx_size / frmlen_tx maths. The two formulas agree
        // from HQ to MQ only by coincidence and diverge below it (Q3: 7
        // vs the library's real 8; Q5: 9 vs 10) -- ASK THE LIBRARY'S OWN
        // MATHS (pico-link-qx8's doctrine), never restate a table.
        // Sized for Q5 (198 kbps), not a fixed 330u -- the connect-time
        // priming cushion this hint feeds (a2dp.c's STREAM_ESTABLISHED
        // handler, sec 4.3) is never recomputed mid-stream, so sizing for
        // the worst case every Adaptive stream can now reach is what
        // makes a LIVE floor change never need a re-prime. Pinned streams
        // still size for their own fixed bitrate (floor==ceiling there).
        uint32_t worst_case_kbps = s_ldac_adaptive ? 198u : (uint32_t)kbps;
        uint32_t bytes_per_frame = (worst_case_kbps * 1000u) / 3000u;
        uint32_t raw_frames_per_packet =
            bytes_per_frame > 0 ? (uint32_t)(PL_LDAC_INIT_MTU - PL_LDAC_TX_HEADER_SIZE) / bytes_per_frame
                                 : (uint32_t)(PL_LDAC_INIT_MTU - PL_LDAC_TX_HEADER_SIZE);
        uint32_t clamped_frames_per_packet = raw_frames_per_packet < 2u   ? 2u
                                              : raw_frames_per_packet > 15u ? 15u
                                                                            : raw_frames_per_packet;
        out_frame->self_packetising_frames_per_packet = (uint16_t)clamped_frames_per_packet;
        pl_log(
            "ldac: self_packetising_frames_per_packet=%lu (worst_case_kbps=%lu bytes_per_frame=%lu "
            "tx_size=%d mtu=%d adaptive=%d)\r\n",
            (unsigned long)clamped_frames_per_packet, (unsigned long)worst_case_kbps, (unsigned long)bytes_per_frame,
            PL_LDAC_INIT_MTU - PL_LDAC_TX_HEADER_SIZE, PL_LDAC_INIT_MTU, (int)s_ldac_adaptive
        );
    }

    return true;
}

static pl_codec_encode_result_t pl_codec_ldac_encode(void *state, const int16_t *pcm, uint8_t *out, uint16_t out_cap) {
    pl_ldac_encoder_t *enc = (pl_ldac_encoder_t *)state;
    // out_cap is a2dp.c's generous, fixed slot headroom
    // (PL_A2DP_PAYLOAD_SLOT_BYTES, 1030 bytes) -- never the limiting
    // factor here. libldac itself never writes more than PL_LDAC_INIT_MTU
    // (679) bytes per completed payload (ldacBT.h's encode doc comment:
    // "encoded data size for output will be determined by the value of
    // mtu"), and 679 < 1030 by construction (see PL_LDAC_INIT_MTU's doc
    // comment above). ldacBT_encode has no caller-supplied capacity
    // parameter to pass this through to, so there is nothing further to
    // enforce here -- documented, not silently ignored.
    (void)out_cap;

    if (enc->handle == NULL) {
        return (pl_codec_encode_result_t){.ok = false, .bytes_written = 0, .frames_emitted = 0, .payload_complete = false};
    }

    // ldacBT_encode's contract (ldacBT.h's doc comment above the
    // declaration): consumes exactly LDACBT_ENC_LSU (128) samples/channel
    // from `pcm` (codec_table.h's contract already guarantees a2dp.c hands
    // us exactly that many every call, via out_frame->
    // pcm_frames_per_encoded_frame above); pcm_used reports bytes
    // consumed; stream_sz/frame_num are 0 whenever libldac is still
    // accumulating internally toward one PL_LDAC_INIT_MTU-sized payload --
    // that is a NORMAL, successful call, not a failure (see
    // pl_codec_encode_result_t's doc comment on codec_table.h). No
    // allocation, no logging, no blocking here (design sec 5's IRQ-context
    // contract) -- ldacBT_encode's own heap use is confined to
    // ldacBT_get_handle(), called only from init() above.
    int pcm_used = 0;
    int stream_sz = 0;
    int frame_num = 0;
    int status = ldacBT_encode(enc->handle, (void *)(uintptr_t)pcm, &pcm_used, out, &stream_sz, &frame_num);
    if (status != 0) {
        return (pl_codec_encode_result_t){.ok = false, .bytes_written = 0, .frames_emitted = 0, .payload_complete = false};
    }

    // Bead pico-link-dge6 (one-pick lag): this is the ONLY place libldac
    // actually adopts a pending eqmid/frmlen change into
    // hLdacBT->bitrate -- ldacBT_encode() itself notices tgt_eqmid !=
    // eqmid at a frame boundary and calls ldacBT_update_frmlen
    // internally (ldacBT_internal.c). Re-reading ldacBT_get_bitrate()
    // right here, in the same encoder-owning context that just called
    // ldacBT_encode(), is therefore the earliest point the transition is
    // actually visible -- unlike the old read inside
    // pl_codec_ldac_apply_pending_tuning() (removed above), which fired
    // before the library had adopted anything. Only write the volatile
    // cache when the value actually changed, to keep this common-path
    // encode call (~100/s) cheap on the no-op case. s_ldac_current_kbps
    // is `volatile` and read cross-core by a2dp.c's
    // pl_a2dp_poll_ldac_bitrate/pl_codec_ldac_current_kbps -- plain
    // 32-bit-aligned access is atomic on Cortex-M33 (same doctrine as
    // s_ldac_applied_rung above), so no additional barrier is needed.
    int live_kbps = ldacBT_get_bitrate(enc->handle);
    if (live_kbps > 0 && (uint32_t)live_kbps != s_ldac_current_kbps) {
        s_ldac_current_kbps = (uint32_t)live_kbps;
    }

    return (pl_codec_encode_result_t){
        .ok = true,
        .bytes_written = (uint16_t)stream_sz,
        .frames_emitted = (uint16_t)frame_num,
        // A positive stream_sz means libldac just handed back one
        // complete "ldac_transport_frame" sequence sized to
        // PL_LDAC_INIT_MTU -- exactly a completed AVDTP payload from
        // a2dp.c's point of view (this is the self-packetising half of
        // pico-link-cz0.5.6's uniformised vtable; codec_sbc.c's row is the
        // other half, always false).
        .payload_complete = stream_sz > 0,
    };
}

static void pl_codec_ldac_deinit(void *state) {
    pl_ldac_encoder_t *enc = (pl_ldac_encoder_t *)state;
    // Never actually called by a2dp.c today (same as codec_sbc.c's
    // deinit -- see that file's doc comment), kept correct anyway: fully
    // release the heap allocation ldacBT_get_handle() made, rather than
    // just closing it, since this path means the row itself is being torn
    // down (not merely reconfigured for a reconnect -- that path is
    // init()'s close-and-reuse branch above).
    if (enc->handle != NULL) {
        ldacBT_close_handle(enc->handle);
        ldacBT_free_handle(enc->handle);
        enc->handle = NULL;
    }
}

pl_codec_t pl_codec_ldac = {
    .display_name = "LDAC",
    .avdtp_codec_type = AVDTP_CODEC_NON_A2DP,
    .vendor_id = 0x0000012D,
    .vendor_codec_id = 0x00AA,
    .codec_id = PL_CODEC_ID_LDAC,

    // Preference-ordered table walk (a2dp.c's CAPABILITIES_COMPLETE
    // handler, design sec 4.3): LDAC is tried first. This field is
    // documentation of that fact -- the walk itself iterates PL_CODECS in
    // ARRAY order (codec_table.c), which is what actually governs; both
    // must agree (codec_table.c's own doc comment: "table sorted by this").
    .preference = 0,
    .capabilities = s_ldac_capabilities,
    .capabilities_len = sizeof(s_ldac_capabilities),
    .configuration = s_ldac_configuration,
    .configuration_len = sizeof(s_ldac_configuration),

    .local_seid = 0, // filled in by codec_table.c's pl_codec_table_init()

    .init = pl_codec_ldac_init,
    .encode = pl_codec_ldac_encode,
    .deinit = pl_codec_ldac_deinit,
    // Bead pico-link-7jol.3, design sec 6.3: the optional ABR-apply slot.
    // NULL for codec_sbc.c's row (SBC has no ladder).
    .apply_pending_tuning = pl_codec_ldac_apply_pending_tuning,
    .state = &s_ldac_encoder,
};
