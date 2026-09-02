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
// encoded_frame_bytes == 0 means variable-length, self-packetising output
// (SBC never sets this -- it is fixed-size per configuration; LDAC does,
// bead pico-link-cz0.5.6 -- see pl_codec_encode_result_t's payload_complete
// field, which is how a2dp.c's single fill loop tells the two shapes
// apart without ever branching on codec identity).
typedef struct {
    uint16_t pcm_frames_per_encoded_frame; // e.g. SBC 48k/8sb/16blk -> 128
    uint16_t encoded_frame_bytes;          // 0 == variable, query per-encode
    // Bead pico-link-cz0.5.6: bytes of AVDTP media-payload HEADER this
    // codec's packets carry (SBC and LDAC both use the classic 1-byte
    // fragmentation/start/last/num_frames header -- see codec_sbc.c's row
    // and the LDAC row for the shared value). Ownership of the "-1"
    // reserved-header-byte correction moves here from a2dp.c's old
    // hardcoded constant (pl_a2dp_usable_payload used to assume exactly 1
    // unconditionally); every row must set this even though today both
    // rows agree on 1, so a future codec with a different header shape is
    // a table-row change, not a2dp.c surgery.
    uint8_t header_bytes;
    uint32_t nominal_bitrate_bps;          // what the panel shows (S2)
    // Bead pico-link-pbv: worst-case wall-clock time one encode() call can
    // take, measured on real hardware plus margin -- NOT a live
    // measurement (see a2dp.c's enc_max_us for that). Round 1 used this to
    // derive a per-tick FRAME-COUNT dwell cap at STREAM_ESTABLISHED
    // (PL_A2DP_MAX_ENCODE_DWELL_US / worst_case_encode_us); round 2 found
    // that frame-count proxy indistinguishable from healthy in steady
    // state and replaced it with a direct TIME check against
    // PL_A2DP_MAX_ENCODE_DWELL_US inside the fill loop itself (a2dp.c's
    // stop_dwell) -- worst_case_encode_us is kept here as the codec-table
    // row's own honest declaration of its worst case (a future consumer,
    // e.g. an admission check before enabling a slower codec, may still
    // want it) but a2dp.c's dwell bound no longer reads it directly. SBC's
    // row sets 800 (measured max on real hardware was 574-582us across
    // several runs; 800 is that plus margin, not a
    // theoretical derivation -- see codec_sbc.c).
    uint32_t worst_case_encode_us;
} pl_codec_frame_info_t;

// Bead pico-link-cz0.5.6 (Andreas's decision 2026-08-31, design doc open
// decision (a)): the uniformised encode() result. Every codec row is
// self-packetising from a2dp.c's point of view -- one call may emit zero,
// one, or (in principle) more complete encoded frames, and may or may not
// finish a whole AVDTP payload. This is the ONE seam that lets SBC's
// today-unchanged fixed-size accumulation and LDAC's libldac-driven
// variable-size accumulation share a single drain path in a2dp.c with no
// codec-identity branch anywhere (Andreas's 2026-08-29 ruling,
// codec_table.h:4-11).
typedef struct {
    // False only on a genuine encode failure (e.g. SBC's out_cap-too-small
    // guard, or a real libldac error return) -- NOT set false merely
    // because this call produced no output yet (LDAC accumulating
    // internally is a normal, successful call with bytes_written == 0).
    // a2dp.c treats !ok exactly like today's "encode returned 0" failure
    // path: count pkt_fail, stop the fill loop for this tick.
    bool ok;
    // Bytes appended to `out` THIS call (0 is normal for a self-packetising
    // codec still accumulating). Never exceeds the `out_cap` passed in.
    uint16_t bytes_written;
    // Encoded audio frames represented by bytes_written, for the AVDTP
    // media payload header's num_frames byte (pl_a2dp_slot_t.frames). SBC:
    // always 1 when ok. LDAC: 0 while accumulating, ldacBT_encode's own
    // frame_num when a payload completes.
    uint16_t frames_emitted;
    // True: the codec itself says this payload is complete and must be
    // sealed/sent NOW, regardless of how much room is left in the AVDTP
    // payload slot (LDAC -- libldac packetises to its own configured MTU).
    // False: the codec has no opinion -- a2dp.c's own capacity-based
    // sealing (comparing accumulated bytes against the negotiated MTU)
    // continues to govern, EXACTLY as it does today (SBC -- fixed-size
    // frames, a2dp.c decides how many fit per packet). This is the one
    // field that lets both codec shapes drive the same fill loop.
    bool payload_complete;
} pl_codec_encode_result_t;

