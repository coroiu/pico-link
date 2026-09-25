// Pico Link firmware -- DSP effects fixed-rate stage implementation. See
// dsp.h's module doc for scope, ownership-per-core and the `_rt_` naming
// convention cmake/check_dsp_not_in_flash.cmake enforces.
#include "dsp.h"

#include <string.h>

#include "hardware/sync.h" // __dmb()
#include "pico/platform.h" // __not_in_flash_func

// __get_FPSCR/__set_FPSCR (pl_dsp_rt_core1_init's FZ bit) are CMSIS core
// intrinsics, not part of pico-sdk's own headers -- RP2350.h pulls in
// core_cm33.h/cmsis_gcc.h transitively. pico_stdlib (linked by every
// pico_link target) already depends on the CMSIS interface library, so
// this include needs no extra CMakeLists wiring. The host test build
// supplies a trivial stub (see firmware/tests' build comment for this
// file), same trick test_pcm_ring_cross_core.c uses for hardware/sync.h.
#include "RP2350.h"

#ifdef PL_DEBUG_REMOTE
// RBJ/bs2b coefficient math for the canned debug programs ONLY -- see
// dsp.h's doc comment on pl_dsp_debug_load_program for why this is the one
// place in this module allowed to turn a musical parameter (Hz, dB, Q)
// into a filter coefficient. Runs on core0/thread context (debug_remote.c
// calls this from its poll, never from core1), so libm is fine here even
// though the realtime kernel below needs none.
#include <math.h>
#endif

// ---------------------------------------------------------------------
// Per-channel filter state. One biquad state pair per band (TDF-II needs
// exactly z1/z2 -- no separate x-history), plus the crossfeed's own
// one-pole lowpass and one-pole-one-zero shelf state. Two full copies
// exist at any time only during a crossfade (s_active_state and
// s_old_state below) -- see pl_dsp_rt_apply_pending.
// ---------------------------------------------------------------------
typedef struct {
    float bq_l_z1[PL_DSP_MAX_BIQUADS];
    float bq_l_z2[PL_DSP_MAX_BIQUADS];
    float bq_r_z1[PL_DSP_MAX_BIQUADS];
    float bq_r_z2[PL_DSP_MAX_BIQUADS];
    float xfeed_lp_l_y1;
    float xfeed_lp_r_y1;
    float xfeed_hs_l_x1;
    float xfeed_hs_l_y1;
    float xfeed_hs_r_x1;
    float xfeed_hs_r_y1;
} pl_dsp_filter_state_t;

// ---------------------------------------------------------------------
// Bank handoff (design sec 3.3). All four of these are file-scope SRAM
// statics (never core1's 4KB stack, per design sec 1.2's memory rule) --
// .bss zero-init makes s_bank[*] both start as PL_DSP_PROGRAM_OFF-shaped
// (n_biquads==0, xfeed_on==0, fs_hz==0) without any explicit init code.
// ---------------------------------------------------------------------
static PlDspProgram s_bank[2];
static volatile uint32_t s_pub_gen;
static volatile uint32_t s_ack_gen;
static PlDspProgram s_pending;
static bool s_pending_valid;

// Core1-private active/old program + state. Never touched from core0 --
// see dsp.h's ownership-per-core doc comment. .bss zero-init gives a
// bypassed active program and silent state at boot, matching a build that
// never called pl_dsp_rt_apply_pending() at all.
static PlDspProgram s_active_program;
static pl_dsp_filter_state_t s_active_state;
static PlDspProgram s_old_program;
static pl_dsp_filter_state_t s_old_state;
static bool s_crossfade_pending;

// Report-window stats (design sec 1.3), producer on core1/IRQ
// (pl_dsp_rt_record_time / the saturate clip counter), consumer on thread
// context (pl_dsp_report_window) -- same racy-but-diagnostic-only
// discipline as a2dp.c's enc_sum_us block; see that struct's doc comment.
static volatile uint32_t s_dsp_sum_us;
static volatile uint32_t s_dsp_count;
static volatile uint32_t s_dsp_win_max_us;
static volatile uint32_t s_dsp_clip_count;

// ---------------------------------------------------------------------
// core0 API
// ---------------------------------------------------------------------

