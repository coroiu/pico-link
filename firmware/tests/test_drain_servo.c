// Pico Link firmware -- host-buildable test for bead pico-link-ycm2.7.1:
// the drain-rate PI servo (drain_servo.c) that replaces usb_audio.c's
// former explicit-feedback PI loop as the rate matcher.
// `.planning/design/2026-10-19-adaptive-usb-audio.md` sec 2b/7 step 1.
//
// LINKS the REAL firmware/src/drain_servo.c and firmware/src/pcm_ring.c
// unmodified -- same convention as test_pcm_ring_cross_core.c. Neither
// file has a BTstack/pico-sdk dependency beyond hardware/sync.h's __dmb(),
// substituted with a trivial host stub (see build instructions below).
//
// Build + run:
//   mkdir -p /tmp/hostinc/hardware
//   printf 'static inline void __dmb(void) { __sync_synchronize(); }\n' \
//     > /tmp/hostinc/hardware/sync.h
//   cc -std=c11 -Wall -Wextra -I firmware/src -I /tmp/hostinc \
//      firmware/tests/test_drain_servo.c \
//      firmware/src/drain_servo.c firmware/src/pcm_ring.c \
//      -o /tmp/test_drain_servo && /tmp/test_drain_servo
//
// What this proves:
//   (a) sign convention -- ring fill ABOVE target produces POSITIVE
//       servo_ppm ("drain faster"), ring fill BELOW target produces
//       NEGATIVE servo_ppm -- the flip from the old feedback loop (design
//       sec 2b), not its old sign.
//   (b) the output clamp is +/-1000ppm (widened from the old loop's
//       500ppm), reached under a sustained large error and never
//       exceeded.
//   (c) pl_drain_servo_reset() carries the LEARNED offset into the
//       integral term instead of zeroing it -- a fresh reset at the same
//       operating point reproduces the pre-reset ppm immediately, not
//       after a re-convergence walk.
//   (d) pl_drain_servo_bus_reset() clears the learned offset to zero.
//   (e) pl_drain_servo_accrue_extra_samples carries a sub-sample fixed-
//       point remainder: a small ppm correction that would truncate to
//       zero every single call still accumulates a nonzero extra sample
//       over many calls, and its sign matches servo_ppm's sign.
#include <assert.h>
#include <stdio.h>

#include "drain_servo.h"
#include "pcm_ring.h"

// Bead pico-link-ycm2.7.1's own learn-gate cadence (drain_servo.c's
// PL_DRAIN_SERVO_LEARN_GATE_TICKS) is private to that file -- this test
// mirrors the value from drain_servo.h's doc comment ("roughly once a
// second ... at this function's own ~1ms call cadence") rather than
// reaching into the .c file, same arm's-length convention
// test_ldac_abr_controller.c uses for constants it needs to mirror.
#define LEARN_GATE_TICKS 1000u

static void prime_ring_to(uint32_t target_bytes, uint32_t fill_bytes) {
    pl_pcm_reset();
    pl_pcm_set_target_fill_bytes(target_bytes);
    static uint8_t zeros[8192];
    uint32_t remaining = fill_bytes;
    while (remaining > 0) {
        uint32_t chunk = remaining > sizeof(zeros) ? (uint32_t)sizeof(zeros) : remaining;
        pl_pcm_push(zeros, chunk);
        remaining -= chunk;
    }
}

