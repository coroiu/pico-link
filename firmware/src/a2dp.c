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
// AND a bounded per-tick dwell cap (frames_per_tick_cap, computed at
// STREAM_ESTABLISHED from the negotiated payload size and the codec's
// worst_case_encode_us -- codec_table.h) so a real backlog can never make
// one IRQ dwell arbitrarily long. Before pico-link-pbv this file had ONLY
// the second half (a fixed PL_A2DP_MAX_FRAMES_PER_TICK=5) and NO clock at
// all -- pl_a2dp_fill_sbc_buffer was a pure data-driven drain, exactly
// what design sec 1 (lines 44-48) names and forbids, and it only looked
// stable because the accidental rate limiter (cap 5 x the negotiated
// payload's frame-boundary interaction x the real ~91Hz tick rate = ~319
// frames/s) happened to sit below the 375 frames/s real-time demand.
// Raising the cap alone (to e.g. 7) would have removed that accidental
// limiter without replacing it with a real one -- the ring would drain
// faster than the host supplies, empty out, and look "fixed" for about
// ten seconds before failing the other direction. See bead pico-link-pbv's
// comment history for the measurement that found this and Ada's design
// for the fix.
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

// Matches a2dp_source_demo.c's SBC_STORAGE_SIZE -- generous headroom over
// any real AVDTP media MTU (~650-1013B typical), -1 reserved for the SBC
// media payload header byte (num_frames), see pl_a2dp_send_media_packet.
#define SBC_STORAGE_SIZE 1030

// design sec 1: our own crystal, via btstack_run_loop timers, paces the
// A2DP media stream -- matches a2dp_source_demo.c's own AUDIO_TIMEOUT_MS.
#define PL_A2DP_AUDIO_TIMEOUT_MS 10

// Bead pico-link-pbv: PL_A2DP_MAX_FRAMES_PER_TICK (a fixed constant, 5) is
// GONE -- replaced by s_ctx.frames_per_tick_cap, computed once per stream
// at STREAM_ESTABLISHED from the negotiated payload size and the codec's
// worst_case_encode_us (codec_table.h), bounding the same "one IRQ dwell
// must stay bounded" property (design sec 3.5/5) but now correctly, in
// terms of what was actually negotiated rather than a guessed constant.
// This is a SAFETY bound on top of, not instead of, the real clock
// (samples_owed credit-pacing below) -- see the module doc's sec 11.2
// rewrite for why both are required together.
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

    // Computed once per stream at STREAM_ESTABLISHED (replaces the deleted
    // PL_A2DP_MAX_FRAMES_PER_TICK constant) -- see PL_A2DP_MAX_ENCODE_DWELL_US's
    // doc comment above. This is a SAFETY bound on top of the credit clock,
    // not a substitute for it.
    uint32_t frames_per_tick_cap;
    // Falsifier counter (pico-link-pbv acceptance): nonzero in steady
    // state would mean the dwell-safety cap, not the credit clock, is what
    // is actually limiting throughput -- i.e. the fix did not really take.
    volatile uint32_t ticks_cap_bound;

    // Bead pico-link-pbv (C5): cumulative frames dropped by
    // pl_pcm_trim_to() at the PRIMING->STREAMING transition -- see that
    // call site. Acceptance requires this stay 0 in a healthy run; nonzero
    // means priming is regularly overshooting the target fill by more than
    // a trivial amount.
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