// Pinned per-row codec identity -- design finding 1.1
// (.planning/design/2026-09-02-device-page-seam.md sec 1.1, bead
// pico-link-ay0.1). This is NOT the PL_CODECS array index: array order is
// the negotiation PREFERENCE order (codec_table.c's own doc comment) and is
// designed to change as rows are inserted -- a persisted
// pl_persist_device_record_t::codec_id byte that meant "index into
// PL_CODECS" would silently repoint at the wrong codec the day a row is
// inserted ahead of it, with a valid CRC and no way to detect it. These
// values are therefore ABI to the flash store: pinned here, never renumbered,
// never reused even if a row is later removed.
//
// 0 is reserved for "Automatic" / "unset" and must never be assigned to a
// table row -- pl_a2dp_init's boot check (a2dp.c) halts if it is.
#define PL_CODEC_ID_AUTOMATIC 0u
#define PL_CODEC_ID_SBC 1u
#define PL_CODEC_ID_LDAC 2u
// Next new codec's id is 3, allocated here (not at the call site) and
// permanent from the moment it ships.

// One codec table row. Statically allocated (one instance per codec,
// defined in that codec's own .c file, e.g. codec_sbc.c's pl_codec_sbc) --
// never malloc'd (design sec 5: the encode path must not allocate).
typedef struct pl_codec {
    // --- identity ---
    const char *display_name; // "SBC" / "LDAC" / "AAC" -- EXACTLY the panel string (S2).
    uint8_t avdtp_codec_type; // AVDTP_CODEC_SBC | ..._MPEG_2_4_AAC | ..._NON_A2DP
    uint32_t vendor_id;       // vendor-specific only (LDAC 0x0000012D), else 0
    uint16_t vendor_codec_id; // vendor-specific only (LDAC 0x00AA), else 0
    // PL_CODEC_ID_* above -- a pinned, persisted, never-recycled identity.
    // Never the array index. See this header's doc comment just above.
    uint8_t codec_id;

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
    // Consumes exactly out_frame.pcm_frames_per_encoded_frame interleaved
    // int16 stereo PCM frames (from `init`'s out_frame) and MAY append 0 or
    // more complete encoded bytes to `out` (capacity `out_cap`) -- see
    // pl_codec_encode_result_t's doc comment for the full contract. CONTRACT
    // (design sec 5), unchanged by pico-link-cz0.5.6: no allocation, no
    // logging, no blocking, no Rust -- this runs in the cyw43/BTstack
    // background IRQ (0xFF).
    pl_codec_encode_result_t (*encode)(void *state, const int16_t *pcm, uint8_t *out, uint16_t out_cap);
    void (*deinit)(void *state);
    void *state; // statically allocated per codec, never malloc'd
} pl_codec_t;

// Preference order. SBC is LAST and ALWAYS PRESENT once more rows exist:
// the A2DP spec mandates it of every implementation, so it is the floor
// the fallback chain terminates on. S1's whole table is { &pl_codec_sbc }.
extern pl_codec_t *const PL_CODECS[];
extern const size_t PL_CODEC_COUNT;

// Resolves a pinned codec_id (PL_CODEC_ID_* above, e.g. from a persisted
// device-settings pin) back to its table row, independent of PL_CODECS'
// current array order -- this is the whole point of finding 1: a caller
// that looks up BY ID, never by array position, is immune to a future row
// insertion. Returns NULL for PL_CODEC_ID_AUTOMATIC or any id no current
// row claims. Design task 3 (the pinned negotiation walk, a2dp.c) is the
// first real caller; exposed here because it is a codec-table primitive,
// not a2dp.c's own logic.
pl_codec_t *pl_codec_table_find_by_id(uint8_t codec_id);

// Same search, parameterized over an explicit table -- exposed only so
// firmware/tests/test_codec_id_stability.c can exercise the real search
// logic against a deliberately REORDERED copy of a table (proving lookup
// is by id, not by position) without needing to mutate the real, global
// PL_CODECS. pl_codec_table_find_by_id() above is a thin wrapper over this.
pl_codec_t *pl_codec_table_find_by_id_in(pl_codec_t *const *table, size_t count, uint8_t codec_id);

#endif // PICO_LINK_CODEC_TABLE_H
