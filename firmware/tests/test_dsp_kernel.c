// Pico Link firmware -- host-buildable test for bead pico-link-ryw.1 (C DSP
// engine, design .planning/design/2026-09-25-dsp-effects-stage.md sec 3.3's
// closing list: "bank protocol invariants, bypass bit-exactness, biquad
// impulse response vs a Rust-emitted coefficient fixture, saturation and
// the clip count, crossfade endpoints").
//
// Links the REAL firmware/src/dsp.c unmodified, same convention as
// test_pcm_ring_cross_core.c. dsp.c's only non-libc dependencies are
// hardware/sync.h's __dmb(), pico/platform.h's __not_in_flash_func macro,
// and RP2350.h's __get_FPSCR/__set_FPSCR (CMSIS FPU intrinsics, used only
// by pl_dsp_rt_core1_init, which this test does not call) -- none of which
// have any observable effect on a single-threaded host build, so trivial
// stubs are correct here, not just convenient.
//
// pico-link-6cho note (see this bead's dispatch): dsp.c DOES link cleanly
// on host with these three one-line stub headers -- there was no case here
// where "cannot link the real .c file" forced testing a copy.
//
// Build + run (no CMake target exists for this -- same standalone-host-
// binary convention as test_pcm_ring_cross_core.c / test_codec_id_stability.c):
//   mkdir -p /tmp/hostinc/hardware /tmp/hostinc/pico
//   printf 'static inline void __dmb(void) { __sync_synchronize(); }\n' \
//     > /tmp/hostinc/hardware/sync.h
//   printf '#define __not_in_flash_func(f) f\n' > /tmp/hostinc/pico/platform.h
//   printf 'static inline unsigned int __get_FPSCR(void) { return 0; }\n' \
//     > /tmp/hostinc/RP2350.h
//   printf 'static inline void __set_FPSCR(unsigned int v) { (void)v; }\n' \
//     >> /tmp/hostinc/RP2350.h
//   cc -std=c11 -Wall -Wextra -DPL_DEBUG_REMOTE -I firmware/src -I /tmp/hostinc \
//      firmware/tests/test_dsp_kernel.c firmware/src/dsp.c -lm \
//      -o /tmp/test_dsp_kernel && /tmp/test_dsp_kernel
//
// What this proves: the bank handoff's coalesce-before-ack and
// apply-at-block-boundary protocol; that an Off/no-program kernel call
// never mutates its buffer (the A/B bypass arm every hardware measurement
// in ryw.2 depends on); that a single identity biquad is a no-op and a
// known-gain biquad matches its closed-form output; that saturation clamps
// and counts correctly; and that a crossfade block's first sample is
// (approximately) the old program's output and its last sample is
// (approximately) the new program's. What it does NOT prove: real-time
// behaviour on actual hardware (timing, FPU flush-to-zero, cross-core
// memory ordering under a real multicore scheduler) -- that is ryw.2's
// hardware gate, out of scope here.
#include <assert.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "dsp.h"

static void reset_all(void) {
    pl_dsp_rt_reset_state();
    // Drain any pending bank handoff left over from a previous test case
    // by publishing an explicit Off program and letting apply_pending
    // consume it -- keeps each test case's starting state independent.
    PlDspProgram off;
    memset(&off, 0, sizeof(off));
    off.fs_hz = PL_DSP_EXPECTED_FS_HZ;
    off.preamp = 1.0f;
    pl_dsp_submit(&off);
    pl_dsp_rt_apply_pending();
    // apply_pending always starts a crossfade; consume it with a silent
    // block so the NEXT test's first real process() call sees no pending
    // crossfade.
    int16_t silence[8] = {0};
    pl_dsp_rt_process(silence, 4);
    pl_dsp_rt_reset_state();
}

