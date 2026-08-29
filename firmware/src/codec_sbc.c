// Pico Link firmware -- the SBC codec table row implementation (M4 S1,
// bead pico-link-cz0.5.2). See codec_sbc.h's module doc.
//
// The encoder itself is BTstack's own bluedroid-derived SBC encoder
// (pico_btstack_sbc_encoder, linked in CMakeLists.txt -- ships in the
// pico-sdk, no new fetch/submodule per design sec 11.6). This file is just
// the pl_codec_t vtable glue around it -- init()/encode()/deinit() shaped
// exactly like a2dp_source_demo.c's own configure/encode call sites (read
// directly from the pico-sdk's vendored btstack submodule), reworked to
// hang off codec_table.h's vtable instead of file-scope globals.
#include "codec_sbc.h"

#include <string.h>

#include "classic/avdtp.h"
#include "classic/btstack_sbc.h"
#include "classic/btstack_sbc_bluedroid.h"

// AVDTP media codec capability bytes we advertise for SBC -- identical in
// shape to BlueKitchen's own a2dp_source_demo.c's
// media_sbc_codec_capabilities (both sample rates, all block
// lengths/subbands/allocation methods, bitpool range 2-53). Not a
// transcription of USBPods; read from the pico-sdk's vendored BTstack
// example only.
static uint8_t s_sbc_capabilities[] = {
    (AVDTP_SBC_44100 << 4) | (AVDTP_SBC_48000 << 4) | AVDTP_SBC_STEREO,
    0xFF, // all block lengths / subbands / allocation methods
    2,
    53, // bitpool range
};

// a2dp_source_create_stream_endpoint's scratch "default configuration"
// buffer -- BTstack owns writes into this; this project never reads it
// back (the real negotiated config arrives via the SBC subevent's own
// accessors, decoded by a2dp.c into pl_codec_sbc_negotiated_t -- see
// codec_sbc.h's doc comment on why that's a different buffer).
static uint8_t s_sbc_configuration[4];

// The encoder instance -- statically allocated, never malloc'd (design
// sec 5). One instance is enough: S1 has exactly one paired sink and no
// concurrent streams.
typedef struct {
    const btstack_sbc_encoder_t *instance;
    btstack_sbc_encoder_bluedroid_t state;
} pl_sbc_encoder_t;

static pl_sbc_encoder_t s_sbc_encoder;

static bool pl_codec_sbc_init(
    void *state, const uint8_t *configuration, uint8_t configuration_len, pl_codec_format_t *out_format,
    pl_codec_frame_info_t *out_frame
) {
    pl_sbc_encoder_t *enc = (pl_sbc_encoder_t *)state;
    if (configuration_len != sizeof(pl_codec_sbc_negotiated_t)) {
        return false;
    }
    pl_codec_sbc_negotiated_t cfg;
    memcpy(&cfg, configuration, sizeof(cfg));

    enc->instance = btstack_sbc_encoder_bluedroid_init_instance(&enc->state);
    enc->instance->configure(
        &enc->state, SBC_MODE_STANDARD, (uint8_t)cfg.block_length, (uint8_t)cfg.subbands,
        (btstack_sbc_allocation_method_t)cfg.allocation_method, (uint16_t)cfg.sampling_frequency,
        (uint8_t)cfg.max_bitpool_value, (btstack_sbc_channel_mode_t)cfg.channel_mode
    );

    out_format->sample_rate_hz = (uint32_t)cfg.sampling_frequency;
    out_format->channels = 2;
    out_format->bits_per_sample = 16;

    uint16_t pcm_frames = (uint16_t)enc->instance->num_audio_frames(&enc->state);
    uint16_t frame_bytes = enc->instance->sbc_buffer_length(&enc->state);
    out_frame->pcm_frames_per_encoded_frame = pcm_frames;
    out_frame->encoded_frame_bytes = frame_bytes;
    // Nominal bitrate: frame_bytes*8 bits per pcm_frames samples, scaled to
    // the negotiated sample rate. E.g. ~119B/128 samples @ 48kHz -> ~357kbps.
    out_frame->nominal_bitrate_bps =
        pcm_frames > 0 ? (uint32_t)(((uint64_t)frame_bytes * 8u * (uint64_t)cfg.sampling_frequency) / pcm_frames) : 0;

    return true;
}

static uint16_t pl_codec_sbc_encode(void *state, const int16_t *pcm, uint8_t *out, uint16_t out_cap) {
    pl_sbc_encoder_t *enc = (pl_sbc_encoder_t *)state;
    uint16_t frame_bytes = enc->instance->sbc_buffer_length(&enc->state);
    if (frame_bytes == 0 || frame_bytes > out_cap) {
        return 0;
    }
    // encode_signed_16's return is a bluedroid status code, not a byte
    // count (matches a2dp_source_demo.c -- it discards the return value
    // too and just trusts sbc_buffer_length()). No allocation, no
    // logging, no blocking -- design sec 5's IRQ-context contract.
    enc->instance->encode_signed_16(&enc->state, pcm, out);
    return frame_bytes;
}

static void pl_codec_sbc_deinit(void *state) {
    (void)state;
    // btstack_sbc_encoder_bluedroid has no explicit teardown -- its state
    // lives in the static pl_sbc_encoder_t above and is simply
    // reconfigured on the next init() (a fresh connect/reconnect cycle).
}

pl_codec_t pl_codec_sbc = {
    .display_name = "SBC",
    .avdtp_codec_type = AVDTP_CODEC_SBC,
    .vendor_id = 0,
    .vendor_codec_id = 0,

    .preference = 0, // only row in S1's table; S4 gives LDAC a lower number
    .capabilities = s_sbc_capabilities,
    .capabilities_len = sizeof(s_sbc_capabilities),
    .configuration = s_sbc_configuration,
    .configuration_len = sizeof(s_sbc_configuration),

    .local_seid = 0, // filled in by codec_table.c's pl_codec_table_init()

    .init = pl_codec_sbc_init,
    .encode = pl_codec_sbc_encode,
    .deinit = pl_codec_sbc_deinit,
    .state = &s_sbc_encoder,
};
