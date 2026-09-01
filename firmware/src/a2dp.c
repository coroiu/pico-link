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
#include "pcm_ring.h"
#include "persist.h"
#include "pl_prio.h"
#include "usb_audio.h"
#include "usb_pump.h"
#include "watchdog_sup.h"

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
#define PL_A2DP_TX_QUEUE_SLOTS 5

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
// LISTENING TEST 2026-08-31 (bead pico-link-p1r / the LDAC crackle):
// 6000u was an SBC-era value from when a frame cost ~800us. An LDAC frame
// costs ~1159us, so 6000 caps the fill loop at 5 frames/tick while credit
// needs 6.26 -- measured on hardware as 18.5% of ticks tripping this
// backstop (pico-link-zmg, delta stop_dwell/delta tick_count = 0.1851)
// alongside a 9.8% encode shortfall. 10000u allows 8 frames/tick.
// The loop-hogging this bound protects against is already moot at 6Hz
// (pico-link-p1r): 4ms more dwell against a 164ms iteration is noise.
#define PL_A2DP_MAX_ENCODE_DWELL_US 10000u

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
    // being filled; tx_tail is the next slot to send; tx_count is the
    // number of SEALED, unsent slots. send_requested mirrors whether we
    // currently have an outstanding request_can_send_now with BTstack.
    //
    // CONCURRENCY: none. The media timer handler (producer, fills/seals)
    // and the A2DP packet handler's CAN_SEND_MEDIA_PACKET_NOW case
    // (consumer, sends) both run in the BTstack run loop on the same 0xFF
    // background IRQ context, and BTstack does not re-enter its own run
    // loop -- see this file's module doc for the IRQ-context contract.
    // tx_head/tx_tail/tx_count are therefore PLAIN, NON-VOLATILE fields,
    // not cross-context shared state like pcm_ring.h's ring (which IS
    // genuinely producer/consumer across contexts and needs its
    // discipline). Do NOT add atomics, memory barriers, or volatile here
    // -- the visual similarity to pcm_ring invites "hardening" that would
    // be pure cost with no correctness benefit, because there is no second
    // context to race against.
    pl_a2dp_slot_t tx[PL_A2DP_TX_QUEUE_SLOTS];
    uint8_t tx_head;
    uint8_t tx_tail;
    uint8_t tx_count;
    bool send_requested;

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
    // seal because every slot is full (tx_count == PL_A2DP_TX_QUEUE_SLOTS)
    // -- the only remaining loop-stop condition besides dwell/credit/
    // ring-empty. Predicted 0 for SBC (acceptance item 1); nonzero means
    // the SEND side, not fill, is the limiter -- read grants/s.
    volatile uint32_t stop_queue_full;

    // Bead pico-link-85v (D7): payloads_sealed's rate vs pkt_sent's rate is
    // the single most important stop/send-side split -- divergence means
    // the send side, not fill, is the limiter. Incremented once per SEAL
    // (a full slot handed off to the tx ring), replacing stop_packet_full's
    // old (and, post-D1, incorrect) role of standing in for this event.
    volatile uint32_t payloads_sealed;
    // High-water of tx_count, reset at STREAM_STARTED (pico-link-r44
    // lesson: a high-water mark that never resets poisons later
    // derivations). tx_depth_max == PL_A2DP_TX_QUEUE_SLOTS - 1 means D2's
    // depth is marginal.
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
//
// Bead pico-link-cz0.5.6: the reserved header size is now the ACTIVE
// codec row's own header_bytes (codec_table.h), not a hardcoded 1 --
// SBC's row sets header_bytes=1, reproducing today's behaviour exactly;
// a future codec with a different header shape changes only its own row.
static inline uint32_t pl_a2dp_usable_payload(int max_media_payload_size, uint8_t header_bytes) {
    return max_media_payload_size > (int)header_bytes ? (uint32_t)(max_media_payload_size - (int)header_bytes) : 0u;
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
// arm a send. Caller is responsible for the tx_count < PL_A2DP_TX_QUEUE_SLOTS
// guard (stop_queue_full) BEFORE calling -- this function does not check it.
static void pl_a2dp_seal_head(void) {
    pl_a2dp_slot_t *head = &s_ctx.tx[s_ctx.tx_head];
    head->rtp_ts = s_ctx.rtp_next;
    s_ctx.rtp_next += (uint32_t)head->frames * s_ctx.frame.pcm_frames_per_encoded_frame;
    s_ctx.tx_head = (uint8_t)((s_ctx.tx_head + 1) % PL_A2DP_TX_QUEUE_SLOTS);
    s_ctx.tx_count++;
    if (s_ctx.tx_count > s_ctx.tx_depth_max) {
        s_ctx.tx_depth_max = s_ctx.tx_count;
    }
    s_ctx.payloads_sealed++;
    if (!s_ctx.send_requested) {
        s_ctx.send_requested = true;
        a2dp_source_stream_endpoint_request_can_send_now(s_ctx.a2dp_cid, s_ctx.local_seid);
    }
}

static void pl_a2dp_tx_flush(void) {
    for (uint8_t i = 0; i < PL_A2DP_TX_QUEUE_SLOTS; i++) {
        s_ctx.tx[i].len = 0;
        s_ctx.tx[i].frames = 0;
    }
    s_ctx.tx_head = 0;
    s_ctx.tx_tail = 0;
    s_ctx.tx_count = 0;
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

    // Bead pico-link-85v (D4): the dwell budget, derived fresh every call.
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

    uint8_t frames_this_tick = 0;
    bool starved = false;
    uint32_t dwell_us = 0;
    for (;;) {
        // 0. Dwell safety: has this tick's fill loop already consumed its
        // (freshly derived) dwell budget? The one true safety-trip stop
        // reason -- see this function's doc comment.
        if (dwell_us >= dwell_budget_us) {
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

        // 1b. Queue-full, checked BEFORE touching the head slot.
        //
        // This looks redundant with the queue-full check inside the
        // payload-full branch below, and today it IS: when tx_count ==
        // SLOTS the head slot aliases tx_tail's sealed-unsent payload,
        // but every seal happens BECAUSE the slot could not take another
        // frame, so that stale head always re-trips payload-full and
        // breaks there before anything is written into it.
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
        if (s_ctx.tx_count >= PL_A2DP_TX_QUEUE_SLOTS) {
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
            if (s_ctx.tx_count >= PL_A2DP_TX_QUEUE_SLOTS) {
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
            if (s_ctx.tx_count >= PL_A2DP_TX_QUEUE_SLOTS) {
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
static void pl_a2dp_send_media_packet(void) {
    s_ctx.grants++;

    if (s_ctx.tx_count == 0) {
        s_ctx.spurious_grants++;
        s_ctx.send_requested = false;
        return;
    }

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
    s_ctx.tx_tail = (uint8_t)((s_ctx.tx_tail + 1) % PL_A2DP_TX_QUEUE_SLOTS);
    s_ctx.tx_count--;

    if (s_ctx.tx_count > 0) {
        a2dp_source_stream_endpoint_request_can_send_now(s_ctx.a2dp_cid, s_ctx.local_seid);
    } else {
        s_ctx.send_requested = false;
    }
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
        // Bead pico-link-85v (D4): stash for pl_a2dp_fill's duty-bound
        // derivation -- same elapsed_us, no new time_us_64() call.
        s_ctx.last_tick_elapsed_us = elapsed_us;
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

    // Bead pico-link-85v (D1): the old "only fill if not already waiting
    // on a grant" gate is GONE -- that was the actual ceiling mechanism
    // (a tick with sbc_ready_to_send still true did no filling at all).
    // pl_a2dp_fill now self-manages sealing and re-arming
    // request_can_send_now as slots fill, so it is simply called every
    // tick regardless of any pending grant.
    pl_a2dp_fill();

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
            // Fresh discovery pass starting -- clear any capability bits
            // left over from a previous connection attempt.
            memset(&s_ctx.discovered, 0, sizeof(s_ctx.discovered));
            pl_log("a2dp: signaling connected, cid=0x%02x\r\n", cid);
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
                pl_bt_push_connect_failed(s_ctx.connect_addr, PL_FAILURE_REASON_REJECTED);
                break;
            }
            s_ctx.local_seid = a2dp_subevent_stream_established_get_local_seid(packet);
            int mtu = a2dp_max_media_payload_size(s_ctx.a2dp_cid, s_ctx.local_seid);
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
                if (required_slots > PL_A2DP_TX_QUEUE_SLOTS) {
                    pl_log(
                        "a2dp: WARNING tx queue depth %u required but only %u compiled in "
                        "(b_tick=%lu frames_per_packet=%lu worst_case_encode_us=%lu)\r\n",
                        (unsigned)required_slots, (unsigned)PL_A2DP_TX_QUEUE_SLOTS, (unsigned long)b_tick,
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
            // Bead pico-link-cz0.5.6: computed for real now that a second
            // row exists (design sec 4.3) -- degraded means the negotiated
            // codec (s_ctx.codec) was NOT PL_CODECS' own most-preferred
            // row, i.e. some higher-preference row (LDAC) was tried first
            // and refused/fell through, so the connection settled for a
            // lower one (SBC). PL_CODECS[0] is that most-preferred row by
            // construction (codec_table.c's own array-order-is-preference
            // rule); s_ctx.codec is always non-NULL here (STREAM_STARTED
            // cannot be reached without a prior successful codec init).
            pl_bt_push_connect_succeeded(s_ctx.connect_addr, PL_CODEC_COUNT > 0 && s_ctx.codec != PL_CODECS[0]);
            break;

        case A2DP_SUBEVENT_STREAM_SUSPENDED:
            pl_wdt_set_enabled(PL_WDT_MEDIA, false);
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
            pl_log("a2dp: signaling connection released\r\n");
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

void pl_a2dp_disconnect(void) {
    if (s_ctx.a2dp_cid == 0) {
        pl_log("a2dp: disconnect requested, no active a2dp_cid, no-op\r\n");
        return;
    }
    uint8_t status = a2dp_source_disconnect(s_ctx.a2dp_cid);
    pl_log("a2dp: disconnect requested, a2dp_cid=0x%04x status=0x%02x\r\n", s_ctx.a2dp_cid, status);
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
    pl_log(
        "a2dp: frames_per_packet=%lu credit_clamped_samples=%lu credit_clamp_events=%lu flush_frames=%lu "
        "resync_drops=%lu fill_ema=%ld fill_min=%lu\r\n",
        (unsigned long)s_ctx.frames_per_packet, (unsigned long)s_ctx.credit_clamped_samples,
        (unsigned long)s_ctx.credit_clamp_events, (unsigned long)s_ctx.flush_frames, (unsigned long)s_ctx.resync_drops,
        (long)pl_usb_audio_fb_fill_ema(), (unsigned long)pl_usb_audio_fill_min()
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
