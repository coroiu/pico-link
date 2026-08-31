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

    int status = ldacBT_init_handle_encode(
        enc->handle, PL_LDAC_INIT_MTU, LDACBT_EQMID_HQ, (int)cfg.channel_mode, LDACBT_SMPL_FMT_S16, 48000
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
    } else {
        out_frame->nominal_bitrate_bps = (uint32_t)kbps * 1000u;
        pl_log("ldac: nominal bitrate %d kbps from ldacBT_get_bitrate\r\n", kbps);
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
    .state = &s_ldac_encoder,
};
