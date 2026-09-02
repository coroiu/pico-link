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

// Bead pico-link-19c: BOTH the encoded frame byte length and the nominal
// bitrate depend on `sbc_buffer_length()`, which returns bluedroid's
// `u16PacketLength` -- and that field has exactly one assignment site in
// the whole vendored tree, inside the packing step of a SUCCESSFUL encode
// (pico-sdk lib/btstack/3rd-party/bluedroid/encoder/srce/sbc_packing.c:237).
// `SBC_Encoder_Init()`/`configure()` never touches it. So reading it right
// after `configure()`, before any encode has ever run, is a chicken-and-egg
// deadlock: the field is zero-initialized static state, and the ONLY way
// to make it correct is to actually run one encode first.
//
// No BTstack/bluedroid API exposes the real encoded byte length before
// that first encode -- confirmed by reading lib/btstack/3rd-party/
// bluedroid/encoder/srce/sbc_encoder.c's SBC_Encoder_Init(), which has the
// A2DP-spec frame-length formula only as COMMENTED-OUT, unexercised dead
// code (and even that dead code has no branch at all for MONO/DUAL_CHANNEL
// mode -- an incomplete reference, not a usable one). Reviving that by
// hand would be a second, never-tested implementation of the same math,
// exactly the kind of risk this fix exists to remove, not add.
//
// a2dp_source_demo.c's own fill loop (a2dp_source_demo.c:432-452) never
// hits this: it doesn't query `sbc_buffer_length()` until AFTER its first
// real `encode_signed_16()` call, which is the same real audio callers get
// later -- it just never NEEDS the answer until then. This function does
// need the answer immediately (a2dp.c logs frame_bytes/nominal_bitrate at
// negotiation time, before any audio flows), so it primes the encoder with
// one throwaway silent-PCM encode right here -- running the exact same
// `encode_signed_16`/packing path every real encode will use, discarding
// only the OUTPUT BYTES, not the mechanism. `s_priming_pcm` is sized for
// the largest possible `num_audio_frames()` this project's capabilities
// (codec_sbc.c's `s_sbc_capabilities`) can ever negotiate (8 subbands * 16
// blocks = 128 PCM frames * 2 channels); `s_priming_out` is sized well
// above the ~119-byte real-world frame size this same file's nominal-
// bitrate comment already documented, comfortably inside bluedroid's own
// 1000-byte internal packet buffer.
#define PL_SBC_MAX_PRIMING_SAMPLES (8 * 16 * 2)
#define PL_SBC_MAX_FRAME_BYTES 200

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
    out_frame->pcm_frames_per_encoded_frame = pcm_frames;

    // Priming encode -- see the doc comment above. Negotiation time only
    // (not the IRQ-context audio hot path), so a static scratch buffer and
    // a synchronous call here are fine.
    static int16_t s_priming_pcm[PL_SBC_MAX_PRIMING_SAMPLES];
    static uint8_t s_priming_out[PL_SBC_MAX_FRAME_BYTES];
    uint16_t priming_samples = (uint16_t)(pcm_frames * 2u /* channels */);
    if (priming_samples > 0 && priming_samples <= PL_SBC_MAX_PRIMING_SAMPLES) {
        memset(s_priming_pcm, 0, (size_t)priming_samples * sizeof(int16_t));
        enc->instance->encode_signed_16(&enc->state, s_priming_pcm, s_priming_out);
    }

    uint16_t frame_bytes = enc->instance->sbc_buffer_length(&enc->state);
    out_frame->encoded_frame_bytes = frame_bytes;
    // Bead pico-link-cz0.5.6: SBC's AVDTP media payload uses the classic
    // 1-byte fragmentation/start/last/num_frames header (unchanged from
    // what a2dp.c hardcoded before this bead -- see codec_table.h's
    // header_bytes doc comment).
    out_frame->header_bytes = 1;
    // Nominal bitrate: frame_bytes*8 bits per pcm_frames samples, scaled to
    // the negotiated sample rate. E.g. ~119B/128 samples @ 48kHz -> ~357kbps.
    out_frame->nominal_bitrate_bps =
        pcm_frames > 0 ? (uint32_t)(((uint64_t)frame_bytes * 8u * (uint64_t)cfg.sampling_frequency) / pcm_frames) : 0;
    // Bead pico-link-pbv: measured max real encode_signed_16 call on real
    // hardware across several runs was 574-582us (a2dp.c's enc_max_us,
    // pico-link-19c/asj/pbv sessions); 800 is that plus margin, not a
    // theoretical derivation. See pl_codec_frame_info_t's doc comment.
    out_frame->worst_case_encode_us = 800;

    return true;
}

