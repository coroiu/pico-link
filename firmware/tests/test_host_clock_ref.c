// Pico Link firmware -- host-buildable test for bead pico-link-ycm2.7.1.1:
// the independent host-clock-offset reference (host_clock_ref.c).
//
// LINKS the REAL firmware/src/host_clock_ref.c unmodified. It has zero
// pico-sdk/tinyusb dependency (takes a SOF edge's (time_us, count) pair as
// plain parameters -- see its module doc), so no host stub headers are
// needed at all.
//
// Build + run:
//   cc -std=c11 -Wall -Wextra -I firmware/src \
//      firmware/tests/test_host_clock_ref.c firmware/src/host_clock_ref.c \
//      -o /tmp/test_host_clock_ref && /tmp/test_host_clock_ref
//
// What this proves:
//   (a) PL_HOST_CLOCK_PPM_NOT_READY until a full ~10s window completes.
//   (b) An exactly-nominal host (1000 SOF/s on our clock) reports ~0ppm.
//   (c) A host running fast reports POSITIVE ppm, accurate to within a
//       couple of ppm of the injected 220ppm offset -- this is the actual
//       fix this bead makes: the first cut's edge-COUNTING-inside-a-
//       fixed-window design could only resolve ~100ppm (it could not
//       separate 0 from 220); timestamping real edges and measuring the
//       elapsed time BETWEEN them removes that phase ambiguity entirely.
//       Same sign convention host_clock_ref.h documents for "ring tends
//       to run high".
//   (d) A host running slow reports NEGATIVE ppm, same precision.
//   (e) pl_host_clock_ref_bus_reset() returns to NOT_READY and a fresh
//       window measures independently of windows before the reset.
//   (f) A quiet bus (SOF count not advancing between calls) does not
//       restart or corrupt the window -- it is a safe no-op.
#include <assert.h>
#include <stdio.h>

#include "host_clock_ref.h"

#define NOMINAL_SOF_HZ 1000u

// Feeds poll() a sequence of SOF "edges" at EXACTLY the injected ppm rate
// -- unlike the first cut's per-second synthetic host (which had to carry
// a fractional sof-per-second remainder and therefore itself quantized to
// ~100ppm), this generates edges at their true period directly in
// microseconds, so the test can tell a correct sub-ppm result from an
// incorrect one. period_us is a double on purpose (the whole point of
// this bead is that real elapsed time between edges is the precise
// quantity, not an edge count) -- host_clock_ref.c itself never sees this
// double, only the resulting integer microsecond timestamps.
static void run_edges(uint64_t *time_us, uint32_t *sof_count, int edge_count, int64_t ppm_offset) {
    double period_us = 1000.0 / (1.0 + (double)ppm_offset / 1000000.0);
    static double carried_us = 0.0;
    for (int i = 0; i < edge_count; i++) {
        carried_us += period_us;
        uint64_t whole_us = (uint64_t)carried_us;
        carried_us -= (double)whole_us;
        *time_us += whole_us;
        *sof_count += 1;
        pl_host_clock_ref_poll(*time_us, *sof_count);
    }
}

int main(void) {
    uint64_t time_us = 0;
    uint32_t sof_count = 0;

    // (a) not ready before any window completes. The very first poll()
    // call only records the window's starting edge snapshot (doc
    // comment) -- prime it explicitly at t=0 so every run_edges window
    // below measures a clean span from a known start.
    assert(pl_host_clock_ref_ppm() == PL_HOST_CLOCK_PPM_NOT_READY);
    pl_host_clock_ref_poll(time_us, sof_count);
    assert(pl_host_clock_ref_ppm() == PL_HOST_CLOCK_PPM_NOT_READY);
    run_edges(&time_us, &sof_count, 5000, 0); // 5s of nominal edges
    assert(pl_host_clock_ref_ppm() == PL_HOST_CLOCK_PPM_NOT_READY);

    // (b) nominal host: finish out the >=10s window (10,001 edges from
    // t=0 guarantees us_delta >= 10,000,000us even with per-edge rounding).
    run_edges(&time_us, &sof_count, 5001, 0);
    int32_t ppm_nominal = pl_host_clock_ref_ppm();
    assert(ppm_nominal != PL_HOST_CLOCK_PPM_NOT_READY);
    assert(ppm_nominal > -2 && ppm_nominal < 2);
    printf("nominal host: host_ppm=%ld (expected ~0)\n", (long)ppm_nominal);

    // (c) host running +220ppm fast (the 2026-08-31 measurement this bead
    // exists to corroborate) -- a fresh >=10s window (10,050 edges: a
    // fast host's period is slightly SHORTER than nominal, so it takes
    // slightly more than 10,000 edges to span >=10,000,000us -- the
    // margin here just needs to clear that, not be exact). Tight
    // tolerance: this is exactly the case the first cut's design could
    // not resolve from 0ppm at all (both read within its ~100ppm
    // quantization floor).
    run_edges(&time_us, &sof_count, 10050, 220);
    int32_t ppm_fast = pl_host_clock_ref_ppm();
    assert(ppm_fast > 215 && ppm_fast < 225);
    printf("fast host: host_ppm=%ld (expected ~220)\n", (long)ppm_fast);

    // (d) host running -150ppm slow.
    run_edges(&time_us, &sof_count, 10050, -150);
    int32_t ppm_slow = pl_host_clock_ref_ppm();
    assert(ppm_slow < -145 && ppm_slow > -155);
    printf("slow host: host_ppm=%ld (expected ~-150)\n", (long)ppm_slow);

    // (e) bus reset goes back to NOT_READY, and a fresh window measures
    // independently (does not blend in the -150ppm history above --
    // without the reset, host_ppm would still read solidly negative here).
    pl_host_clock_ref_bus_reset();
    assert(pl_host_clock_ref_ppm() == PL_HOST_CLOCK_PPM_NOT_READY);
    pl_host_clock_ref_poll(time_us, sof_count); // re-prime, same reasoning as the (a) priming call above
    assert(pl_host_clock_ref_ppm() == PL_HOST_CLOCK_PPM_NOT_READY);
    run_edges(&time_us, &sof_count, 10050, 50);
    int32_t ppm_after_reset = pl_host_clock_ref_ppm();
    assert(ppm_after_reset > 45 && ppm_after_reset < 55);
    printf("post-reset host: host_ppm=%ld (expected ~50)\n", (long)ppm_after_reset);

    // (f) a quiet bus (sof_count not advancing) must not restart or
    // corrupt the in-progress window -- feed several calls at the SAME
    // (time_us, sof_count) partway through a fresh window, then finish it
    // with real edges, and confirm the measurement is unaffected.
    pl_host_clock_ref_bus_reset();
    pl_host_clock_ref_poll(time_us, sof_count);
    run_edges(&time_us, &sof_count, 3000, 0); // 3s in
    for (int i = 0; i < 5; i++) {
        pl_host_clock_ref_poll(time_us, sof_count); // stale -- bus gone quiet
    }
    assert(pl_host_clock_ref_ppm() == PL_HOST_CLOCK_PPM_NOT_READY); // window not corrupted/restarted
    run_edges(&time_us, &sof_count, 7050, 0); // finish the window at 0ppm
    int32_t ppm_after_quiet = pl_host_clock_ref_ppm();
    assert(ppm_after_quiet > -2 && ppm_after_quiet < 2);
    printf("post-quiet-bus host: host_ppm=%ld (expected ~0)\n", (long)ppm_after_quiet);

    printf("test_host_clock_ref: ALL CHECKS PASSED\n");
    return 0;
}