void pl_dsp_service(void) {
    if (s_pending_valid && s_ack_gen == s_pub_gen) {
        // Core1 can only be reading s_bank[s_pub_gen & 1] right now (it
        // switches banks only inside pl_dsp_rt_apply_pending, which
        // requires s_pub_gen to have already moved past whatever it last
        // acked) -- so the OTHER bank is safe to overwrite here. See
        // design sec 3.3's "why it is safe" paragraph.
        s_bank[(s_pub_gen + 1u) & 1u] = s_pending;
        __dmb();
        s_pub_gen++;
        s_pending_valid = false;
    }
}

void pl_dsp_submit(const PlDspProgram *p) {
    if (p == NULL) {
        return;
    }
    s_pending = *p;
    // Bead pico-link-ryw.5's CONTRACT comment: validate n_biquads at this
    // FFI boundary. `core` is the sole producer and its own preset model
    // caps a preset at PL_DSP_MAX_BIQUADS bands (dsp::preset::MAX_BANDS),
    // so this should never actually fire -- but n_biquads rides in from
    // across the FFI seam as a plain uint8_t with no compiler-enforced
    // bound, and pl_dsp_rt_process's per-block loop
    // (`for (i = 0; i < prog->n_biquads; i++) ... prog->biquad[i]`) would
    // otherwise read past the fixed 10-slot biquad[] array embedded in
    // this same struct -- not a wild out-of-bounds access (it reads
    // whatever bytes happen to follow within s_bank/s_pending), but
    // definitely not defined behaviour either. Clamp rather than trust.
    if (s_pending.n_biquads > PL_DSP_MAX_BIQUADS) {
        s_pending.n_biquads = PL_DSP_MAX_BIQUADS;
    }
    s_pending_valid = true;
    pl_dsp_service();
}

void pl_dsp_report_window(uint32_t *dsp_mean_us, uint32_t *dsp_win_max_us, uint32_t *dsp_clip_count) {
    // Snapshot sum/count together before resetting either, same ordering
    // a2dp.c's own enc_mean_us read uses, so a producer write racing this
    // read can only widen the window slightly, never divide by a count
    // that doesn't match the sum it's paired with.
    uint32_t sum_now = s_dsp_sum_us;
    uint32_t count_now = s_dsp_count;
    uint32_t win_max_now = s_dsp_win_max_us;
    uint32_t clip_now = s_dsp_clip_count;
    s_dsp_sum_us = 0;
    s_dsp_count = 0;
    s_dsp_win_max_us = 0;
    s_dsp_clip_count = 0;

    if (dsp_mean_us != NULL) {
        *dsp_mean_us = count_now > 0 ? sum_now / count_now : 0;
    }
    if (dsp_win_max_us != NULL) {
        *dsp_win_max_us = win_max_now;
    }
    if (dsp_clip_count != NULL) {
        *dsp_clip_count = clip_now;
    }
}

// ---------------------------------------------------------------------
// Realtime kernel (core1 / legacy IRQ only) -- every entry point below
// this line that touches audio state is __not_in_flash_func and named
// pl_dsp_rt_*. See cmake/check_dsp_not_in_flash.cmake.
// ---------------------------------------------------------------------

// True when `p` describes a program the kernel must actually run:
// nonzero work (a band or crossfeed) AND a sample rate that matches the
// realtime path (design sec 1.2: "the kernel bypasses on mismatch"). An
// all-.bss-zero PlDspProgram (n_biquads=0, xfeed_on=0, fs_hz=0) is
// therefore inactive by construction, with no explicit "Off" sentinel
// needed.
static inline bool pl_dsp_program_is_active(const PlDspProgram *p) {
    return (p->n_biquads > 0 || p->xfeed_on) && p->fs_hz == PL_DSP_EXPECTED_FS_HZ;
}

// One a0-normalised biquad, Transposed Direct Form II -- see PlBiquad's
// doc comment in dsp.h for the difference equation. `static inline`: this
// has no standalone symbol once inlined into its (own _rt_-prefixed)
// caller, so its machine code travels with the caller's SRAM placement
// rather than needing its own entry in the not-in-flash check.
static inline float pl_dsp_biquad_tdf2(const PlBiquad *bq, float *z1, float *z2, float x) {
    float y = bq->b0 * x + *z1;
    *z1 = bq->b1 * x - bq->a1 * y + *z2;
    *z2 = bq->b2 * x - bq->a2 * y;
    return y;
}