static void test_bypass_bit_exact(void) {
    reset_all();
    int16_t pcm[8] = {100, -200, 300, -400, 500, -600, 700, -800};
    int16_t before[8];
    memcpy(before, pcm, sizeof(pcm));
    pl_dsp_rt_process(pcm, 4);
    assert(memcmp(pcm, before, sizeof(pcm)) == 0);
    printf("test_bypass_bit_exact: OK\n");
}

static void test_bank_protocol_coalesces(void) {
    reset_all();

    // n_biquads must be nonzero on BOTH programs here: pl_dsp_program_is_
    // active() (design sec 1.2's bypass definition) is "0 biquads AND
    // crossfeed off", independent of preamp -- a preamp-only, zero-band
    // program is bypass BY DESIGN (a real Rust-built program never emits
    // one: the auto preamp is unity whenever there is nothing to make
    // headroom for). Use a distinguishing single gain biquad on each so
    // this test actually exercises "which generation is active", not an
    // accidental bypass.
    PlDspProgram a;
    memset(&a, 0, sizeof(a));
    a.fs_hz = PL_DSP_EXPECTED_FS_HZ;
    a.preamp = 1.0f;
    a.n_biquads = 1;
    a.biquad[0].b0 = 0.5f; // gain-of-0.5

    PlDspProgram b = a;
    b.biquad[0].b0 = 0.25f; // gain-of-0.25

    // pl_dsp_submit calls pl_dsp_service() itself. The FIRST submit's
    // publish succeeds immediately (ack==pub at rest: a lands in the free
    // bank and s_pub_gen advances). The SECOND submit's publish is then
    // blocked (ack != pub, core1 hasn't applied a's generation yet), so b
    // sits in s_pending, unconsumed -- design sec 3.3: "An unacked submit
    // waits in s_pending, and a newer one overwrites it." There is only
    // ever one s_pending slot, so a third submit here would overwrite b
    // with no trace of a second coalesce; not exercised separately since
    // the mechanism is the same overwrite either way.
    pl_dsp_submit(&a);
    pl_dsp_submit(&b);

    // core1 applies a's generation and crossfades into it.
    pl_dsp_rt_apply_pending();
    int16_t block1[8] = {1000, 1000, 1000, 1000, 1000, 1000, 1000, 1000};
    pl_dsp_rt_process(block1, 4);

    // Now that ack==pub again (a's generation was just applied), a fresh
    // pl_dsp_service() call -- the same one main.c's superloop would make
    // every iteration regardless of whether a submit happened that
    // iteration -- flushes the still-pending b into the free bank.
    pl_dsp_service();
    pl_dsp_rt_apply_pending();
    int16_t block2[8] = {1000, 1000, 1000, 1000, 1000, 1000, 1000, 1000};
    pl_dsp_rt_process(block2, 4); // consumes b's crossfade too

    // Steady state (no crossfade in flight) must now reflect b, not a:
    // b.preamp = 0.25, no biquads/crossfeed -> every sample is 1000*0.25 = 250.
    int16_t block3[8] = {1000, 1000, 1000, 1000, 1000, 1000, 1000, 1000};
    pl_dsp_rt_process(block3, 4);
    for (int i = 0; i < 8; i++) {
        assert(block3[i] == 250);
    }
    printf("test_bank_protocol_coalesces: OK\n");
}

static void test_identity_biquad_is_noop(void) {
    reset_all();
    PlDspProgram p;
    memset(&p, 0, sizeof(p));
    p.fs_hz = PL_DSP_EXPECTED_FS_HZ;
    p.preamp = 1.0f;
    p.n_biquads = 1;
    p.biquad[0].b0 = 1.0f; // identity: y[n] = x[n]
    p.biquad[0].b1 = 0.0f;
    p.biquad[0].b2 = 0.0f;
    p.biquad[0].a1 = 0.0f;
    p.biquad[0].a2 = 0.0f;
    pl_dsp_submit(&p);
    pl_dsp_rt_apply_pending();

    int16_t warmup[8] = {5, -5, 10, -10, 15, -15, 20, -20};
    pl_dsp_rt_process(warmup, 4); // absorb the one-block crossfade
    pl_dsp_rt_apply_pending(); // no-op, nothing new pending

    int16_t pcm[8] = {100, -200, 300, -400, 500, -600, 700, -800};
    int16_t before[8];
    memcpy(before, pcm, sizeof(pcm));
    pl_dsp_rt_process(pcm, 4);
    for (int i = 0; i < 8; i++) {
        assert(pcm[i] == before[i]);
    }
    printf("test_identity_biquad_is_noop: OK\n");
}

