// Pico Link firmware -- host-buildable test for bead pico-link-ycm2.7.1.1:
// the independent host-clock-offset reference (host_clock_ref.c).
//
// LINKS the REAL firmware/src/host_clock_ref.c unmodified. It has zero
// pico-sdk/tinyusb dependency (takes now_us/sof_count as plain parameters
// -- see its module doc), so no host stub headers are needed at all.
//
// Build + run:
//   cc -std=c11 -Wall -Wextra -I firmware/src \
//      firmware/tests/test_host_clock_ref.c firmware/src/host_clock_ref.c \
//      -lm -o /tmp/test_host_clock_ref && /tmp/test_host_clock_ref
//   (-lm is for the TEST's own synthetic-host generator, which uses
//   floor() to carry fractional per-second SOF counts -- host_clock_ref.c
//   itself is pure integer arithmetic, no libm dependency.)
//
// What this proves:
//   (a) PL_HOST_CLOCK_PPM_NOT_READY until a full ~10s window completes.
//   (b) An exactly-nominal host (1000 SOF/s on our clock) reports ~0ppm.
//   (c) A host running fast (more SOFs per our-clock window than nominal)
//       reports POSITIVE ppm, magnitude within rounding of the injected
//       offset -- same sign convention host_clock_ref.h documents for
//       "ring tends to run high".
//   (d) A host running slow reports NEGATIVE ppm.
//   (e) pl_host_clock_ref_bus_reset() returns to NOT_READY and a fresh
//       window measures independently of windows before the reset.
#include <assert.h>
#include <math.h>
#include <stdio.h>

#include "host_clock_ref.h"

#define NOMINAL_SOF_HZ 1000u

// Feeds poll() a sequence of 1s-spaced samples computing sof_count from an
// injected ppm offset, mirroring main.c's real ~1Hz call cadence (the
// module itself gates to the coarser ~10s window internally). Fractional
// per-second SOF counts (a 220ppm offset is 0.22 extra SOFs/s, which would
// truncate to 0 if tracked as an integer every single second) are carried
// in a double accumulator HERE, in the test's synthetic host model -- this
// is purely about generating an accurate injected signal; host_clock_ref.c
// itself stays all-integer (see its own doc comment) because it only ever
// sees the two integer endpoints of a window, never these intermediate
// points.
static void run_for_seconds(uint64_t *now_us, uint32_t *sof_count, int seconds, int64_t ppm_offset) {
    static double carried_sof = 0.0;
    for (int i = 0; i < seconds; i++) {
        *now_us += 1000000u;
        carried_sof += (double)NOMINAL_SOF_HZ * (1.0 + (double)ppm_offset / 1000000.0);
        uint32_t whole = (uint32_t)floor(carried_sof);
        carried_sof -= (double)whole;
        *sof_count += whole;
        pl_host_clock_ref_poll(*now_us, *sof_count);
    }
}

int main(void) {
    uint64_t now_us = 0;
    uint32_t sof_count = 0;

    // (a) not ready before any window completes. The very first poll()
    // call only records the window's starting snapshot (host_clock_ref.h's
    // doc comment) -- call it explicitly at t=0 so every run_for_seconds
    // window below measures a clean N-second span from a known start,
    // rather than losing its first second to this priming call.
    assert(pl_host_clock_ref_ppm() == PL_HOST_CLOCK_PPM_NOT_READY);
    pl_host_clock_ref_poll(now_us, sof_count);
    assert(pl_host_clock_ref_ppm() == PL_HOST_CLOCK_PPM_NOT_READY);
    run_for_seconds(&now_us, &sof_count, 5, 0);
    assert(pl_host_clock_ref_ppm() == PL_HOST_CLOCK_PPM_NOT_READY);

    // (b) nominal host: finish out the 10s window at 0ppm injected offset.
    run_for_seconds(&now_us, &sof_count, 5, 0);
    int32_t ppm_nominal = pl_host_clock_ref_ppm();
    assert(ppm_nominal != PL_HOST_CLOCK_PPM_NOT_READY);
    assert(ppm_nominal > -5 && ppm_nominal < 5);
    printf("nominal host: host_ppm=%ld (expected ~0)\n", (long)ppm_nominal);

    // (c) host running +220ppm fast (the 2026-08-31 measurement this bead
    // exists to corroborate) -- a fresh 10s window. Tolerance is wide
    // (+/-120ppm around the injected value) because a 10s window at 1000
    // SOF/s has inherent +/-1-SOF quantization (1 SOF ~= 100ppm here) --
    // this is the SAME discrete-counting quantization the real instrument
    // will have on hardware, not a test bug; what this proves is sign and
    // rough magnitude, not exact reproduction.
    run_for_seconds(&now_us, &sof_count, 10, 220);
    int32_t ppm_fast = pl_host_clock_ref_ppm();
    assert(ppm_fast > 100 && ppm_fast < 300);
    printf("fast host: host_ppm=%ld (expected ~220, +/-1 SOF quantization)\n", (long)ppm_fast);

    // (d) host running -150ppm slow.
    run_for_seconds(&now_us, &sof_count, 10, -150);
    int32_t ppm_slow = pl_host_clock_ref_ppm();
    assert(ppm_slow < -50 && ppm_slow > -250);
    printf("slow host: host_ppm=%ld (expected ~-150, +/-1 SOF quantization)\n", (long)ppm_slow);

    // (e) bus reset goes back to NOT_READY, and a fresh window measures
    // independently (does not blend in the -150ppm history above --
    // without the reset, host_ppm would still be solidly negative here).
    pl_host_clock_ref_bus_reset();
    assert(pl_host_clock_ref_ppm() == PL_HOST_CLOCK_PPM_NOT_READY);
    pl_host_clock_ref_poll(now_us, sof_count); // re-prime the window start, same reasoning as the (a) priming call above
    assert(pl_host_clock_ref_ppm() == PL_HOST_CLOCK_PPM_NOT_READY);
    run_for_seconds(&now_us, &sof_count, 10, 50);
    int32_t ppm_after_reset = pl_host_clock_ref_ppm();
    assert(ppm_after_reset > -50 && ppm_after_reset < 150);
    printf("post-reset host: host_ppm=%ld (expected ~50, +/-1 SOF quantization)\n", (long)ppm_after_reset);

    printf("test_host_clock_ref: ALL CHECKS PASSED\n");
    return 0;
}
