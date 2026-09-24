// Pico Link firmware -- host-buildable test for bead pico-link-dge6:
// raising LDAC quality on the device was a silent no-op when the stream
// started pinned below HQ.
//
// MODEL test, not a link test -- same convention as
// test_ldac_abr_controller.c and test_paired_device_upserted_ldac_quality_echo.c.
// firmware/src/codec_ldac.c cannot be linked on host (ldacBT.h, pico-sdk).
// The two functions below are copied verbatim from:
//   - codec_ldac.c's pl_ldac_quality_to_rung (the fixed-quality -> ladder
//     rung map)
//   - codec_ldac.c's pl_codec_ldac_init init-time seeding of
//     s_ldac_applied_rung / s_ldac_target_rung (the two lines this bead
//     changed)
// If either real function changes, update both here and there -- nothing
// enforces that they stay in sync (same LIMITATION as the sibling model
// tests above).
//
// What this proves:
//   1. pl_ldac_quality_to_rung matches the REAL libldac EQMID table
//      (ldacBT_internal.c's tbl_ldacbt_eqmid_property: HQ=0/990, SQ=1/660,
//      Q0=2/492, Q1=3/396, MQ=4/330) -- SQ must map to rung 1, not 2.
//   2. model_init_seed_rungs (the fixed init()-time seed) sets BOTH
//      applied_rung and target_rung to the rung matching the persisted
//      pinned quality, so a later pin to a DIFFERENT quality never
//      computes target == applied by coincidence and no-ops via
//      pl_codec_ldac_apply_pending_tuning's early-return guard --
//      reproducing (before the fix) and refuting (after) the exact "pin
//      HQ from a fresh SQ connect, bitrate stays 660000" symptom.
//
// Build + run (no CMake target -- same convention as the other firmware/
// tests/test_*.c files):
//   cc -std=c11 -Wall -Wextra firmware/tests/test_ldac_pin_rung_seed.c \
//      -o /tmp/test_ldac_pin_rung_seed && /tmp/test_ldac_pin_rung_seed
#include <assert.h>
#include <stdint.h>
#include <stdio.h>

// --- Copied from codec_ldac.c ---

// ldac_quality_1based values, same as codec_ldac.h's public constants.
#define PL_LDAC_QUALITY_HQ 1
#define PL_LDAC_QUALITY_SQ 2
#define PL_LDAC_QUALITY_MQ 3

// Fixed (post-fix), copied verbatim from codec_ldac.c's
// pl_ldac_quality_to_rung -- bead pico-link-dge6's secondary fix.
static int32_t model_quality_to_rung(uint8_t ldac_quality_1based) {
    switch (ldac_quality_1based) {
        case 2: // 660 kbps / SQ
            return 1;
        case 3: // 330 kbps / MQ
            return 4;
        case 0:
        case 1: // 990 kbps / HQ
        default:
            return 0;
    }
}

// Copied verbatim from codec_ldac.c's pl_codec_ldac_init: the two lines
// this bead changed. Takes the persisted pinned quality (adaptive streams
// always pass HQ here, matching pl_ldac_quality_to_initial_state) and
// returns the (applied_rung, target_rung) pair init() now seeds.
static void model_init_seed_rungs(uint8_t pending_ldac_quality, int32_t *out_applied, int32_t *out_target) {
    int32_t seeded = model_quality_to_rung(pending_ldac_quality);
    *out_applied = seeded;
    *out_target = seeded;
}

int main(void) {
    // 1. Rung mapping matches the real libldac table positions.
    assert(model_quality_to_rung(PL_LDAC_QUALITY_HQ) == 0);
    assert(model_quality_to_rung(PL_LDAC_QUALITY_SQ) == 1); // was 2 pre-fix
    assert(model_quality_to_rung(PL_LDAC_QUALITY_MQ) == 4);

    // 2. Init seeds applied/target to the SAME rung as the quality the
    // stream actually started at -- so a later pin to a different quality
    // never computes target == applied by coincidence.
    int32_t applied, target;

    // Repro: device pinned at SQ (the reported bug's starting condition).
    model_init_seed_rungs(PL_LDAC_QUALITY_SQ, &applied, &target);
    assert(applied == 1 && target == 1);
    // Pin to HQ afterwards: pl_codec_ldac_pin_now would set
    // target = model_quality_to_rung(HQ) == 0, which now DIFFERS from the
    // seeded applied_rung (1) -- apply_pending_tuning's early-return guard
    // does NOT fire, so the walk actually proceeds. Pre-fix, init()
    // hardcoded applied=target=0, so this pin computed target(0) ==
    // applied(0) and silently no-op'd -- exactly the reported bug.
    int32_t pin_to_hq_target = model_quality_to_rung(PL_LDAC_QUALITY_HQ);
    assert(pin_to_hq_target != applied);

    // Repro: device pinned at MQ, pin up to SQ.
    model_init_seed_rungs(PL_LDAC_QUALITY_MQ, &applied, &target);
    assert(applied == 4 && target == 4);
    int32_t pin_to_sq_target = model_quality_to_rung(PL_LDAC_QUALITY_SQ);
    assert(pin_to_sq_target != applied);

    // Adaptive / HQ start still seeds rung 0, unchanged from before this
    // bead -- a fresh HQ connect pinning DOWN to SQ or MQ must still work
    // (this was the direction that already worked pre-fix).
    model_init_seed_rungs(PL_LDAC_QUALITY_HQ, &applied, &target);
    assert(applied == 0 && target == 0);

    printf("test_ldac_pin_rung_seed: OK\n");
    return 0;
}