// Rounds to nearest and saturates to int16 range, counting every sample
// that had to clamp. Manual round-half-away-from-zero (not lrintf) so this
// stays libm-free, matching design sec 1.2's "core1 needs no libm".
static inline int16_t pl_dsp_saturate(float x, volatile uint32_t *clip_count) {
    if (x > 32767.0f) {
        (*clip_count)++;
        return 32767;
    }
    if (x < -32768.0f) {
        (*clip_count)++;
        return -32768;
    }
    float rounded = x >= 0.0f ? x + 0.5f : x - 0.5f;
    return (int16_t)rounded;
}

// One stereo sample through preamp -> crossfeed -> EQ cascade, per design
// sec 1.2's ordering ("Crossfeed and EQ commute when the EQ is identical
// L/R" -- this module only ever applies identical L/R EQ, so the order
// chosen here is a fixed convention, not a correctness requirement).
static inline void pl_dsp_process_sample(
    const PlDspProgram *prog, pl_dsp_filter_state_t *st, float in_l, float in_r, float *out_l, float *out_r
) {
    float l = in_l * prog->preamp;
    float r = in_r * prog->preamp;

    if (prog->xfeed_on) {
        // Direct path: first-order high shelf, difference-form-I
        // (y[n] = b0*x[n] + b1*x[n-1] + a1*y[n-1]).
        float direct_l = prog->xfeed_hs_b0 * l + prog->xfeed_hs_b1 * st->xfeed_hs_l_x1 + prog->xfeed_hs_a1 * st->xfeed_hs_l_y1;
        st->xfeed_hs_l_x1 = l;
        st->xfeed_hs_l_y1 = direct_l;
        float direct_r = prog->xfeed_hs_b0 * r + prog->xfeed_hs_b1 * st->xfeed_hs_r_x1 + prog->xfeed_hs_a1 * st->xfeed_hs_r_y1;
        st->xfeed_hs_r_x1 = r;
        st->xfeed_hs_r_y1 = direct_r;

        // Cross path: first-order lowpass on the OPPOSITE channel
        // (y[n] = b0*x[n] + a1*y[n-1]), fed from the OTHER channel's raw
        // (pre-shelf) preamped sample -- bs2b-style.
        float cross_from_r = prog->xfeed_lp_b0 * r + prog->xfeed_lp_a1 * st->xfeed_lp_r_y1;
        st->xfeed_lp_r_y1 = cross_from_r;
        float cross_from_l = prog->xfeed_lp_b0 * l + prog->xfeed_lp_a1 * st->xfeed_lp_l_y1;
        st->xfeed_lp_l_y1 = cross_from_l;

        l = (direct_l + prog->xfeed_gain * cross_from_r) * prog->xfeed_norm;
        r = (direct_r + prog->xfeed_gain * cross_from_l) * prog->xfeed_norm;
    }

    for (uint8_t i = 0; i < prog->n_biquads; i++) {
        const PlBiquad *bq = &prog->biquad[i];
        l = pl_dsp_biquad_tdf2(bq, &st->bq_l_z1[i], &st->bq_l_z2[i], l);
        r = pl_dsp_biquad_tdf2(bq, &st->bq_r_z1[i], &st->bq_r_z2[i], r);
    }

    *out_l = l;
    *out_r = r;
}

void __not_in_flash_func(pl_dsp_rt_core1_init)(void) {
    // Cortex-M33 FPSCR bit 24 is FZ (flush-to-zero): forces subnormal
    // results to zero instead of taking the (much slower) subnormal
    // microcode path. A silent stream's IIR state decays toward zero
    // asymptotically and would otherwise spend cycles on subnormals with
    // nothing audible to show for it -- design sec 1.2.
    uint32_t fpscr = __get_FPSCR();
    fpscr |= (1u << 24);
    __set_FPSCR(fpscr);
}

void __not_in_flash_func(pl_dsp_rt_reset_state)(void) {
    memset(&s_active_state, 0, sizeof(s_active_state));
    memset(&s_old_state, 0, sizeof(s_old_state));
    s_crossfade_pending = false;
}

void __not_in_flash_func(pl_dsp_rt_apply_pending)(void) {
    uint32_t g = s_pub_gen;
    if (g != s_ack_gen) {
        __dmb();
        s_old_program = s_active_program;
        s_old_state = s_active_state;
        s_active_program = s_bank[g & 1u];
        memset(&s_active_state, 0, sizeof(s_active_state));
        s_crossfade_pending = true;
        s_ack_gen = g;
    }
}

