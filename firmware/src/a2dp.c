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
// worst_tick_interval_us or (preferably, since that field is a
// monotone maximum across the WHOLE stream, only reset at
// STREAM_STARTED -- bead pico-link-r44) the current tick's own
// elapsed_us -- never from PL_A2DP_AUDIO_TIMEOUT_MS.
#include "a2dp.h"

#include <string.h>

#include "pico/time.h"

#include "btstack.h"

#include "ldacBT.h" // LDACBT_SAMPLING_FREQ_048000/LDACBT_CHANNEL_MODE_STEREO -- the AVDTP_CODEC_NON_A2DP arm below

#include "bt.h"
#include "codec_ldac.h"
#include "codec_sbc.h"
#include "codec_table.h"
#include "hardware/sync.h" // __dmb() -- tx ring cross-core barriers, see s_ctx.tx doc comment
#include "media_keys.h"
#include "pcm_ring.h"
#include "persist.h"
#include "pl_prio.h"
#include "usb_audio.h"
#include "usb_pump.h"
#include "volume.h" // pl_volume_take_avrcp_desired (T3) / pl_volume_notify_sink, pl_volume_take_fu_report (T4, pico-link-4v2.4)
#include "watchdog_sup.h"

#ifdef PL_ENCODER_ON_CORE1
// Bead pico-link-nli.4 (G3, epic pico-link-nli): the LDAC encoder moves to
// core1. pico/multicore.h for multicore_launch_core1(); flash_lockout.h for
// pl_flash_lockout_core1_init() (G1, pico-link-nli.2) -- see this file's
// "CORE1" section below, just above pl_a2dp_media_timer_handler.
#include "pico/multicore.h"

#include "flash_lockout.h"
#endif

// Bead pico-link-85v (D1): renamed from SBC_STORAGE_SIZE -- generous
// headroom over any real AVDTP media MTU (~650-1013B typical), -1 reserved
// for the media payload header byte (num_frames), see pl_a2dp_slot_t. The
// old SBC_ prefix was a codec-identity smell in a file forbidden from
// having one (module doc, constraint 2) -- this is a payload-storage
// ceiling, not an SBC property. Same value, 1030, matches
// a2dp_source_demo.c's own SBC_STORAGE_SIZE.
#define PL_A2DP_PAYLOAD_SLOT_BYTES 1030

// Bead pico-link-85v (D2): tx queue depth. N = 1 (head, being filled) +
// B_tick (sealed this tick) + B_tick (unsent from last tick), where
// B_tick = ceil(dwell_budget / (worst_case_encode_us * frames_per_packet)).
// At the compile-time backstop (PL_A2DP_MAX_ENCODE_DWELL_US = 6000us):
//   SBC:      ceil(6000 / (800 * 7))  = ceil(1.07) = 2
//   LDAC HQ:  ceil(6000 / (1100 * 3)) = ceil(1.82) = 2  (see
//             .planning/design/2026-08-30-ldac.md's tightened ~1100us/frame
//             stopping rule, amended by this same design's D4 consequence)
// N = 1 + 2*2 = 5. Recomputed per-stream at STREAM_ESTABLISHED against the
// actually-negotiated frames_per_packet and the active codec row's
// worst_case_encode_us -- see that handler for the loud WARNING if the
// live derivation would exceed this compile-time count (pico-link-r44
// lesson: a silent clamp is worse than a loud one).
//
// Bead pico-link-nli.3 (G2, `.planning/decisions/2026-09-03-ldac-encoder-on-
// core1.md` sec 3.2): 5 -> 8. Raised to a POWER OF TWO so tx_count's
// derived-count subtraction below can mask instead of modulo, and to add
// headroom now that a filled slot may sit unsent across a cross-core
// handoff rather than just across one tick.
#define PL_A2DP_TX_QUEUE_SLOTS 8
_Static_assert(
    (PL_A2DP_TX_QUEUE_SLOTS & (PL_A2DP_TX_QUEUE_SLOTS - 1)) == 0,
    "PL_A2DP_TX_QUEUE_SLOTS must be a power of two for the tx_head - tx_tail mask derivation"
);
#define PL_A2DP_TX_QUEUE_MASK (PL_A2DP_TX_QUEUE_SLOTS - 1u)

// design sec 1: our own crystal, via btstack_run_loop timers, paces the
// A2DP media stream -- matches a2dp_source_demo.c's own AUDIO_TIMEOUT_MS.
#define PL_A2DP_AUDIO_TIMEOUT_MS 10

// Bead pico-link-du0, design section 21 E17/C8: how often the OUT-meter
// peak/RMS accumulator below is reduced to one Event::LevelsChanged push.
// `core`'s own render-side staleness window (`OUT_LEVEL_STALE_AFTER`,
// `core/src/render/hero.rs`) is derived from this exact cadence and must
// not silently drift from it.
//
// Bead pico-link-ajj (step D, 2026-09-06): lowered from the previous
// 250ms/~4Hz to 50ms/20Hz -- gated on measuring core0's superloop rate
// under a REAL live LDAC stream first (`pl_a2dp_poll_levels`, called once
// per superloop iteration in main.c, is the hard ceiling on how often a
// fresh reading can actually be read and rendered). Measured on hardware,
// WH-1000XM3 connected and genuinely streaming (usb-audio: streaming +
// a2dp: codec=LDAC + pkt_sent climbing, not idle/codec=none): the
// pl_loop_prof "lpf ph=tot" histogram's p50 iteration period was 20ms
// (~50 iterations/s), with two independent frame-counter-vs-wall-clock
// spot checks giving ~86/s and ~165/s -- every one of those, including
// the most conservative (p50), clears the ~20/s (50ms) floor this
// interval needs by 2.5x-8x. This supersedes the previous "ship at 250ms,
// do not tune" override on this same bead -- that override predates this
// measurement.
//
// The publish reduction itself (2 divisions for the mean-square + 2
// `pl_a2dp_isqrt` calls, each an internal division per Newton iteration)
// is still negligible at the new rate: it runs once per push, not once
// per sample, so 50ms -> 20 pushes/s is ~700 divisions/s total -- versus
// the LDAC encoder's own `enc_max_us` measured at ~1.9ms *per frame* on
// this same hardware (a2dp.c's own PL_A2DP_MAX_ENCODE_DWELL_US
// falsifier), i.e. several orders of magnitude more budget than this adds.
#define PL_A2DP_LEVEL_PUSH_INTERVAL_MS 50

// Bead pico-link-648: delay before the single bounded 0x0b retry. Measured
// on hardware (bead comments, 2026-09-01): the stale ACL cleared itself
// about 1s after the 0x0b failure. Rounded up for margin, still well
// inside the couple of seconds a fresh boot's auto-reconnect can afford to
// spend.
#define PL_A2DP_RECONNECT_RETRY_DELAY_MS 1500

// Bead pico-link-4vb.2 (bug 3): how long the wizard's plain-success screen
// stays up before auto-dismissing to Home. "A couple of seconds" per the
// investigation -- long enough to register as a real confirmation, short
// enough that Andreas isn't left waiting on a screen he's done with.
#define PL_A2DP_WIZARD_DISMISS_DELAY_MS 2000

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
// LISTENING TEST 2026-08-31 (bead pico-link-p1r / the LDAC crackle):
// 6000u was an SBC-era value from when a frame cost ~800us. An LDAC frame
// costs ~1159us, so 6000 caps the fill loop at 5 frames/tick while credit
// needs 6.26 -- measured on hardware as 18.5% of ticks tripping this
// backstop (pico-link-zmg, delta stop_dwell/delta tick_count = 0.1851)
// alongside a 9.8% encode shortfall. 10000u allows 8 frames/tick.
// The loop-hogging this bound protects against is already moot at 6Hz
// (pico-link-p1r): 4ms more dwell against a 164ms iteration is noise.
#define PL_A2DP_MAX_ENCODE_DWELL_US 10000u

// Bead pico-link-nli.4 (G3), G4 review hardening: a hard per-call
// wall-clock cap on pl_a2dp_fill()'s loop under PL_ENCODER_ON_CORE1,
// independent of PL_A2DP_MAX_ENCODE_DWELL_US above (that computation stays
// #ifndef'd out for core1 -- see its own doc comment -- this is a NEW,
// separate bound, not a revival of it). Why one is needed here and not
// under the legacy build: samples_owed is clamped to what's actually in
// the PCM ring (see samples_owed's/credit_clamped_samples's doc comments),
// so a single pl_a2dp_fill() call's iteration count is bounded by ring
// depth, not by wall time directly -- a large backlog immediately before a
// quiesce (e.g. after a scheduling gap) could in principle still push one
// call's real duration past pl_a2dp_core1_quiesce_and_wait()'s 5ms
// timeout, which would then be racing a live, still-encoding core1 rather
// than the wedged one it was designed to detect -- a different and worse
// failure. 2ms leaves 3ms of headroom under that 5ms budget for the
// in-flight call to actually finish once it trips this cap and returns.
// See pl_a2dp_fill's loop (stop_core1_budget) for the check that uses it.
#define PL_A2DP_CORE1_FILL_BUDGET_US (2u * 1000u)

// Bead pico-link-85v (D4): work-bound multiplier -- the fill loop never
// needs more than CATCHUP_K times the work real time has owed it; beyond
// that is not catch-up, it is running ahead of the sink. K=2 retires a
// backlog at twice real time: a 100ms stall absorbs in 100ms.
#define PL_A2DP_CATCHUP_K 2u

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

// Bead pico-link-85v (D1): one fixed payload slot. byte 0 of data[] is a
// reserved media-payload header (the SBC media header / frame-count byte
// today); do not hardcode that offset anywhere new -- route it through
// pl_a2dp_usable_payload (this file, below), the single correction point.
// pico-link-cz0.5.6 moves ownership of that header byte to the codec via
// pl_codec_frame_info_t's header_bytes field; this slot shape does not
// change when that lands.
typedef struct {
    uint8_t data[PL_A2DP_PAYLOAD_SLOT_BYTES];
    uint16_t len;    // bytes INCLUDING the reserved header byte
    uint16_t frames; // accumulated during fill -- NOT derived by division (D5)
    uint32_t rtp_ts; // stamped at SEAL, not at send
} pl_a2dp_slot_t;

typedef struct {
    uint16_t a2dp_cid;
    uint8_t local_seid;
    uint8_t remote_seid;
    bd_addr_t connect_addr; // for ConnectFailed's addr payload

    pl_codec_t *codec; // NULL until A2DP_SUBEVENT_SIGNALING_MEDIA_CODEC_SBC_CONFIGURATION
    pl_codec_format_t format;
    pl_codec_frame_info_t frame;

    // Bead pico-link-cz0.5.5 (LDAC L2): capability bits captured from
    // A2DP_SUBEVENT_SIGNALING_MEDIA_CODEC_SBC_CAPABILITY while BTstack
    // walks the remote's SEPs, read back at
    // A2DP_SUBEVENT_SIGNALING_CAPABILITIES_COMPLETE to drive the
    // preference-ordered PL_CODECS walk (pl_a2dp_choose_codec). SBC-shaped
    // only for now -- codec_table.c has no other row; L3 adds an OTHER-
    // capability slot alongside this one when the LDAC row lands. First
    // offering remote SEP wins (a real headphone offers SBC at most once).
    // Reset at SIGNALING_CONNECTION_ESTABLISHED, the one point a fresh
    // discovery pass begins.
    struct {
        bool sbc_offered;
        uint8_t sbc_remote_seid;
        uint8_t sbc_sampling_frequency_bitmap;
        uint8_t sbc_channel_mode_bitmap;
        uint8_t sbc_block_length_bitmap;
        uint8_t sbc_subbands_bitmap;
        uint8_t sbc_allocation_method_bitmap;
        uint8_t sbc_min_bitpool_value;
        uint8_t sbc_max_bitpool_value;

        // Bead pico-link-cz0.5.6 (LDAC L3): the vendor-codec counterpart
        // of the sbc_* fields above, captured from
        // A2DP_SUBEVENT_SIGNALING_MEDIA_CODEC_OTHER_CAPABILITY. "other"
        // because the field names are generic (any AVDTP_CODEC_NON_A2DP
        // row could populate them, matched by vendor_id/vendor_codec_id --
        // see the capability handler below); LDAC is simply the only such
        // row PL_CODECS has today. Same "first offering remote SEP wins"
        // rule as SBC, independently -- a remote offering both an SBC SEP
        // and a vendor SEP populates both halves of this struct in the
        // same discovery pass, no collision.
        bool other_offered;
        uint8_t other_remote_seid;
        uint8_t other_sampling_frequency_bitmap;
        uint8_t other_channel_mode_bitmap;
    } discovered;

    pl_a2dp_media_state_t state;
    btstack_timer_source_t media_timer;
    bool timer_armed;

    // Bead pico-link-648: bounded single retry for
    // ERROR_CODE_ACL_CONNECTION_ALREADY_EXISTS (0x0b) at
    // A2DP_SUBEVENT_SIGNALING_CONNECTION_ESTABLISHED. Measured on hardware:
    // after a short-gap reboot the headset still holds the old ACL, HCI
    // reports 0x0b, and BTstack frees the failed attempt's own connection
    // object with no resume path (see pl_a2dp_failure_reason_for_status's
    // doc comment) -- but a SECOND, unsolicited
    // HCI_EVENT_CONNECTION_COMPLETE with status=0x00 on a fresh handle
    // follows about a second later with nothing on our side asking for it
    // (most likely the headset's own auto-reconnect re-paging us). This
    // timer waits out that window once, then reissues establish_stream on
    // the (by then) cleared ACL. reconnect_retry_used caps it at exactly
    // one attempt per connect() call -- a second 0x0b after the retry is
    // reported to the UI as a real failure, same as any other status.
    btstack_timer_source_t reconnect_retry_timer;
    bool reconnect_retry_armed; // true iff reconnect_retry_timer is currently added to the run loop
    bool reconnect_retry_used;  // true once this connect() attempt has already spent its one retry
    // Bead pico-link-cz0.6 code review: the exact status that armed the
    // retry above, so the STREAM_ESTABLISHED cascade-suppression check
    // (below) can confirm the cascaded failure is really the SAME failure
    // being retried, not a genuinely different one that happens to land in
    // the same ~1.5s window. Meaningful only while reconnect_retry_armed is
    // true.
    uint8_t reconnect_retry_armed_status;

    // Bead pico-link-4vb.2 (bug 2): true once ConnectSucceeded/
    // LinkState(Connected) has been pushed for the CURRENT stream --
    // guards against A2DP_SUBEVENT_STREAM_STARTED (which no longer pushes
    // anything itself) somehow firing twice, and is the natural place to
    // hang "has this stream already announced success" for any future
    // caller. Reset false at A2DP_SUBEVENT_SIGNALING_CONNECTION_
    // ESTABLISHED (a fresh discovery pass, same reset point as
    // s_ctx.discovered above).
    bool connect_succeeded_pushed;

    // Bead pico-link-4vb.2 (bug 3): PL_EVENT_TAG_WIZARD_AUTO_DISMISS had no
    // producer anywhere in firmware -- this one-shot timer is armed right
    // after a ConnectSucceeded push (STREAM_ESTABLISHED above) and fires
    // pl_bt_push_wizard_auto_dismiss() a couple of seconds later. Same
    // armed-flag idiom as reconnect_retry_armed above.
    btstack_timer_source_t wizard_dismiss_timer;
    bool wizard_dismiss_armed;

    int max_media_payload_size;
    // Bead pico-link-85v (D1): rtp_next replaces the old rtp_timestamp --
    // stamped into a slot at SEAL time (not at send), and advanced by
    // frames*pcm_frames_per_encoded_frame at seal, not at send. Preserved
    // across STREAM_SUSPENDED (auto-resume continues one timeline); reset
    // to 0 at STREAM_ESTABLISHED/STREAM_RELEASED/
    // SIGNALING_CONNECTION_RELEASED -- see D6's flush table.
    uint32_t rtp_next;

    // Bead pico-link-85v (D1): the tx ring. Replaces sbc_storage/
    // sbc_storage_count/sbc_ready_to_send. tx_head is the slot currently
    // being filled/sealed (producer side); tx_tail is the next slot to
    // send (consumer side). send_requested mirrors whether we currently
    // have an outstanding request_can_send_now with BTstack.
    //
    // CONCURRENCY, REWRITTEN for bead pico-link-nli.3 (G2). This used to
    // say "none", correctly, because both the fill/seal side and the send
    // side ran in the same 0xFF background IRQ context on one core and an
    // IRQ-nesting argument covered it. As of the `pico-link-nli` epic
    // (`.planning/decisions/2026-09-03-ldac-encoder-on-core1.md` sec 3.2)
    // that stops being true: G3 moves the fill/seal side to core1 thread
    // context while the send side stays core0 0xFF. Two cores execute
    // simultaneously, so an IRQ-nesting argument protects nothing here --
    // a stale "no second context" comment sitting above genuinely
    // concurrent code is worse than no comment. This bead (G2) makes the
    // ring ready for that split WHILE STILL SINGLE-CORE, so any regression
    // it causes is attributable to the ring change alone:
    //
    // - tx_head/tx_tail are `volatile uint32_t`. Each side writes only its
    //   own index (producer writes tx_head, consumer writes tx_tail),
    //   same discipline pcm_ring.h already has.
    // - There is no stored tx_count. It was a read-modify-write touched by
    //   BOTH sides (incremented at seal, decremented at send) -- exactly
    //   the shape that is a lost update once the two sides are on
    //   different cores. The count is DERIVED as
    //   `(tx_head - tx_tail) & PL_A2DP_TX_QUEUE_MASK` by pl_a2dp_tx_count()
    //   below. PL_A2DP_TX_QUEUE_SLOTS is a power of two (8) precisely so
    //   this mask is exact without a modulo.
    // - `__dmb()` brackets each side's publish: the producer orders its
    //   slot writes before publishing tx_head; the consumer orders its
    //   read of tx_head before it reads that slot's data. See
    //   pl_a2dp_seal_head() and pl_a2dp_send_media_packet() below.
    pl_a2dp_slot_t tx[PL_A2DP_TX_QUEUE_SLOTS];
    volatile uint32_t tx_head;
    volatile uint32_t tx_tail;
    bool send_requested;

    // Bead pico-link-nli.4 (G3): `volatile`. Written by pl_a2dp_fill() --
    // core1's thread context as of this epic -- and read by core0's media
    // timer handler for the auto-pause decision while streaming is
    // genuinely concurrent on both cores (not a quiesced transition point).
    // pause_requested/auto_resume/state below stay plain: they are written
    // and read ONLY from core0 (the packet handler and the media timer
    // handler), never touched by pl_a2dp_fill() itself.
    volatile uint32_t silent_ticks;
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
    // Bead pico-link-85v (D4): this tick's own measured interval (the same
    // elapsed_us computed for worst_tick_interval_us/credit accrual below),
    // stashed for pl_a2dp_fill's duty-bound derivation -- same producer
    // context, no new time_us_64() call. 0 means "not yet measured" (boot,
    // or the very first tick ever) -- pl_a2dp_fill treats 0 as "duty bound
    // not yet known" and skips it rather than spuriously binding at 0.
    uint32_t last_tick_elapsed_us;
    // Cumulative frames actually encoded -- the RATE (delta between two
    // pl_a2dp_report samples, ~1s apart) is what the pbv acceptance
    // criterion calls "enc_frames": must read 375 +/- 2 per second in
    // steady state once credit-pacing is correct. Renamed from
    // frames_filled_total (pbv's original diagnostic-only name) to match
    // the design's own vocabulary now that it is a real pass/fail signal,
    // not just a diagnostic.
    volatile uint32_t enc_frames_total;
    // Bead pico-link-85v (D1/D7): ticks_send_pending is RETIRED -- there is
    // no sbc_ready_to_send left to pend on; the fill loop now seals and
    // continues instead of stopping on a pending send. grants (below)
    // replaces its regression-check role.

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

    // --- bead pico-link-pbv round 2 (C2-2), retuned by pico-link-85v
    // (D1/D7): stop-REASON instrumentation. Each counts the actual break
    // site reached in pl_a2dp_fill's loop -- see that function's doc
    // comment for what each one means and why stop_dwell, not
    // stop_credit/stop_queue_full/stop_ring_empty, is the one true
    // safety-trip signal that must read 0 in a healthy run.
    //
    // stop_packet_full is RETIRED by pico-link-85v D1/D5/D7: a full head
    // slot is no longer a loop *stop* -- it is a SEAL, and the loop
    // continues into the next slot. payloads_sealed (below) counts the
    // same event correctly, without conflating it with a real stop.
    // stop_packet_full_hot is RETIRED with it -- its ceiling
    // (one-packet-per-tick output) is exactly what this design removes;
    // stop_queue_full is its replacement tripwire, for the NEW ceiling
    // (no free slot).
    volatile uint32_t stop_credit;
    volatile uint32_t stop_ring_empty;
    volatile uint32_t stop_dwell;
    // Bead pico-link-85v (D7): the new ceiling tripwire, inheriting
    // stop_packet_full_hot's job. Incremented when the fill loop cannot
    // seal because every slot is full (pl_a2dp_tx_count() >=
    // PL_A2DP_TX_QUEUE_SLOTS - 1 -- see pl_a2dp_tx_count()'s doc comment
    // for why the usable ceiling is SLOTS-1, not SLOTS, as of bead
    // pico-link-nli.3) -- the only remaining loop-stop condition besides
    // dwell/credit/ring-empty. Predicted 0 for SBC (acceptance item 1);
    // nonzero means the SEND side, not fill, is the limiter -- read
    // grants/s.
    volatile uint32_t stop_queue_full;

    // Bead pico-link-nli.4 (G3), G4 review hardening: counts trips of
    // PL_A2DP_CORE1_FILL_BUDGET_US's wall-clock cap -- see that macro's
    // doc comment. ONLY wired under PL_ENCODER_ON_CORE1 (asserted 0 under
    // the legacy build, same convention as stop_dwell there); a nonzero
    // reading here under core1 means a single fill() call actually
    // approached the quiesce timeout's margin on real backlog, worth
    // investigating but not itself a correctness bug -- the cap's job is
    // exactly to make that case return promptly instead of overrunning.
    volatile uint32_t stop_core1_budget;

    // Bead pico-link-85v (D7): payloads_sealed's rate vs pkt_sent's rate is
    // the single most important stop/send-side split -- divergence means
    // the send side, not fill, is the limiter. Incremented once per SEAL
    // (a full slot handed off to the tx ring), replacing stop_packet_full's
    // old (and, post-D1, incorrect) role of standing in for this event.
    volatile uint32_t payloads_sealed;
    // High-water of pl_a2dp_tx_count(), reset at STREAM_STARTED
    // (pico-link-r44 lesson: a high-water mark that never resets poisons
    // later derivations). tx_depth_max == PL_A2DP_TX_QUEUE_SLOTS - 1
    // means D2's depth is AT the usable ceiling (see pl_a2dp_tx_count()),
    // i.e. maximally marginal, not merely close to it as it was when
    // tx_count was a true stored counter that could reach SLOTS itself.
    volatile uint32_t tx_depth_max;
    // CAN_SEND_MEDIA_PACKET_NOW events. grants/s vs pkt_sent/s separates
    // "grants are slow" (ACL-credit-bound, expected) from "we did not ask"
    // (a re-arm bug). grants/s is also the number that answers whether
    // LDAC HQ's 140 packets/s is reachable at all -- see the design's
    // capture item (h).
    volatile uint32_t grants;
    // A CAN_SEND_MEDIA_PACKET_NOW grant that arrived with tx_count == 0
    // (D6: a grant already pending inside BTstack when a flush emptied the
    // queue). Nonzero only around SUSPEND/reconnect is healthy; nonzero
    // mid-stream is a bug.
    volatile uint32_t spurious_grants;
    // High-water of fill dwell (dwell_us in pl_a2dp_fill), reset at
    // STREAM_STARTED. Without this, "stop_dwell reads 0" says nothing
    // about how close the loop came to the backstop.
    volatile uint32_t dwell_max_us;
    // Bead pico-link-okx D10: the uncounted break below (pl_a2dp_fill_sbc_buffer,
    // "got != pcm_bytes_needed") -- the ring's fill_bytes() promised at
    // least pcm_bytes_needed but pl_pcm_read() returned less. Shouldn't
    // happen; counted so a conservation check can tell "never happens"
    // from "silently happens sometimes" instead of assuming the former.
    volatile uint32_t fill_short_read;

    // Bead pico-link-8b7: OUT-meter publish instrumentation. Bead
    // pico-link-nli.5 (G4) retargeted these from a bt.c ring push to the
    // seqlock snapshot (pl_a2dp_publish_levels), but the counters mean the
    // same things. level_push_count is a real snapshot publish (the ONLY
    // thing pl_a2dp_poll_levels/hero.rs's OUT_LEVEL_STALE_AFTER can see).
    // level_push_skip_empty is pl_a2dp_publish_levels's sample_count==0
    // early return (suspect 1 in the original bead). level_push_skip_interval
    // is the 250ms-not-elapsed early return (expected to dominate in a
    // healthy run -- fill() is called far more often than every 250ms).
    volatile uint32_t level_push_count;
    volatile uint32_t level_push_skip_empty;
    volatile uint32_t level_push_skip_interval;

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
    // acceptance's conservation check (A1) still sums this term). As of
    // pico-link-fhf this is the primary surplus-side resync counter: the
    // media-timer trim block below increments it by pl_pcm_trim_to()'s
    // return value whenever the EMA sustains above the hysteresis band.
    volatile uint32_t resync_drops;

    // Bead pico-link-fhf: how many DISCRETE trims fired, as opposed to how
    // many frames they dropped (resync_drops). Needed because resync_drops
    // alone can't distinguish one large trim from many small ones -- that
    // distinction is exactly the pass/fail line in the injection test (test
    // A: exactly one event vs a double-cut).
    volatile uint32_t resync_events;

    // Bead pico-link-fhf: pbv_now_us at the last trim, for the
    // PL_PCM_TRIM_MIN_INTERVAL_US lockout gate in the media-timer handler.
    uint64_t last_resync_us;
} pl_a2dp_ctx_t;