static void test_known_gain_biquad_matches_closed_form(void) {
    reset_all();
    PlDspProgram p;
    memset(&p, 0, sizeof(p));
    p.fs_hz = PL_DSP_EXPECTED_FS_HZ;
    p.preamp = 1.0f;
    p.n_biquads = 1;
    // A pure gain-of-2 "biquad" (b0=2, everything else 0) has a trivial
    // closed form: y[n] = 2*x[n], independent of history.
    p.biquad[0].b0 = 2.0f;
    p.biquad[0].b1 = 0.0f;
    p.biquad[0].b2 = 0.0f;
    p.biquad[0].a1 = 0.0f;
    p.biquad[0].a2 = 0.0f;
    pl_dsp_submit(&p);
    pl_dsp_rt_apply_pending();

    int16_t warmup[8] = {0};
    pl_dsp_rt_process(warmup, 4);
    pl_dsp_rt_apply_pending();

    int16_t pcm[4] = {100, -200, 300, -400};
    pl_dsp_rt_process(pcm, 2);
    assert(pcm[0] == 200);
    assert(pcm[1] == -400);
    assert(pcm[2] == 600);
    assert(pcm[3] == -800);
    printf("test_known_gain_biquad_matches_closed_form: OK\n");
}

static void test_saturation_clips_and_counts(void) {
    reset_all();
    PlDspProgram p;
    memset(&p, 0, sizeof(p));
    p.fs_hz = PL_DSP_EXPECTED_FS_HZ;
    p.preamp = 1.0f;
    p.n_biquads = 1;
    p.biquad[0].b0 = 4.0f; // gain-of-4, guaranteed to clip near full-scale input
    pl_dsp_submit(&p);
    pl_dsp_rt_apply_pending();

    int16_t warmup[8] = {0};
    pl_dsp_rt_process(warmup, 4);
    pl_dsp_rt_apply_pending();

    // Drain any clip count the crossfade block itself produced, so this
    // case's assertion is about ITS OWN block only.
    pl_dsp_report_window(NULL, NULL, NULL);

    int16_t pcm[4] = {20000, -20000, 100, -100};
    pl_dsp_rt_process(pcm, 2);
    assert(pcm[0] == 32767);
    assert(pcm[1] == -32768);
    assert(pcm[2] == 400);
    assert(pcm[3] == -400);

    uint32_t clip_count = 0;
    pl_dsp_report_window(NULL, NULL, &clip_count);
    assert(clip_count == 2);
    printf("test_saturation_clips_and_counts: OK\n");
}

static void test_crossfade_endpoints(void) {
    reset_all();

    // Old program: silence-passthrough Off (bypass).
    // New program: gain-of-2, no crossfeed.
    PlDspProgram new_prog;
    memset(&new_prog, 0, sizeof(new_prog));
    new_prog.fs_hz = PL_DSP_EXPECTED_FS_HZ;
    new_prog.preamp = 1.0f;
    new_prog.n_biquads = 1;
    new_prog.biquad[0].b0 = 2.0f;

    pl_dsp_submit(&new_prog);
    pl_dsp_rt_apply_pending(); // starts the crossfade: old=Off (dry), new=gain-of-2

    int16_t pcm[64];
    for (int i = 0; i < 32; i++) {
        pcm[2 * i] = 1000;
        pcm[2 * i + 1] = 1000;
    }
    pl_dsp_rt_process(pcm, 32);

    // First sample: t=0, mix should be ~100% old (dry passthrough) = 1000.
    assert(pcm[0] == 1000);
    // Last sample: t=1, mix should be ~100% new (gain-of-2) = 2000.
    assert(pcm[62] == 2000);
    printf("test_crossfade_endpoints: OK\n");
}

