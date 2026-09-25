// Pico Link firmware -- host-buildable test for bead pico-link-i6zn:
// codec_ldac.c's self_packetising_frames_per_packet derivation.
//
// This is a MODEL test, not a link test -- same convention as
// test_a2dp_tx_ring_count.c (see that file's doc comment for why
// codec_ldac.c/a2dp.c can't be linked on host: ldacBT.h, BTstack, pico-sdk).
// The formula below is copied verbatim from codec_ldac.c's
// pl_codec_ldac_init (bytes_per_frame/raw_frames_per_packet/clamp) -- if
// that changes, update both here and there.
//
// Bead pico-link-d42g (design .planning/design/2026-09-25-adaptive-floor.md
// sec 0/3): the OLD formula here (679/(kbps/3+3)) double-counted LDAC's
// own 3-byte per-frame transport header AND used the raw MTU (679)
// instead of libldac's real payload budget, tx_size = mtu -
// LDACBT_TX_HEADER_SIZE (18) -- ldacBT_api.c's own
// nfrm_in_pkt = tx_size / frmlen_tx maths (frmlen_tx there already
// includes the frame header on the OTHER side of the division, unlike the
// old formula here). The two formulas agreed from HQ to MQ only by
// coincidence and diverged below it (Q3: 7 vs the library's real 8; Q5: 9
// vs 10) -- this bead's whole reason for existing is to reach Q3/Q5, so
// the coincidence stops covering the range this codec now uses. Verifies
// the exact NFRM sequence Ada's design doc cites: HQ 990kbps -> 2
// frames/packet through Q5 198kbps -> 10 frames/packet, matching
// tbl_ldacbt_config's own NFRM column exactly, not by luck.
//
// Build + run (no CMake target -- same convention as the other firmware/
// tests/test_*.c files):
//   cc -std=c11 -Wall -Wextra firmware/tests/test_ldac_frames_per_packet.c \
//      -o /tmp/test_ldac_frames_per_packet && /tmp/test_ldac_frames_per_packet
#include <assert.h>
#include <stdint.h>
#include <stdio.h>

#define PL_LDAC_INIT_MTU 679
// Bead pico-link-d42g: libldac's own per-packet transport header size
// (firmware/vendor/libldac/src/ldacBT_internal.h:61,
// LDACBT_TX_HEADER_SIZE, currently 18) -- restated locally, same
// convention as codec_ldac.c's own PL_LDAC_TX_HEADER_SIZE (kept internal
// to the library, not re-included here).
#define PL_LDAC_TX_HEADER_SIZE 18

static uint16_t model_frames_per_packet(int kbps) {
    if (kbps <= 0) {
        return 0; // codec_ldac.c's "bitrate unknown -> hint unknown" branch
    }
    uint32_t bytes_per_frame = ((uint32_t)kbps * 1000u) / 3000u;
    uint32_t raw = bytes_per_frame > 0 ? (uint32_t)(PL_LDAC_INIT_MTU - PL_LDAC_TX_HEADER_SIZE) / bytes_per_frame
                                       : (uint32_t)(PL_LDAC_INIT_MTU - PL_LDAC_TX_HEADER_SIZE);
    uint32_t clamped = raw < 2u ? 2u : (raw > 15u ? 15u : raw);
    return (uint16_t)clamped;
}

int main(void) {
    // --- The full 9-rung ladder, HQ..Q5, matching libldac's own
    // tbl_ldacbt_config NFRM column exactly (design sec 0's worked
    // arithmetic: 661/(kbps/3), truncating integer division). ---
    assert(model_frames_per_packet(990) == 2);  // HQ
    assert(model_frames_per_packet(660) == 3);  // SQ
    assert(model_frames_per_packet(492) == 4);  // Q0
    assert(model_frames_per_packet(396) == 5);  // Q1
    assert(model_frames_per_packet(330) == 6);  // MQ
    assert(model_frames_per_packet(282) == 7);  // Q2
    assert(model_frames_per_packet(246) == 8);  // Q3 -- was 7 under the old (wrong) formula
    assert(model_frames_per_packet(216) == 9);  // Q4
    assert(model_frames_per_packet(198) == 10); // Q5 -- was 9 under the old (wrong) formula
    printf("ok:   full HQ..Q5 ladder matches libldac's own NFRM column: 2,3,4,5,6,7,8,9,10\n");

    // --- Monotone with falling bitrate across the whole ladder. ---
    int kbps_ladder[9] = {990, 660, 492, 396, 330, 282, 246, 216, 198};
    for (int i = 1; i < 9; i++) {
        assert(model_frames_per_packet(kbps_ladder[i]) > model_frames_per_packet(kbps_ladder[i - 1]));
    }
    printf("ok:   frames_per_packet strictly increases as bitrate falls, HQ through Q5\n");

    // --- The bug this bead's predecessor (pico-link-i6zn) fixed: EVERY
    // real bitrate on the ladder is well above the ancient hardcoded
    // fallback of 1 (encoded_frame_bytes == 0 forced a `: 1u` branch that
    // no longer exists in a2dp.c). ---
    assert(model_frames_per_packet(990) > 1);
    assert(model_frames_per_packet(198) > 1);
    printf("ok:   every real LDAC rung exceeds the old hardcoded frames_per_packet=1\n");

    // --- Clamp floor: an absurdly high bitrate (hypothetical) must not
    // report frames_per_packet=1 -- libldac's documented packing minimum is
    // 2 (matches codec_ldac.c's clamp). ---
    assert(model_frames_per_packet(3000) == 2);
    printf("ok:   clamp floor holds at 2 for an implausibly high bitrate\n");

    // --- Clamp ceiling: an absurdly low bitrate must not exceed 15,
    // libldac's documented packing maximum. Also exercises the
    // bytes_per_frame == 0 guard (integer division truncates to 0 below
    // ~3 kbps), which the old formula's "+3" denominator never triggered. ---
    assert(model_frames_per_packet(1) == 15);
    printf("ok:   clamp ceiling holds at 15 for an implausibly low bitrate (bytes_per_frame == 0 "
           "guard)\n");

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
