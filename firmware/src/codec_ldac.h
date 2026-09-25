// Pico Link firmware -- the LDAC codec table row (M4 S4, bead
// pico-link-cz0.5.6). See codec_table.h's module doc for the table this
// plugs into, and .planning/design/2026-08-30-ldac.md for the design of
// record this implements (stage L3).
//
// LDAC is a vendor-specific A2DP codec (AVDTP_CODEC_NON_A2DP) -- there is
// no BlueKitchen/BTstack-native support for it (unlike SBC), so this row
// owns the whole AVDTP media codec information shape itself: an 8-byte
// vendor blob (vendor ID + vendor codec ID + a one-byte sampling-frequency
// bitmap + a one-byte channel-mode bitmap), matching the well-known LDAC
// A2DP vendor codec layout cited in the design doc's Q2. The encoder
// itself is Sony/libldac (Apache-2.0), vendored under
// firmware/vendor/libldac (bead pico-link-cz0.5.4, L0) -- read directly
// from its own headers, never from USBPods.
#ifndef PICO_LINK_CODEC_LDAC_H
#define PICO_LINK_CODEC_LDAC_H

#include "codec_table.h"

// The row itself -- referenced by codec_table.c's PL_CODECS array. Defined
// (not just declared) in codec_ldac.c, statically allocated, never
// malloc'd (the row struct and its capability/configuration buffers; the
// libldac HANDLE_LDAC_BT itself is heap-allocated ONCE per init() call by
// ldacBT_get_handle() -- design doc Q1, negotiation-time only, confirmed
// never called from encode()).
extern pl_codec_t pl_codec_ldac;

// a2dp.c decodes A2DP_SUBEVENT_SIGNALING_MEDIA_CODEC_OTHER_CONFIGURATION's
// raw media_codec_information bytes (offsets 6/7 of the same 8-byte vendor
// blob shape the capability bytes use -- see codec_ldac.c's
// s_ldac_capabilities) into one of these and passes a pointer to it as
// pl_codec_ldac's init()'s `configuration` argument -- this row's own
// private negotiated-config shape, not raw AVDTP wire bytes (same pattern
// as codec_sbc.h's pl_codec_sbc_negotiated_t; see codec_table.h's doc
// comment on pl_codec::init for why that's fine).
typedef struct {
    uint8_t sampling_frequency; // single LDACBT_SAMPLING_FREQ_* bit (ldacBT.h)
    uint8_t channel_mode;       // single LDACBT_CHANNEL_MODE_* bit (ldacBT.h)
} pl_codec_ldac_negotiated_t;

// The ONE configuration this project ever offers or accepts for LDAC
// (48kHz/stereo -- see codec_ldac.c's doc comment on
// s_ldac_negotiated_info), exposed so a2dp.c's CAPABILITIES_COMPLETE
// handler (the AVDTP_CODEC_NON_A2DP arm of its preference-ordered table
// walk) can pass it straight to a2dp_source_set_config_other once it has
// checked the remote's discovered capability bitmaps accept it. STATIC
// LIFETIME -- a2dp_source_set_config_other stores this pointer, does not
// copy it (design doc trap #1); this buffer must outlive the connection,
// which a file-scope array in codec_ldac.c does by construction.
extern const uint8_t pl_codec_ldac_negotiated_info[8];

// The ladder's rung count -- the PHYSICAL ladder libldac's own EQMID table
// offers at 48kHz (design sec 0.1, bead pico-link-d42g's Q5 rail): HQ(0)
// 990, SQ(1) 660, Q0(2) 492, Q1(3) 396, MQ(4) 330, Q2(5) 282, Q3(6) 246,
// Q4(7) 216, Q5(8) 198 kbps. Exposed so a2dp.c's controller (the DECIDE
// phase) can bound its own rung arithmetic without duplicating the
// literal -- codec_ldac.c is still the only place that walks the ladder.
// Rungs 0..4 are unchanged from before bead pico-link-d42g (pins,
// pl_ldac_quality_to_rung and the init seed still land only on 0/1/4);
// rungs 5..8 exist only for Adaptive (and the LDAC RUNG debug pin) to
// reach. The runtime FLOOR -- how far down Adaptive is currently allowed
// to step -- is a SEPARATE value, see pl_codec_ldac_set_floor/
// pl_codec_ldac_floor_rung below; this constant is the hard physical
// rail, never the user-facing minimum.
#define PL_LDAC_ADAPTIVE_LADDER_RUNGS 9

// --- Bead pico-link-7jol.3: the LDAC ABR ladder and the pinned-quality
// setting. See .planning/design/2026-09-07-ldac-abr-control-loop.md,
// especially sec 5 (pin-vs-adaptive is ONE mapping function, kept in
// codec_ldac.c) and sec 6 (the core0-decide / encoder-context-apply
// split this API exists to support). a2dp.c owns the controller's
// decide phase (q_ema, the bands, the dwell timers); this module owns the
// ladder itself and every ldacBT_* call, per sec 6.3's module boundary.

// Sets the per-device persisted quality choice (persist.h's 1-based
// ldac_quality: 0 = unset, 1/2/3 = pinned 990/660/330, 4 = Adaptive).
// Consumed once, synchronously, by the NEXT pl_codec_ldac_init() call
// (i.e. row->init(), called from a2dp.c's pl_a2dp_finish_codec_
// negotiation, BEFORE init() runs -- design sec 11.2). Safe to call at any
// time, including while a different codec (SBC) is the live stream -- it
// only ever affects this row's own pending state, never touches a live
// handle, so it is the "safe no-op on the live stream" sec 11.2 requires.
void pl_codec_ldac_set_quality(uint8_t ldac_quality_1based);

