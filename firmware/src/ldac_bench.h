// Pico Link firmware -- LDAC L0 on-target offline benchmark (bead
// pico-link-cz0.5.4, design doc .planning/design/2026-08-30-ldac.md Q1/Q3).
//
// TEMPORARY, DEV-ONLY MEASUREMENT CODE. This is NOT a feature -- it exists
// only to answer L0's four questions on real hardware:
//   - us/ldacBT_encode() at EQMID HQ (990k) / SQ (660k) / MQ (330k)
//   - whether the build path uses double-precision arithmetic (it does
//     not -- see firmware/vendor/libldac/PROVENANCE.md's "Arithmetic mode"
//     section, confirmed by reading every vendored .c file) and whether a
//     fixed-point macro exists (_32BIT_FIXED_POINT -- exists, deliberately
//     not defined here, see PROVENANCE.md)
//   - heap high-water from ldacBT_get_handle()+ldacBT_init_handle_encode(),
//     and confirmation encode() allocates nothing
//   - the real value of LDACBT_ENC_LSU in the vendored header
//
// Feeds synthetic PCM into the vendored encoder and reports over the debug
// console (pl_log). Gated behind PL_DIAG_LDAC_BENCH (undefined by default,
// pass -DPL_DIAG_LDAC_BENCH to CMAKE_C_FLAGS/CMAKE_CXX_FLAGS to enable),
// matching this file's PL_DIAG_COLOR_TEST/PL_DIAG_MADCTL_TEST siblings in
// main.c. No A2DP wiring -- nothing in the media path calls this.
#ifndef PL_LDAC_BENCH_H
#define PL_LDAC_BENCH_H

// Runs the benchmark, logs results via pl_log(), then returns. Caller
// (main.c, under PL_DIAG_LDAC_BENCH) halts afterward -- this function does
// not halt itself, so it stays easy to call from a test harness later if
// wanted.
void pl_ldac_bench_run(void);

#endif // PL_LDAC_BENCH_H
