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
// would taint this whole binary, including core/ui-ffi). The ONE
// substantive divergence (sec 11.2): a2dp_demo_audio_timeout_handler's
// samples_ready accounting (elapsed_time * sample_rate, feeding a
// synthetic sine/mod source) is REPLACED, not copied -- our audio comes
// from pcm_ring.h's ring (fed by usb_audio.c's real USB PCM), and encode
// work is capped at PL_A2DP_MAX_FRAMES_PER_TICK per timer tick rather than
// ever trying to "catch up" on an unbounded backlog (design sec 3.5).
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
#include "usb_pump.h"

// Matches a2dp_source_demo.c's SBC_STORAGE_SIZE -- generous headroom over
// any real AVDTP media MTU (~650-1013B typical), -1 reserved for the SBC
// media payload header byte (num_frames), see pl_a2dp_send_media_packet.
#define SBC_STORAGE_SIZE 1030

// design sec 1: our own crystal, via btstack_run_loop timers, paces the
// A2DP media stream -- matches a2dp_source_demo.c's own AUDIO_TIMEOUT_MS.
#define PL_A2DP_AUDIO_TIMEOUT_MS 10

// design sec 3.5 / sec 5: cap encode work per tick at ~one media packet's
// worth. Never "catch up" unboundedly.
#define PL_A2DP_MAX_FRAMES_PER_TICK 5

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

    // --- bead pico-link-pbv instrumentation: is the media timer firing at
    // its intended ~10ms cadence, and is the fill loop actually draining
    // PL_A2DP_MAX_FRAMES_PER_TICK frames per tick? Counters only, updated
    // from IRQ context (same producer as the block above), read from
    // pl_a2dp_report (thread context) -- never pl_log from the hot path
    // itself. worst_tick_interval_us/tick_count answer "how fast is the
    // timer really firing"; frames_filled_total (divided by tick_count
    // between two report samples) answers "how many frames/tick are we
    // actually filling" against the PL_A2DP_MAX_FRAMES_PER_TICK=5 cap.
    volatile uint64_t last_tick_us;
    volatile uint32_t worst_tick_interval_us;
    volatile uint32_t tick_count;
    volatile uint32_t frames_filled_total;
    // Tests a specific hypothesis: does the fill loop stall waiting on
    // BTstack's async A2DP_SUBEVENT_STREAMING_CAN_SEND_MEDIA_PACKET_NOW
    // grant (the sbc_ready_to_send handoff below) for a meaningful
    // fraction of ticks? Counter only.
    volatile uint32_t ticks_send_pending;
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

// Fills s_ctx.sbc_storage from the PCM ring, up to PL_A2DP_MAX_FRAMES_PER_TICK
// encoded frames or until the storage buffer can't hold another whole
// frame within max_media_payload_size, whichever comes first -- mirrors
// a2dp_demo_fill_sbc_audio_buffer's two stop conditions exactly, with
// "samples_ready >= needed" replaced by "the ring actually has that much
// PCM buffered" (the sec 11.2 divergence). IRQ context (0xFF) -- see this
// file's module doc. No allocation (s_pcm_scratch is static), no logging,
// no blocking; codec->encode() carries the same contract (codec_table.h).
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
    while (frames_this_tick < PL_A2DP_MAX_FRAMES_PER_TICK && pl_pcm_fill_bytes() >= pcm_bytes_needed &&
           (uint32_t)(s_ctx.sbc_storage_count + frame_bytes) <= (uint32_t)s_ctx.max_media_payload_size) {
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
        frames_this_tick++;
    }

    // Bead pico-link-pbv: cumulative frames actually filled, for computing
    // an average frames/tick against PL_A2DP_MAX_FRAMES_PER_TICK's cap of
    // 5 once divided by the tick_count delta between two report samples
    // (pl_a2dp_report, thread context). Counter only.
    s_ctx.frames_filled_total += frames_this_tick;

    if (frames_this_tick == 0) {
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
    // handler, not the nominal PL_A2DP_AUDIO_TIMEOUT_MS. Counter only --
    // no pl_log, matches usb_pump.c's own worst_interval_us pattern for
    // exactly this class of measurement.
    uint64_t pbv_now_us = time_us_64();
    if (s_ctx.last_tick_us != 0) {
        uint32_t pbv_interval_us = (uint32_t)(pbv_now_us - s_ctx.last_tick_us);
        if (pbv_interval_us > s_ctx.worst_tick_interval_us) {
            s_ctx.worst_tick_interval_us = pbv_interval_us;
        }
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
    // Bead pico-link-pbv: cumulative, never reset -- compute deltas
    // between two consecutive report lines (1s apart) to get real tick
    // rate (tick_count delta) and average frames/tick (frames_filled_total
    // delta / tick_count delta) against the PL_A2DP_MAX_FRAMES_PER_TICK=5
    // cap. worst_tick_interval_us is the worst single interval seen since
    // streaming started (never reset, so it only ever grows -- a spike
    // stays visible even if the average recovers).
    pl_log(
        "a2dp: tick_count=%lu worst_tick_interval_us=%lu frames_filled_total=%lu ticks_send_pending=%lu\r\n",
        (unsigned long)s_ctx.tick_count, (unsigned long)s_ctx.worst_tick_interval_us,
        (unsigned long)s_ctx.frames_filled_total, (unsigned long)s_ctx.ticks_send_pending
    );
}
