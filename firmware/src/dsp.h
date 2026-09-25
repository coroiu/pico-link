// Pico Link firmware -- DSP effects fixed-rate stage (bead pico-link-ryw.1,
// design .planning/design/2026-09-25-dsp-effects-stage.md sec 1-3).
//
// WHAT THIS MODULE IS: a block-rate (128 frames/block at 48kHz LDAC) float
// EQ + crossfeed kernel that runs INSIDE pl_a2dp_fill's per-unit loop
// (a2dp.c), between pl_pcm_read and pl_a2dp_accumulate_levels -- see that
// call site for the exact insertion. It never runs anywhere else: not in
// pl_pcm_push (core0, ISO cadence, irregular batch sizes -- design sec
// 1.1), not in a timer, not from Rust.
//
// C/RUST SPLIT (design sec 3.1): Rust (a later bead, ryw.3/ryw.5) computes
// EVERY coefficient in a PlDspProgram -- RBJ biquads, bs2b crossfeed, the
// auto preamp. This module NEVER computes a coefficient from a musical
// parameter (Hz, dB, Q) except inside its own PL_DEBUG_REMOTE canned test
// programs (dsp_debug.inc below), which exist ONLY to bench/exercise the
// kernel before Rust exists -- see pl_dsp_debug_load_program's doc comment.
// Rust never runs on core1 or in an IRQ (C-first ADR); this module is pure
// C for exactly that reason.
//
// OWNERSHIP ACROSS CORES (design sec 3.3):
//   - pl_dsp_submit / pl_dsp_service: core0 (or thread context under the
//     deprecated PL_ENCODER_ON_CORE1=OFF build -- see that section's own
//     doc comment) ONLY. Never called from core1.
//   - pl_dsp_rt_*: core1 (or IRQ context under OFF) ONLY. The `_rt_`
//     infix marks every function that MUST resolve outside flash
//     (0x10xxxxxx) in the final link -- see cmake/check_dsp_not_in_flash.cmake,
//     which greps the linked ELF's symbol table for exactly this prefix.
//     Do not rename one of these functions without updating that script,
//     and do not use the prefix on anything that ISN'T part of the
//     realtime kernel (a false positive there would make the check
//     mean nothing).
//   - pl_dsp_report_window / the DSPPROG debug loader: thread context
//     (main.c's report cadence / debug_remote.c's poll), reading stats
//     the realtime side produced. Same racy-but-diagnostic-only discipline
//     as a2dp.c's own enc_sum_us/enc_count (see that struct's doc comment)
//     -- a torn read can only skew one report window, never corrupt state
//     anything else depends on.
//
// BYPASS IS STRUCTURAL (design sec 1.2): pl_dsp_rt_process() returns
// before touching a single sample when there is no active program and no
// crossfade in flight, so the OFF/no-preset output is bit-exact with
// main's pre-DSP behaviour -- this is the A/B arm every hardware
// measurement (ryw.2) compares against.
#ifndef PICO_LINK_DSP_H
#define PICO_LINK_DSP_H

#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// The realtime path is fixed at 48kHz -- codec_ldac.c:518. A program whose
// fs_hz doesn't match this is treated as inactive (bypassed), same as a
// program with zero biquads and crossfeed off -- see
// pl_dsp_program_is_active in dsp.c. 96k would double the per-block cost
// (design sec 1.2) and is out of scope.
#define PL_DSP_EXPECTED_FS_HZ 48000u

// Design sec 3.2. Matches the FFI struct ryw.5 will add to pico_link_ui.h;
// this bead defines it here in plain C so the kernel and its host test can
// exist before that FFI lands.
#define PL_DSP_MAX_BIQUADS 10u

// Design sec 1.3: the worst-case per-block DSP cost folded into the
// quiesce _Static_assert (a2dp.c, PL_A2DP_CORE1_FILL_BUDGET_US +
// PL_A2DP_WORST_CASE_ENCODE_US_MAX < PL_A2DP_QUIESCE_TIMEOUT_US). 300us is
// well above the ~135us design sec 1.3 estimates for 10 stereo bands plus
// crossfeed; ryw.2's M0 bench (gate: <=150us/block, stop above 250us) is
// the hardware check on this margin.
#define PL_DSP_WORST_CASE_US 300u