// One block's worth of the one-block linear crossfade (design sec 1.2):
// runs BOTH the old program (against its own, carried-over state) and the
// new program (against freshly-zeroed state) over the same input block,
// then mixes sample-by-sample with a 0->1 ramp. A program that was itself
// bypass (Off) on either side of the switch contributes its DRY input
// instead of running the kernel -- this is what makes an Off<->On
// transition click-free without a special case (design sec 1.2: "Off<->On
// uses the same path with a dry old side").
static void __not_in_flash_func(pl_dsp_rt_process_crossfade_block)(int16_t *pcm, uint32_t frame_count) {
    bool old_active = pl_dsp_program_is_active(&s_old_program);
    bool new_active = pl_dsp_program_is_active(&s_active_program);
    float denom = frame_count > 1u ? (float)(frame_count - 1u) : 1.0f;

    for (uint32_t i = 0; i < frame_count; i++) {
        float in_l = (float)pcm[2u * i];
        float in_r = (float)pcm[2u * i + 1u];

        float old_l = in_l;
        float old_r = in_r;
        if (old_active) {
            pl_dsp_process_sample(&s_old_program, &s_old_state, in_l, in_r, &old_l, &old_r);
        }

        float new_l = in_l;
        float new_r = in_r;
        if (new_active) {
            pl_dsp_process_sample(&s_active_program, &s_active_state, in_l, in_r, &new_l, &new_r);
        }

        float t = frame_count > 1u ? (float)i / denom : 1.0f;
        float mix_l = old_l * (1.0f - t) + new_l * t;
        float mix_r = old_r * (1.0f - t) + new_r * t;

        pcm[2u * i] = pl_dsp_saturate(mix_l, &s_dsp_clip_count);
        pcm[2u * i + 1u] = pl_dsp_saturate(mix_r, &s_dsp_clip_count);
    }
}

void __not_in_flash_func(pl_dsp_rt_process)(int16_t *pcm, uint32_t frame_count) {
    if (s_crossfade_pending) {
        pl_dsp_rt_process_crossfade_block(pcm, frame_count);
        s_crossfade_pending = false;
        return;
    }

    if (!pl_dsp_program_is_active(&s_active_program)) {
        // BYPASS IS STRUCTURAL (design sec 1.2): no program, no crossfade
        // -- `pcm` is never even read, so output is bit-exact with a
        // build that never called this function.
        return;
    }

    for (uint32_t i = 0; i < frame_count; i++) {
        float in_l = (float)pcm[2u * i];
        float in_r = (float)pcm[2u * i + 1u];
        float out_l;
        float out_r;
        pl_dsp_process_sample(&s_active_program, &s_active_state, in_l, in_r, &out_l, &out_r);
        pcm[2u * i] = pl_dsp_saturate(out_l, &s_dsp_clip_count);
        pcm[2u * i + 1u] = pl_dsp_saturate(out_r, &s_dsp_clip_count);
    }
}

void __not_in_flash_func(pl_dsp_rt_record_time)(uint32_t dt_us) {
    s_dsp_sum_us += dt_us;
    s_dsp_count++;
    if (dt_us > s_dsp_win_max_us) {
        s_dsp_win_max_us = dt_us;
    }
}

// ---------------------------------------------------------------------
// Debug-only canned program loader (design sec 1.4). See dsp.h's doc
// comment on pl_dsp_debug_load_program for scope; everything below this
// line does not exist in a shipping build.
// ---------------------------------------------------------------------
#ifdef PL_DEBUG_REMOTE

#define PL_DSP_DEBUG_PI 3.14159265358979323846f

// Standard RBJ Audio EQ Cookbook peaking-EQ formula (a0-normalised on the
// way out) -- the exact form design sec 1.4's program 2 (10 peaking bands)
// needs to be representative of a real preset's CPU cost. Runs on
// core0/thread context only; see this file's top-of-#ifdef comment on why
// libm is fine here.
static void pl_dsp_debug_rbj_peaking(float fs, float f0, float gain_db, float q, PlBiquad *out) {
    float a = powf(10.0f, gain_db / 40.0f);
    float w0 = 2.0f * PL_DSP_DEBUG_PI * f0 / fs;
    float alpha = sinf(w0) / (2.0f * q);
    float cos_w0 = cosf(w0);

    float b0 = 1.0f + alpha * a;
    float b1 = -2.0f * cos_w0;
    float b2 = 1.0f - alpha * a;
    float a0 = 1.0f + alpha / a;
    float a1 = -2.0f * cos_w0;
    float a2 = 1.0f - alpha / a;

    out->b0 = b0 / a0;
    out->b1 = b1 / a0;
    out->b2 = b2 / a0;
    out->a1 = a1 / a0;
    out->a2 = a2 / a0;
}

