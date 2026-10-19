// Pico Link firmware -- the drain-rate PI servo. See drain_servo.h's module
// doc for the design pointer, the actuator change (host feedback -> A2DP
// credit clock) and the sign-flip rationale.
//
// This is usb_audio.c's former pl_usb_audio_feedback_task PI loop, MOVED
// (not rewritten) -- same EMA shift (>>6, ~64ms time constant against the
// a2dp.c media timer's ~11ms drain cadence, see the original bead
// pico-link-pbv doc comment this carries forward), same Kp/Ki gains, same
// anti-windup-on-the-accumulator discipline. What changed: the actuator
// (tud_audio_fb_set -> pl_a2dp_accrue_credit's extra-samples term), the
// sign of the P/I terms (ring high now means "drain faster", not "ask host
// to send faster"), the output clamp (500 -> 1000ppm), and the integral
// reset (pl_drain_servo_reset no longer zeroes the I term -- it reseeds
// from a learned offset that survives the reset, see below).
//
// Context/locking: pl_drain_servo_tick() runs from the 0xC0 worker IRQ
// (usb_pump.c), the SAME context and cadence pl_usb_audio_feedback_task
// used to run from -- see that file's own doc comment on why this is safe
// relative to tud_task(). pl_drain_servo_reset() runs from a2dp.c's
// STREAM_STARTED handler and resync-trim COMPLETE phase, same two call
// sites and same cross-context precedent pl_usb_audio_fb_reset already
// established (plain statics, not volatile, because these resets are rare
// discrete events against the ~1ms tick, exactly the reasoning that
// function's own history already settled). pl_drain_servo_ppm() is read by
// pl_a2dp_accrue_credit, which under PL_ENCODER_ON_CORE1 runs on CORE1 --
// s_servo_ppm is therefore the one field in this file that IS volatile and
// single-writer (this file's tick(), core0 only), matching design sec 2b's
// "One aligned volatile int32, single writer, no lock."
#include "drain_servo.h"

#include <stdbool.h>

#include "pcm_ring.h"

// Bead pico-link-nxf (carried forward): PROPORTIONAL GAIN, kept numerically
// identical to the old loop's PL_FB_KP_PPM so the plant/gain relationship
// (+/-500ppm moves the ring at ~96 B/s) that was tuned against hardware
// still holds with the new actuator.
#define PL_DRAIN_SERVO_KP_PPM 500

// Integral scale, carried forward unchanged from PL_FB_KI_DIV -- same ~11s
// convergence at a 222ppm crystal offset, measured on hardware under the
// old loop.
#define PL_DRAIN_SERVO_KI_DIV 100000

// Bead pico-link-ycm2.7.1, design sec 2b: widened from the old loop's
// 500ppm -- that bound was a USB feedback-format practicality (the 10.14
// fixed-point field's class-defined deviation range), not a plant limit,
// and the design calls for +/-1000ppm now that the actuator is purely
// internal arithmetic with no USB wire format to fit.
#define PL_DRAIN_SERVO_MAX_PPM 1000

// Anti-windup bound on the integral accumulator alone (same discipline the
// old loop used): the accumulator is clamped so its own ppm contribution
// can never exceed the total output clamp on its own.
#define PL_DRAIN_SERVO_I_ACCUM_MAX ((int32_t)PL_DRAIN_SERVO_MAX_PPM * PL_DRAIN_SERVO_KI_DIV)

// ~1Hz gate for the learned-offset snapshot below, expressed at this
// function's own ~1ms call cadence -- same cadence assumption the EMA's
// >>6 shift already makes (see this file's module doc).
#define PL_DRAIN_SERVO_LEARN_GATE_TICKS 1000u

// --- State written only by pl_drain_servo_tick() (core0, the 0xC0 worker
// IRQ), read by pl_drain_servo_reset()/the accessors below from whatever
// context calls them -- see this file's module doc for why plain statics
// (not volatile) are the correct, already-established convention here. ---
static int32_t s_fill_ema;
static uint32_t s_fill_min = 0xFFFFFFFFu;
static int32_t s_i_accum;
static int32_t s_learned_ppm;
static uint32_t s_learn_gate_counter;