// One a0-normalised biquad in Transposed Direct Form II: a0 is implicitly
// 1 (already divided through), so the difference equation is
//   y[n] = b0*x[n] + z1
//   z1'  = b1*x[n] - a1*y[n] + z2
//   z2'  = b2*x[n] - a2*y[n]
// See pl_dsp_biquad_tdf2 in dsp.c. Matches the field order design sec 3.2
// specifies for the future FFI struct.
typedef struct {
    float b0;
    float b1;
    float b2;
    float a1;
    float a2;
} PlBiquad;

// A complete, self-contained DSP program: everything the realtime kernel
// needs to process one block, with no external state. Rust (ryw.3/ryw.5)
// builds these; this module only applies them. An all-zero PlDspProgram
// (the .bss-initial state of every static instance, and PL_DSP_PROGRAM_OFF
// below) is BYPASS -- n_biquads==0 and xfeed_on==0 -- see
// pl_dsp_program_is_active.
typedef struct {
    uint32_t fs_hz;
    float preamp; // linear gain, not dB -- Rust's job to convert
    uint8_t n_biquads; // 0..PL_DSP_MAX_BIQUADS
    uint8_t xfeed_on;
    // bs2b-style crossfeed (design sec 1.2): a first-order lowpass on the
    // cross path (y[n] = xfeed_lp_b0*x[n] + xfeed_lp_a1*y[n-1]) summed
    // into a first-order high-shelf on the direct path
    // (y[n] = xfeed_hs_b0*x[n] + xfeed_hs_b1*x[n-1] + xfeed_hs_a1*y[n-1]),
    // scaled by xfeed_gain and xfeed_norm. All Rust-computed; the kernel
    // just evaluates the two difference equations.
    float xfeed_lp_b0;
    float xfeed_lp_a1;
    float xfeed_gain;
    float xfeed_hs_b0;
    float xfeed_hs_b1;
    float xfeed_hs_a1;
    float xfeed_norm;
    PlBiquad biquad[PL_DSP_MAX_BIQUADS];
} PlDspProgram;

// --- core0 API (thread context / main superloop only) ---

// Publishes *p as the pending program. Coalesces: a submit that arrives
// before core1 has acknowledged the previous one OVERWRITES s_pending --
// only the newest submission before an ack ever reaches core1, so a d-pad
// burst of edits costs at most one publish (design sec 3.3). Calls
// pl_dsp_service() internally, so a bare pl_dsp_submit() is enough to
// attempt an immediate publish; callers that also want to retry a publish
// that a still-unacked previous bank blocked (e.g. every superloop
// iteration) should call pl_dsp_service() again on its own -- see that
// function.
void pl_dsp_submit(const PlDspProgram *p);

// Attempts to move s_pending into the free bank, if the previous publish
// has been acknowledged (s_ack_gen == s_pub_gen) and something is
// actually pending. Safe and cheap to call every superloop iteration with
// nothing pending (design sec 3.3's ownership argument: core0 only writes
// a bank core1 is guaranteed not to be reading).
void pl_dsp_service(void);

// Reads and resets the current report window's stats (same
// read-resets-the-window discipline as a2dp.c's enc_mean_us -- see that
// struct's doc comment). dsp_clip_count is ALSO windowed (design sec 1.3:
// "Add windowed dsp_mean_us, dsp_win_max_us and dsp_clip to
// pl_a2dp_report") -- a caller that wants a cumulative total should keep
// its own running sum across calls. Any pointer may be NULL if the caller
// doesn't want that value (it is still reset).
void pl_dsp_report_window(uint32_t *dsp_mean_us, uint32_t *dsp_win_max_us, uint32_t *dsp_clip_count);

// --- core1 (or legacy IRQ) API -- realtime, `_rt_`-prefixed, must never
// resolve into flash. Called ONLY from a2dp.c's fill path. ---