int main(void) {
    const uint32_t target = 4608; // PL_PCM_TARGET_FILL_BYTES's usual value, but set explicitly -- not load-bearing here

    // --- (a) sign convention: ring HIGH -> servo_ppm POSITIVE ---
    prime_ring_to(target, target + 2000);
    pl_drain_servo_reset();
    for (int i = 0; i < 200; i++) {
        pl_drain_servo_tick();
    }
    int32_t ppm_high = pl_drain_servo_ppm();
    assert(ppm_high > 0 && "ring above target must drain FASTER (positive ppm), not slower");
    printf("ok:   ring above target -> servo_ppm=%ld (positive, drains faster)\n", (long)ppm_high);

    // --- (a) sign convention: ring LOW -> servo_ppm NEGATIVE ---
    prime_ring_to(target, target > 2000 ? target - 2000 : 0);
    pl_drain_servo_reset();
    for (int i = 0; i < 200; i++) {
        pl_drain_servo_tick();
    }
    int32_t ppm_low = pl_drain_servo_ppm();
    assert(ppm_low < 0 && "ring below target must drain SLOWER (negative ppm), not faster");
    printf("ok:   ring below target -> servo_ppm=%ld (negative, drains slower)\n", (long)ppm_low);

    // --- (b) output clamp at +/-1000ppm under a sustained large error ---
    prime_ring_to(target, target + 60000); // far beyond any band -- this ring push also exercises overrun-safe pl_pcm_push, not under test here
    pl_drain_servo_reset();
    for (uint32_t i = 0; i < LEARN_GATE_TICKS * 3; i++) {
        pl_drain_servo_tick();
    }
    int32_t ppm_railed = pl_drain_servo_ppm();
    assert(ppm_railed == 1000 && "a sustained large positive error must rail at exactly +1000ppm");
    assert(pl_drain_servo_rail_ticks() > 0 && "rail_ticks must count the railed ticks");
    printf("ok:   sustained large error rails at exactly +1000ppm, rail_ticks=%lu\n", (unsigned long)pl_drain_servo_rail_ticks());

    // --- (c) learned-offset carry-over across pl_drain_servo_reset() ---
    // This is a MODEL test of the controller only (design sec 2b's
    // closed-loop plant -- the sink actually draining the ring -- is not
    // simulated here, same scope boundary test_ldac_abr_controller.c
    // documents for its own decide/apply model). Build up SOME nonzero
    // learned offset by holding a small, unrailed error inside the
    // target/4 learn band long enough for the ~1Hz learn gate to snapshot
    // it at least once -- what value it lands on is not asserted, only
    // that pl_drain_servo_reset() carries it forward intact.
    uint32_t small_err = target / 8; // well inside the target/4 learn-eligibility band
    prime_ring_to(target, target + small_err);
    pl_drain_servo_reset();
    for (uint32_t i = 0; i < LEARN_GATE_TICKS + 1; i++) {
        pl_drain_servo_tick();
    }
    int32_t learned_before = pl_drain_servo_learned_ppm();
    assert(learned_before != 0 && "a run with a sustained unrailed error must have learned a nonzero offset");

    // Re-prime the ring to sit EXACTLY at target (err=0) and reset -- with
    // err=0 the P term contributes nothing, so a single tick's output is
    // determined ENTIRELY by the seeded I term. The OLD loop's
    // pl_usb_audio_fb_reset unconditionally zeroed the integral here; this
    // one must seed it from the learned offset instead (design sec 2b,
    // "Required details: learned-offset reseed").
    prime_ring_to(target, target);
    pl_drain_servo_reset();
    assert(pl_drain_servo_learned_ppm() == learned_before && "pl_drain_servo_reset must NOT clear the learned offset");
    pl_drain_servo_tick();
    int32_t ppm_one_tick_after_reset = pl_drain_servo_ppm();
    assert(
        ppm_one_tick_after_reset == learned_before &&
        "with err=0, one tick after reset must reproduce the learned offset EXACTLY (I term seeded from it, P term is 0) -- the old loop's zeroed-I reset would read 0 here instead"
    );
    printf(
        "ok:   reset seeds the integral from the learned offset, not zero (learned=%ld, ppm one tick after reset at err=0: %ld)\n",
        (long)learned_before, (long)ppm_one_tick_after_reset
    );

    // --- (d) bus reset clears the learned offset ---
    pl_drain_servo_bus_reset();
    assert(pl_drain_servo_learned_ppm() == 0 && "pl_drain_servo_bus_reset must clear the learned offset to zero");
    assert(pl_drain_servo_ppm() == 0 && "pl_drain_servo_bus_reset must clear the published servo_ppm to zero");
    printf("ok:   bus_reset clears learned_ppm and servo_ppm to 0\n");

    // --- (e) accrual carries a sub-sample fixed-point remainder ---
    // Force a small, fixed positive servo_ppm by priming a converged
    // positive-error state (no bus_reset/learn interference -- just read
    // whatever pl_drain_servo_tick() currently publishes).
    prime_ring_to(target, target + 48); // a small, steady positive error (48 -- PL_PCM_FRAME_BYTES-aligned, pcm_ring.c rejects misaligned pushes wholesale)
    pl_drain_servo_reset();
    for (int i = 0; i < 50; i++) {
        pl_drain_servo_tick();
    }
    int32_t small_ppm = pl_drain_servo_ppm();
    assert(small_ppm > 0 && small_ppm < 100 && "expected a small positive ppm for a small positive error (test setup assumption)");

    // Each call's raw contribution (elapsed_us * sample_rate * ppm / 1e12)
    // is far below 1 sample for a 1ms tick at a realistic rung's worth of
    // ppm -- without a carried remainder, pl_drain_servo_accrue_extra_samples
    // would return 0 EVERY call, forever. Accumulate over enough calls that
    // the true (fractional) correction exceeds one whole sample and prove a
    // nonzero extra sample eventually appears, with the correct sign.
    int64_t total_extra = 0;
    int zero_every_call = 1;
    for (int i = 0; i < 5000; i++) {
        int32_t extra = pl_drain_servo_accrue_extra_samples(1000u /* 1ms, this servo's own tick cadence */, 48000u);
        if (extra != 0) {
            zero_every_call = 0;
        }
        total_extra += extra;
    }
    assert(!zero_every_call && "a carried remainder must eventually surface a nonzero extra-sample correction");
    assert(total_extra > 0 && "a sustained positive ppm must accrue a net POSITIVE extra-sample total");
    printf("ok:   accrual carries a sub-sample remainder (total_extra=%lld over 5000 1ms calls at ppm=%ld)\n", (long long)total_extra, (long)small_ppm);

    printf("ALL DRAIN SERVO MODEL TESTS PASSED\n");
    return 0;
}