// Bead pico-link-9eq2.3.2 precedent (fb_rail_ticks): cumulative, read from
// thread context (pl_a2dp_report) -- volatile for the same single-aligned-
// word "benign race, old or new value, never a tear" convention that
// counter used.
static volatile uint32_t s_rail_ticks;

// Design sec 2b: "One aligned volatile int32, single writer, no lock" --
// written only here, read by pl_a2dp_accrue_credit (core0 under the OFF
// build, core1 under PL_ENCODER_ON_CORE1).
static volatile int32_t s_servo_ppm;

// Carried fixed-point remainder for pl_drain_servo_accrue_extra_samples
// below -- see that function's doc comment for why a remainder MUST be
// carried (a realistic ppm correction is sub-sample-per-tick and would
// truncate to zero forever otherwise). Single-writer: that function is
// only ever called from pl_a2dp_accrue_credit, in the one context allowed
// to run the credit-clock fill loop for a given build (a2dp.c's own
// module doc on samples_owed/samples_owed_rem_us makes the identical
// argument for that function's state).
static int64_t s_accrue_rem_carry;

void pl_drain_servo_tick(void) {
    uint32_t fill_now = pl_pcm_fill_bytes();
    if (fill_now < s_fill_min) {
        s_fill_min = fill_now;
    }
    s_fill_ema += ((int32_t)fill_now - s_fill_ema) >> 6;

    // Guarded by the same pl_usb_audio_streaming() check at both call
    // sites (usb_pump.c) the old loop had -- target_bytes is already
    // negotiated (STREAM_ESTABLISHED precedes STREAM_STARTED) by the time
    // this ever runs, so no divide-by-zero guard is needed here, same as
    // the old loop never needed one.
    int32_t target_bytes = (int32_t)pl_pcm_target_fill_bytes();
    int32_t err_bytes = s_fill_ema - target_bytes;

    // Sign FLIP from the old feedback loop (design sec 2b): ring high
    // (err positive) now means "drain FASTER" (ppm positive), not "ask the
    // host to send faster" -- the old loop's leading minus is gone.
    int32_t p_ppm = (err_bytes * PL_DRAIN_SERVO_KP_PPM) / target_bytes;

    // Integrate (sign-flipped: += not -=, matching the P term's flip),
    // then clamp the accumulator itself -- clamping only the output is the
    // classic windup bug (same reasoning the old loop's own comment gave).
    s_i_accum += err_bytes;
    bool rail_this_tick = false;
    if (s_i_accum > PL_DRAIN_SERVO_I_ACCUM_MAX) {
        s_i_accum = PL_DRAIN_SERVO_I_ACCUM_MAX;
        rail_this_tick = true;
    }
    if (s_i_accum < -PL_DRAIN_SERVO_I_ACCUM_MAX) {
        s_i_accum = -PL_DRAIN_SERVO_I_ACCUM_MAX;
        rail_this_tick = true;
    }
    int32_t ppm = p_ppm + (s_i_accum / PL_DRAIN_SERVO_KI_DIV);
    if (ppm > PL_DRAIN_SERVO_MAX_PPM) {
        ppm = PL_DRAIN_SERVO_MAX_PPM;
        rail_this_tick = true;
    }
    if (ppm < -PL_DRAIN_SERVO_MAX_PPM) {
        ppm = -PL_DRAIN_SERVO_MAX_PPM;
        rail_this_tick = true;
    }
    if (rail_this_tick) {
        s_rail_ticks++;
    }
    s_servo_ppm = ppm;

    // Learned-offset snapshot (design sec 2b, "Required details: learned-
    // offset reseed"): only while NOT railed and the EMA sits close to
    // target (within target/4) -- a cushion hold winds the integral toward
    // rail exactly like the old loop's did (design sec 5's own warning),
    // and snapshotting mid-hold would learn a transient, not the
    // steady-state crystal offset this is meant to capture.
    if (++s_learn_gate_counter >= PL_DRAIN_SERVO_LEARN_GATE_TICKS) {
        s_learn_gate_counter = 0;
        if (!rail_this_tick) {
            int32_t abs_err = err_bytes < 0 ? -err_bytes : err_bytes;
            if (abs_err < target_bytes / 4) {
                s_learned_ppm = ppm;
            }
        }
    }
}