static void test_report_window_resets(void) {
    reset_all();
    pl_dsp_rt_record_time(100);
    pl_dsp_rt_record_time(200);
    uint32_t mean = 0;
    uint32_t win_max = 0;
    pl_dsp_report_window(&mean, &win_max, NULL);
    assert(mean == 150);
    assert(win_max == 200);

    // A second read with nothing recorded in between must report zeros --
    // proves the window actually reset, not just that it can compute a
    // mean once.
    pl_dsp_report_window(&mean, &win_max, NULL);
    assert(mean == 0);
    assert(win_max == 0);
    printf("test_report_window_resets: OK\n");
}

// Bead pico-link-ryw.5's CONTRACT comment: pl_dsp_submit must clamp
// n_biquads at the FFI boundary rather than trust a value that could crash
// or misbehave when it walks past the fixed 10-slot biquad[] array. Sets
// n_biquads to 255 (an obviously-malformed value no real Rust producer
// would send, since core's own preset model caps at PL_DSP_MAX_BIQUADS
// bands) with only the first 10 biquad slots meaningfully populated (a
// gain-of-0.5 stage followed by 9 identity stages) -- if the clamp landed
// at exactly PL_DSP_MAX_BIQUADS (not one more, not one fewer), processing
// a block is both crash-free AND numerically exact: 0.5 gain through 9
// identity stages is still exactly *0.5.
static void test_submit_clamps_out_of_range_n_biquads(void) {
    reset_all();
    PlDspProgram p;
    memset(&p, 0, sizeof(p));
    p.fs_hz = PL_DSP_EXPECTED_FS_HZ;
    p.preamp = 1.0f;
    p.n_biquads = 255; // malformed -- far past PL_DSP_MAX_BIQUADS (10)
    p.biquad[0].b0 = 0.5f; // gain-of-0.5
    for (uint32_t i = 1; i < PL_DSP_MAX_BIQUADS; i++) {
        p.biquad[i].b0 = 1.0f; // identity
    }
    pl_dsp_submit(&p);
    pl_dsp_rt_apply_pending();

    int16_t warmup[8] = {0};
    pl_dsp_rt_process(warmup, 4);
    pl_dsp_rt_apply_pending();

    int16_t pcm[4] = {1000, -2000, 4000, -8000};
    pl_dsp_rt_process(pcm, 2);
    assert(pcm[0] == 500);
    assert(pcm[1] == -1000);
    assert(pcm[2] == 2000);
    assert(pcm[3] == -4000);
    printf("test_submit_clamps_out_of_range_n_biquads: OK\n");
}