// Fills s_ctx.sbc_storage from the PCM ring under CREDIT PACING (bead
// pico-link-pbv fix -- see the module doc's sec 11.2 rewrite and
// s_ctx.samples_owed's doc comment for why this replaced a pure
// data-driven drain). IRQ context (0xFF) -- see this file's module doc.
// No allocation (s_pcm_scratch is static), no logging, no blocking;
// codec->encode() carries the same contract (codec_table.h).
//
// The three stop conditions are checked IN THIS ORDER -- credit, then
// packet-full, then ring-empty -- because the order is what makes
// `starved` (and therefore underrun_events/silent_ticks) mean anything.
// Under credit pacing, frames_this_tick == 0 ROUTINELY means "the clock
// owed less than one whole frame this tick" (normal -- most ticks fire
// faster than one SBC frame's worth of real time, ~2.67ms at 48kHz/128),
// NOT "the ring is empty". Only the ring-empty check, reached with the
// clock AND packet room both still willing, is a real starvation signal
// -- conflating the two would fire underrun_events (and eventually
// a2dp_source_pause_stream) on ordinary ticks, mid-music.
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

    uint8_t frames_this_tick = 0;
    bool starved = false;
    while (frames_this_tick < s_ctx.frames_per_tick_cap) {
        // 1. Credit: has the real-time clock actually owed us a whole
        // encoded frame's worth of samples yet? Most-common stop reason
        // by far in steady state -- not a fault.
        if (s_ctx.samples_owed < pcm_frame_count) {
            break;
        }
        // 2. Packet-full: is there room for one more frame in the current
        // AVDTP payload? Also not a fault -- just means it's time to send.
        if ((uint32_t)(s_ctx.sbc_storage_count + frame_bytes) > (uint32_t)s_ctx.max_media_payload_size) {
            break;
        }
        // 3. Ring-empty: the clock says we should encode and there is
        // room to, but the ring genuinely has nothing to give us. This is
        // the ONE true starvation signal.
        if (pl_pcm_fill_bytes() < pcm_bytes_needed) {
            starved = true;
            break;
        }

        uint32_t got = pl_pcm_read((uint8_t *)s_pcm_scratch, pcm_bytes_needed);
        if (got != pcm_bytes_needed) {
            break; // ring gave less than its own fill_bytes() promised -- shouldn't happen, defend anyway
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

    // Falsifier counter: were we still going when the dwell-safety cap
    // itself stopped us (as opposed to credit/packet/ring)? Nonzero in
    // steady state means the cap, not the clock, is the real limiter --
    // see PL_A2DP_MAX_ENCODE_DWELL_US's doc comment.
    if (frames_this_tick == s_ctx.frames_per_tick_cap) {
        s_ctx.ticks_cap_bound++;
    }

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
        s_ctx.samples_owed += (uint32_t)(owed_us_hz / 1000000u);
        s_ctx.samples_owed_rem_us = (uint32_t)(owed_us_hz % 1000000u);
    }
    s_ctx.last_tick_us = pbv_now_us;
    s_ctx.tick_count++;

    if (s_ctx.state == PL_A2DP_MEDIA_PRIMING) {
        if (pl_pcm_fill_bytes() >= PL_PCM_TARGET_FILL_BYTES) {
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
        if ((uint32_t)(s_ctx.sbc_storage_count + s_ctx.frame.encoded_frame_bytes) > (uint32_t)s_ctx.max_media_payload_size) {
            s_ctx.sbc_ready_to_send = true;
            a2dp_source_stream_endpoint_request_can_send_now(s_ctx.a2dp_cid, s_ctx.local_seid);
        }
    } else {
        s_ctx.ticks_send_pending++;
    }

    // design sec 3.5 case 2: host silent -> not a fault. Auto-pause once,
    // wait for SUSPENDED, then re-prime (see the SUSPENDED case below).
    if (s_ctx.silent_ticks >= PL_A2DP_HOST_SILENT_TICKS && !s_ctx.pause_requested) {
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

            // Bead pico-link-pbv (C3): frames_per_tick_cap, replacing the
            // deleted PL_A2DP_MAX_FRAMES_PER_TICK constant -- computed
            // from what was ACTUALLY negotiated (this connection's real
            // payload size and the codec row's measured worst-case encode
            // time) rather than a guessed-at fixed number. Two
            // independent ceilings, take the tighter: frames_per_packet
            // (how many frames fit in one AVDTP payload -- no reason to
            // ever fill more than that in one tick) and dwell_cap (how
            // many encode() calls fit inside the IRQ-dwell safety budget,
            // PL_A2DP_MAX_ENCODE_DWELL_US, at this codec's
            // worst_case_encode_us). MAX(1, ...) so a degenerate
            // negotiation (a tiny MTU) can never produce a cap of 0 and
            // stall the drain entirely.
            uint32_t frames_per_packet = s_ctx.frame.encoded_frame_bytes > 0
                                              ? (uint32_t)s_ctx.max_media_payload_size / s_ctx.frame.encoded_frame_bytes
                                              : 1u;
            uint32_t dwell_cap = s_ctx.frame.worst_case_encode_us > 0
                                      ? PL_A2DP_MAX_ENCODE_DWELL_US / s_ctx.frame.worst_case_encode_us
                                      : frames_per_packet;
            s_ctx.frames_per_tick_cap = btstack_max(1u, btstack_min(frames_per_packet, dwell_cap));
            // Signalling context, not the media hot path -- pl_log is
            // fine here (see this file's module doc). Startup-only line,
            // not rate-limited: fires once per stream, same as the
            // "stream established"/"codec=" lines already here.
            pl_log(
                "a2dp: frames_per_tick_cap=%lu (frames_per_packet=%lu dwell_cap=%lu worst_case_encode_us=%lu)\r\n",
                (unsigned long)s_ctx.frames_per_tick_cap, (unsigned long)frames_per_packet, (unsigned long)dwell_cap,
                (unsigned long)s_ctx.frame.worst_case_encode_us
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
            pl_pcm_reset();

            pl_log("a2dp: stream established, max_media_payload_size=%d\r\n", s_ctx.max_media_payload_size);
            // design sec 3.5 case 3: prime before starting -- see this
            // file's PL_A2DP_MEDIA_PRIMING doc comment.
            s_ctx.state = PL_A2DP_MEDIA_PRIMING;
            pl_a2dp_media_timer_arm();
            break;
        }

        case A2DP_SUBEVENT_STREAM_STARTED:
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
            // Bead pico-link-pbv (C5): PRIMING's own wait condition is
            // "fill >= target", checked once per ~10ms tick -- a few more
            // USB packets can land between that check passing and this
            // event actually arriving, so fill can be slightly ABOVE
            // target here. Trim the overshoot down to exactly target so
            // streaming starts at the intended ~24ms latency, not
            // whatever it happened to overshoot to. Counted, not silent --
            // acceptance requires this stay 0 in a healthy run (see
            // resync_drops's doc comment on pl_a2dp_ctx_t).
            s_ctx.resync_drops += pl_pcm_trim_to(PL_PCM_TARGET_FILL_BYTES);
            pl_log("a2dp: stream started\r\n");
            pl_bt_push_link_state_connected();
            // S1's table has exactly one row -- SBC is never "not the
            // first row we'd have accepted", so degraded is always false
            // here (design sec 4.3). S4 computes this for real once a
            // second row exists.
            pl_bt_push_connect_succeeded(false);
            break;

        case A2DP_SUBEVENT_STREAM_SUSPENDED:
            pl_log("a2dp: stream suspended (auto_resume=%d)\r\n", s_ctx.auto_resume ? 1 : 0);
            s_ctx.sbc_storage_count = 0;
            s_ctx.sbc_ready_to_send = false;
            s_ctx.silent_ticks = 0;
            s_ctx.pause_requested = false;
            pl_pcm_reset();
            s_ctx.state = s_ctx.auto_resume ? PL_A2DP_MEDIA_PRIMING : PL_A2DP_MEDIA_IDLE;
            s_ctx.auto_resume = false;
            break;

        case A2DP_SUBEVENT_STREAM_RELEASED:
            pl_log("a2dp: stream released\r\n");
            s_ctx.state = PL_A2DP_MEDIA_IDLE;
            s_ctx.codec = NULL;
            if (s_ctx.timer_armed) {
                btstack_run_loop_remove_timer(&s_ctx.media_timer);
                s_ctx.timer_armed = false;
            }
            pl_pcm_reset();
            break;

        case A2DP_SUBEVENT_SIGNALING_CONNECTION_RELEASED:
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

void pl_a2dp_report(void) {
    static uint64_t s_last_report_us = 0;
    uint64_t now_us = time_us_64();
    if (s_last_report_us != 0 && now_us - s_last_report_us < 1000000) {
        return;
    }
    s_last_report_us = now_us;

    const char *codec_name = s_ctx.codec != NULL ? s_ctx.codec->display_name : "none";
    pl_log(
        "a2dp: codec=%s bitrate=%lu fill=%lu/%lu ovr_frames=%lu und=%lu\r\n", codec_name,
        (unsigned long)s_ctx.frame.nominal_bitrate_bps, (unsigned long)pl_pcm_fill_bytes(),
        (unsigned long)PL_PCM_TARGET_FILL_BYTES, (unsigned long)pl_pcm_overrun_frames(),
        (unsigned long)s_ctx.underrun_events
    );
    pl_log(
        "a2dp: enc_max_us=%lu pkt_sent=%lu pkt_fail=%lu misaligned=%lu\r\n", (unsigned long)s_ctx.enc_max_us,
        (unsigned long)s_ctx.pkt_sent, (unsigned long)s_ctx.pkt_fail, (unsigned long)pl_pcm_misaligned()
    );
    // Cumulative, never reset -- compute deltas between two consecutive
    // report lines (~1s apart) to get real tick rate (tick_count delta)
    // and enc_frames RATE (enc_frames_total delta, the pbv acceptance
    // criterion: must read 375 +/- 2 per second in steady state).
    // worst_tick_interval_us is the worst single interval seen since
    // streaming started (never reset, so a spike stays visible even if
    // the average recovers) -- also falsifier #1 (pico-link-pbv): must
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
    // Bead pico-link-pbv fix instrumentation. fill_ema/fill_min come from
    // usb_audio.c's feedback task (its own ~1ms sampling of
    // pl_pcm_fill_bytes(), far finer-grained than this file's ~11ms tick)
    // -- see design sec 2.1 for why the EMA, not raw fill, is the correct
    // thing to evaluate the pass criterion against. Acceptance: fill_ema
    // in 3456..5760, fill_min >= 1024, ovr_frames 0 and flat over >= 5
    // minutes, resync_drops == 0. ticks_cap_bound nonzero in steady state
    // is a falsifier (the dwell-safety cap, not the credit clock, would be
    // the real limiter -- see frames_per_tick_cap's doc comment).
    pl_log(
        "a2dp: frames_per_tick_cap=%lu ticks_cap_bound=%lu resync_drops=%lu fill_ema=%ld fill_min=%lu\r\n",
        (unsigned long)s_ctx.frames_per_tick_cap, (unsigned long)s_ctx.ticks_cap_bound,
        (unsigned long)s_ctx.resync_drops, (long)pl_usb_audio_fb_fill_ema(), (unsigned long)pl_usb_audio_fill_min()
    );
}