// RBJ low-shelf, used only by program 3's deliberate clip test.
static void pl_dsp_debug_rbj_low_shelf(float fs, float f0, float gain_db, float q, PlBiquad *out) {
    float a = powf(10.0f, gain_db / 40.0f);
    float w0 = 2.0f * PL_DSP_DEBUG_PI * f0 / fs;
    float alpha = sinf(w0) / (2.0f * q);
    float cos_w0 = cosf(w0);
    float sqrt_a = sqrtf(a);

    float b0 = a * ((a + 1.0f) - (a - 1.0f) * cos_w0 + 2.0f * sqrt_a * alpha);
    float b1 = 2.0f * a * ((a - 1.0f) - (a + 1.0f) * cos_w0);
    float b2 = a * ((a + 1.0f) - (a - 1.0f) * cos_w0 - 2.0f * sqrt_a * alpha);
    float a0 = (a + 1.0f) + (a - 1.0f) * cos_w0 + 2.0f * sqrt_a * alpha;
    float a1 = -2.0f * ((a - 1.0f) + (a + 1.0f) * cos_w0);
    float a2 = (a + 1.0f) + (a - 1.0f) * cos_w0 - 2.0f * sqrt_a * alpha;

    out->b0 = b0 / a0;
    out->b1 = b1 / a0;
    out->b2 = b2 / a0;
    out->a1 = a1 / a0;
    out->a2 = a2 / a0;
}

// Fills a representative (not tuned -- see dsp.h) bs2b-style crossfeed
// into `p`. The direct-path shelf is left as identity (b0=1, b1=0, a1=0):
// the real shelf coefficients are ryw.2/ryw.8's tuning job, and an
// identity shelf still exercises the crossfeed CPU cost and the
// structural sum/normalise path this bench needs to measure.
static void pl_dsp_debug_fill_crossfeed(PlDspProgram *p) {
    p->xfeed_on = 1;
    float fc = 700.0f; // representative bs2b-ish cutoff
    float x = expf(-2.0f * PL_DSP_DEBUG_PI * fc / (float)PL_DSP_EXPECTED_FS_HZ);
    p->xfeed_lp_b0 = 1.0f - x;
    p->xfeed_lp_a1 = x;
    p->xfeed_hs_b0 = 1.0f;
    p->xfeed_hs_b1 = 0.0f;
    p->xfeed_hs_a1 = 0.0f;
    p->xfeed_gain = 0.3f;
    p->xfeed_norm = 1.0f / (1.0f + p->xfeed_gain);
}

bool pl_dsp_debug_load_program(int n) {
    PlDspProgram p;
    memset(&p, 0, sizeof(p));
    p.fs_hz = PL_DSP_EXPECTED_FS_HZ;

    switch (n) {
        case 0:
            // Off: all-zero is already bypass (n_biquads=0, xfeed_on=0).
            p.preamp = 1.0f;
            break;
        case 1:
            p.preamp = 0.5f; // -6dB, legacy fixed headroom (auto preamp is Rust's job)
            pl_dsp_debug_fill_crossfeed(&p);
            break;
        case 2: {
            p.preamp = 0.5f;
            pl_dsp_debug_fill_crossfeed(&p);
            p.n_biquads = PL_DSP_MAX_BIQUADS;
            static const float freqs[PL_DSP_MAX_BIQUADS] = {31.0f, 62.0f, 125.0f, 250.0f, 500.0f, 1000.0f,
                                                              2000.0f, 4000.0f, 8000.0f, 16000.0f};
            for (uint8_t i = 0; i < PL_DSP_MAX_BIQUADS; i++) {
                pl_dsp_debug_rbj_peaking((float)PL_DSP_EXPECTED_FS_HZ, freqs[i], 3.0f, 1.0f, &p.biquad[i]);
            }
            break;
        }
        case 3:
            p.preamp = 0.95f; // near-unity, deliberately leaves little headroom
            p.n_biquads = 1;
            pl_dsp_debug_rbj_low_shelf((float)PL_DSP_EXPECTED_FS_HZ, 150.0f, 9.0f, 0.707f, &p.biquad[0]);
            break;
        default:
            return false;
    }

    pl_dsp_submit(&p);
    return true;
}

#endif // PL_DEBUG_REMOTE