// Bead pico-link-ryw.5's CONTRACT comment: the exact field mapping from
// core::dsp::coeffs::CrossfeedCoeffs to PlDspProgram (lo_b0 -> xfeed_lp_b0,
// lo_a1 -> xfeed_lp_a1, hi_b0/hi_b1/hi_a1 -> xfeed_hs_b0/b1/a1, norm_gain ->
// xfeed_norm, xfeed_gain ALWAYS 1.0). The coefficients below are
// core::dsp::coeffs::crossfeed_coeffs(CrossfeedLevel::Weak, 48000)'s real
// output (captured via a one-off `cargo test -p pico-link-core --lib
// dsp::tests -- --nocapture` run against a temporary print, per this
// bead's dispatch -- see the bead's completion comment) and the
// xfeed_process reference outputs are core::dsp::coeffs::CrossfeedCoeffs::
// process()'s own per-sample reference application of those SAME
// coefficients (the function core's own coeffs.rs tests trust) -- so this
// test proves the C kernel, fed the CONTRACT-mapped fields, reproduces
// core's reference response, not just that a copy is a copy.
static void test_ryw5_crossfeed_field_mapping_matches_rust_reference(void) {
    reset_all();
    PlDspProgram p;
    memset(&p, 0, sizeof(p));
    p.fs_hz = PL_DSP_EXPECTED_FS_HZ;
    p.preamp = 1.0f;
    p.n_biquads = 0;
    p.xfeed_on = 1;
    p.xfeed_lp_b0 = 0.04025238f; // lo_b0
    p.xfeed_lp_a1 = 0.91244286f; // lo_a1
    p.xfeed_gain = 1.0f; // CONTRACT: always 1.0, never derived
    p.xfeed_hs_b0 = 0.97213835f; // hi_b0
    p.xfeed_hs_b1 = -0.87791145f; // hi_b1
    p.xfeed_hs_a1 = 0.87791145f; // hi_a1
    p.xfeed_norm = 0.81200564f; // norm_gain
    pl_dsp_submit(&p);
    pl_dsp_rt_apply_pending();

    // Absorb the one-block crossfade with silence so it doesn't perturb
    // the steady-state samples asserted on below.
    int16_t warmup[8] = {0};
    pl_dsp_rt_process(warmup, 4);
    pl_dsp_rt_apply_pending();

    // core::dsp::coeffs::CrossfeedCoeffs::process()'s own reference output
    // for these four stereo input pairs, fed through the SAME coefficients
    // above (see this function's doc comment for provenance).
    int16_t pcm[8] = {9000, -6000, 3000, 3000, -1500, 6000, 0, 0};
    pl_dsp_rt_process(pcm, 4);

    // Rounded to nearest int16 (round-half-away-from-zero, matching
    // pl_dsp_saturate) from the f32 reference outputs:
    //   [9000,-6000]   -> [6908.325,  -4442.1245]  -> [6908,  -4442]
    //   [3000, 3000]   -> [2108.5054,  2853.7813]  -> [2109,   2854]
    //   [-1500,6000]   -> [-1278.281,  5066.6777]  -> [-1278,  5067]
    //   [0, 0]         -> [-48.69039,   180.73375] -> [-49,     181]
    assert(pcm[0] == 6908);
    assert(pcm[1] == -4442);
    assert(pcm[2] == 2109);
    assert(pcm[3] == 2854);
    assert(pcm[4] == -1278);
    assert(pcm[5] == 5067);
    assert(pcm[6] == -49);
    assert(pcm[7] == 181);
    printf("test_ryw5_crossfeed_field_mapping_matches_rust_reference: OK\n");
}

#ifdef PL_DEBUG_REMOTE
static void test_debug_loader_off_is_bypass(void) {
    reset_all();
    assert(pl_dsp_debug_load_program(0));
    pl_dsp_rt_apply_pending();
    int16_t warmup[8] = {0};
    pl_dsp_rt_process(warmup, 4); // absorb the crossfade into silence

    int16_t pcm[8] = {11, -22, 33, -44, 55, -66, 77, -88};
    int16_t before[8];
    memcpy(before, pcm, sizeof(pcm));
    pl_dsp_rt_process(pcm, 4);
    assert(memcmp(pcm, before, sizeof(pcm)) == 0);

    assert(!pl_dsp_debug_load_program(4)); // out of range
    assert(!pl_dsp_debug_load_program(-1));
    printf("test_debug_loader_off_is_bypass: OK\n");
}
#endif

int main(void) {
    test_bypass_bit_exact();
    test_bank_protocol_coalesces();
    test_identity_biquad_is_noop();
    test_known_gain_biquad_matches_closed_form();
    test_saturation_clips_and_counts();
    test_crossfade_endpoints();
    test_report_window_resets();
    test_submit_clamps_out_of_range_n_biquads();
    test_ryw5_crossfeed_field_mapping_matches_rust_reference();
#ifdef PL_DEBUG_REMOTE
    test_debug_loader_off_is_bypass();
#endif
    printf("all tests passed\n");
    return 0;
}