static pl_a2dp_ctx_t s_ctx;

#ifdef PL_DEBUG_REMOTE
// Bead pico-link-fhf, test A. Set by pl_a2dp_debug_skip_media_ticks()
// (thread context, debug_remote.c's poll) and consumed at the top of the
// drain step in pl_a2dp_media_timer_handler (the same 0xFF IRQ context that
// owns every other s_ctx field this file writes from there) -- single
// aligned 32-bit word, one thread-context writer, one IRQ-context
// reader/decrementer, so no tear and no lock needed, same discipline as
// pcm_ring's s_head/s_tail.
static volatile uint32_t s_debug_skip_media_ticks;
#endif

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

// Bead pico-link-du0 (design section 21 E17/C8): the OUT-meter accumulator.
// Updated cheaply (integer only, no sqrt/float) inside pl_a2dp_fill's
// per-unit loop, right where the PCM already sits in s_pcm_scratch for
// encoding -- "sampled cheaply where the PCM already is", never a
// separate read of its own. Reduced to one seqlock-snapshot publish every
// PL_A2DP_LEVEL_PUSH_INTERVAL_MS by pl_a2dp_publish_levels (bead
// pico-link-nli.5, G4 -- originally a direct Event::LevelsChanged ring
// push, see that function's doc comment), called once per pl_a2dp_fill
// call. That call site is IRQ context (media timer handler) under
// PL_ENCODER_ON_CORE1=OFF and core1 thread context under =ON -- either
// way this whole struct is touched from exactly one context per build, so
// no lock is needed here (same single-writer shape s_ctx itself has; the
// seqlock in pl_a2dp_publish_levels is for its OWN reader, a different
// core/context, not for this accumulator).
typedef struct {
    uint32_t peak_l; // running max abs sample this window (0..32768)
    uint32_t peak_r;
    uint64_t sum_sq_l; // running sum of squared samples this window, for RMS
    uint64_t sum_sq_r;
    uint32_t sample_count; // stereo frames accumulated this window
    uint64_t last_push_us; // time_us_64() at the last push (0 == never pushed)
} pl_a2dp_level_accum_t;

static pl_a2dp_level_accum_t s_level_accum;

// Bead pico-link-nli.5 (G4, design sec 5): the cross-core level publish.
// Replaces the old bt.c-ring push (pl_bt_push_levels_changed, deleted by
// this bead) with a seqlock snapshot: a level is not an event -- the
// newest value is always the wanted one, and a ring that drops the NEWEST
// entry when full (bt.c's own documented policy) is exactly backwards for
// that. A seqlock has no queue to overflow: the writer always wins, and
// the reader (pl_a2dp_poll_levels below, core0's superloop) simply takes
// whatever is current, retrying only if it caught a write in progress.
//
// `seq` is even when the snapshot is quiescent and odd while a write is in
// flight. The writer brackets its field writes with two `__dmb()`s (same
// publish-after-write discipline as the tx ring's `tx_head`, see that
// field's doc comment) so the reader never observes a field write before
// the odd `seq` that guards it, nor the final even `seq` before the field
// writes it guards. This is the standard seqlock shape (Linux's
// `include/linux/seqlock.h` is the canonical reference) with plain
// `__dmb()` in place of `smp_wmb()`/`smp_rmb()` -- RP2350's two M33s share
// coherent SRAM with no cache to maintain, so only store ORDERING needs
// enforcing, exactly design sec 3.1's argument for the two PCM/tx rings.
//
// No lock: the writer never blocks and never disables interrupts, which is
// what makes this legal to call from core1 (design sec 4.3 invariant 3 --
// the multicore lockout handshake in flash_lockout.c depends on core1
// never sitting in a critical section).
//
// `seq == 0` is the sentinel "never published" -- the writer's first
// publish takes it to 2 (0 -> 1 -> 2), never back to 0, so the reader can
// tell "no sample yet" from "a real even sequence" unambiguously.
typedef struct {
    volatile uint32_t seq;
    uint8_t peak_l;
    uint8_t peak_r;
    uint8_t rms_l;
    uint8_t rms_r;
} pl_a2dp_level_snapshot_t;

static pl_a2dp_level_snapshot_t s_level_snapshot;

// Folds `frame_count` stereo PCM frames (interleaved L/R int16, exactly
// s_pcm_scratch's own layout) into s_level_accum. Integer-only: an abs
// and a compare for peak, one 16x16->32-bit multiply-accumulate for the
// RMS sum-of-squares -- negligible next to the encode call this sits
// beside (dwell_max_us tracks THAT cost, not this one, deliberately kept
// separate so this addition is visible if it ever isn't negligible).
static inline void pl_a2dp_accumulate_levels(const int16_t *pcm, uint16_t frame_count) {
    for (uint16_t i = 0; i < frame_count; i++) {
        int32_t l = pcm[2 * i];
        int32_t r = pcm[2 * i + 1];
        uint32_t abs_l = (uint32_t)(l < 0 ? -l : l);
        uint32_t abs_r = (uint32_t)(r < 0 ? -r : r);
        if (abs_l > s_level_accum.peak_l) {
            s_level_accum.peak_l = abs_l;
        }
        if (abs_r > s_level_accum.peak_r) {
            s_level_accum.peak_r = abs_r;
        }
        s_level_accum.sum_sq_l += (uint64_t)((int64_t)l * (int64_t)l);
        s_level_accum.sum_sq_r += (uint64_t)((int64_t)r * (int64_t)r);
    }
    s_level_accum.sample_count += frame_count;
}

// Integer square root (Newton's method, a handful of iterations) -- no
// libm dependency for the once-per-push RMS reduction below. `value` is
// at most a uint32_t's worth of mean-square (see the call site), so this
// converges in well under 32 iterations; capped defensively anyway.
static uint32_t pl_a2dp_isqrt(uint64_t value) {
    if (value == 0) {
        return 0;
    }
    uint64_t x = value;
    uint64_t y = (x + 1) / 2;
    for (int i = 0; i < 32 && y < x; i++) {
        x = y;
        y = (x + value / x) / 2;
    }
    return (uint32_t)x;
}

// Reduces s_level_accum to one seqlock-published snapshot, if
// PL_A2DP_LEVEL_PUSH_INTERVAL_MS has elapsed since the last one AND at
// least one sample was accumulated this window (an empty window -- the
// ring genuinely starved, design's "silent, not zero-but-live" case --
// publishes nothing rather than a misleading all-zero reading; the Home
// hero's own staleness window then correctly shows the meter as absent
// once PL_A2DP_LEVEL_PUSH_INTERVAL_MS's worth of silence has passed. See
// core/src/render/hero.rs's OUT_LEVEL_STALE_AFTER doc comment). Called
// once per pl_a2dp_fill invocation.
//
// Bead pico-link-nli.5 (G4): this is s_level_snapshot's SOLE writer, but
// which execution context that is depends on PL_ENCODER_ON_CORE1 -- IRQ
// context (media timer handler) when OFF, core1 thread context when ON.
// Either way there is exactly one writer at a time, so no writer-side lock
// is needed; the seqlock exists for the READER (a different core, or a
// different context on the same core) to observe consistent fields, not
// to arbitrate between writers. Invariant 4 (no pl_log) and invariant 6
// (no bt.c ring push) both hold here in both builds -- this function
// touches only its own file-scope statics and the seqlock below.
//
// peak_l/peak_r/rms_l/rms_r are linear 0-255 (matching
// PlLevelsChangedPayload's scale, 255 == full-scale/clipping): a 16-bit
// PCM sample's magnitude tops out at 32768, so `>> 7` maps that range
// onto 0-255 (32768 >> 7 == 256, clamped to 255 below for the exact
// full-scale sample).
static void pl_a2dp_publish_levels(void) {
    if (s_level_accum.sample_count == 0) {
        s_ctx.level_push_skip_empty++;
        return;
    }
    uint64_t now = time_us_64();
    if (s_level_accum.last_push_us != 0 &&
        now - s_level_accum.last_push_us < (uint64_t)PL_A2DP_LEVEL_PUSH_INTERVAL_MS * 1000) {
        s_ctx.level_push_skip_interval++;
        return;
    }
    s_ctx.level_push_count++;

    uint32_t mean_sq_l = (uint32_t)(s_level_accum.sum_sq_l / s_level_accum.sample_count);
    uint32_t mean_sq_r = (uint32_t)(s_level_accum.sum_sq_r / s_level_accum.sample_count);
    uint32_t rms_l = pl_a2dp_isqrt(mean_sq_l);
    uint32_t rms_r = pl_a2dp_isqrt(mean_sq_r);

    uint8_t peak_l_u8 = (uint8_t)(s_level_accum.peak_l >> 7 > 255 ? 255 : s_level_accum.peak_l >> 7);
    uint8_t peak_r_u8 = (uint8_t)(s_level_accum.peak_r >> 7 > 255 ? 255 : s_level_accum.peak_r >> 7);
    uint8_t rms_l_u8 = (uint8_t)(rms_l >> 7 > 255 ? 255 : rms_l >> 7);
    uint8_t rms_r_u8 = (uint8_t)(rms_r >> 7 > 255 ? 255 : rms_r >> 7);

    // Seqlock write. seq starts even (or 0, the sentinel); bump to odd
    // FIRST so a reader that samples mid-write sees odd and retries, THEN
    // publish the fields, THEN bump back to even so a reader that sampled
    // the new even seq is guaranteed (by the __dmb() below) to see the
    // fields that go with it, not a torn mix of old and new.
    uint32_t seq = s_level_snapshot.seq;
    s_level_snapshot.seq = seq + 1u; // odd -- write in flight, readers must retry
    __dmb(); // publish-after-write half 1: the odd seq must be visible before the fields change
    s_level_snapshot.peak_l = peak_l_u8;
    s_level_snapshot.peak_r = peak_r_u8;
    s_level_snapshot.rms_l = rms_l_u8;
    s_level_snapshot.rms_r = rms_r_u8;
    __dmb(); // publish-after-write half 2: the fields must be visible before the even seq is
    s_level_snapshot.seq = seq + 2u; // even -- consistent, and never back to the 0 sentinel

    s_level_accum.peak_l = 0;
    s_level_accum.peak_r = 0;
    s_level_accum.sum_sq_l = 0;
    s_level_accum.sum_sq_r = 0;
    s_level_accum.sample_count = 0;
    s_level_accum.last_push_us = now;
}

// Bead pico-link-nli.5 (G4): s_level_snapshot's sole reader. Intended to be
// called once per superloop iteration, thread context (main.c), AFTER
// pl_ui_tick -- deliberately, not before. pl_ui_tick is what advances the
// app core's own clock (ui-ffi's `now_us`, which becomes
// Event::LevelsChanged's received_at on the Rust side, core/src/app.rs's
// `on_levels_changed`); calling this before tick would stamp a fresh
// sample with the PREVIOUS iteration's clock value, which was
// pico-link-8b7's original staleness-at-birth bug (main.c used to drain
// BTstack/level events before ticking). Reading directly in the superloop
// and pushing right here closes that at the root: the sample is now
// timestamped in the same iteration that will render it.
//
// No time-based rate limit is needed on this side beyond "did the
// sequence actually change since last time": the writer already
// rate-limits itself to one publish per PL_A2DP_LEVEL_PUSH_INTERVAL_MS
// (pl_a2dp_publish_levels above), so an unchanged sequence means nothing
// new landed, and re-pushing the same values with a fresher received_at
// would be misleading, not helpful.
void pl_a2dp_poll_levels(struct PlUi *ui) {
    static uint32_t s_last_seen_seq;

    uint32_t seq_before = 0;
    uint32_t seq_after = 0;
    uint8_t peak_l = 0;
    uint8_t peak_r = 0;
    uint8_t rms_l = 0;
    uint8_t rms_r = 0;
    bool consistent = false;

    // Bounded retry, not a spin: an odd seq or a seq that changed under us
    // means the writer was mid-publish, and that critical section is four
    // field writes between two __dmb()s -- microseconds. A handful of
    // retries comfortably covers that; if it's still inconsistent after
    // this many tries something is more wrong than a race (a wedged
    // writer), and spinning the superloop waiting for it would turn a
    // diagnostic overlay into a real-time hazard. Bail and try again next
    // iteration instead.
    for (int tries = 0; tries < 8; tries++) {
        seq_before = s_level_snapshot.seq;
        if (seq_before & 1u) {
            continue; // writer mid-publish
        }
        __dmb();
        peak_l = s_level_snapshot.peak_l;
        peak_r = s_level_snapshot.peak_r;
        rms_l = s_level_snapshot.rms_l;
        rms_r = s_level_snapshot.rms_r;
        __dmb();
        seq_after = s_level_snapshot.seq;
        if (seq_after == seq_before) {
            consistent = true;
            break;
        }
    }
    if (!consistent) {
        return;
    }
    if (seq_before == 0 || seq_before == s_last_seen_seq) {
        // 0: never published yet (the sentinel, see s_level_snapshot's doc
        // comment). Otherwise: same value already delivered -- see this
        // function's doc comment for why that's a no-op, not a re-push.
        return;
    }
    s_last_seen_seq = seq_before;

    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_LEVELS_CHANGED,
        .payload = {.levels_changed = {.peak_l = peak_l, .peak_r = peak_r, .rms_l = rms_l, .rms_r = rms_r}},
    };
    pl_ui_push_event(ui, event);
}

// T3 (pico-link-4v2.3), design sec 9: the AVRCP connection's cid, tracked
// here because this is the one handler that sees both
// CONNECTION_ESTABLISHED and CONNECTION_RELEASED. 0 is BTstack's "no
// connection" sentinel (cids are allocated starting at 1) -- matches
// volume.c's own convention of a dirty flag rather than a magic value.
// Read from pl_a2dp_avrcp_volume_service() below, which runs on the SAME
// cyw43 background IRQ (0xFF) this handler runs on, so no synchronization
// is needed between the two -- both are BTstack packet-handler/timer
// callbacks on one run loop, never preempted by each other.
static uint16_t s_avrcp_cid;

// T3: gates "one SET_ABSOLUTE_VOLUME in flight at a time" (design sec 9's
// explicit constraint -- AVCTP shares the link with A2DP media and this
// project has a documented crackle history from flooding it). Also 0xFF
// context only, same reasoning as s_avrcp_cid above.
static bool s_avrcp_volume_set_in_flight;
static uint64_t s_avrcp_volume_set_deadline_us;
#define PL_AVRCP_VOLUME_SET_TIMEOUT_US (1500ull * 1000ull)