static pl_codec_encode_result_t pl_codec_sbc_encode(void *state, const int16_t *pcm, uint8_t *out, uint16_t out_cap) {
    pl_sbc_encoder_t *enc = (pl_sbc_encoder_t *)state;
    // Bead pico-link-19c: this used to read `sbc_buffer_length()` BEFORE
    // calling `encode_signed_16()` and bail out if it read zero -- which,
    // per the doc comment on `pl_codec_sbc_init` above, it always did on
    // this instance's very first call (and, because that early return
    // meant `encode_signed_16` was never reached, forever after too:
    // nothing ever primed the field). `pl_codec_sbc_init` now guarantees
    // at least one real encode has already happened by the time this is
    // ever called, so `sbc_buffer_length()` is only trustworthy AFTER an
    // encode -- read it there instead, matching a2dp_source_demo.c's own
    // fill loop (a2dp_source_demo.c:446-451), which encodes first and
    // only re-queries the length afterward. `out_cap` is checked against a
    // known-safe worst case up front (see PL_SBC_MAX_FRAME_BYTES above)
    // since bluedroid's own `encode_signed_16` performs no bounds-checking
    // of its own against the caller-supplied `out` buffer.
    if (out_cap < PL_SBC_MAX_FRAME_BYTES) {
        return (pl_codec_encode_result_t){.ok = false, .bytes_written = 0, .frames_emitted = 0, .payload_complete = false};
    }
    // encode_signed_16's return is a bluedroid status code, not a byte
    // count (matches a2dp_source_demo.c -- it discards the return value
    // too). No allocation, no logging, no blocking -- design sec 5's
    // IRQ-context contract.
    enc->instance->encode_signed_16(&enc->state, pcm, out);
    uint16_t written = enc->instance->sbc_buffer_length(&enc->state);
    // Bead pico-link-cz0.5.6 (uniformised vtable): PROVABLE equivalence
    // with the pre-vtable code, not just assumed -- the old
    // `pl_codec_sbc_encode` returned 0 (a2dp.c's fill loop then treated it
    // as a hard failure, pkt_fail++, break) on EITHER the out_cap guard
    // above OR sbc_buffer_length() itself reading 0 after a real encode.
    // ok=false here reproduces BOTH paths bit-for-bit, not just the guard
    // -- do not simplify this to an unconditional ok=true, that would be a
    // real (if likely unreachable in practice, per pico-link-19c's priming
    // fix) behaviour change on the proven-audible path.
    if (written == 0) {
        return (pl_codec_encode_result_t){.ok = false, .bytes_written = 0, .frames_emitted = 0, .payload_complete = false};
    }
    // payload_complete is always false -- SBC has no opinion on when a
    // PAYLOAD (as opposed to one encoded frame) is complete, exactly
    // reproducing the pre-vtable behaviour where a2dp.c alone decided how
    // many fixed-size SBC frames fit in one AVDTP MTU
    // (pl_a2dp_usable_payload's capacity check, still in a2dp.c,
    // unchanged).
    return (pl_codec_encode_result_t){.ok = true, .bytes_written = written, .frames_emitted = 1, .payload_complete = false};
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
    .codec_id = PL_CODEC_ID_SBC,

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
