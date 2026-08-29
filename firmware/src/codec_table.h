// Pico Link firmware -- the codec table (M4 design
// .planning/design/2026-08-29-a2dp-source-pipeline.md sec 4.2).
//
// Andreas's ruling 2026-08-29: no LDAC-or-SBC branch anywhere in this
// codebase -- a codec TABLE instead, so AAC/LDAC later is a new row plus an
// encoder, not surgery. This header defines that row shape. S1 (this bead,
// pico-link-cz0.5.2) populates it with exactly one row, SBC -- see
// codec_sbc.h/.c. S4 (a separate bead) prepends an LDAC row; no call site
// in a2dp.c or codec_table.c may switch on codec identity (the reviewer
// checks this by grepping for AVDTP_CODEC_SBC/"SBC" outside codec_sbc.c and
// this table).
#ifndef PICO_LINK_CODEC_TABLE_H
#define PICO_LINK_CODEC_TABLE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

// The PCM format a codec's init() negotiated -- always 16-bit interleaved
// stereo for every codec this project supports; sample_rate_hz is the one
// field that actually varies (SBC negotiates 44100 or 48000).
typedef struct {
    uint32_t sample_rate_hz;
    uint8_t channels;
    uint8_t bits_per_sample;
} pl_codec_format_t;

// What one call to encode() consumes/produces, so a2dp.c's media timer
// never needs to know which codec is active to drive it correctly.
// encoded_frame_bytes == 0 means variable-length output (not used by SBC,
// which is fixed-size per configuration; reserved for a future codec).
typedef struct {
    uint16_t pcm_frames_per_encoded_frame; // e.g. SBC 48k/8sb/16blk -> 128
    uint16_t encoded_frame_bytes;          // 0 == variable, query per-encode
    uint32_t nominal_bitrate_bps;          // what the panel shows (S2)
    // Bead pico-link-pbv: worst-case wall-clock time one encode() call can
    // take, measured on real hardware plus margin -- NOT a live
    // measurement (see a2dp.c's enc_max_us for that). This bounds
    // frames_per_tick_cap (a2dp.c, computed at STREAM_ESTABLISHED as
    // PL_A2DP_MAX_ENCODE_DWELL_US / worst_case_encode_us) so the media
    // timer's per-tick IRQ dwell stays bounded even if credit-pacing
    // (design sec 1) would otherwise allow more frames in one tick after a
    // backlog. SBC's row sets 800 (measured max on real hardware was
    // 574-582us across several runs; 800 is that plus margin, not a
    // theoretical derivation -- see codec_sbc.c).
    uint32_t worst_case_encode_us;
} pl_codec_frame_info_t;

// One codec table row. Statically allocated (one instance per codec,
// defined in that codec's own .c file, e.g. codec_sbc.c's pl_codec_sbc) --
// never malloc'd (design sec 5: the encode path must not allocate).
typedef struct pl_codec {
    // --- identity ---
    const char *display_name; // "SBC" / "LDAC" / "AAC" -- EXACTLY the panel string (S2).
    uint8_t avdtp_codec_type; // AVDTP_CODEC_SBC | ..._MPEG_2_4_AAC | ..._NON_A2DP
    uint32_t vendor_id;       // vendor-specific only (LDAC 0x0000012D), else 0
    uint16_t vendor_codec_id; // vendor-specific only (LDAC 0x00AA), else 0

    // --- negotiation ---
    uint8_t preference; // lower tried first; table sorted by this (S4)
    const uint8_t *capabilities; // AVDTP media codec capability bytes we advertise
    uint8_t capabilities_len;
    uint8_t *configuration; // writable; a2dp_source_create_stream_endpoint's scratch buffer
    uint8_t configuration_len;

    // --- assigned at init ---
    uint8_t local_seid; // from a2dp_source_create_stream_endpoint()

    // --- encoder vtable ---
    // `configuration`/`configuration_len` here are THIS ROW's own private
    // representation of "what the remote negotiated" -- not necessarily the
    // same bytes as the `configuration`/`configuration_len` fields above
    // (which are AVDTP-level scratch BTstack owns). For SBC, a2dp.c decodes
    // BTstack's A2DP_SUBEVENT_SIGNALING_MEDIA_CODEC_SBC_CONFIGURATION fields
    // into a private struct and passes a pointer to that; a future codec is
    // free to define its own shape. Returns false if this build cannot
    // honour the config -> caller falls through to the next table row (S4).
    bool (*init)(
        void *state, const uint8_t *configuration, uint8_t configuration_len, pl_codec_format_t *out_format,
        pl_codec_frame_info_t *out_frame
    );
    // Encodes exactly out_frame.pcm_frames_per_encoded_frame interleaved
    // int16 stereo frames (from `init`'s out_frame) into `out`; returns
    // bytes written, 0 on failure (including out_cap too small). CONTRACT
    // (design sec 5): no allocation, no logging, no blocking, no Rust --
    // this runs in the cyw43/BTstack background IRQ (0xFF).
    uint16_t (*encode)(void *state, const int16_t *pcm, uint8_t *out, uint16_t out_cap);
    void (*deinit)(void *state);
    void *state; // statically allocated per codec, never malloc'd
} pl_codec_t;

// Preference order. SBC is LAST and ALWAYS PRESENT once more rows exist:
// the A2DP spec mandates it of every implementation, so it is the floor
// the fallback chain terminates on. S1's whole table is { &pl_codec_sbc }.
extern pl_codec_t *const PL_CODECS[];
extern const size_t PL_CODEC_COUNT;

#endif // PICO_LINK_CODEC_TABLE_H