// T4 (pico-link-4v2.4), design sec 6/9: whether the connected sink actually
// supports AVRCP_NOTIFICATION_EVENT_VOLUME_CHANGED, learned from the
// AVRCP_SUBEVENT_NOTIFICATION_STATE response to our
// avrcp_controller_enable_notification() call below.
// PL_AVRCP_VOLUME_NOTIFY_UNKNOWN until that response (or a fresh connect)
// says otherwise -- a rejection is a SUPPORTED outcome per the design, not
// a bug: direction A still works, direction B is silently absent. 0xFF
// context only, same reasoning as s_avrcp_cid above.
typedef enum {
    PL_AVRCP_VOLUME_NOTIFY_UNKNOWN = 0,
    PL_AVRCP_VOLUME_NOTIFY_SUPPORTED,
    PL_AVRCP_VOLUME_NOTIFY_UNSUPPORTED,
} PlAvrcpVolumeNotifySupport;
static PlAvrcpVolumeNotifySupport s_avrcp_volume_notify_support = PL_AVRCP_VOLUME_NOTIFY_UNKNOWN;

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
            s_avrcp_cid = avrcp_subevent_connection_established_get_avrcp_cid(packet);
            pl_log("avrcp: connection established, cid=%u\r\n", s_avrcp_cid);
            // T4 (pico-link-4v2.4), design sec 9: register for
            // AVRCP_NOTIFICATION_EVENT_VOLUME_CHANGED as soon as the
            // signaling connection exists -- this is direction B's only
            // entry point. Outcome (supported/rejected) arrives later as
            // AVRCP_SUBEVENT_NOTIFICATION_STATE in the controller packet
            // handler below.
            s_avrcp_volume_notify_support = PL_AVRCP_VOLUME_NOTIFY_UNKNOWN;
            uint8_t enable_status = avrcp_controller_enable_notification(s_avrcp_cid, AVRCP_NOTIFICATION_EVENT_VOLUME_CHANGED);
            pl_log("avrcp: enable VOLUME_CHANGED notification -> status=0x%02x cid=%u\r\n", enable_status, s_avrcp_cid);
            break;
        case AVRCP_SUBEVENT_CONNECTION_RELEASED:
            pl_log("avrcp: connection released, cid=%u\r\n", s_avrcp_cid);
            s_avrcp_cid = 0;
            s_avrcp_volume_set_in_flight = false;
            s_avrcp_volume_notify_support = PL_AVRCP_VOLUME_NOTIFY_UNKNOWN;
            break;
        default:
            break;
    }
}

// T3 (pico-link-47z.3, design sec 3): media-key passthrough FROM the
// headphones arrives here, not in pl_a2dp_avrcp_controller_packet_handler
// above -- BTstack's avrcp_target.c is the module that handles
// AVRCP_CMD_OPCODE_PASS_THROUGH and emits AVRCP_SUBEVENT_OPERATION
// (avrcp_target.c:1092-1109); the controller module only emits
// OPERATION_START/COMPLETE for commands *we* send, which nothing here does
// yet. This runs on the cyw43/BTstack background IRQ (priority 0xFF, same
// context as pl_bt_packet_handler -- see bt.c:50-127's doc comment), so it
// must only push into media_keys.c's ring and return; it must NEVER call
// tud_hid_* directly (that is media_keys.c's exclusive job, see its module
// doc) nor call into Rust (pico-link-6o2's constraint).
//
// avrcp_target_operation_accepted() is already called by BTstack itself at
// avrcp_target.c:1104, BEFORE this handler's event is even emitted -- we
// must not call avrcp_target_operation_accepted/rejected ourselves, or the
// AVCTP response would be double-sent.
static void pl_a2dp_avrcp_target_packet_handler(uint8_t packet_type, uint16_t channel, uint8_t *packet, uint16_t size) {
    (void)channel;
    (void)size;
    if (packet_type != HCI_EVENT_PACKET) {
        return;
    }
    if (hci_event_packet_get_type(packet) != HCI_EVENT_AVRCP_META) {
        return;
    }
    if (packet[2] != AVRCP_SUBEVENT_OPERATION) {
        return;
    }

    uint8_t operation_id = avrcp_subevent_operation_get_operation_id(packet);
    // button_pressed: nonzero on press, zero on release (avrcp_target.c:1102
    // derives it from (packet[6] & 0x80) == 0, but BTstack's own accessor
    // already gives us the resolved value) -- press and release arrive as
    // two separate AVRCP_SUBEVENT_OPERATION events with the same
    // operation_id, design sec 3.2.
    bool pressed = avrcp_subevent_operation_get_button_pressed(packet) != 0;

    // pico-link-wnk: this is the ONE unambiguous signal that a real headphone
    // button press physically arrived over AVRCP passthrough -- distinct from
    // any console-injected MEDIA PLAYPAUSE/NEXT/PREV command, which never
    // reaches this function. Log unconditionally, unconditionally on every
    // operation_id (even ones this handler ignores below), so a single
    // morning button-press is visible in the log with no ambiguity about
    // which producer fired. Timestamp is time_us_64() -- this handler runs on
    // the cyw43/BTstack background IRQ, so it is not gated by the superloop's
    // own cadence.
    pl_log(
        "avrcp-target: PASSTHROUGH operation_id=0x%02x %s at t=%llu us\r\n", operation_id,
        pressed ? "PRESS" : "release", (unsigned long long)time_us_64()
    );

    // Design sec 3.4's mapping. PLAY and PAUSE both map to the single
    // 0x00CD toggle usage -- deliberately, not an oversight: we never call
    // avrcp_target_set_playback_status, so the headphone's own view of
    // play/pause state is not ours to keep in sync, and mapping both
    // operation IDs to the toggle is correct regardless of which one the
    // XM3 happens to send. Discrete play/pause is a later bead once
    // playback status is reported upstream (design sec 9).
    uint16_t usage;
    switch (operation_id) {
        case AVRCP_OPERATION_ID_PLAY:
        case AVRCP_OPERATION_ID_PAUSE:
            usage = PL_MEDIA_KEY_USAGE_PLAY_PAUSE;
            break;
        case AVRCP_OPERATION_ID_FORWARD:
            usage = PL_MEDIA_KEY_USAGE_SCAN_NEXT;
            break;
        case AVRCP_OPERATION_ID_BACKWARD:
            usage = PL_MEDIA_KEY_USAGE_SCAN_PREV;
            break;
        default:
            // Anything else (design sec 3.4: "anything else -- ignored, no
            // report") -- e.g. volume up/down, which this bead does not
            // implement. Still log it: the PASSTHROUGH line above already
            // proved the button arrived, but this line says whether THIS
            // firmware maps it to anything, which matters if the morning
            // test is a button this handler does not recognize.
            pl_log("avrcp-target: operation_id=0x%02x not mapped, no HID report\r\n", operation_id);
            return;
    }

    if (pressed) {
        pl_media_keys_push_press(usage);
    } else {
        pl_media_keys_push_release();
    }
    pl_log("avrcp-target: mapped to HID usage 0x%04x, pushed to media_keys ring\r\n", (unsigned)usage);
}

// T3 (pico-link-4v2.3), design sec 9: the response side of
// avrcp_controller_set_absolute_volume, sent from
// pl_a2dp_avrcp_volume_service() below. Clears the in-flight gate so the
// next SET can go out.
//
// T4 (pico-link-4v2.4), design sec 9: also handles direction B --
// AVRCP_SUBEVENT_NOTIFICATION_VOLUME_CHANGED (the sink telling us its
// volume moved) and AVRCP_SUBEVENT_NOTIFICATION_STATE (the
// enable_notification response, which is where a rejection surfaces).
static void pl_a2dp_avrcp_controller_packet_handler(uint8_t packet_type, uint16_t channel, uint8_t *packet, uint16_t size) {
    (void)channel;
    (void)size;
    if (packet_type != HCI_EVENT_PACKET) {
        return;
    }
    if (hci_event_packet_get_type(packet) != HCI_EVENT_AVRCP_META) {
        return;
    }
    switch (packet[2]) {
        case AVRCP_SUBEVENT_SET_ABSOLUTE_VOLUME_RESPONSE: {
            uint16_t cid = avrcp_subevent_set_absolute_volume_response_get_avrcp_cid(packet);
            uint8_t applied = avrcp_subevent_set_absolute_volume_response_get_absolute_volume(packet);
            pl_log("avrcp: SET_ABSOLUTE_VOLUME response cid=%u applied=%u\r\n", cid, applied);
            s_avrcp_volume_set_in_flight = false;
            break;
        }
        case AVRCP_SUBEVENT_NOTIFICATION_STATE: {
            // Bead pico-link-2ue/rmp's design sec 6 M1 verdict made this
            // path real: does the sink support
            // AVRCP_NOTIFICATION_EVENT_VOLUME_CHANGED at all? A rejection
            // here (status != SUCCESS) is a SUPPORTED outcome (design sec
            // 4.3's table, sec 9's T4 doc) -- direction A keeps working,
            // direction B is silently absent. Not scoped to any other
            // event_id: this device only ever registers for
            // VOLUME_CHANGED, but check anyway rather than assume.
            uint8_t event_id = avrcp_subevent_notification_state_get_event_id(packet);
            if (event_id != (uint8_t)AVRCP_NOTIFICATION_EVENT_VOLUME_CHANGED) {
                break;
            }
            uint8_t status = avrcp_subevent_notification_state_get_status(packet);
            uint8_t enabled = avrcp_subevent_notification_state_get_enabled(packet);
            if (status == ERROR_CODE_SUCCESS) {
                s_avrcp_volume_notify_support = PL_AVRCP_VOLUME_NOTIFY_SUPPORTED;
                pl_log("avrcp: VOLUME_CHANGED notification state -> enabled=%u (supported)\r\n", enabled);
            } else {
                s_avrcp_volume_notify_support = PL_AVRCP_VOLUME_NOTIFY_UNSUPPORTED;
                pl_log("avrcp: VOLUME_CHANGED notification REJECTED status=0x%02x -- sink does not support direction B\r\n", status);
            }
            break;
        }
        case AVRCP_SUBEVENT_NOTIFICATION_VOLUME_CHANGED: {
            // Direction B (design sec 9, T4): the sink is telling us its
            // volume changed. `absolute_volume` is already 0..127, the
            // canonical AVRCP domain -- design sec 5, no mapping needed.
            // Feed it through the SAME loop rule host SETs go through
            // (volume.c's pl_volume_notify_sink); it is what decides
            // whether this is actually new or an echo to be absorbed.
            uint8_t volume = avrcp_subevent_notification_volume_changed_get_absolute_volume(packet);
            pl_log("avrcp: VOLUME_CHANGED notification -> absolute_volume=%u\r\n", volume);
            pl_volume_notify_sink(volume);
            // AVRCP notifications are one-shot per the spec's
            // INTERIM/CHANGED lifecycle -- BTstack's avrcp_controller.c
            // (avrcp_controller_handle_notification, CHANGED_STABLE case)
            // already re-arms internally unless we ask it to deregister,
            // but re-registering here too is cheap, idempotent per
            // avrcp_controller_register_notification's own early-return
            // guards, and removes any dependence on that internal
            // behaviour continuing to hold -- see this bead's comment log
            // for the source read that found it.
            if (s_avrcp_cid != 0) {
                avrcp_controller_enable_notification(s_avrcp_cid, AVRCP_NOTIFICATION_EVENT_VOLUME_CHANGED);
            }
            break;
        }
        default:
            break;
    }
}

