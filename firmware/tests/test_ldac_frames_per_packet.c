// Pico Link firmware -- host-buildable test for bead pico-link-i6zn:
// codec_ldac.c's self_packetising_frames_per_packet derivation.
//
// This is a MODEL test, not a link test -- same convention as
// test_a2dp_tx_ring_count.c (see that file's doc comment for why
// codec_ldac.c/a2dp.c can't be linked on host: ldacBT.h, BTstack, pico-sdk).
// The formula below is copied verbatim from codec_ldac.c's
// pl_codec_ldac_init (bytes_per_frame/frmlen_tx/clamp) -- if that changes,
// update both here and there.
//
// Verifies the exact numbers Ada's design doc
// (.planning/design/2026-09-07-ldac-abr-control-loop.md sec 4.2) cites:
// HQ 990kbps -> 2 frames/packet (1024B PCM excursion), MQ 330kbps ->
// 6 frames/packet (3072B) -- and that the OLD hardcoded-1 behaviour this
// bead fixes was wrong by exactly that 2x-6x factor.
//
// Build + run (no CMake target -- same convention as the other firmware/
// tests/test_*.c files):
//   cc -std=c11 -Wall -Wextra firmware/tests/test_ldac_frames_per_packet.c \
//      -o /tmp/test_ldac_frames_per_packet && /tmp/test_ldac_frames_per_packet
#include <assert.h>
#include <stdint.h>
#include <stdio.h>

#define PL_LDAC_INIT_MTU 679

static uint16_t model_frames_per_packet(int kbps) {
    if (kbps <= 0) {
        return 0; // codec_ldac.c's "bitrate unknown -> hint unknown" branch
    }
    uint32_t bytes_per_frame = ((uint32_t)kbps * 1000u) / 3000u;
    uint32_t frmlen_tx = bytes_per_frame + 3u;
    uint32_t raw = frmlen_tx > 0 ? (uint32_t)PL_LDAC_INIT_MTU / frmlen_tx : 0u;
    uint32_t clamped = raw < 2u ? 2u : (raw > 15u ? 15u : raw);
    return (uint16_t)clamped;
}

int main(void) {
    // --- The two headline numbers from the design doc, computed from the
    // library's own bitrate maths, not restated as a table. ---
    assert(model_frames_per_packet(990) == 2); // HQ: 1024B PCM excursion
    assert(model_frames_per_packet(330) == 6); // MQ: 3072B PCM excursion
    printf("ok:   HQ 990kbps -> 2 frames/packet (1024B PCM excursion)\n");
    printf("ok:   MQ 330kbps -> 6 frames/packet (3072B PCM excursion)\n");

    // --- SQ sits between the two, monotone with falling bitrate. ---
    uint16_t sq = model_frames_per_packet(660);
    assert(sq == 3);
    assert(sq > model_frames_per_packet(990) && sq < model_frames_per_packet(330));
    printf("ok:   SQ 660kbps -> 3 frames/packet, strictly between HQ and MQ\n");

    // --- The bug this bead fixes: EVERY one of these real bitrates is
    // 2x-6x the hardcoded fallback of 1 that a2dp.c used before this bead
    // (encoded_frame_bytes == 0 forced the `: 1u` branch every time). ---
    assert(model_frames_per_packet(990) > 1);
    assert(model_frames_per_packet(660) > 1);
    assert(model_frames_per_packet(330) > 1);
    printf("ok:   every real LDAC rung exceeds the old hardcoded frames_per_packet=1\n");

    // --- Clamp floor: an absurdly high bitrate (hypothetical) must not
    // report frames_per_packet=1 -- libldac's documented packing minimum is
    // 2 (matches codec_ldac.c's clamp). ---
    assert(model_frames_per_packet(3000) == 2);
    printf("ok:   clamp floor holds at 2 for an implausibly high bitrate\n");

    // --- Clamp ceiling: an absurdly low bitrate must not exceed 15,
    // libldac's documented packing maximum. ---
    assert(model_frames_per_packet(1) == 15);
    printf("ok:   clamp ceiling holds at 15 for an implausibly low bitrate\n");

    // --- Unknown bitrate (ldacBT_get_bitrate failure) reports 0, i.e.
    // "no hint" -- a2dp.c's fallback-with-warning governs instead of a
    // fabricated number (pico-link-r44's "never invent" lesson, same
    // discipline as nominal_bitrate_bps's own 0-on-failure branch). ---
    assert(model_frames_per_packet(0) == 0);
    assert(model_frames_per_packet(-1) == 0);
    printf("ok:   unknown bitrate reports frames_per_packet hint = 0, not a guess\n");

    printf("\nAll checks passed.\n");
    return 0;
}
