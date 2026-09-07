// Pico Link firmware -- host-buildable test for bead pico-link-i6zn:
// a2dp.c's STREAM_ESTABLISHED frames_per_packet selection and the
// one_packet_bytes/priming cushion derivation that reads it.
//
// MODEL test, not a link test -- same convention as
// test_a2dp_tx_ring_count.c. The two functions below are copied verbatim
// from a2dp.c's STREAM_ESTABLISHED handler (the frames_per_packet
// selection this bead changed, and the unchanged one_packet_bytes
// arithmetic that consumes it) -- if either changes there, update both
// here and there.
//
// What this proves, for a self-packetising codec (LDAC): an n-frames-per-
// packet row produces an n-times-larger one_packet_bytes cushion than the
// old hardcoded-1 fallback -- the exact property the bead's fix must have.
//
// Build + run (no CMake target -- same convention as the other firmware/
// tests/test_*.c files):
//   cc -std=c11 -Wall -Wextra \
//      firmware/tests/test_a2dp_priming_cushion_frames_per_packet.c \
//      -o /tmp/test_a2dp_priming_cushion && /tmp/test_a2dp_priming_cushion
#include <assert.h>
#include <stdint.h>
#include <stdio.h>

#define PL_PCM_FRAME_BYTES 4u // 16-bit stereo == 4 bytes/PCM frame

// Copied from codec_table.h's pl_codec_frame_info_t -- only the fields
// this model needs.
typedef struct {
    uint16_t pcm_frames_per_encoded_frame;
    uint16_t encoded_frame_bytes;         // 0 == self-packetising
    uint16_t self_packetising_frames_per_packet; // bead pico-link-i6zn
} model_frame_info_t;

// Copied verbatim from a2dp.c's STREAM_ESTABLISHED handler (post-fix):
// the three-way selection this bead introduced. `usable_payload` stands in
// for pl_a2dp_usable_payload(...)'s result and `warned` reports whether the
// doubly-defensive warning branch fired (should never happen for a
// correctly-wired codec row).
static uint32_t model_select_frames_per_packet(model_frame_info_t frame, uint32_t usable_payload, int *warned) {
    *warned = 0;
    if (frame.encoded_frame_bytes > 0) {
        uint32_t v = usable_payload / frame.encoded_frame_bytes;
        return v < 1u ? 1u : v;
    } else if (frame.self_packetising_frames_per_packet > 0) {
        return frame.self_packetising_frames_per_packet;
    } else {
        *warned = 1;
        return 1u;
    }
}

// Copied verbatim from a2dp.c's one_packet_bytes line (unchanged by this
// bead -- it was always correct; frames_per_packet feeding it was the bug).
static uint32_t model_one_packet_bytes(uint32_t frames_per_packet, uint16_t pcm_frames_per_encoded_frame) {
    return frames_per_packet * (uint32_t)pcm_frames_per_encoded_frame * PL_PCM_FRAME_BYTES;
}

int main(void) {
    // --- SBC (fixed-size): unaffected by this bead. encoded_frame_bytes
    // > 0 still governs, self_packetising_frames_per_packet is ignored even
    // if (defensively) non-zero. ---
    {
        model_frame_info_t sbc = {.pcm_frames_per_encoded_frame = 128, .encoded_frame_bytes = 119, .self_packetising_frames_per_packet = 0};
        int warned;
        uint32_t fpp = model_select_frames_per_packet(sbc, 663u, &warned);
        assert(!warned);
        assert(fpp == 663u / 119u); // == 5
        printf("ok:   SBC (fixed-size) frames_per_packet unaffected, still divides encoded_frame_bytes\n");
    }

    // --- LDAC HQ: self-packetising, honest hint reports 2 frames/packet
    // (design doc's 1024B PCM excursion). one_packet_bytes is now 2x what
    // the old `: 1u` fallback produced. ---
    {
        model_frame_info_t ldac_hq = {.pcm_frames_per_encoded_frame = 128, .encoded_frame_bytes = 0, .self_packetising_frames_per_packet = 2};
        int warned;
        uint32_t fpp = model_select_frames_per_packet(ldac_hq, 663u, &warned);
        assert(!warned);
        assert(fpp == 2u);
        uint32_t one_packet_bytes = model_one_packet_bytes(fpp, ldac_hq.pcm_frames_per_encoded_frame);
        uint32_t old_buggy_one_packet_bytes = model_one_packet_bytes(1u, ldac_hq.pcm_frames_per_encoded_frame);
        assert(one_packet_bytes == 1024u); // 2 * 128 * 4
        assert(one_packet_bytes == 2u * old_buggy_one_packet_bytes);
        printf("ok:   LDAC HQ: one_packet_bytes=1024B, exactly 2x the old hardcoded-1 cushion\n");
    }

    // --- LDAC MQ: 6 frames/packet, 6x the old cushion, matching the
    // design doc's 3072B worst-case excursion. ---
    {
        model_frame_info_t ldac_mq = {.pcm_frames_per_encoded_frame = 128, .encoded_frame_bytes = 0, .self_packetising_frames_per_packet = 6};
        int warned;
        uint32_t fpp = model_select_frames_per_packet(ldac_mq, 663u, &warned);
        assert(!warned);
        assert(fpp == 6u);
        uint32_t one_packet_bytes = model_one_packet_bytes(fpp, ldac_mq.pcm_frames_per_encoded_frame);
        uint32_t old_buggy_one_packet_bytes = model_one_packet_bytes(1u, ldac_mq.pcm_frames_per_encoded_frame);
        assert(one_packet_bytes == 3072u); // 6 * 128 * 4
        assert(one_packet_bytes == 6u * old_buggy_one_packet_bytes);
        printf("ok:   LDAC MQ: one_packet_bytes=3072B, exactly 6x the old hardcoded-1 cushion\n");
    }

    // --- General property: for ANY n, an n-frames-per-packet self-
    // packetising row must produce an n-times-larger cushion than a
    // 1-frame-per-packet row (the property the bug report demanded be
    // tested). ---
    for (uint16_t n = 2; n <= 15; n++) {
        model_frame_info_t row = {.pcm_frames_per_encoded_frame = 128, .encoded_frame_bytes = 0, .self_packetising_frames_per_packet = n};
        int warned;
        uint32_t fpp = model_select_frames_per_packet(row, 663u, &warned);
        assert(!warned);
        uint32_t cushion_n = model_one_packet_bytes(fpp, row.pcm_frames_per_encoded_frame);
        uint32_t cushion_1 = model_one_packet_bytes(1u, row.pcm_frames_per_encoded_frame);
        assert(cushion_n == (uint32_t)n * cushion_1);
    }
    printf("ok:   n frames/packet -> exactly n-times-larger cushion, for n in 2..15\n");

    // --- Defensive fallback: a self-packetising row that forgot to set the
    // hint (hint == 0) must fall back to 1 AND raise the warning flag --
    // pico-link-r44's "warn loudly, never silently clamp" lesson. ---
    {
        model_frame_info_t broken_row = {.pcm_frames_per_encoded_frame = 128, .encoded_frame_bytes = 0, .self_packetising_frames_per_packet = 0};
        int warned;
        uint32_t fpp = model_select_frames_per_packet(broken_row, 663u, &warned);
        assert(warned);
        assert(fpp == 1u);
        printf("ok:   a self-packetising row with hint==0 falls back to 1 AND is flagged, not silent\n");
    }

    printf("\nAll checks passed.\n");
    return 0;
}