// T3 (pico-link-4v2.3): called once per pl_bt_wdt_heartbeat_handler tick
// (bt.c, cyw43 background IRQ 0xFF -- design sec 2/9's "-> heartbeat (0xFF)
// -> avrcp_controller_set_absolute_volume" path). Consumes volume.c's
// outbound AVRCP latch and sends AT MOST one SET_ABSOLUTE_VOLUME, gated on
// the previous one's response (or a timeout) -- design sec 9's explicit
// "one SET in flight at a time" constraint, since AVCTP shares the link
// with A2DP media (this project's crackle history).
void pl_a2dp_avrcp_volume_service(uint64_t now_us) {
    if (s_avrcp_volume_set_in_flight) {
        if (now_us < s_avrcp_volume_set_deadline_us) {
            return; // still waiting for AVRCP_SUBEVENT_SET_ABSOLUTE_VOLUME_RESPONSE
        }
        pl_log("avrcp: SET_ABSOLUTE_VOLUME timed out waiting for response, cid=%u\r\n", s_avrcp_cid);
        s_avrcp_volume_set_in_flight = false;
    }

    uint8_t level;
    if (!pl_volume_take_avrcp_desired(&level)) {
        return; // nothing new since the last tick
    }
    if (s_avrcp_cid == 0) {
        // No AVRCP connection right now -- the value is dropped, not
        // queued (design sec 8/9 doesn't call for a retry-on-reconnect;
        // the next real host/sink edge will re-propagate once a
        // connection exists). Logged so this is visible, not silent.
        pl_log("avrcp: volume SET dropped, no AVRCP connection, level=%u\r\n", level);
        return;
    }
    uint8_t status = avrcp_controller_set_absolute_volume(s_avrcp_cid, level);
    if (status != ERROR_CODE_SUCCESS) {
        pl_log("avrcp: avrcp_controller_set_absolute_volume failed status=%u level=%u cid=%u\r\n", status, level, s_avrcp_cid);
        return;
    }
    s_avrcp_volume_set_in_flight = true;
    s_avrcp_volume_set_deadline_us = now_us + PL_AVRCP_VOLUME_SET_TIMEOUT_US;
    pl_log("avrcp: SET_ABSOLUTE_VOLUME sent level=%u cid=%u\r\n", level, s_avrcp_cid);
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
//
// Bead pico-link-cz0.5.6: the reserved header size is now the ACTIVE
// codec row's own header_bytes (codec_table.h), not a hardcoded 1 --
// SBC's row sets header_bytes=1, reproducing today's behaviour exactly;
// a future codec with a different header shape changes only its own row.
static inline uint32_t pl_a2dp_usable_payload(int max_media_payload_size, uint8_t header_bytes) {
    return max_media_payload_size > (int)header_bytes ? (uint32_t)(max_media_payload_size - (int)header_bytes) : 0u;
}

// Bead pico-link-nli.3 (G2): the tx ring's depth, DERIVED rather than
// stored. The old stored `tx_count` was a read-modify-write touched by
// both the seal side (++) and the send side (--); that is a lost update
// the instant the two sides are on different cores (`pico-link-nli`
// epic, G3). tx_head/tx_tail are each written by only one side, so
// `(tx_head - tx_tail) & PL_A2DP_TX_QUEUE_MASK` needs no lock -- but the
// mask means this ring uses the SAME one-slot-reserved discipline
// pcm_ring.h's "capacity-1 usable" comment documents, and for the same
// reason: with tx_head/tx_tail each reduced mod PL_A2DP_TX_QUEUE_SLOTS
// (as they always have been, just via `%` before this bead and via `&`
// now), a masked subtraction cannot tell "0 sealed" from "SLOTS sealed"
// apart -- both give tx_head == tx_tail. Reserving one slot -- i.e.
// treating PL_A2DP_TX_QUEUE_SLOTS - 1 (7 of 8) as the real usable depth
// and never letting a caller seal past it -- removes the ambiguity: the
// count can now only ever read [0, SLOTS-1], so SLOTS-1 unambiguously
// means "at capacity" and is never confused with "empty" (0). This is
// WHY every full-check below now compares against
// `PL_A2DP_TX_QUEUE_SLOTS - 1`, not `PL_A2DP_TX_QUEUE_SLOTS` as the old
// stored counter's checks did -- that counter could legitimately reach
// SLOTS itself (all slots holding sealed data) because it was a true
// count, not a masked index distance. The new usable ceiling (7) is
// still strictly more headroom than the old hard ceiling (5), so this is
// not a capacity regression.
//
// Wraparound: unsigned subtraction of two uint32_t is defined to wrap
// modulo 2^32, so `tx_head - tx_tail` yields the true forward distance
// even across a 2^32 rollover (not reachable in any uptime that
// matters, but the arithmetic needs no special case for it), and the
// subsequent `& PL_A2DP_TX_QUEUE_MASK` reduces that distance into
// [0, SLOTS) exactly as it does for any non-wrapped difference.
static inline uint32_t pl_a2dp_tx_count(void) {
    return (s_ctx.tx_head - s_ctx.tx_tail) & PL_A2DP_TX_QUEUE_MASK;
}

// Bead pico-link-85v (D6): resets the tx ring to empty -- called at every
// point that already calls pl_pcm_reset() (STREAM_ESTABLISHED/SUSPENDED/
// RELEASED) plus SIGNALING_CONNECTION_RELEASED, which flushed nothing
// before this bead. rtp_next is deliberately NOT touched here -- it is
// preserved across SUSPEND (auto-resume continues one timeline) and reset
// explicitly by the three call sites that want it reset. A grant can
// arrive after this runs (send_requested may already be true inside
// BTstack) -- pl_a2dp_send_media_packet's tx_count==0 guard handles it
// (counted as spurious_grants).
// Bead pico-link-cz0.5.6: factored out of pl_a2dp_fill's two seal sites --
// the fixed-size capacity-full seal (SBC, unchanged behaviour) and the
// codec-reported payload_complete seal (LDAC, self-packetising). Both
// mean the same thing operationally: hand the head slot to the tx ring and
// arm a send. Caller is responsible for the
// `pl_a2dp_tx_count() < PL_A2DP_TX_QUEUE_SLOTS - 1` guard (stop_queue_full)
// BEFORE calling -- this function does not check it.
//
// Bead pico-link-nli.3 (G2): this is the tx ring's PRODUCER side, and as
// of the `pico-link-nli` epic (G3) it is the side that moves to core1.
// The `__dmb()` below is publish-after-write: it orders every store into
// *head (the RTP timestamp, and -- already done by the caller -- the
// slot's encoded payload bytes) BEFORE the tx_head update that hands the
// slot to core0's send side. Without it a core0 consumer could observe
// the new tx_head and read a still-in-flight/torn slot.
static void pl_a2dp_seal_head(void) {
    pl_a2dp_slot_t *head = &s_ctx.tx[s_ctx.tx_head];
    head->rtp_ts = s_ctx.rtp_next;
    s_ctx.rtp_next += (uint32_t)head->frames * s_ctx.frame.pcm_frames_per_encoded_frame;
    __dmb(); // publish-after-write: slot contents visible before tx_head advances
    s_ctx.tx_head = (s_ctx.tx_head + 1u) & PL_A2DP_TX_QUEUE_MASK;
    uint32_t depth = pl_a2dp_tx_count();
    if (depth > s_ctx.tx_depth_max) {
        s_ctx.tx_depth_max = depth;
    }
    s_ctx.payloads_sealed++;
#ifndef PL_ENCODER_ON_CORE1
    if (!s_ctx.send_requested) {
        s_ctx.send_requested = true;
        a2dp_source_stream_endpoint_request_can_send_now(s_ctx.a2dp_cid, s_ctx.local_seid);
    }
#endif
    // Bead pico-link-nli.4 (G3): under PL_ENCODER_ON_CORE1 this function
    // runs on core1, and a2dp_source_stream_endpoint_request_can_send_now
    // is a BTstack call -- forbidden from core1 (design sec 4.3 invariant
    // 5/6; BTstack is not reentrant across cores and owns no cross-core
    // locking of its own). The request is issued instead by core0's media
    // timer handler's "send kick" (design sec 5: "if slots pending and not
    // send_requested, request now") -- see pl_a2dp_media_timer_handler
    // below. send_requested itself stays a plain (non-volatile) bool under
    // core1 mode because it is then written/read ONLY from core0 (this
    // function no longer touches it; pl_a2dp_send_media_packet and the
    // media timer's send-kick are both core0) -- no cross-core access, no
    // barrier needed.
}

// Bead pico-link-nli.3 (G2): still core0-only today, and per the
// `pico-link-nli` epic design (sec 4.2) it stays that way after G3 too --
// every call site (STREAM_ESTABLISHED/SUSPENDED/RELEASED,
// SIGNALING_CONNECTION_RELEASED) runs on core0 while core1 is quiesced by
// the state-machine handshake, so this never races the producer side. No
// barrier needed here for that reason; do not add one speculatively.
static void pl_a2dp_tx_flush(void) {
    for (uint8_t i = 0; i < PL_A2DP_TX_QUEUE_SLOTS; i++) {
        s_ctx.tx[i].len = 0;
        s_ctx.tx[i].frames = 0;
    }
    s_ctx.tx_head = 0;
    s_ctx.tx_tail = 0;
    s_ctx.send_requested = false;
}

// Fills the tx ring from the PCM ring under CREDIT PACING (bead
// pico-link-pbv fix -- see the module doc's sec 11.2 rewrite and
// s_ctx.samples_owed's doc comment for why this replaced a pure
// data-driven drain). IRQ context (0xFF) -- see this file's module doc.
// No allocation (s_pcm_scratch is static, and the tx ring is a fixed
// array), no logging, no blocking; codec->encode() carries the same
// contract (codec_table.h).
//
// Bead pico-link-85v (D1/D4): renamed from pl_a2dp_fill_sbc_buffer. The
// loop is bounded by a real TIME budget, freshly derived every call (D4):
//   dwell_budget_us = min(PL_A2DP_MAX_ENCODE_DWELL_US backstop,
//                          CATCHUP_K * owed_frames * worst_case_encode_us,
//                          2/3 * this tick's own elapsed_us)
//                     floored at worst_case_encode_us (never deadlock).
// stop_dwell is the true safety-trip counter this produces -- nonzero in
// steady state means the dwell-safety bound, not the credit clock, is the
// actual limiter, which must never happen with the credit clamp in place.
//
// D1's core change: a full head slot is no longer a loop STOP. It is a
// SEAL -- the slot is handed to the tx ring and the loop continues into
// the next slot in the SAME tick. Only "no free slot" (stop_queue_full)
// ends the loop early for lack-of-room reasons; the earlier
// one-packet-per-tick ceiling (today a2dp.c's "tick with sbc_ready_to_send
// still true does no filling at all") is gone.
//
// The stop conditions are checked IN THIS ORDER -- dwell, credit,
// queue-full (inside the seal branch), ring-empty -- because the order is
// what makes `starved` (and therefore underrun_events/silent_ticks) mean
// anything. Under credit pacing, frames_this_tick == 0 ROUTINELY means
// "the clock owed less than one whole frame this tick" (normal -- most
// ticks fire faster than one SBC frame's worth of real time, ~2.67ms at
// 48kHz/128), NOT "the ring is empty". Only the ring-empty check, reached
// with the clock AND a slot both still willing, is a real starvation
// signal -- conflating the two would fire underrun_events on ordinary
// ticks, mid-music.
static void pl_a2dp_fill(void) {
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

    // Bead pico-link-pbv round 2 (C2-10), round 3 (R3-4), unchanged by
    // pico-link-85v: see pl_a2dp_usable_payload's doc comment for why this
    // must be the one shared computation -- the single correction point
    // for the reserved header-byte offset (do not hardcode `1` anywhere
    // else, D5/pico-link-cz0.5.6).
    uint32_t usable_payload = pl_a2dp_usable_payload(s_ctx.max_media_payload_size, s_ctx.frame.header_bytes);

#ifndef PL_ENCODER_ON_CORE1
    // Bead pico-link-85v (D4): the dwell budget, derived fresh every call.
    // Bead pico-link-nli.4 (G3), design sec 4.1 change 2: RETIRED under
    // PL_ENCODER_ON_CORE1 -- this bound exists solely to stop one IRQ
    // dwelling too long and starving something else on the SAME core;
    // core1 has no other tenant to yield to. This whole computation
    // (including its read of s_ctx.last_tick_elapsed_us, a core0-written
    // field) is compiled OUT under core1 mode rather than left as a
    // functionally-dead cross-core read -- see stop_dwell's own doc comment
    // on pl_a2dp_ctx_t for why the counter itself stays, wired to nothing,
    // asserted 0.
    uint32_t owed_frames = pcm_frame_count > 0 ? s_ctx.samples_owed / pcm_frame_count : 0;
    uint32_t work_bound_us =
        (uint32_t)((uint64_t)PL_A2DP_CATCHUP_K * owed_frames * s_ctx.frame.worst_case_encode_us);
    uint32_t duty_bound_us = s_ctx.last_tick_elapsed_us != 0
                                  ? (uint32_t)(((uint64_t)2 * s_ctx.last_tick_elapsed_us) / 3)
                                  : PL_A2DP_MAX_ENCODE_DWELL_US; // not yet measured -- don't spuriously bind
    uint32_t dwell_budget_us = PL_A2DP_MAX_ENCODE_DWELL_US;
    if (work_bound_us < dwell_budget_us) {
        dwell_budget_us = work_bound_us;
    }
    if (duty_bound_us < dwell_budget_us) {
        dwell_budget_us = duty_bound_us;
    }
    if (dwell_budget_us < s_ctx.frame.worst_case_encode_us) {
        dwell_budget_us = s_ctx.frame.worst_case_encode_us; // floored -- never deadlock
    }
#endif

    uint8_t frames_this_tick = 0;
    bool starved = false;
    // Bead pico-link-nli.4 (G3): dwell_us itself is kept unconditionally
    // (still feeds dwell_max_us below, a useful diagnostic even without
    // enforcement) -- only the enforcement break is retired under core1
    // mode.
    uint32_t dwell_us = 0;
    for (;;) {
#ifndef PL_ENCODER_ON_CORE1
        // 0. Dwell safety: has this tick's fill loop already consumed its
        // (freshly derived) dwell budget? The one true safety-trip stop
        // reason -- see this function's doc comment.
        if (dwell_us >= dwell_budget_us) {
            s_ctx.stop_dwell++;
            break;
        }
#else
        // 0. Bead pico-link-nli.4 (G3), G4 review hardening: core1's own
        // wall-clock cap -- see PL_A2DP_CORE1_FILL_BUDGET_US's doc
        // comment. Deliberately reuses dwell_us (real measured encode
        // time, accumulated below) rather than adding a second timer read
        // per iteration -- dwell_us is already kept unconditionally for
        // exactly this kind of diagnostic, per its own doc comment above.
        // This does NOT touch the retired frame-count dwell_budget_us
        // computation (still #ifndef'd out above) -- it is a new,
        // independent bound.
        if (dwell_us >= PL_A2DP_CORE1_FILL_BUDGET_US) {
            s_ctx.stop_core1_budget++;
            break;
        }
#endif
        // 1. Credit: has the real-time clock actually owed us a whole
        // encoded frame's worth of samples yet? Most-common stop reason
        // by far in steady state -- not a fault.
        if (s_ctx.samples_owed < pcm_frame_count) {
            s_ctx.stop_credit++;
            break;
        }

        // 1b. Queue-full, checked BEFORE touching the head slot.
        //
        // This looks redundant with the queue-full check inside the
        // payload-full branch below, and today it IS: when the queue is
        // at its usable ceiling the head slot aliases tx_tail's
        // sealed-unsent payload, but every seal happens BECAUSE the slot
        // could not take another frame, so that stale head always
        // re-trips payload-full and breaks there before anything is
        // written into it.
        //
        // That safety is EMERGENT, not structural -- it rests on the
        // invariant "every sealed slot is full". pico-link-cz0.5.6 breaks
        // that invariant on purpose: LDAC self-packetises and seals when
        // its encoder reports payload_complete, which can be well short
        // of usable_payload. A short sealed slot WOULD have room, would
        // fall through to the encode below, and would silently corrupt a
        // sealed, unsent packet already queued for transmission.
        //
        // So make the invariant explicit here rather than leaving the
        // next codec row to discover it as an audio-corruption bug.
        //
        // Bead pico-link-nli.3 (G2): the comparison is `SLOTS - 1`, not
        // `SLOTS` -- see pl_a2dp_tx_count()'s doc comment for why a
        // derived, masked count needs a reserved slot to disambiguate
        // full from empty.
        if (pl_a2dp_tx_count() >= PL_A2DP_TX_QUEUE_SLOTS - 1u) {
            s_ctx.stop_queue_full++;
            break;
        }

        pl_a2dp_slot_t *head = &s_ctx.tx[s_ctx.tx_head];
        if (head->len == 0) {
            // Fresh slot (never filled, or freed by a send/flush -- both
            // reset len to 0). Reserve the header byte(s) (data[0..],
            // written at send time -- see pl_a2dp_send_media_packet).
            // Bead pico-link-cz0.5.6: header_bytes is the active codec
            // row's own declared header size (codec_table.h), not a
            // hardcoded 1 -- SBC's row sets it to 1, reproducing today's
            // behaviour exactly.
            head->len = s_ctx.frame.header_bytes;
            head->frames = 0;
        }
        uint32_t data_bytes_so_far = (uint32_t)head->len - s_ctx.frame.header_bytes;

        // 2. Payload-full: is there room for one more frame in the head
        // slot? Not a fault -- it means this slot is done. SEAL it (D1)
        // and continue into the next slot in the SAME tick, unless there
        // is no next slot (stop_queue_full -- the new ceiling tripwire).
        //
        // Bead pico-link-cz0.5.6: this capacity-based seal is a FIXED-SIZE
        // codec concept -- it only applies when frame_bytes > 0 (SBC).
        // Self-packetising codecs (frame_bytes == 0, e.g. LDAC) have no
        // fixed per-frame size to check against usable_payload here; they
        // seal via the payload_complete branch after encode() below
        // instead. This is a branch on the row's DECLARED FRAME SHAPE
        // (codec_table.h's own encoded_frame_bytes==0 convention, S1's
        // design), not a codec-identity branch -- see codec_table.h:4-11.
        if (frame_bytes > 0 && data_bytes_so_far + frame_bytes > usable_payload) {
            if (pl_a2dp_tx_count() >= PL_A2DP_TX_QUEUE_SLOTS - 1u) {
                s_ctx.stop_queue_full++;
                break;
            }
            pl_a2dp_seal_head();
            continue; // re-evaluate dwell/credit/queue-full against the fresh head slot
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

        // Bead pico-link-du0: fold this unit's PCM into the OUT-meter
        // accumulator right where it's already sitting for encoding --
        // before the encode call below, so this addition is never mixed
        // into dwell_us/enc_max_us's own encode-cost accounting.
        pl_a2dp_accumulate_levels(s_pcm_scratch, pcm_frame_count);

        uint64_t t0 = time_us_64();
        pl_codec_encode_result_t result = s_ctx.codec->encode(
            s_ctx.codec->state, s_pcm_scratch, &head->data[head->len], (uint16_t)(sizeof(head->data) - head->len)
        );
        uint32_t dt = (uint32_t)(time_us_64() - t0);
        if (dt > s_ctx.enc_max_us) {
            s_ctx.enc_max_us = dt;
        }
        dwell_us += dt;
        if (dwell_us > s_ctx.dwell_max_us) {
            s_ctx.dwell_max_us = dwell_us;
        }
        if (!result.ok) {
            s_ctx.pkt_fail++;
            break;
        }

        // Bead pico-link-cz0.5.6: bytes_written/frames_emitted may both be
        // 0 -- a self-packetising codec (LDAC) accumulating internally
        // with nothing to hand back yet. That is a SUCCESSFUL call (PCM
        // was consumed), not a failure and not a stop condition -- the PCM
        // unit's samples are spent below and the loop continues to the
        // next unit exactly as if a fixed-size codec had produced a frame.
        if (result.bytes_written > 0) {
            head->len = (uint16_t)(head->len + result.bytes_written);
            head->frames = (uint16_t)(head->frames + result.frames_emitted);
        }
        s_ctx.samples_owed -= pcm_frame_count;
        frames_this_tick++;

        // Bead pico-link-cz0.5.6: the codec-driven seal. Only self-
        // packetising codecs ever set this (SBC's row always returns
        // payload_complete=false, see codec_sbc.c -- this branch is
        // therefore dead code on the proven-audible SBC path, not a
        // behaviour change to it). The tx_count guard mirrors the
        // fixed-size seal's own stop_queue_full check above.
        if (result.payload_complete) {
            if (pl_a2dp_tx_count() >= PL_A2DP_TX_QUEUE_SLOTS - 1u) {
                s_ctx.stop_queue_full++;
                break;
            }
            pl_a2dp_seal_head();
        }
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

    // Bead pico-link-du0: reduce/publish the OUT-meter accumulator, once
    // per fill call regardless of how many units this tick encoded --
    // keeps the once-per-window isqrt/publish cost off the per-unit hot
    // path above.
    //
    // Bead pico-link-nli.5 (G4): unconditional in both builds. Under
    // PL_ENCODER_ON_CORE1 this runs in core1 thread context and writes
    // only the seqlock below (design sec 4.3 invariant 6 -- no bt.c ring
    // push here, unlike the pico-link-nli.4 interim state this replaces);
    // under the OFF path it's the same IRQ-context call site pico-link-du0
    // originally wired. Either way the write lands in s_level_snapshot,
    // and core0's superloop (pl_a2dp_poll_levels, called from main.c after
    // pl_ui_tick) is what turns it into the actual LevelsChanged push.
    pl_a2dp_publish_levels();
}

// Bead pico-link-85v (D1/D6): sends the slot at tx_tail as one AVDTP media
// packet -- the tx-ring counterpart of the old single-buffer
// pl_a2dp_send_media_packet (SBC media header byte = frame count, RTP
// timestamp is the slot's own rtp_ts, stamped at SEAL not at send). Called
// from A2DP_SUBEVENT_STREAMING_CAN_SEND_MEDIA_PACKET_NOW, IRQ context.
//
// The re-arm below (request_can_send_now while the queue is still
// non-empty) is the whole fix this bead exists for, and it is legal:
// btstack's avdtp.c clears stream_endpoint->request_can_send_now BEFORE
// dispatching to us, then re-checks it AFTER we return and re-requests if
// we set it -- verified against pico-sdk 2.1.1's vendored btstack,
// avdtp.c:546-552. The next grant arrives as soon as L2CAP has an ACL
// buffer, not at the next media tick -- the drain becomes ACL-credit-bound
// rather than tick-bound.
//
// pico-link-j68 (D6): a grant can arrive after pl_a2dp_tx_flush() already
// ran (SUSPEND/reconnect) -- send_requested may still be true inside
// BTstack. The tx_count==0 guard below is the fix: previously this
// function checked neither state nor storage count and sent an
// unsolicited 1-byte payload with num_frames=0.
//
// Bead pico-link-nli.3 (G2): this is the tx ring's CONSUMER side, and
// stays on core0 (0xFF, this file's IRQ-context contract) even after the
// `pico-link-nli` epic moves the producer (pl_a2dp_seal_head) to core1.
// Two `__dmb()`s bracket the cross-core-sensitive part:
// - read-after-acquire, AFTER the tx_count()==0 check and BEFORE touching
//   `slot`: orders the read of tx_head/tx_tail (via pl_a2dp_tx_count()
//   above) before the reads of slot->frames/rtp_ts/data/len, mirroring
//   pl_a2dp_seal_head()'s publish-after-write on the other side --
//   without it this core could observe a just-sealed tx_head and still
//   read stale/torn slot bytes. It must sit after the tx_count() load
//   (an early return has nothing to order) and before the slot reads
//   (the actual dependency it protects), not before the tx_count() load
//   itself.
// - publish-after-read, before advancing tx_tail: orders every read of
//   `slot` above it before the tx_tail update that tells core1 the slot
//   is free to reuse. Without it core1 could start overwriting the slot
//   while this function is still mid-read of it.
static void pl_a2dp_send_media_packet(void) {
    s_ctx.grants++;

    if (pl_a2dp_tx_count() == 0) {
        s_ctx.spurious_grants++;
        s_ctx.send_requested = false;
        return;
    }
    __dmb(); // read-after-acquire: order the tx_count() read above before the slot reads that follow

    pl_a2dp_slot_t *slot = &s_ctx.tx[s_ctx.tx_tail];
    // Bead pico-link-cz0.5.6: byte 0 is the num_frames header content for
    // BOTH codec rows today (SBC and LDAC each declare header_bytes==1 --
    // codec_table.h) -- header_bytes governs reservation/offset math
    // elsewhere in this file, not this byte's content, which is specific
    // to the shared 1-byte fragmentation/start/last/num_frames format.
    slot->data[0] = (uint8_t)slot->frames; // (fragmentation<<7)|(start<<6)|(last<<5)|num_frames -- no fragmentation here
    uint8_t status = a2dp_source_stream_send_media_payload_rtp(
        s_ctx.a2dp_cid, s_ctx.local_seid, 0, slot->rtp_ts, slot->data, slot->len
    );
    if (status == ERROR_CODE_SUCCESS) {
        s_ctx.pkt_sent++;
    } else {
        s_ctx.pkt_fail++;
    }

    // a2dp_source_stream_send_media_payload_rtp copies into an L2CAP
    // buffer synchronously -- the slot is free the instant this call
    // returns (no in-flight retention, no DMA-lifetime hazard). len == 0
    // is this slot's "fresh" sentinel for pl_a2dp_fill's next use of it.
    slot->len = 0;
    slot->frames = 0;
    __dmb(); // publish-after-read: slot reads above complete before tx_tail frees the slot for core1 to reuse
    s_ctx.tx_tail = (s_ctx.tx_tail + 1u) & PL_A2DP_TX_QUEUE_MASK;

    if (pl_a2dp_tx_count() > 0) {
        a2dp_source_stream_endpoint_request_can_send_now(s_ctx.a2dp_cid, s_ctx.local_seid);
    } else {
        s_ctx.send_requested = false;
    }
}

// Bead pico-link-nli.4 (G3), design sec 4.1 change 1: extracted from
// pl_a2dp_media_timer_handler's body so the SAME credit-clock arithmetic
// (samples_owed/samples_owed_rem_us accrual + the ring-keyed clamp) can be
// driven by two different tick sources without being re-derived: the media
// timer's own ~10ms elapsed_us under the legacy (single-core) build, or
// core1's own time_us_64() delta under PL_ENCODER_ON_CORE1 (see the CORE1
// section below). elapsed_us is the only input; every field this touches
// (samples_owed, samples_owed_rem_us, credit_clamped_samples,
// credit_clamp_events) is owned by whichever context is currently allowed
// to run the fill loop (core0 legacy, core1 under this epic) -- never both
// at once, so no cross-core synchronization is needed here even though the
// FUNCTION itself is now shared code.
static void pl_a2dp_accrue_credit(uint32_t elapsed_us) {
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
    // The real PCM-sample-frames accrued THIS tick. Bead pico-link-nzw:
    // no longer a term of the credit clamp bound below (that bound is
    // now keyed to the ring's fill, not to per-tick accrual or packet
    // size -- see the clamp's own comment) -- still needed here to
    // update samples_owed/samples_owed_rem_us.
    uint32_t accrued_this_tick = (uint32_t)(owed_us_hz / 1000000u);
    s_ctx.samples_owed += accrued_this_tick;
    s_ctx.samples_owed_rem_us = (uint32_t)(owed_us_hz % 1000000u);

    // Bead pico-link-pbv ROUND 3 (R3-1) / bead pico-link-nzw: clamp the
    // credit to the largest backlog the drain can genuinely retire.
    // Round 2's bound (exactly one packet) sat precisely on the
    // drain's natural steady-state operating point (measured ~7.2
    // frames against a 7-frame bound), so it stopped being a windup
    // guard and became an in-band regulator that converts ordinary
    // tick jitter into permanently destroyed credit -- see
    // credit_clamped_samples's doc comment on pl_a2dp_ctx_t for the
    // full mechanism.
    //
    // Round 3's fix (keyed to frames_per_packet, i.e. PACKET SIZE) was
    // itself wrong: credit is only fictitious when the RING is dry.
    // Keying the bound to packet size means it can ALSO bind while the
    // ring is deep, where the credit is genuine and destroying it is
    // simply lost drain -- every such event is a permanent step up in
    // ring fill, removable only by the +/-500ppm USB feedback loop at
    // ~96 B/s (roughly 48 SECONDS to work off one clamp event). See
    // bead pico-link-nzw and .planning/design/2026-08-30-pcm-pacing.md
    // finding 2.
    //
    // The corrected bound is keyed to the RING, not the packet: the
    // ring's current fill converted to sample-frames (pl_pcm_fill_bytes()
    // / PL_PCM_FRAME_BYTES -- everything the drain could legitimately
    // retire right now) plus one frame (the sub-frame remainder
    // pcm_frame_count granularity forces). This never binds while the
    // ring is full/deep (fill_bytes alone already exceeds any real
    // samples_owed there), and is exactly the deficit-side resync the
    // clamp was meant to be: it only bites when the ring is dry enough
    // that samples_owed has run ahead of what physically exists to
    // drain.
    //
    // pcm_frame_count_for_clamp guards against clamping before a codec
    // has negotiated (frames_per_packet/pcm_frames_per_encoded_frame
    // both 0 pre-negotiation) -- accrual during IDLE/PRIMING is
    // discarded at STREAM_STARTED anyway (see samples_owed's doc
    // comment), so skipping the clamp there is harmless.
    uint32_t pcm_frame_count_for_clamp = s_ctx.frame.pcm_frames_per_encoded_frame;
    if (s_ctx.frames_per_packet > 0 && pcm_frame_count_for_clamp > 0) {
        uint32_t max_samples_owed = pl_pcm_fill_bytes() / PL_PCM_FRAME_BYTES + pcm_frame_count_for_clamp;
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

#ifdef PL_ENCODER_ON_CORE1
// === CORE1: the LDAC encoder (bead pico-link-nli.4, epic pico-link-nli,
// design of record .planning/decisions/2026-09-03-ldac-encoder-on-core1.md
// sec 2/4). Everything below this point until the matching #endif runs on
// core1, or is core0's half of the handshake that talks to it. Nothing
// outside this section (and the two `#ifdef`s already threaded through
// pl_a2dp_seal_head/pl_a2dp_fill above) is aware core1 exists.
//
// THE FIVE INVARIANTS THIS SECTION MUST HOLD, at every call site below:
// no Rust, no alloc, no interrupts-off (never save_and_disable_interrupts),
// no pl_log, no bt.c ring push. A sixth, structural one: no BTstack call
// (a2dp_source_*, pl_persist_*) -- covered by construction, since this
// section never calls any of those; the ones that were needed
// (request_can_send_now) moved to core0's media timer instead (see
// pl_a2dp_seal_head's doc comment and the "send kick" in
// pl_a2dp_media_timer_handler below).

typedef enum {
    PL_ENC_STATE_IDLE = 0,
    PL_ENC_STATE_RUNNING,
    PL_ENC_STATE_DRAINING,
} pl_enc_state_t;

// Written ONLY by core0 (this file's packet-handler transition points),
// read ONLY by core1 (pl_a2dp_core1_entry's loop) -- design sec 4.2's
// IDLE -> RUNNING -> DRAINING -> IDLE state machine, the one authoritative
// signal for whether core1 may touch any fill/seal-side shared field.
// `volatile`: read every core1 loop iteration; written from an execution
// context the compiler has no visibility into.
static volatile pl_enc_state_t s_enc_state = PL_ENC_STATE_IDLE;

// Written ONLY by core1, read ONLY by core0. true whenever core1 has
// observed a non-RUNNING state and is therefore NOT touching any fill/seal
// field -- core0's DRAINING transition spins on this (bounded) before it
// touches rtp_next/the tx ring/pl_pcm_reset/codec state. Starts true: core1
// has not launched yet, so there is nothing to quiesce.
static volatile bool s_enc_quiesced = true;

// Bumped once per core1 loop iteration that completes, whether or not that
// iteration actually encoded anything -- proves LOOP progress, the same
// "still alive", not-throughput contract every other pl_wdt_kick producer
// already has (watchdog_sup.h's module doc). Read by core0's media timer,
// which feeds PL_WDT_ENCODER from it -- core1 must NEVER call pl_wdt_kick()
// itself (that would be a cross-core call into a module whose state
// core1 does not own, and is not one of the two things -- flash-lockout
// registration and the fill loop -- this core exists to do).
static volatile uint32_t s_enc_heartbeat;

// Diagnostics: how many times core0's bounded quiesce wait
// (pl_a2dp_core1_quiesce_and_wait) actually hit its timeout instead of
// observing s_enc_quiesced in time. NOT fatal (contrast the flash lockout's
// END timeout, ADR sec 7.1) -- see that function's doc comment for why
// proceeding anyway, logged via this counter, is the correct response for
// a cooperative software flag rather than a latched SDK handshake.
static volatile uint32_t s_enc_quiesce_timeouts;

// Cooperative poll bound for the quiesce handshake -- NOT the flash
// lockout's own 20ms IRQ-serviced budget (flash_lockout.c's
// PL_FLASH_LOCKOUT_TIMEOUT_US): that one is bounded by interrupt latency
// regardless of what core1's thread-mode loop is doing, because core1
// never disables interrupts (invariant 3) and the lockout's SIO FIFO IRQ
// preempts it either way. This bound instead waits for a plain volatile
// flag that core1 only updates between iterations of its own C loop, so it
// needs enough margin for one in-flight encode() call plus a queue-full
// seal to finish unwinding -- LDAC HQ's measured in-situ encode cost is
// 1495-1517us (ADR sec 11.0); 5ms is generous headroom above any single
// frame.
#define PL_A2DP_QUIESCE_TIMEOUT_US (5u * 1000u)

// PL_A2DP_CORE1_FILL_BUDGET_US (2ms, defined near PL_A2DP_MAX_ENCODE_DWELL_US
// above pl_a2dp_fill) leaves 3ms of headroom under this 5ms budget for an
// in-flight fill() call to finish once its own cap trips -- see that
// macro's doc comment for the full reasoning.

// Non-blocking follow-up from pico-link-nli.4's code review: previously
// PL_A2DP_CORE1_FILL_BUDGET_US was a bare magic number with no compile-time
// tie to the numbers that make 2ms safe. PL_A2DP_WORST_CASE_ENCODE_US_MAX
// mirrors the largest worst_case_encode_us across the codec table
// (codec_ldac.c's 2000; codec_sbc.c's is 800) -- duplicated here
// deliberately, not `#include`d from a shared constant, so this assert
// fails loudly at compile time if a future codec's worst case grows
// without anyone re-checking this budget. The quantity being bounded is
// pl_a2dp_fill's true worst-case dwell once its own 2ms cap trips: the cap
// is checked only between units (a2dp.c's stop_core1_budget site), so one
// more worst-case encode can still complete after the cap fires -- see
// PL_A2DP_CORE1_FILL_BUDGET_US's own doc comment above.
#define PL_A2DP_WORST_CASE_ENCODE_US_MAX 2000u
_Static_assert(
    PL_A2DP_CORE1_FILL_BUDGET_US + PL_A2DP_WORST_CASE_ENCODE_US_MAX < PL_A2DP_QUIESCE_TIMEOUT_US,
    "PL_A2DP_CORE1_FILL_BUDGET_US plus one more worst-case encode call must stay under "
    "PL_A2DP_QUIESCE_TIMEOUT_US, or core0's quiesce handshake can time out against a fill() "
    "call that is still legitimately finishing, not a wedged core1"
);

// core1's own credit-clock tick source (design sec 4.1 change 1): a
// core1-private time_us_64() delta, replacing the media timer's role.
// Plain static, not part of s_ctx -- core0 never reads or writes this.
static uint64_t s_enc_last_tick_us;

// The core1 thread-mode entry point (multicore_launch_core1's target).
// Registers as the flash lockout's victim FIRST (flash_lockout.h) -- core0
// flash writes must never assume core1 is safely parked before this call
// has run -- and never returns (design sec 4.2: core1 is launched once at
// boot and never reset).
static void pl_a2dp_core1_entry(void) {
    pl_flash_lockout_core1_init();
    s_enc_last_tick_us = time_us_64();

    for (;;) {
        // Bare volatile read of a naturally-aligned enum: cannot tear on
        // this architecture, and a one-iteration-late observation of a
        // state change is harmless -- the safety boundary is the quiesce
        // handshake below (s_enc_quiesced), not the timeliness of this
        // read.
        pl_enc_state_t state = s_enc_state;
        if (state != PL_ENC_STATE_RUNNING) {
            // Not running: touch NOTHING shared beyond the two flags below,
            // then park in WFE instead of bare-spinning (bead pico-link-1n4.3
            // / ADR sec 12.4's F2 -- measured on hardware at 6.28M spins/s and
            // 73M added XIP accesses/s for zero work, F0). Interrupts stay
            // enabled for the entire life of this function (invariant 3), so
            // WFE is NOT an interrupts-disabled wait: the multicore lockout
            // handler (and any future doorbell) keeps being serviced exactly
            // as before, waking this core the same way any IRQ always does.
            //
            // RACE-FREE BY CONSTRUCTION, not by luck: WFE/SEV share one
            // architectural event flag per core, and SEV -- per hardware/
            // sync.h's own doc comment -- "sends an event to both cores".
            // The flag latches independently of instruction ordering: if
            // pl_a2dp_core1_arm_running()'s __sev() (the only place that
            // transitions this core out of non-RUNNING, see that function)
            // fires ANY time between this core's last state re-check and its
            // WFE retiring -- including strictly before this loop iteration
            // even started -- the flag is already set and WFE returns
            // immediately without sleeping. There is no window in which a
            // state change can be missed: either this core observes RUNNING
            // on its next volatile read (state was already updated), or it
            // was still non-RUNNING when read but the pending/latched event
            // wakes the WFE right after, and the very next loop iteration
            // re-reads state and finds RUNNING. Nothing here is edge-
            // triggered on the SEV call itself; only the state variable is
            // ever branched on, so a "lost" event costs at most one extra,
            // harmless WFE wake -- it can never cost a missed transition.
            s_enc_quiesced = true;
            s_enc_heartbeat++;
            __wfe();
            continue;
        }
        s_enc_quiesced = false;

        uint64_t now = time_us_64();
        uint32_t elapsed_us = (uint32_t)(now - s_enc_last_tick_us);
        s_enc_last_tick_us = now;
        pl_a2dp_accrue_credit(elapsed_us);
        pl_a2dp_fill();

        s_enc_heartbeat++;
    }
}

// Core1's stack, deliberately NOT the linker-provided .stack1_dummy in
// SCRATCH_X (pico-link-0zr, found by Tex 2026-09-03 diagnosing
// pico-link-gmy). pico-sdk 2.1.1's memmap_default.ld places core0's stack
// (.stack_dummy) in the fixed 4KB SCRATCH_Y region and core1's
// (.stack1_dummy) in the fixed 4KB SCRATCH_X region, on the documented
// assumption -- stated in the ld script's own comment -- that "if core 1
// stack is not used then all of SCRATCH_X is free". This firmware sets
// PICO_STACK_SIZE=0xC000 (48KB, for cyw43/BTstack init) at a time when
// core1 was never launched (see the PICO_STACK_SIZE comment above), so
// __StackBottom = 0x20082000 - 0xC000 = 0x20076000 -- and core0's stack
// range 0x20076000-0x20082000 STRICTLY CONTAINS core1's SCRATCH_X range
// 0x20080000-0x20081000. G3 launched core1 and the assumption stopped
// holding: core0 recursing past 4KB of its own 48KB budget silently writes
// into core1's stack. Measured on hardware: two different core1 HardFault
// signatures (a precise bus fault on a stack-derived load, and an INVSTATE
// on a smashed LR) under load, both consistent with an external writer
// stomping core1's stack, and core1's own high-water usage a comfortable
// 1672 of 4096 bytes -- i.e. core1 was not overflowing its own stack, so
// something else was writing into it. A/B with address as the only
// variable -- same 4096-byte size, this array instead of SCRATCH_X --
// eliminated the reboots: ~4 in 10 minutes of loaded streaming down to 0 in
// 20 minutes at matched load. This is the minimal, proven fix. The
// sustainable fix -- moving core0's stack into main RAM so SCRATCH_X can
// stay the SDK-intended core1 stack -- is a linker-script change and is
// Ada's call, filed separately; do not restructure the memory map here.
static uint32_t s_core1_stack[1024] __attribute__((aligned(8)));

// Backstop for the class of bug this array fixes, since the ld script
// itself provides none (its only stack ASSERT compares heap against main
// RAM, not core0's stack against core1's -- see the doc comment above).
// Checks the ARRANGEMENT WE ACTUALLY LANDED ON: does s_core1_stack, above,
// overlap core0's real stack range [__StackBottom, __StackTop)? It does
// NOT check __StackOneTop/__StackOneBottom (the SCRATCH_X slot the linker
// still reserves for a core1 stack that no longer lives there) -- those
// linker symbols are unaffected by moving core1's stack to .bss, so a
// check against them would report the pre-existing (harmless, because
// unused) SCRATCH_X collision on every single boot rather than the live
// one this fix actually addresses. If core0's stack budget
// (PICO_STACK_SIZE, CMakeLists.txt) or this array's placement/size ever
// changes such that they collide, this must catch it -- so it is
// unconditional (NOT #ifndef NDEBUG-gated), matching persist.c's
// pl_persist_check_no_firmware_collision() precedent: a plain assert()
// would be silently elided by CMAKE_BUILD_TYPE unset -> NDEBUG defined
// (pico-sdk's forced-Release default), which is exactly the build type
// most likely to be flashed for a real hardware soak.
static void pl_a2dp_assert_core1_stack_no_overlap(void) {
    extern uint32_t __StackBottom;
    extern uint32_t __StackTop;
    uintptr_t core0_lo = (uintptr_t)&__StackBottom;
    uintptr_t core0_hi = (uintptr_t)&__StackTop;
    uintptr_t core1_lo = (uintptr_t)s_core1_stack;
    uintptr_t core1_hi = (uintptr_t)s_core1_stack + sizeof(s_core1_stack);
    if (core1_lo < core0_hi && core0_lo < core1_hi) {
        pl_log(
            "FATAL: core1 stack [0x%08lx, 0x%08lx) overlaps core0's stack range [0x%08lx, 0x%08lx) "
            "-- see a2dp.c's s_core1_stack doc comment (bead pico-link-0zr) -- halting\r\n",
            (unsigned long)core1_lo, (unsigned long)core1_hi, (unsigned long)core0_lo, (unsigned long)core0_hi
        );
        while (true) {
            tight_loop_contents();
        }
    }
}

// Launches core1 into pl_a2dp_core1_entry(). Call once, after cyw43/BTstack
// init (design sec 8's G3 bead description) -- core1 then runs forever;
// there is no corresponding "stop core1" function (design sec 4.2).
void pl_a2dp_launch_core1(void) {
    pl_a2dp_assert_core1_stack_no_overlap();
    multicore_launch_core1_with_stack(pl_a2dp_core1_entry, s_core1_stack, sizeof(s_core1_stack));
}

// Core0's half of the quiesce handshake (design sec 4.2). Sets DRAINING and
// spins, bounded, for core1 to leave the fill body. MUST be called before
// any of the STREAM_SUSPENDED/STREAM_RELEASED/SIGNALING_CONNECTION_RELEASED
// handlers below touch rtp_next/the tx ring (pl_a2dp_tx_flush)/pl_pcm_reset/
// codec state -- mirroring the exact reasoning pl_a2dp_tx_flush's own doc
// comment already gives for why those call sites are safe. A timeout here
// is counted, not fatal (see s_enc_quiesce_timeouts's doc comment): the
// caller still completes its own transition regardless, because holding a
// stream-teardown open forever on a maybe-wedged core1 would trade a
// bounded, logged race for an unbounded hang -- s_enc_heartbeat/
// PL_WDT_ENCODER remains the real backstop if core1 is genuinely dead.
static void pl_a2dp_core1_quiesce_and_wait(void) {
    __dmb(); // publish-after-write: order any of THIS caller's own prior writes before the state change core1 observes
    s_enc_state = PL_ENC_STATE_DRAINING;
    uint64_t deadline = time_us_64() + PL_A2DP_QUIESCE_TIMEOUT_US;
    while (!s_enc_quiesced) {
        if (time_us_64() >= deadline) {
            s_enc_quiesce_timeouts++;
            break;
        }
    }
    __dmb(); // read-after-acquire: order the s_enc_quiesced read above before this caller's own shared-state writes that follow
}

// Core0's half of arming core1 for a fresh stream (design sec 4.2): resets
// the credit-clock fields core1 owns, THEN publishes RUNNING -- core1 only
// starts reading/writing them once it observes RUNNING (pl_a2dp_core1_entry
// above), so this publish-after-write ordering is what makes the reset
// race-free without needing any cooperation from core1 itself.
static void pl_a2dp_core1_arm_running(void) {
    s_ctx.samples_owed = 0;
    s_ctx.samples_owed_rem_us = 0;
    __dmb();
    s_enc_state = PL_ENC_STATE_RUNNING;
    // Wake core1 out of the WFE it parks in while non-RUNNING (bead
    // pico-link-1n4.3). __sev() is unconditional and cheap -- if core1 is
    // not currently asleep (e.g. still unwinding a prior DRAINING
    // iteration) this is a harmless no-op event, never a correctness
    // requirement; the state read in pl_a2dp_core1_entry's loop is what
    // core1 actually acts on. See that loop's WFE comment for why the two
    // together are race-free.
    __sev();
}

// Diagnostics for G3/G5's reporter -- see s_enc_quiesce_timeouts's doc
// comment. Zero in a healthy run.
uint32_t pl_a2dp_encoder_quiesce_timeouts(void) {
    return s_enc_quiesce_timeouts;
}
#endif // PL_ENCODER_ON_CORE1

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
        // Bead pico-link-85v (D4): stash for pl_a2dp_fill's duty-bound
        // derivation -- same elapsed_us, no new time_us_64() call.
        s_ctx.last_tick_elapsed_us = elapsed_us;
        if (elapsed_us > s_ctx.worst_tick_interval_us) {
            s_ctx.worst_tick_interval_us = elapsed_us;
        }

#ifndef PL_ENCODER_ON_CORE1
        // Bead pico-link-nli.4 (G3), design sec 4.1 change 1: under
        // PL_ENCODER_ON_CORE1 the credit clock is accrued by core1 itself,
        // from its own time_us_64() delta -- see pl_a2dp_core1_entry in the
        // CORE1 section above. Calling it again here would double-accrue.
        pl_a2dp_accrue_credit(elapsed_us);
#endif
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

#ifndef PL_ENCODER_ON_CORE1
    // Bead pico-link-fhf: hysteresis-banded discrete resync. Ring fill
    // under credit pacing is a free integrator (drain is defined by our
    // own crystal, the same crystal supply is regulated against, so there
    // is no restoring force) -- the +-500ppm feedback loop is ~100x too
    // weak to remove an offset in useful time, so a sustained offset must
    // be removed discretely instead of left to integrate toward overflow
    // (the ~5.4 minute collapse this bead exists to fix). Evaluated on the
    // EMA, NEVER raw fill -- raw fill sawtooths by up to one tick's drain
    // (~3840B), comparable to the band itself, so a raw-fill comparison
    // would trip constantly. See the bead's design comment sec 1-3.
    //
    // Skip while the host is silent (a paused host must not be trimmed on
    // its way to auto-pause) and while inside the minimum interval (the
    // EMA holds a stale pre-trim value for ~5 tau after a reseed; an
    // ungated second tick would cut another band's worth on a reading that
    // no longer exists).
    //
    // GATED ON !PL_ENCODER_ON_CORE1 (code review, 2026-09-07): pl_pcm_trim_to
    // writes pcm_ring's s_tail, which under PL_ENCODER_ON_CORE1 is owned by
    // core1's pl_a2dp_core1_entry loop (it calls pl_pcm_read(), the other
    // read-modify-write of s_tail) -- running this block unconditionally in
    // core0's IRQ would race that write with no lock, exactly the hazard
    // pcm_ring.h's module doc calls out for a genuinely cross-core consumer.
    // This mirrors every other core1 carve-out in this function. Dormant
    // today (PL_ENCODER_ON_CORE1 defaults OFF), but core1 mode currently has
    // NO resync mechanism until pico-link-quzf lands a core1-safe design --
    // do not remove this guard without that design in place.
    if (!host_silent) {
        int32_t fill_ema = pl_usb_audio_fb_fill_ema();
        uint32_t target = pl_pcm_target_fill_bytes();
        if (fill_ema > 0 && (uint32_t)fill_ema > target + PL_PCM_TRIM_BAND_BYTES &&
            (pbv_now_us - s_ctx.last_resync_us) > PL_PCM_TRIM_MIN_INTERVAL_US) {
            uint32_t dropped = pl_pcm_trim_to(target);
            s_ctx.resync_drops += dropped;
            s_ctx.resync_events++;
            s_ctx.last_resync_us = pbv_now_us;
            // Reseed the EMA LAST, from the post-trim fill -- ordering
            // matters, this is what makes the min-interval lockout above
            // sufficient rather than merely helpful. (A 0xC0 preemption of
            // this 0xFF handler between the trim and the reseed can fold
            // one stale pre-trim sample into the EMA first -- harmless,
            // one >>6 step, ~340B of transient error against a 2880B band,
            // and both values are single aligned 32-bit words so there is
            // no tear; no critical section needed.)
            pl_usb_audio_fb_reset();
        }
    }
    // NO pl_log in this block -- this file's module doc forbids it in the
    // hot path, and it killed the ISO-OUT endpoint once already
    // (pico-link-0d2). All reporting for this mechanism happens in
    // pl_a2dp_report, at thread context.

    // Bead pico-link-85v (D1): the old "only fill if not already waiting
    // on a grant" gate is GONE -- that was the actual ceiling mechanism
    // (a tick with sbc_ready_to_send still true did no filling at all).
    // pl_a2dp_fill now self-manages sealing and re-arming
    // request_can_send_now as slots fill, so it is simply called every
    // tick regardless of any pending grant.
#ifdef PL_DEBUG_REMOTE
    // Bead pico-link-fhf, test A: consume one tick of the debug skip
    // one-shot HERE, at the drain call site -- everything else in this
    // handler (tick bookkeeping, credit accrual, the resync trim check
    // above) still runs normally; only the drain itself goes idle, which
    // is what lets the ring gain fill at the full 192 B/ms rate.
    if (s_debug_skip_media_ticks > 0) {
        s_debug_skip_media_ticks--;
    } else {
        pl_a2dp_fill();
    }
#else
    pl_a2dp_fill();
#endif
#else
    // Bead pico-link-nli.4 (G3), design sec 5: core1 fills and seals; this
    // handler's job shrinks to the "send kick" -- if a slot is pending and
    // we don't already have an outstanding request, ask BTstack for one.
    // pl_a2dp_seal_head() (core1) no longer calls
    // a2dp_source_stream_endpoint_request_can_send_now itself (that would
    // be a BTstack call from core1, forbidden) -- this is where that
    // request now originates instead. send_requested is plain (not
    // volatile): under this build it is written/read ONLY from core0 (this
    // check, and pl_a2dp_send_media_packet's own re-arm) -- see
    // pl_a2dp_seal_head's doc comment.
    if (pl_a2dp_tx_count() > 0 && !s_ctx.send_requested) {
        s_ctx.send_requested = true;
        a2dp_source_stream_endpoint_request_can_send_now(s_ctx.a2dp_cid, s_ctx.local_seid);
    }

    // Bead pico-link-nli.4 (G3): feed PL_WDT_ENCODER from core1's heartbeat
    // advancing, never from core1 calling pl_wdt_kick() itself (invariant:
    // core1 must not call into a module whose state it does not own -- see
    // s_enc_heartbeat's doc comment in the CORE1 section above). A plain
    // compare-and-kick, same "prove forward progress" contract as every
    // other subsystem.
    static uint32_t s_last_seen_enc_heartbeat;
    uint32_t heartbeat_now = s_enc_heartbeat;
    if (heartbeat_now != s_last_seen_enc_heartbeat) {
        s_last_seen_enc_heartbeat = heartbeat_now;
        pl_wdt_kick(PL_WDT_ENCODER);
    }
#endif

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

// Bead pico-link-cz0.5.6: factored out of the SBC_CONFIGURATION handler so
// OTHER_CONFIGURATION (LDAC, and any future vendor row) can share it --
// table lookup by local_seid, row->init(), and the settle/announce tail
// (pl_bt_push_connect_step/_codec_changed). Callers own decoding their own
// subevent's raw fields into the row-specific `cfg`/`cfg_len` shape first;
// this function is generic across whatever that shape turns out to be.
static void pl_a2dp_finish_codec_negotiation(uint8_t local_seid, const uint8_t *cfg, uint8_t cfg_len) {
    // Table lookup by local_seid -- written generically (S1 had one row;
    // S4/pico-link-cz0.5.6 adds a second). No call site here assumes SBC
    // is the only possible row.
    pl_codec_t *row = NULL;
    for (size_t i = 0; i < PL_CODEC_COUNT; i++) {
        if (PL_CODECS[i]->local_seid == local_seid) {
            row = PL_CODECS[i];
            break;
        }
    }
    if (row == NULL || !row->init(row->state, cfg, cfg_len, &s_ctx.format, &s_ctx.frame)) {
        pl_log("a2dp: codec init FAILED for local_seid %u\r\n", local_seid);
        pl_bt_push_connect_failed(s_ctx.connect_addr, PL_FAILURE_REASON_NO_A2DP_SINK);
        return;
    }
    s_ctx.codec = row;
    s_ctx.local_seid = local_seid;
    pl_log(
        "a2dp: codec=%s sample_rate=%lu pcm_frames_per_frame=%u frame_bytes=%u nominal_bitrate=%lu\r\n",
        row->display_name, (unsigned long)s_ctx.format.sample_rate_hz, s_ctx.frame.pcm_frames_per_encoded_frame,
        s_ctx.frame.encoded_frame_bytes, (unsigned long)s_ctx.frame.nominal_bitrate_bps
    );
    pl_bt_push_connect_step(PL_CONNECT_STEP_NEGOTIATING_CODEC);

    // Bead pico-link-1v5: tells the Home hero which codec is now live and
    // at what nominal bitrate, so it stops reading "NO LINK" once a
    // device is actually connected. Never called from the media timer
    // path (s_ctx.frame is already fully populated by row->init above, so
    // this reads only settled state).
    pl_bt_push_codec_changed(s_ctx.connect_addr, row->display_name, (uint8_t)strlen(row->display_name), s_ctx.frame.nominal_bitrate_bps);
}

static void pl_a2dp_media_timer_arm(void) {
    btstack_run_loop_remove_timer(&s_ctx.media_timer); // safe even if not currently added
    btstack_run_loop_set_timer_handler(&s_ctx.media_timer, pl_a2dp_media_timer_handler);
    btstack_run_loop_set_timer(&s_ctx.media_timer, PL_A2DP_AUDIO_TIMEOUT_MS);
    btstack_run_loop_add_timer(&s_ctx.media_timer);
    s_ctx.timer_armed = true;
}

// Bead pico-link-648: maps a raw HCI status byte from
// A2DP_SUBEVENT_SIGNALING_CONNECTION_ESTABLISHED's failure case to the
// closest-justified ConnectFailureReason -- previously every non-SUCCESS
// status was collapsed into PL_FAILURE_REASON_REJECTED, throwing away
// information ConnectFailureReason already models. Only maps codes with an
// unambiguous match to an existing category; everything else stays
// REJECTED rather than inventing a new one (explicit instruction: "do not
// invent categories for codes you cannot justify").
static uint32_t pl_a2dp_failure_reason_for_status(uint8_t status) {
    switch (status) {
        case ERROR_CODE_PAGE_TIMEOUT:
        case ERROR_CODE_CONNECTION_TIMEOUT:
            // No response within the connection window -- exactly
            // PL_FAILURE_REASON_TIMEOUT's own definition.
            return PL_FAILURE_REASON_TIMEOUT;
        case ERROR_CODE_PIN_OR_KEY_MISSING:
            // The remote has no link key on record for us (or vice versa)
            // -- a fresh SSP/pairing dialog is needed, which this product
            // cannot drive without on-screen text entry. Matches
            // PL_FAILURE_REASON_NEEDS_PIN's own non-retryable semantics.
            return PL_FAILURE_REASON_NEEDS_PIN;
        case ERROR_CODE_ACL_CONNECTION_ALREADY_EXISTS:
            // NOT a remote rejection -- this is BTstack's/the controller's
            // OWN local HCI-level bookkeeping reporting a conflict (see
            // persist.c's pico-link-648 investigation notes and hci.c's
            // own hci_handle_connection_failed, which discards our local
            // connection tracking with no recovery path). Matches
            // PL_FAILURE_REASON_RADIO_ERROR's own definition: "The
            // radio/HCI layer itself reported an error (not a per-device
            // remote-side rejection)".
            return PL_FAILURE_REASON_RADIO_ERROR;
        default:
            return PL_FAILURE_REASON_REJECTED;
    }
}

// Bead pico-link-648. Extracted from pl_a2dp_connect() (below) so the
// 0x0b retry handler can reissue establish_stream WITHOUT going through
// pl_a2dp_connect()'s own reset of reconnect_retry_used -- that reset is
// what caps this at exactly one retry per connect() call; calling it again
// from the retry path itself would turn "one retry" into "retry forever
// against a headset that keeps saying 0x0b".
static void pl_a2dp_establish_stream_now(const uint8_t *addr) {
    bd_addr_t local_addr;
    memcpy(local_addr, addr, 6);
    memcpy(s_ctx.connect_addr, addr, 6);

    // Bead pico-link-cz0.6 (M5 persistence), design point 4: "forced flush
    // ... BEFORE arming a stream" -- this is that call site. Runs in the
    // cyw43/BTstack background IRQ (pico-link-ouw's deferred-queue
    // consumer), so this only flags urgency -- the actual write (if any is
    // pending) happens on the superloop's next iteration, in thread
    // context, once it's confirmed safe. A no-op if nothing is pending.
    pl_persist_request_urgent_flush();

    uint8_t status = a2dp_source_establish_stream(local_addr, &s_ctx.a2dp_cid);
    if (status != ERROR_CODE_SUCCESS) {
        pl_log("a2dp: establish_stream rejected, status=0x%02x\r\n", status);
        pl_bt_push_connect_failed(addr, PL_FAILURE_REASON_RADIO_ERROR);
        return;
    }
    pl_bt_push_connect_step(PL_CONNECT_STEP_SETTING_UP_AUDIO);
}

// Bead pico-link-648: cancels any pending 0x0b retry. Safe to call even
// when nothing is armed (btstack_run_loop_remove_timer is a no-op on a
// timer that isn't currently added -- same idiom as
// pl_a2dp_media_timer_arm above). Called on a fresh connect() (a new
// attempt supersedes any stale retry from a previous one), on a
// successful SIGNALING_CONNECTION_ESTABLISHED, on user disconnect, and on
// SIGNALING_CONNECTION_RELEASED -- so a stale timer can never fire into a
// live or torn-down session.
static void pl_a2dp_reconnect_retry_cancel(void) {
    if (!s_ctx.reconnect_retry_armed) {
        return;
    }
    btstack_run_loop_remove_timer(&s_ctx.reconnect_retry_timer);
    s_ctx.reconnect_retry_armed = false;
}

static void pl_a2dp_reconnect_retry_handler(btstack_timer_source_t *ts) {
    (void)ts;
    s_ctx.reconnect_retry_armed = false;
    pl_log("a2dp: 0x0b retry firing, reissuing establish_stream (single bounded retry)\r\n");
    pl_a2dp_establish_stream_now(s_ctx.connect_addr);
}

// Bead pico-link-648: arms the one bounded retry. Marks
// reconnect_retry_used so a second 0x0b (from this retry itself, or any
// later failure before the next fresh connect()) is reported to the UI as
// a real failure instead of retrying again.
static void pl_a2dp_reconnect_retry_arm(uint8_t status) {
    s_ctx.reconnect_retry_used = true;
    pl_log(
        "a2dp: 0x0b (ACL connection already exists) -- arming single bounded retry in %ums\r\n",
        (unsigned)PL_A2DP_RECONNECT_RETRY_DELAY_MS
    );
    btstack_run_loop_remove_timer(&s_ctx.reconnect_retry_timer); // safe even if not currently added
    btstack_run_loop_set_timer_handler(&s_ctx.reconnect_retry_timer, pl_a2dp_reconnect_retry_handler);
    btstack_run_loop_set_timer(&s_ctx.reconnect_retry_timer, PL_A2DP_RECONNECT_RETRY_DELAY_MS);
    btstack_run_loop_add_timer(&s_ctx.reconnect_retry_timer);
    s_ctx.reconnect_retry_armed = true;
    s_ctx.reconnect_retry_armed_status = status;
}

// Bead pico-link-4vb.2 (bug 3): cancels the wizard-dismiss timer if it's
// currently armed -- same idiom as pl_a2dp_reconnect_retry_cancel above
// (safe to call unconditionally; a no-op when nothing is armed). Called on
// every path that tears down or restarts the session before the timer's
// own delay elapses, so a stale dismiss can never fire into a screen it no
// longer applies to: SIGNALING_CONNECTION_RELEASED/STREAM_RELEASED (bug
// 3.5's disconnect handling), and a fresh connect() attempt superseding
// whatever came before it.
static void pl_a2dp_wizard_dismiss_timer_cancel(void) {
    if (!s_ctx.wizard_dismiss_armed) {
        return;
    }
    btstack_run_loop_remove_timer(&s_ctx.wizard_dismiss_timer);
    s_ctx.wizard_dismiss_armed = false;
}

static void pl_a2dp_wizard_dismiss_timer_handler(btstack_timer_source_t *ts) {
    (void)ts;
    s_ctx.wizard_dismiss_armed = false;
    pl_bt_push_wizard_auto_dismiss();
}

// Arms the one-shot PL_A2DP_WIZARD_DISMISS_DELAY_MS timer -- see this
// field's doc comment on pl_a2dp_ctx_t. Called right after a
// ConnectSucceeded push (STREAM_ESTABLISHED below); core's own guard on
// Event::WizardAutoDismiss (only acts on a plain, non-degraded success)
// makes it safe to arm this unconditionally rather than branching on
// `degraded` here too.
static void pl_a2dp_wizard_dismiss_timer_arm(void) {
    btstack_run_loop_remove_timer(&s_ctx.wizard_dismiss_timer); // safe even if not currently added
    btstack_run_loop_set_timer_handler(&s_ctx.wizard_dismiss_timer, pl_a2dp_wizard_dismiss_timer_handler);
    btstack_run_loop_set_timer(&s_ctx.wizard_dismiss_timer, PL_A2DP_WIZARD_DISMISS_DELAY_MS);
    btstack_run_loop_add_timer(&s_ctx.wizard_dismiss_timer);
    s_ctx.wizard_dismiss_armed = true;
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
                // Bead pico-link-648: 0x0b means the stale ACL from before
                // a reboot is still up on the headset's side -- see this
                // field's doc comment on pl_a2dp_ctx_t. Retry exactly
                // once; a second 0x0b (reconnect_retry_used already true)
                // falls through to the normal failure report below, same
                // as every other status.
                if (status == ERROR_CODE_ACL_CONNECTION_ALREADY_EXISTS && !s_ctx.reconnect_retry_used) {
                    pl_a2dp_reconnect_retry_arm(status);
                    break;
                }
                pl_bt_push_connect_failed(s_ctx.connect_addr, pl_a2dp_failure_reason_for_status(status));
                break;
            }
            // A real success cancels any retry that might still be armed
            // (shouldn't happen -- the retry path re-issues on the same
            // connect_addr -- but a fresh connect() racing a stale timer
            // is exactly the "cannot fire into a live session" case this
            // bead's acceptance criteria calls out).
            pl_a2dp_reconnect_retry_cancel();
            // Bead pico-link-4vb.2: a fresh connect() attempt supersedes
            // whatever the previous session left armed/pushed -- same
            // reasoning as the reconnect-retry cancel just above.
            pl_a2dp_wizard_dismiss_timer_cancel();
            s_ctx.connect_succeeded_pushed = false;
            s_ctx.a2dp_cid = cid;
            // Fresh discovery pass starting -- clear any capability bits
            // left over from a previous connection attempt.
            memset(&s_ctx.discovered, 0, sizeof(s_ctx.discovered));
            pl_log("a2dp: signaling connected, cid=0x%02x\r\n", cid);
            // T3 (pico-link-4v2.3) finding, verified on hardware: a2dp.h's
            // module doc assumed "many real sinks open an AVRCP channel
            // unprompted right after A2DP connects" and this file never
            // called avrcp_connect() itself. Against Andreas's own
            // headphones that assumption is FALSE -- A2DP connected and
            // streamed fine for 30+ seconds with no AVRCP_SUBEVENT_
            // CONNECTION_ESTABLISHED ever arriving, which also explains
            // pico-link-wnk (HID media keys, direction B's AVRCP_SUBEVENT_
            // OPERATION path, not working on this headset -- same missing
            // channel). Initiate it ourselves; if the sink also opens it
            // independently, avrcp_connect() on an already-connecting/
            // connected cid returns a benign non-success status here and
            // s_avrcp_cid is still set exactly once, from the
            // CONNECTION_ESTABLISHED event handler below (single source of
            // truth, unchanged).
            {
                uint16_t requested_avrcp_cid = 0;
                uint8_t avrcp_connect_status = avrcp_connect(s_ctx.connect_addr, &requested_avrcp_cid);
                pl_log(
                    "avrcp: connect requested status=0x%02x requested_cid=%u\r\n", avrcp_connect_status,
                    requested_avrcp_cid
                );
            }
            break;
        }

        case A2DP_SUBEVENT_SIGNALING_MEDIA_CODEC_SBC_CAPABILITY: {
            uint16_t cid = a2dp_subevent_signaling_media_codec_sbc_capability_get_a2dp_cid(packet);
            if (cid != s_ctx.a2dp_cid) {
                break;
            }
            // First offering remote SEP wins -- see s_ctx.discovered's doc
            // comment.
            if (!s_ctx.discovered.sbc_offered) {
                s_ctx.discovered.sbc_offered = true;
                s_ctx.discovered.sbc_remote_seid = a2dp_subevent_signaling_media_codec_sbc_capability_get_remote_seid(packet);
                s_ctx.discovered.sbc_sampling_frequency_bitmap =
                    a2dp_subevent_signaling_media_codec_sbc_capability_get_sampling_frequency_bitmap(packet);
                s_ctx.discovered.sbc_channel_mode_bitmap =
                    a2dp_subevent_signaling_media_codec_sbc_capability_get_channel_mode_bitmap(packet);
                s_ctx.discovered.sbc_block_length_bitmap =
                    a2dp_subevent_signaling_media_codec_sbc_capability_get_block_length_bitmap(packet);
                s_ctx.discovered.sbc_subbands_bitmap = a2dp_subevent_signaling_media_codec_sbc_capability_get_subbands_bitmap(packet);
                s_ctx.discovered.sbc_allocation_method_bitmap =
                    a2dp_subevent_signaling_media_codec_sbc_capability_get_allocation_method_bitmap(packet);
                s_ctx.discovered.sbc_min_bitpool_value = a2dp_subevent_signaling_media_codec_sbc_capability_get_min_bitpool_value(packet);
                s_ctx.discovered.sbc_max_bitpool_value = a2dp_subevent_signaling_media_codec_sbc_capability_get_max_bitpool_value(packet);
            }
            break;
        }

        case A2DP_SUBEVENT_SIGNALING_MEDIA_CODEC_OTHER_CAPABILITY: {
            // Bead pico-link-cz0.5.6 (LDAC L3): BTstack's generic
            // catch-all for any AVDTP_CODEC_NON_A2DP SEP the remote
            // offers -- raw bytes only (no BTstack-native decode exists
            // for vendor codecs, unlike SBC above). Identify LDAC by
            // matching the wire's own vendor_id/vendor_codec_id against a
            // PL_CODECS row's declared identity -- a DATA-DRIVEN match
            // against the table, not a codec-identity branch (same "wire
            // fact, not business logic" reasoning a2dp.c's
            // avdtp_codec_type dispatch already relies on elsewhere in
            // this file); a remote offering some OTHER vendor codec we
            // have no row for simply matches nothing and is ignored.
            uint16_t cid = a2dp_subevent_signaling_media_codec_other_capability_get_a2dp_cid(packet);
            if (cid != s_ctx.a2dp_cid) {
                break;
            }
            uint16_t info_len = a2dp_subevent_signaling_media_codec_other_capability_get_media_codec_information_len(packet);
            const uint8_t *info = a2dp_subevent_signaling_media_codec_other_capability_get_media_codec_information(packet);
            if (info_len < 8 || info == NULL) {
                break; // too short to carry any vendor codec identity we know
            }
            uint32_t vendor_id =
                (uint32_t)info[0] | ((uint32_t)info[1] << 8) | ((uint32_t)info[2] << 16) | ((uint32_t)info[3] << 24);
            uint16_t vendor_codec_id = (uint16_t)((uint32_t)info[4] | ((uint32_t)info[5] << 8));

            if (!s_ctx.discovered.other_offered) {
                for (size_t i = 0; i < PL_CODEC_COUNT; i++) {
                    pl_codec_t *row = PL_CODECS[i];
                    if (row->avdtp_codec_type != AVDTP_CODEC_NON_A2DP) {
                        continue;
                    }
                    if (row->vendor_id != vendor_id || row->vendor_codec_id != vendor_codec_id) {
                        continue;
                    }
                    s_ctx.discovered.other_offered = true;
                    s_ctx.discovered.other_remote_seid = a2dp_subevent_signaling_media_codec_other_capability_get_remote_seid(packet);
                    s_ctx.discovered.other_sampling_frequency_bitmap = info[6];
                    s_ctx.discovered.other_channel_mode_bitmap = info[7];
                    break;
                }
            }
            break;
        }

        case A2DP_SUBEVENT_SIGNALING_CAPABILITIES_COMPLETE: {
            uint16_t cid = a2dp_subevent_signaling_capabilities_complete_get_a2dp_cid(packet);
            if (cid != s_ctx.a2dp_cid) {
                break;
            }

            // Preference-ordered table walk (design sec 4.3,
            // .planning/design/2026-08-30-ldac.md Q2/Q5): PL_CODECS is
            // sorted by preference with SBC permanently last as the
            // mandatory fallback floor (codec_table.h:97-99). Pick the
            // first row the remote also offered and hand it to BTstack's
            // matching a2dp_source_set_config_* -- which call depends on
            // avdtp_codec_type, an AVDTP wire-level fact, not a
            // codec-identity business-logic branch of the kind
            // codec_table.c forbids. Only SBC is wired today; L3 adds an
            // AVDTP_CODEC_NON_A2DP arm here for LDAC.
            bool matched = false;
            for (size_t i = 0; i < PL_CODEC_COUNT; i++) {
                pl_codec_t *row = PL_CODECS[i];
                if (row->avdtp_codec_type == AVDTP_CODEC_SBC) {
                    if (!s_ctx.discovered.sbc_offered) {
                        continue;
                    }
                    avdtp_stream_endpoint_t *local_ep = avdtp_get_stream_endpoint_for_seid(row->local_seid);
                    if (local_ep == NULL) {
                        pl_log("a2dp: no local stream endpoint for local_seid %u\r\n", row->local_seid);
                        continue;
                    }
                    avdtp_configuration_sbc_t configuration;
                    configuration.sampling_frequency =
                        avdtp_choose_sbc_sampling_frequency(local_ep, s_ctx.discovered.sbc_sampling_frequency_bitmap);
                    configuration.channel_mode = avdtp_choose_sbc_channel_mode(local_ep, s_ctx.discovered.sbc_channel_mode_bitmap);
                    configuration.block_length = avdtp_choose_sbc_block_length(local_ep, s_ctx.discovered.sbc_block_length_bitmap);
                    configuration.subbands = avdtp_choose_sbc_subbands(local_ep, s_ctx.discovered.sbc_subbands_bitmap);
                    configuration.allocation_method =
                        avdtp_choose_sbc_allocation_method(local_ep, s_ctx.discovered.sbc_allocation_method_bitmap);
                    configuration.max_bitpool_value = avdtp_choose_sbc_max_bitpool_value(local_ep, s_ctx.discovered.sbc_max_bitpool_value);
                    configuration.min_bitpool_value = avdtp_choose_sbc_min_bitpool_value(local_ep, s_ctx.discovered.sbc_min_bitpool_value);

                    uint8_t status = a2dp_source_set_config_sbc(cid, row->local_seid, s_ctx.discovered.sbc_remote_seid, &configuration);
                    if (status != ERROR_CODE_SUCCESS) {
                        pl_log("a2dp: set_config_sbc FAILED status=0x%02x\r\n", status);
                        continue;
                    }
                    matched = true;
                    break;
                } else if (row->avdtp_codec_type == AVDTP_CODEC_NON_A2DP) {
                    // Bead pico-link-cz0.5.6 (LDAC L3): no BTstack
                    // avdtp_choose_* helper exists for vendor codecs (SBC
                    // above gets one; LDAC does not), so this project
                    // implements its own -- trivially, because it only
                    // ever offers/accepts ONE LDAC configuration
                    // (48kHz/stereo, codec_ldac.c's doc comment). "Choose"
                    // therefore degenerates to "does the remote's
                    // discovered bitmap include our one supported bit",
                    // not a real intersection-and-pick algorithm. This
                    // LDAC-shaped glue living behind the AVDTP_CODEC_
                    // NON_A2DP wire-level-fact arm is the same accepted
                    // pattern as the SBC arm above (a2dp.c's own doc
                    // comment on the SBC_CONFIGURATION handler already
                    // makes this argument for that arm; design sec 4.2's
                    // rule is about the DISPATCH across PL_CODECS, not
                    // about a row's own negotiation-shape code living in
                    // its own arm here).
                    if (!s_ctx.discovered.other_offered) {
                        continue;
                    }
                    if ((s_ctx.discovered.other_sampling_frequency_bitmap & LDACBT_SAMPLING_FREQ_048000) == 0 ||
                        (s_ctx.discovered.other_channel_mode_bitmap & LDACBT_CHANNEL_MODE_STEREO) == 0) {
                        pl_log("a2dp: remote OTHER codec does not support 48kHz/stereo, skipping\r\n");
                        continue;
                    }
                    uint8_t status = a2dp_source_set_config_other(
                        cid, row->local_seid, s_ctx.discovered.other_remote_seid, pl_codec_ldac_negotiated_info,
                        sizeof(pl_codec_ldac_negotiated_info)
                    );
                    if (status != ERROR_CODE_SUCCESS) {
                        pl_log("a2dp: set_config_other FAILED status=0x%02x\r\n", status);
                        continue;
                    }
                    matched = true;
                    break;
                }
                // Future rows fall through here until their own
                // set_config_* arm exists.
            }

            if (!matched) {
                // Fail LOUDLY, not silently: under ENABLE_A2DP_EXPLICIT_CONFIG
                // the state machine parks at A2DP_DISCOVERY_DONE waiting for
                // us -- a no-suitable-codec path that just breaks means the
                // stream silently never configures (the pico-link-r44 failure
                // shape).
                pl_log("a2dp: NO SUITABLE CODEC offered by remote, cid=0x%02x -- stream will not configure\r\n", cid);
                pl_bt_push_connect_failed(s_ctx.connect_addr, PL_FAILURE_REASON_NO_A2DP_SINK);
            }
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

            pl_a2dp_finish_codec_negotiation(local_seid, (const uint8_t *)&cfg, sizeof(cfg));
            break;
        }

        case A2DP_SUBEVENT_SIGNALING_MEDIA_CODEC_OTHER_CONFIGURATION: {
            // Bead pico-link-cz0.5.6 (LDAC L3): the vendor-codec
            // counterpart of SBC_CONFIGURATION above -- forwarded by
            // BTstack's own a2dp.c:754-759, raw bytes only (no
            // BTstack-native decode for vendor codecs). Field offsets
            // mirror the capability bytes' own layout (codec_ldac.c's doc
            // comment): 6/7 are the negotiated single-bit sampling-
            // frequency/channel-mode selections.
            uint16_t cid = a2dp_subevent_signaling_media_codec_other_configuration_get_a2dp_cid(packet);
            if (cid != s_ctx.a2dp_cid) {
                break;
            }
            uint8_t local_seid = a2dp_subevent_signaling_media_codec_other_configuration_get_local_seid(packet);
            s_ctx.remote_seid = a2dp_subevent_signaling_media_codec_other_configuration_get_remote_seid(packet);

            uint16_t info_len = a2dp_subevent_signaling_media_codec_other_configuration_get_media_codec_information_len(packet);
            const uint8_t *info = a2dp_subevent_signaling_media_codec_other_configuration_get_media_codec_information(packet);
            if (info_len < 8 || info == NULL) {
                pl_log("a2dp: OTHER_CONFIGURATION too short (%u bytes)\r\n", (unsigned)info_len);
                pl_bt_push_connect_failed(s_ctx.connect_addr, PL_FAILURE_REASON_NO_A2DP_SINK);
                break;
            }
            // Decoded straight into codec_ldac.h's private shape -- safe
            // because PL_CODECS has exactly one AVDTP_CODEC_NON_A2DP row
            // today (LDAC) and the bytes here are what WE sent via
            // a2dp_source_set_config_other (pl_codec_ldac_negotiated_info,
            // the CAPABILITIES_COMPLETE handler above), so they cannot be
            // any other vendor codec's shape. A second vendor row would
            // need this decode to dispatch on info[0..5]
            // (vendor_id/vendor_codec_id) first, same as the
            // OTHER_CAPABILITY handler above already does.
            pl_codec_ldac_negotiated_t cfg;
            cfg.sampling_frequency = info[6];
            cfg.channel_mode = info[7];

            pl_a2dp_finish_codec_negotiation(local_seid, (const uint8_t *)&cfg, sizeof(cfg));
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
                // Bead pico-link-648, measured on hardware: BTstack's
                // a2dp_source layer cascades the SAME underlying failure
                // that just came through
                // A2DP_SUBEVENT_SIGNALING_CONNECTION_ESTABLISHED into this
                // higher-level completion event too, synchronously, same
                // call stack. If we just armed a 0x0b retry for that
                // failure, don't ALSO tell the UI it failed -- that would
                // flash a failure screen for an attempt we're still
                // retrying. reconnect_retry_armed is only true here in
                // that exact window (it's cleared before this handler can
                // run again for a genuinely new attempt). Bead pico-link-cz0.6
                // code review: also require THIS status to match the one
                // that armed the retry -- a genuinely different failure
                // landing in that same ~1.5s window must still be reported,
                // not silently absorbed into the 0x0b suppression.
                if (s_ctx.reconnect_retry_armed && status == s_ctx.reconnect_retry_armed_status) {
                    pl_log("a2dp: suppressing UI failure push -- 0x0b retry already armed for this attempt\r\n");
                    break;
                }
                pl_bt_push_connect_failed(s_ctx.connect_addr, PL_FAILURE_REASON_REJECTED);
                break;
            }
            s_ctx.local_seid = a2dp_subevent_stream_established_get_local_seid(packet);
            int mtu = a2dp_max_media_payload_size(s_ctx.a2dp_cid, s_ctx.local_seid);
#ifdef PL_ENCODER_ON_CORE1
            // Bead pico-link-nli.4 (G3), G4 review fix: a defensive quiesce
            // BEFORE touching any shared field below, INCLUDING
            // max_media_payload_size itself -- it is read on every
            // pl_a2dp_fill() call (pl_a2dp_usable_payload), so writing it
            // ahead of this quiesce (the original, incorrect ordering)
            // could race a still-RUNNING core1 mid-read of it. By the time
            // a FRESH connection reaches STREAM_ESTABLISHED, any prior
            // session's SIGNALING_CONNECTION_RELEASED handler should
            // already have quiesced and idled core1, so this is normally a
            // same-cycle no-op (s_enc_quiesced already true). Kept
            // unconditional rather than assumed, per pl_a2dp_tx_flush's own
            // doc comment reasoning: every call site that touches these
            // fields must be provably safe, not merely usually safe.
            pl_a2dp_core1_quiesce_and_wait();
#endif
            s_ctx.max_media_payload_size = btstack_min(mtu, (int)PL_A2DP_PAYLOAD_SLOT_BYTES - 1);
            // Bead pico-link-85v (D6): flush the tx ring here too -- a
            // stale payload surviving into a new stream is an audible
            // artefact (a burst of the previous track with a stale RTP
            // timestamp). rtp_next resets to 0 (unlike SUSPEND, which
            // preserves it for auto-resume).
            pl_a2dp_tx_flush();
            s_ctx.rtp_next = 0;
            s_ctx.silent_ticks = 0;
            s_ctx.pause_requested = false;

            // Bead pico-link-pbv (C3, round 1): frames_per_packet --
            // computed from what was ACTUALLY negotiated (this
            // connection's real payload size), not a guessed-at fixed
            // number. Round 2 (C2-1/C2-4) reuses this same value for the
            // credit clamp and the priming-cushion derivation below;
            // round 2 also RETIRES the dwell_cap/frames_per_tick_cap half
            // of round 1's computation -- pl_a2dp_fill now bounds dwell
            // with a real TIME check (D4), not a frame-count proxy for
            // one. Round 3 (R3-4): divide by pl_a2dp_usable_payload's
            // corrected capacity, not the raw negotiated size -- see that
            // helper's doc comment for why using the uncorrected size here
            // was a latent off-by-one that R3-1 would have turned active.
            s_ctx.frames_per_packet = s_ctx.frame.encoded_frame_bytes > 0
                                           ? btstack_max(
                                                 1u,
                                                 pl_a2dp_usable_payload(s_ctx.max_media_payload_size, s_ctx.frame.header_bytes) /
                                                     s_ctx.frame.encoded_frame_bytes
                                             )
                                           : 1u;

            // Bead pico-link-85v (D2): recompute the live tx-queue-depth
            // requirement from what was ACTUALLY negotiated, and warn
            // loudly (never silently clamp -- pico-link-r44 lesson) if it
            // would exceed the compile-time PL_A2DP_TX_QUEUE_SLOTS.
            // B_tick = ceil(PL_A2DP_MAX_ENCODE_DWELL_US /
            //               (worst_case_encode_us * frames_per_packet)),
            // required = 1 (head) + 2*B_tick (sealed this tick + unsent
            // from last tick).
            if (s_ctx.frame.worst_case_encode_us > 0 && s_ctx.frames_per_packet > 0) {
                uint32_t denom = s_ctx.frame.worst_case_encode_us * s_ctx.frames_per_packet;
                uint32_t b_tick = (PL_A2DP_MAX_ENCODE_DWELL_US + denom - 1) / denom; // ceil
                uint32_t required_slots = 1u + 2u * b_tick;
                // Bead pico-link-nli.3 (G2): the usable ceiling is
                // SLOTS - 1, not SLOTS -- one slot is permanently
                // reserved so pl_a2dp_tx_count()'s masked head-tail
                // derivation can disambiguate empty from full (see
                // pcm_ring.c's byte-ring discipline, applied here to
                // the slot ring). Comparing against the raw SLOTS
                // count would silently pass a queue that is actually
                // one slot too small.
                if (required_slots > PL_A2DP_TX_QUEUE_SLOTS - 1u) {
                    pl_log(
                        "a2dp: WARNING tx queue depth %u required but only %u usable of %u compiled in "
                        "(b_tick=%lu frames_per_packet=%lu worst_case_encode_us=%lu)\r\n",
                        (unsigned)required_slots, (unsigned)(PL_A2DP_TX_QUEUE_SLOTS - 1u),
                        (unsigned)PL_A2DP_TX_QUEUE_SLOTS, (unsigned long)b_tick,
                        (unsigned long)s_ctx.frames_per_packet, (unsigned long)s_ctx.frame.worst_case_encode_us
                    );
                }
            }

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
            uint32_t unclamped_priming_target_bytes = btstack_max(derived_cushion, PL_PCM_TARGET_FILL_BYTES);

            // Bead pico-link-r44: worst_tick_interval_us is a monotone
            // maximum that (as of the STREAM_STARTED reset added by this
            // same bead) can still carry forward a single prior stall
            // across a reconnect within the same stream. Left unclamped,
            // one bad tick makes derived_cushion (and therefore
            // priming_target_bytes) exceed PL_PCM_RING_CAPACITY -- a level
            // pl_pcm_fill_bytes() can never reach -- and PRIMING's exit
            // condition (pl_pcm_fill_bytes() >= priming_target_bytes,
            // below) is never satisfied. That is a SILENT total stream
            // failure: no error, no counter, just a connection that never
            // starts playing audio. Clamp to a quarter of ring capacity
            // (8192 bytes -- still several times PL_PCM_TARGET_FILL_BYTES)
            // so priming can always complete, and log loudly when the
            // clamp actually binds since a silent failure is exactly what
            // this bug was.
            s_ctx.priming_target_bytes = btstack_min(unclamped_priming_target_bytes, PL_PCM_RING_CAPACITY / 4u);

            // Bead pico-link-fhf (orchestrator decision on Ada's design
            // sec 7): unify the three consumers of "the target" onto one
            // runtime setpoint, set from the already-clamped, already-
            // derived priming cushion computed above. Before this, the
            // feedback loop (usb_audio.c) and the resync trim (this file's
            // media-timer handler) both regulated against the bare
            // PL_PCM_TARGET_FILL_BYTES macro while PRIMING landed the ring
            // on this larger derived cushion -- the controller then spent
            // ~15s silently walking the ring back down, destroying the
            // cushion its own derivation just built. Setting the runtime
            // setpoint here makes all three agree on the value PRIMING
            // actually achieved.
            pl_pcm_set_target_fill_bytes(s_ctx.priming_target_bytes);

            // Signalling context, not the media hot path -- pl_log is
            // fine here (see this file's module doc). Startup-only line,
            // not rate-limited: fires once per stream, same as the
            // "stream established"/"codec=" lines already here.
            pl_log(
                "a2dp: frames_per_packet=%lu priming_target_bytes=%lu (one_packet_bytes=%lu jitter_bytes=%lu)\r\n",
                (unsigned long)s_ctx.frames_per_packet, (unsigned long)s_ctx.priming_target_bytes,
                (unsigned long)one_packet_bytes, (unsigned long)jitter_bytes
            );
            if (s_ctx.priming_target_bytes != unclamped_priming_target_bytes) {
                pl_log(
                    "a2dp: WARNING priming_target_bytes clamped from %lu to %lu "
                    "(worst_tick_interval_us=%lu would have deadlocked PRIMING)\r\n",
                    (unsigned long)unclamped_priming_target_bytes, (unsigned long)s_ctx.priming_target_bytes,
                    (unsigned long)s_ctx.worst_tick_interval_us
                );
            }

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

            // Bead pico-link-cz0.6 follow-up (Andreas's ruling, 2026-09-01):
            // persist the device record HERE, synchronously, before
            // priming proceeds -- not staged in RAM for a quiet window
            // that may never come. This is the fix for the ordering
            // defect the code review's Finding 1 fix left open: this
            // handler runs on the cyw43/BTstack background async_context
            // (same context BTstack's own put_link_key uses), so calling
            // straight into persist.c here is already safe -- see
            // pl_persist_save_device_now's doc comment (persist.h) for the
            // full rationale and its one carve-out (USB audio already
            // live, pico-link-lmf). Must run BEFORE the PRIMING transition
            // below, or pl_a2dp_streaming() would already read true and
            // the write would be no different from the deferred path this
            // ruling exists to bypass.
            pl_persist_save_device_now(s_ctx.connect_addr);

            // Bead pico-link-4vb.2 (bug 2): the link is fully up here --
            // codec configured, AVDTP media channel open -- so this, not
            // A2DP_SUBEVENT_STREAM_STARTED below, is where
            // ConnectSucceeded/LinkState(Connected) belong. STREAM_STARTED
            // requires the HOST to begin streaming audio (it fires from
            // TinyUSB's own alt-setting activation, entirely outside our
            // control), so gating success on it left the wizard stuck on
            // "Negotiating codec" until Andreas pressed play on the host.
            // s_ctx.codec is guaranteed non-NULL here: it's set by
            // pl_a2dp_finish_codec_negotiation during the codec
            // configuration subevent(s), which BTstack always delivers
            // before STREAM_ESTABLISHED. connect_succeeded_pushed guards
            // against a double push both if STREAM_ESTABLISHED itself
            // somehow re-fires for the same stream and if STREAM_STARTED
            // later arrives (see that case below, which no longer pushes
            // at all) -- reset to false only at the next fresh
            // SIGNALING_CONNECTION_ESTABLISHED (a genuinely new attempt).
            if (!s_ctx.connect_succeeded_pushed) {
                s_ctx.connect_succeeded_pushed = true;
                pl_bt_push_link_state_connected();
                pl_bt_push_connect_succeeded(s_ctx.connect_addr, PL_CODEC_COUNT > 0 && s_ctx.codec != PL_CODECS[0]);

                // Bead pico-link-4vb.2 (bug 3): PL_EVENT_TAG_WIZARD_AUTO_DISMISS
                // had no producer anywhere in firmware -- core already
                // handles the event (pops the wizard back to Home on a
                // plain, non-degraded success) but nothing ever sent it.
                // Arm a couple-second one-shot timer here; degraded
                // success also gets this timer armed (harmless -- core's
                // own guard on Event::WizardAutoDismiss ignores it unless
                // the wizard is showing a plain success).
                pl_a2dp_wizard_dismiss_timer_arm();
            }

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
#ifdef PL_ENCODER_ON_CORE1
            // Bead pico-link-nli.4 (G3): PL_WDT_ENCODER shares PL_WDT_MEDIA's
            // lifecycle -- meaningful only while actually streaming.
            pl_wdt_set_enabled(PL_WDT_ENCODER, true);
#endif
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
#ifdef PL_ENCODER_ON_CORE1
            // Bead pico-link-nli.4 (G3), design sec 4.2: resets
            // samples_owed/samples_owed_rem_us THEN publishes RUNNING (with
            // a __dmb() between) so core1 never observes RUNNING before the
            // reset has landed -- see pl_a2dp_core1_arm_running's doc
            // comment. Replaces the plain reset below.
            pl_a2dp_core1_arm_running();
#else
            s_ctx.samples_owed = 0;
            s_ctx.samples_owed_rem_us = 0;
#endif
            // Bead pico-link-r44: reset the tick-jitter high-water mark
            // exactly at the instant real streaming begins, same reasoning
            // as the credit-clock reset above -- a stall from a PRIOR
            // stream (or from PRIMING/IDLE) must not carry forward into
            // this stream's priming_target_bytes derivation at the next
            // STREAM_ESTABLISHED (a2dp.c's jitter_bytes computation). This
            // was the field's only writer besides that computation; it
            // previously never reset at all (see this file's module doc
            // and worst_tick_interval_us's own field doc, both of which
            // cited that as the reason NOT to use it in the credit clamp --
            // stale now that a reset exists here, see those comments).
            s_ctx.worst_tick_interval_us = 0;
            // Bead pico-link-85v (D7): high-water marks reset at
            // STREAM_STARTED, same reasoning as worst_tick_interval_us
            // above (pico-link-r44 lesson: a high-water mark that never
            // resets poisons later derivations).
            s_ctx.tx_depth_max = 0;
            s_ctx.dwell_max_us = 0;
            // Bead pico-link-pbv round 2 (C2-3): round 1's trim-to-target
            // here is DELETED -- it discarded exactly the cushion that
            // keeps one late tick from reaching zero (PRIMING now waits
            // for the derived priming_target_bytes cushion instead, C2-4
            // above; there is no overshoot left worth trimming away, and
            // trimming it destroyed the margin the fix depends on). Prime
            // to the target and start; do not trim here -- this stays
            // deleted even after pico-link-fhf wires up the media-timer
            // trim block above, because the priming cushion is not
            // overshoot.
            //
            // Bead pico-link-fhf: reset resync_events and last_resync_us
            // at the same instant, same reasoning as the other high-water/
            // clock resets above -- a trim from a PRIOR stream must not
            // gate or be double-counted against this one's test window.
            // resync_drops itself stays a since-boot cumulative counter,
            // same as flush_frames and ovr_frames, so it is NOT reset here
            // -- the conservation check sums it across the whole session.
            s_ctx.resync_events = 0;
            s_ctx.last_resync_us = 0;
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
            // Bead pico-link-4vb.2 (bug 2): ConnectSucceeded/LinkState
            // (Connected) moved to A2DP_SUBEVENT_STREAM_ESTABLISHED above
            // -- the link is fully up well before the host actually
            // starts streaming audio, and gating success on THIS event
            // left the wizard stuck on "Negotiating codec" until the host
            // played something. Nothing to push here any more;
            // connect_succeeded_pushed (set at STREAM_ESTABLISHED) is what
            // stops a stray second push if this event fires for the same
            // stream.
            break;

        case A2DP_SUBEVENT_STREAM_SUSPENDED:
            pl_wdt_set_enabled(PL_WDT_MEDIA, false);
#ifdef PL_ENCODER_ON_CORE1
            pl_wdt_set_enabled(PL_WDT_ENCODER, false);
            // Bead pico-link-nli.4 (G3): quiesce core1 BEFORE touching the
            // tx ring/PCM ring below -- see pl_a2dp_core1_quiesce_and_wait's
            // doc comment. Whether this transition lands in PRIMING
            // (auto-resume) or IDLE, core1 must not run again until the
            // next STREAM_STARTED explicitly re-arms it.
            pl_a2dp_core1_quiesce_and_wait();
            s_enc_state = PL_ENC_STATE_IDLE;
#endif
            pl_log("a2dp: stream suspended (auto_resume=%d)\r\n", s_ctx.auto_resume ? 1 : 0);
            // Bead pico-link-85v (D6): flush the tx ring -- rtp_next is
            // deliberately PRESERVED here (not reset) so auto-resume
            // continues one timeline, unlike ESTABLISHED/RELEASED below.
            pl_a2dp_tx_flush();
            s_ctx.silent_ticks = 0;
            s_ctx.pause_requested = false;
            // Bead pico-link-pbv round 2 (C2-6): count the discard.
            s_ctx.flush_frames += pl_pcm_reset();
            s_ctx.state = s_ctx.auto_resume ? PL_A2DP_MEDIA_PRIMING : PL_A2DP_MEDIA_IDLE;
            s_ctx.auto_resume = false;
            // Bead pico-link-cz0.6 (M5 persistence), design point 4: "flush
            // on stream stop" -- only a real stop (state is now IDLE, not a
            // PRIMING auto-resume) counts as one. Flag-only, see
            // pl_a2dp_connect's call site above for why.
            if (s_ctx.state == PL_A2DP_MEDIA_IDLE) {
                pl_persist_request_urgent_flush();
            }
            break;

        case A2DP_SUBEVENT_STREAM_RELEASED:
            pl_wdt_set_enabled(PL_WDT_MEDIA, false);
#ifdef PL_ENCODER_ON_CORE1
            pl_wdt_set_enabled(PL_WDT_ENCODER, false);
            // Bead pico-link-nli.4 (G3): quiesce before rtp_next/tx-ring/
            // PCM-ring resets below, same reasoning as STREAM_SUSPENDED
            // above. Defensive against SUSPENDED not having already run
            // (e.g. a direct establish->release edge) -- a no-op if core1
            // is already quiesced.
            pl_a2dp_core1_quiesce_and_wait();
            s_enc_state = PL_ENC_STATE_IDLE;
#endif
            pl_log("a2dp: stream released\r\n");
            s_ctx.state = PL_A2DP_MEDIA_IDLE;
            s_ctx.codec = NULL;
            if (s_ctx.timer_armed) {
                btstack_run_loop_remove_timer(&s_ctx.media_timer);
                s_ctx.timer_armed = false;
            }
            // Bead pico-link-85v (D6): flush the tx ring; rtp_next resets
            // (same as ESTABLISHED -- this timeline is over).
            pl_a2dp_tx_flush();
            s_ctx.rtp_next = 0;
            // Bead pico-link-pbv round 2 (C2-6): count the discard.
            s_ctx.flush_frames += pl_pcm_reset();
            // Bead pico-link-cz0.6 (M5 persistence): stream stop, see the
            // STREAM_SUSPENDED case above.
            pl_persist_request_urgent_flush();
            break;

        case A2DP_SUBEVENT_SIGNALING_CONNECTION_RELEASED:
            pl_wdt_set_enabled(PL_WDT_MEDIA, false);
#ifdef PL_ENCODER_ON_CORE1
            pl_wdt_set_enabled(PL_WDT_ENCODER, false);
            // Bead pico-link-nli.4 (G3): quiesce before this handler's own
            // tx-ring/rtp_next/PCM-ring resets below, same reasoning as
            // STREAM_SUSPENDED/STREAM_RELEASED above. Defensive against
            // this being the FIRST teardown event to fire for a given
            // session (e.g. the ACL simply dropping before a clean
            // SUSPENDED/RELEASED pair) -- a no-op if already quiesced.
            pl_a2dp_core1_quiesce_and_wait();
            s_enc_state = PL_ENC_STATE_IDLE;
#endif
            pl_log("a2dp: signaling connection released\r\n");
            // Bead pico-link-648: belt-and-suspenders -- if a retry was
            // still armed when the signaling connection went away some
            // other way, don't let it fire into whatever comes next.
            pl_a2dp_reconnect_retry_cancel();
            // Bead pico-link-4vb.2 (bug 3): a stale dismiss timer must
            // never fire after the session it belonged to is gone.
            pl_a2dp_wizard_dismiss_timer_cancel();
            // Bead pico-link-4vb.5 (Andreas: "it doesn't seem like the
            // home page detects when I turn off my headphones"):
            // PL_LINK_STATE_IDLE had no producer for a real disconnect --
            // BtModel.link_state stayed Connected forever and Home kept
            // showing a live link to hardware that was switched off. This
            // is the authoritative "the A2DP session is over" point
            // (unlike STREAM_SUSPENDED/STREAM_RELEASED, which also fire on
            // an ordinary pause/auto-resume and must NOT read as a
            // disconnect -- see the STREAM_RELEASED case above, left
            // untouched deliberately). pl_bt_push_link_state_disconnected
            // also clears BtModel::connected_codec on the core side (see
            // App::set_link_state's doc comment) the same way the
            // existing non-Connected handling already does, so no
            // separate codec-clear push is needed here.
            pl_bt_push_link_state_disconnected();
            s_ctx.connect_succeeded_pushed = false;
            s_ctx.a2dp_cid = 0;
            s_ctx.codec = NULL;
            s_ctx.state = PL_A2DP_MEDIA_IDLE;
            // Bead pico-link-85v (D6): this handler flushed NOTHING before
            // this bead -- not even the PCM ring. Flush both here, same as
            // ESTABLISHED/RELEASED.
            pl_a2dp_tx_flush();
            s_ctx.rtp_next = 0;
            s_ctx.flush_frames += pl_pcm_reset();
            if (s_ctx.timer_armed) {
                btstack_run_loop_remove_timer(&s_ctx.media_timer);
                s_ctx.timer_armed = false;
            }
            // Bead pico-link-cz0.6 (M5 persistence): stream stop, see the
            // STREAM_SUSPENDED case above.
            pl_persist_request_urgent_flush();
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

    // Design finding 1.1 (.planning/design/2026-09-02-device-page-seam.md
    // sec 1.1, bead pico-link-ay0.1): codec_id is a PINNED per-row identity,
    // persisted to flash -- never the array index above, which is the
    // negotiation preference order and is designed to change. This is the
    // cheapest possible guard against the one mistake (a new row shipped
    // with codec_id 0, or reusing an id already claimed by another row)
    // that would otherwise silently repoint every device that pinned the
    // colliding id at the wrong codec, with a valid CRC and no way to
    // detect it. Halts (like this file's other "must never happen" guards,
    // e.g. persist.c's flash-collision check) rather than limping on with a
    // codec table that cannot be trusted.
    for (size_t i = 0; i < PL_CODEC_COUNT; i++) {
        if (PL_CODECS[i]->codec_id == PL_CODEC_ID_AUTOMATIC) {
            pl_log(
                "a2dp: FATAL codec row \"%s\" has codec_id 0 (PL_CODEC_ID_AUTOMATIC is reserved, never a "
                "table row) -- halting\r\n",
                PL_CODECS[i]->display_name
            );
            while (true) {
                tight_loop_contents();
            }
        }
        for (size_t j = i + 1; j < PL_CODEC_COUNT; j++) {
            if (PL_CODECS[j]->codec_id == PL_CODECS[i]->codec_id) {
                pl_log(
                    "a2dp: FATAL duplicate codec_id %u shared by \"%s\" and \"%s\" -- halting\r\n",
                    (unsigned)PL_CODECS[i]->codec_id, PL_CODECS[i]->display_name, PL_CODECS[j]->display_name
                );
                while (true) {
                    tight_loop_contents();
                }
            }
        }
    }

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

bool pl_a2dp_streaming(void) {
    return s_ctx.state != PL_A2DP_MEDIA_IDLE;
}

void pl_a2dp_connect(const uint8_t *addr) {
    // Bead pico-link-648: a fresh top-level connect attempt gets its own
    // single retry budget, and supersedes any retry still armed from a
    // previous attempt (e.g. the user backed out and reconnected inside
    // the retry's ~1.5s window).
    pl_a2dp_reconnect_retry_cancel();
    s_ctx.reconnect_retry_used = false;
    pl_a2dp_establish_stream_now(addr);
}

void pl_a2dp_disconnect(void) {
    // Bead pico-link-648: a user-initiated disconnect must not let a
    // pending 0x0b retry fire later and reopen a session the user just
    // closed.
    pl_a2dp_reconnect_retry_cancel();
    if (s_ctx.a2dp_cid == 0) {
        pl_log("a2dp: disconnect requested, no active a2dp_cid, no-op\r\n");
        return;
    }
    uint8_t status = a2dp_source_disconnect(s_ctx.a2dp_cid);
    pl_log("a2dp: disconnect requested, a2dp_cid=0x%04x status=0x%02x\r\n", s_ctx.a2dp_cid, status);
}

#ifdef PL_DEBUG_REMOTE
// Bead pico-link-fhf, test A. Thread-context write (debug_remote.c's
// poll, superloop) to the single aligned word the media-timer IRQ
// decrements -- see s_debug_skip_media_ticks's doc comment above. A
// second call before the first one-shot has drained simply overwrites
// the remaining count rather than adding to it, same "last write wins"
// semantics as every other thread-to-IRQ debug knob in this file.
void pl_a2dp_debug_skip_media_ticks(uint32_t ticks) {
    s_debug_skip_media_ticks = ticks;
}
#endif

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
    // Bead pico-link-fhf: fill's denominator is the RUNTIME setpoint
    // (pl_pcm_target_fill_bytes), not the PL_PCM_TARGET_FILL_BYTES macro --
    // STREAM_ESTABLISHED sets the runtime value from the priming cushion
    // (typically higher than the macro for SBC), and this diagnostic ratio
    // must read against whatever the pipeline is actually regulating to,
    // same reasoning as the trim block and usb_audio.c's feedback loop.
    pl_log(
        "a2dp: codec=%s bitrate=%lu fill=%lu/%lu ovr_frames=%lu und=%lu report_dt_us=%lu\r\n", codec_name,
        (unsigned long)s_ctx.frame.nominal_bitrate_bps, (unsigned long)pl_pcm_fill_bytes(),
        (unsigned long)pl_pcm_target_fill_bytes(), (unsigned long)pl_pcm_overrun_frames(),
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
        "a2dp: tick_count=%lu worst_tick_interval_us=%lu enc_frames_total=%lu\r\n", (unsigned long)s_ctx.tick_count,
        (unsigned long)s_ctx.worst_tick_interval_us, (unsigned long)s_ctx.enc_frames_total
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
    // resync_events (pico-link-fhf) is the discrete-trim COUNT alongside
    // resync_drops's frame count -- the injection test's pass criterion
    // needs both: exactly one event distinguishes a correct single cut
    // from a double-cut bug that resync_drops alone can't reveal.
    pl_log(
        "a2dp: frames_per_packet=%lu credit_clamped_samples=%lu credit_clamp_events=%lu flush_frames=%lu "
        "resync_drops=%lu resync_events=%lu fill_ema=%ld fill_min=%lu\r\n",
        (unsigned long)s_ctx.frames_per_packet, (unsigned long)s_ctx.credit_clamped_samples,
        (unsigned long)s_ctx.credit_clamp_events, (unsigned long)s_ctx.flush_frames, (unsigned long)s_ctx.resync_drops,
        (unsigned long)s_ctx.resync_events, (long)pl_usb_audio_fb_fill_ema(), (unsigned long)pl_usb_audio_fill_min()
    );
    // Bead pico-link-pbv round 2 (C2-2), retuned by pico-link-85v: stop-
    // reason breakdown for pl_a2dp_fill's loop. stop_dwell must read 0 in
    // a healthy run (falsifier: the real dwell-safety bound, not the
    // credit clock, would be the actual limiter). stop_credit dominating
    // is the expected healthy reading (most ticks fire faster than one
    // SBC frame's worth of real time). stop_queue_full is the new ceiling
    // tripwire (replacing stop_packet_full_hot) -- must read 0 for SBC;
    // see that field's doc comment on pl_a2dp_ctx_t.
    pl_log(
        "a2dp: stop_credit=%lu stop_queue_full=%lu stop_ring_empty=%lu stop_dwell=%lu fill_short_read=%lu\r\n",
        (unsigned long)s_ctx.stop_credit, (unsigned long)s_ctx.stop_queue_full, (unsigned long)s_ctx.stop_ring_empty,
        (unsigned long)s_ctx.stop_dwell, (unsigned long)s_ctx.fill_short_read
    );
    // Bead pico-link-cz0.5.8 (Ada's step 4): stop_dwell is documented two
    // comments up as "must read 0 in a healthy run" -- it was, all session,
    // and nobody diffed it against a healthy baseline. stop_dwell is
    // cumulative and never reset (see the comment on the tick_count log
    // above), so a real, sustained dwell-cap trip shows as a nonzero DELTA
    // between consecutive report windows, not a one-off blip -- track the
    // last reported value here and warn loudly, same shape as the D2
    // tx-queue-depth check above (never let a real safety-trip go silent).
    static uint32_t s_last_stop_dwell;
    uint32_t stop_dwell_now = s_ctx.stop_dwell;
    if (stop_dwell_now != s_last_stop_dwell) {
        pl_log(
            "a2dp: WARNING stop_dwell rose by %lu this report window (total=%lu) -- the "
            "dwell-safety backstop tripped; encode is NOT finishing inside its credit "
            "budget and pl_a2dp_fill is being force-stopped by PL_A2DP_MAX_ENCODE_DWELL_US, "
            "not by the credit clock. See a2dp.c:609's doc comment.\r\n",
            (unsigned long)(stop_dwell_now - s_last_stop_dwell), (unsigned long)stop_dwell_now
        );
    }
    s_last_stop_dwell = stop_dwell_now;
#ifdef PL_ENCODER_ON_CORE1
    // Bead pico-link-nli.4 (G3), G4 review hardening: same "never let a
    // safety-trip go silent" treatment for stop_core1_budget -- see
    // PL_A2DP_CORE1_FILL_BUDGET_US's doc comment. A rise here means a
    // single fill() call actually approached the quiesce timeout's
    // margin -- worth investigating, but the cap itself did its job by
    // returning promptly rather than overrunning.
    static uint32_t s_last_stop_core1_budget;
    uint32_t stop_core1_budget_now = s_ctx.stop_core1_budget;
    if (stop_core1_budget_now != s_last_stop_core1_budget) {
        pl_log(
            "a2dp: WARNING stop_core1_budget rose by %lu this report window (total=%lu) -- "
            "a fill() call hit PL_A2DP_CORE1_FILL_BUDGET_US's 2ms wall-clock cap on a real "
            "backlog; check quiesce timeouts (pl_a2dp_encoder_quiesce_timeouts) for margin.\r\n",
            (unsigned long)(stop_core1_budget_now - s_last_stop_core1_budget), (unsigned long)stop_core1_budget_now
        );
    }
    s_last_stop_core1_budget = stop_core1_budget_now;
#endif
    // Bead pico-link-85v (D7): the new drain-side counters. payloads_sealed
    // vs pkt_sent (above) is the single most important split -- equal
    // means fill-limited (healthy); sealed > sent means send-limited (read
    // grants below for why). tx_depth_max == PL_A2DP_TX_QUEUE_SLOTS - 1
    // means D2's depth is marginal. grants/s vs pkt_sent/s separates
    // "grants are slow" (ACL-credit-bound) from "we did not ask" (a re-arm
    // bug), and is the number that decides whether LDAC HQ's 140
    // packets/s is reachable at all. spurious_grants > 0 only around
    // SUSPEND/reconnect is healthy (D6); mid-stream is a bug.
    // dwell_max_us is the high-water this tick's dwell reached -- a zero
    // stop_dwell with no dwell_max_us reported is not evidence the
    // backstop has margin.
    pl_log(
        "a2dp: payloads_sealed=%lu tx_depth_max=%lu grants=%lu spurious_grants=%lu dwell_max_us=%lu\r\n",
        (unsigned long)s_ctx.payloads_sealed, (unsigned long)s_ctx.tx_depth_max, (unsigned long)s_ctx.grants,
        (unsigned long)s_ctx.spurious_grants, (unsigned long)s_ctx.dwell_max_us
    );
    // Bead pico-link-8b7: OUT-meter push rate. level_push_count is
    // cumulative -- divide its delta between two report lines by
    // report_dt_us to get the real Event::LevelsChanged rate hero.rs
    // actually sees. Healthy/expected: ~4/s (250ms interval),
    // skip_interval dominating skip_empty.
    pl_log(
        "a2dp: level_push_count=%lu level_push_skip_empty=%lu level_push_skip_interval=%lu\r\n",
        (unsigned long)s_ctx.level_push_count, (unsigned long)s_ctx.level_push_skip_empty,
        (unsigned long)s_ctx.level_push_skip_interval
    );
}

// Bead pico-link-auh, section 1: see a2dp.h's doc comment on this
// function. seq is this function's own monotonic counter -- it exists
// purely as the missed-publish detector (A2 in Ada's design comment's
// acceptance criteria): a hardware capture with a gap in seq means a
// publish cycle was skipped, not that the console dropped a line (this
// slot cannot be dropped by pl_log_ring.c -- see pl_prio.h).
void pl_a2dp_publish_counters(void) {
    static uint32_t s_seq;
    s_seq++;
    pl_prio_publish(
        0,
        "ctr s=%010lu u=%010lu t=%010lu e=%010lu p=%010lu d=%010lu c=%010lu o=%010lu",
        (unsigned long)s_seq,
        (unsigned long)(time_us_64() / 1000),
        (unsigned long)s_ctx.tick_count,
        (unsigned long)s_ctx.enc_frames_total,
        (unsigned long)s_ctx.pkt_sent,
        (unsigned long)s_ctx.stop_dwell,
        (unsigned long)s_ctx.stop_credit,
        (unsigned long)pl_pcm_overrun_frames() // NOT a s_ctx field -- see pl_a2dp_report's ovr_frames= line above
    );
}