// True once init() has configured the CURRENT stream for Adaptive mode
// (persisted quality == 4). Gates a2dp.c's ABR controller evaluation
// (design sec 2.4) -- a2dp.c never inspects codec-private state (EQMID,
// libldac internals) directly to decide this itself.
bool pl_codec_ldac_is_adaptive(void);

// Called from a2dp.c's control loop's DECIDE phase (core0, media-timer
// IRQ) with a new TARGET rung in [0, 4] -- not a delta (design sec 6.2:
// idempotent, so a missed or duplicated write converges rather than
// accumulating error). Clamped internally to the valid range. Writing
// this when the stream is not Adaptive is harmless -- the APPLY phase
// below no-ops unless pl_codec_ldac_is_adaptive() is true.
void pl_codec_ldac_request_rung(int32_t rung);

// The rung the encoder context is currently asked to reach (== the last
// value passed to pl_codec_ldac_request_rung).
int32_t pl_codec_ldac_requested_rung(void);

// The rung actually applied as of the last successful ldacBT_alter_eqmid_
// priority call. Distinct from the requested rung by design (bead
// pico-link-7jol.3's MUST-BUILD trap #4): a stuck walk shows up as a
// persistent divergence here, never silently.
int32_t pl_codec_ldac_applied_rung(void);

// Lifetime observability counters for pl_a2dp_report (design sec 7).
uint32_t pl_codec_ldac_abr_steps_down(void);
uint32_t pl_codec_ldac_abr_steps_up(void);
uint32_t pl_codec_ldac_abr_rail_hits(void);
uint32_t pl_codec_ldac_abr_apply_fail(void);

// Bead pico-link-7jol.5, design `.planning/design/2026-09-07-ldac-quality-
// selector.md` §5: applies a manual quality pick to the CURRENTLY
// established encoder, live, without waiting for a reconnect -- callable
// from any context (only touches this file's own volatile ladder state,
// no flash, no libldac handle access). A no-op if no LDAC encoder is live
// (`s_ldac_encoder.handle == NULL`) -- the caller (bt.c) is expected to
// have already checked pl_a2dp_is_connected_ldac(addr) first, but this
// function stays defensive on its own. `ldac_quality_1based` uses the
// same convention as pl_codec_ldac_set_quality (0 = never chosen, 1/2/3 =
// pinned 990/660/330, 4 = Adaptive). Reuses pl_ldac_quality_to_initial_
// state's ONE mapping function (sec 5.4: "do not scatter the policy") for
// the adaptive-or-not decision; entering Adaptive starts the target from
// wherever the ladder currently sits (no jump) so a2dp.c's decide loop
// picks up from there on its next cycle.
void pl_codec_ldac_pin_now(uint8_t ldac_quality_1based);

// Bead pico-link-d42g (design .planning/design/2026-09-25-adaptive-floor.md
// sec 4): the global, persisted cap on how far down the ladder Adaptive is
// allowed to step (toward robustness). `floor_wire` is persist.h's stored
// ABR-floor byte, NOT a rung -- 0 (unset) and 1 both mean 330kbps, 2 =
// 246kbps, 3 = 198kbps, anything else = 330kbps (same "wire enum
// independent of the ladder" convention as pl_codec_ldac_set_quality's
// ldac_quality byte). Callable from any context -- only touches this
// file's own volatile ladder state, same class as pl_codec_ldac_pin_now.
// Applies live: if Adaptive and the current target already exceeds the
// new floor, the target is pulled in immediately (grants an up-walk on
// the very next apply, no reconnect). The SINGLE enforcement point that
// also covers races against the decide phase's own writes is
// pl_codec_ldac_apply_pending_tuning's clamp (codec_ldac.c) -- this
// function's own immediate pull-in is a convenience, not the safety net.
void pl_codec_ldac_set_floor(uint8_t floor_wire);

// The rung Adaptive is currently capped at (default rung 4 == 330kbps, so
// behaviour is identical to every stream before this bead until a floor is
// explicitly set). a2dp.c's decide phase reads this to cap its own
// down-step request and to detect "at the floor and still congested"
// (abr_floor_hits), exactly the way PL_LDAC_ADAPTIVE_LADDER_RUNGS bounds
// the physical ladder.
int32_t pl_codec_ldac_floor_rung(void);

// PL_DEBUG_REMOTE-only debug pin: forces the live encoder to a SPECIFIC
// rung 0..PL_LDAC_ADAPTIVE_LADDER_RUNGS-1, bypassing the persisted
// ldac_quality mapping entirely (pl_codec_ldac_pin_now only ever reaches
// rungs 0/1/4). Lets a by-ear test reach 246/198 kbps directly without
// having to manufacture real queue congestion. No-op if no LDAC encoder is
// live, same defensive contract as pl_codec_ldac_pin_now. Clamped
// internally to the valid rung range.
void pl_codec_ldac_debug_pin_rung(int32_t rung);

// The live encoder's current effective bitrate, in kbps, or 0 if no LDAC
// encoder is live. Updated only from encoder context (init(), and
// immediately after each successful ldacBT_alter_eqmid_priority call in
// the APPLY phase) -- NEVER by re-querying ldacBT_get_bitrate from a
// different context, which would race the encoder's own concurrent use of
// the handle (bead pico-link-qx8's "ask the library" doctrine applies at
// the moment of the change, not on demand from elsewhere). Safe to read
// from any context (single volatile word, same argument as
// pl_codec_ldac_applied_rung). This is the fact `Event::LdacBitrateChanged`
// carries to core -- see a2dp.c's pl_a2dp_poll_ldac_bitrate.
uint32_t pl_codec_ldac_current_kbps(void);

#endif // PICO_LINK_CODEC_LDAC_H