// One-time per-core1-launch setup: sets FPSCR.FZ (flush-to-zero) so a
// silent stream's IIR tails decay to exact zero instead of spending cycles
// on subnormal arithmetic (design sec 1.2). Call once from
// pl_a2dp_core1_entry, before the run loop -- NOT idempotent-safe to call
// from anywhere else (touches only this core's own FPSCR, which is
// per-core hardware state, so calling it from core0 would do nothing
// useful and calling it from core1 more than once is harmless but
// pointless).
void pl_dsp_rt_core1_init(void);

// Zeroes all filter state (biquad z1/z2 for both channels, crossfeed
// lowpass/shelf state) AND cancels any in-flight crossfade. Call on the
// entering-RUNNING edge (a2dp.c's `entering_running` branch in
// pl_a2dp_core1_entry) -- covers both a fresh stream start and a resume
// out of the WFE park, same edge pl_a2dp_resync_apply's caller uses
// (design sec 1.2: "zeroed on the entering-RUNNING edge"). Does NOT touch
// the active program itself, nor the bank handoff's generation counters --
// a program submitted while parked is still picked up by the next
// pl_dsp_rt_apply_pending() call after this reset.
void pl_dsp_rt_reset_state(void);

// Checks the bank handoff for a newer generation (s_pub_gen != s_ack_gen)
// and, if found, starts a one-block crossfade from the previously-active
// program to the new one (design sec 3.3's apply step) and resets the new
// program's filter state to silence (a freshly-applied program never
// inherits stale IIR history). Call exactly once per pl_a2dp_fill()
// invocation, before the per-unit loop that calls pl_dsp_rt_process() --
// same "once per fill, at the block boundary" placement as
// apply_pending_tuning (a2dp.c:1611-1613). Never blocks: reads two
// volatiles and, at most, copies one PlDspProgram plus zeroes one state
// struct, all core1-local.
void pl_dsp_rt_apply_pending(void);

// The kernel itself: processes `frame_count` interleaved stereo int16
// frames IN PLACE. Structural bypass (design sec 1.2): if there is no
// active program (bypass -- see pl_dsp_program_is_active in dsp.c) and no
// crossfade in flight, returns immediately without touching `pcm` -- the
// caller's int16 buffer is untouched, bit-exact with a build that never
// called this function at all. Otherwise converts to f32, applies preamp
// + crossfeed + the biquad cascade per design sec 1.2's ordering,
// saturates back to int16 (counting clips), all without allocating or
// calling into anything that could block.
//
// `pcm` must point at exactly `frame_count` stereo int16 frames
// (`frame_count * 2` int16 values) -- the same s_pcm_scratch buffer
// a2dp.c's fill loop already reads PCM into before encoding.
void pl_dsp_rt_process(int16_t *pcm, uint32_t frame_count);

// Adds one block's worth of already-measured DSP wall-clock time (t_dsp -
// t0 at the a2dp.c call site) into this window's running sum/count/max,
// for pl_dsp_report_window() to read later. Called once per unit from
// a2dp.c's fill loop, right after pl_dsp_rt_process() returns -- same
// producer-side timing convention as that loop's own enc_sum_us tracking.
void pl_dsp_rt_record_time(uint32_t dt_us);

#ifdef PL_DEBUG_REMOTE
// Debug-only canned program loader (design sec 1.4), compiled ONLY under
// PL_DEBUG_REMOTE (the hard constraint from this bead's description) --
// this entire function and the RBJ/bs2b coefficient math it uses to build
// the canned programs do not exist in a shipping build. Called from
// debug_remote.c's "DSPPROG <n>" command parser. Builds one of four fixed
// programs and pl_dsp_submit()s it:
//   0 = Off (bypass: 0 biquads, crossfeed off)
//   1 = crossfeed only
//   2 = 10 peaking bands + crossfeed (the worst-case bench arm)
//   3 = one +9dB low shelf, near-unity preamp (deliberate clip test)
// These are BENCH/ACCEPTANCE fixtures for ryw.2, not tuned presets -- the
// crossfeed strengths and band placement here are representative, not the
// by-ear-tuned values ryw.8 will pick. Returns false (and submits nothing)
// for n outside 0..3.
bool pl_dsp_debug_load_program(int n);
#endif

#ifdef __cplusplus
}
#endif

#endif // PICO_LINK_DSP_H