int32_t pl_drain_servo_accrue_extra_samples(uint32_t elapsed_us, uint32_t sample_rate_hz) {
    int32_t ppm = s_servo_ppm; // single snapshot read -- see this file's module doc
    if (ppm == 0 && s_accrue_rem_carry == 0) {
        return 0; // common case (servo at rest, no carried debt): skip the 64-bit math entirely
    }
    // accrual = sample_rate_hz * (1 + ppm/1e6) (design sec 2b) -- the base
    // (ppm==0) term is computed separately by the caller (a2dp.c's existing
    // owed_us_hz math); this function returns ONLY the extra term:
    // elapsed_us * sample_rate_hz * ppm / 1e12 (the 1e6 of ppm and the 1e6
    // of us->s each contribute a factor of 1e6 to the divisor). Worst-case
    // magnitude (elapsed_us ~1e6 [a full second, generous], sample_rate_hz
    // 48000, ppm 1000) is ~4.8e13 plus a sub-1e12 carry -- comfortably
    // inside int64_t (max ~9.22e18).
    int64_t numer = (int64_t)elapsed_us * (int64_t)sample_rate_hz * (int64_t)ppm + s_accrue_rem_carry;
    const int64_t divisor = 1000000000000LL; // 1e12 = 1e6 (ppm) * 1e6 (us -> s)
    int64_t extra = numer / divisor;
    int64_t rem = numer % divisor;
    // Floor division, not truncation-toward-zero: C's / and % truncate
    // toward zero, which for a negative numer would silently round the
    // extra-samples term UP (toward zero) every time and leak the
    // remainder in the wrong direction. Normalize to a non-negative
    // remainder in [0, divisor) instead, so a negative servo_ppm's
    // correction is carried with the same sub-ppm precision a positive
    // one gets.
    if (rem < 0) {
        rem += divisor;
        extra -= 1;
    }
    s_accrue_rem_carry = rem;
    return (int32_t)extra;
}

int32_t pl_drain_servo_ppm(void) {
    return s_servo_ppm;
}

int32_t pl_drain_servo_learned_ppm(void) {
    return s_learned_ppm;
}

int32_t pl_drain_servo_fill_ema(void) {
    return s_fill_ema;
}

uint32_t pl_drain_servo_fill_min(void) {
    uint32_t result = s_fill_min;
    s_fill_min = 0xFFFFFFFFu;
    return result;
}

uint32_t pl_drain_servo_rail_ticks(void) {
    return s_rail_ticks;
}

void pl_drain_servo_reset(void) {
    s_fill_ema = (int32_t)pl_pcm_fill_bytes();
    s_fill_min = 0xFFFFFFFFu;
    // Unlike the old pl_usb_audio_fb_reset, the integral accumulator is
    // reseeded from the learned offset, not zeroed (design sec 2b,
    // "Required details"): carrying the learned crystal-offset estimate
    // across a trim or stream restart avoids re-walking the ~11s P-only
    // convergence (and the measured dry-ring regime it produces) every
    // single time.
    s_i_accum = (int32_t)s_learned_ppm * PL_DRAIN_SERVO_KI_DIV;
    s_learn_gate_counter = 0;
    s_accrue_rem_carry = 0;
}

void pl_drain_servo_bus_reset(void) {
    // A different host (or the same host after a real bus reset) has no
    // relationship to a learned crystal-offset estimate from before --
    // clear everything this servo has learned, not just reseed it.
    s_learned_ppm = 0;
    s_i_accum = 0;
    s_servo_ppm = 0;
    s_accrue_rem_carry = 0;
    s_learn_gate_counter = 0;
}
