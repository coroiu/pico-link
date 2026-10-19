// Pico Link firmware -- the drain-rate PI servo (bead pico-link-ycm2.7.1,
// design .planning/design/2026-10-19-adaptive-usb-audio.md sec 2b/7 step 1).
//
// Replaces usb_audio.c's former explicit-feedback PI controller
// (tud_audio_fb_set) as the rate matcher. SAME plant (the PCM ring's fill
// level) and SAME proportional/integral gains as the old feedback loop
// (nxf's Kp=500, Ki divisor 100000 -- +/-500ppm on the host's send rate and
// on this drain's consume rate move the ring at the same ~96 B/s, so the
// tuning carries over unchanged), but the actuator is now the A2DP credit
// clock (a2dp.c's pl_a2dp_accrue_credit) instead of the host's send rate --
// the SINK now absorbs host-vs-board drift, where the ring used to absorb
// it via the old USB feedback loop. The output clamp is widened to
// +/-1000ppm (was a USB-class practicality on the old loop, not a plant
// limit).
//
// Sign convention (FLIPPED from the old feedback loop, design sec 2b):
// positive servo_ppm means "ring is running high, drain FASTER". The old
// loop's positive ppm asked the HOST to send faster for the same
// ring-high condition; now the actuator is this board's own drain, so the
// correction direction had to flip with it. See pl_drain_servo_ppm's doc
// comment.
//
// Home (design sec 2b): this file lives beside pcm_ring, a seam owned by
// neither the USB-audio side nor the A2DP side -- a USB file must not own
// A2DP pacing, and an fb_ name would tell every future reader the host is
// still being steered.
#ifndef PICO_LINK_DRAIN_SERVO_H
#define PICO_LINK_DRAIN_SERVO_H

#include <stdint.h>

// Call once per ~1ms tick, same call site/cadence as the old
// pl_usb_audio_feedback_task (usb_pump.c's 0xC0 worker, guarded on
// pl_usb_audio_streaming()). Samples the PCM ring's fill, updates the EMA
// and the PI terms, and republishes pl_drain_servo_ppm()'s output.
void pl_drain_servo_tick(void);

// Returns this tick's servo-corrected EXTRA sample-frames to add on top of
// an already-computed base accrual (elapsed_us * sample_rate_hz / 1e6,
// a2dp.c's own base term) -- call from pl_a2dp_accrue_credit with the SAME
// elapsed_us and sample_rate_hz it just used for the base term. Fixed
// point throughout (no float): maintains its own carried sub-sample
// remainder across calls, because a realistic correction (tens of ppm) is
// a small fraction of a sample per 1ms tick and would be silently
// truncated to zero forever without a carried remainder -- see the .c
// file's doc comment. May return a negative value (servo_ppm < 0 slows
// the drain).
int32_t pl_drain_servo_accrue_extra_samples(uint32_t elapsed_us, uint32_t sample_rate_hz);

// Current servo correction, in ppm, clamped to +/-1000ppm. Positive: ring
// running high, drain faster (see this file's module doc for the
// sign-flip rationale). Exposed for the console/counter channel (design
// sec 6's acceptance: servo_ppm within +/-20ppm of the measured host
// offset over 60s+).
int32_t pl_drain_servo_ppm(void);

// The learned steady-state offset -- a snapshot of the integral term,
// updated roughly once a second only when the loop is not railed and the
// EMA is close to target (within target/4) -- see the .c file. Carried
// across pl_drain_servo_reset() calls (stream start, trim complete)
// instead of being zeroed, so a fresh stream does not have to re-walk the
// ~11s P-only convergence (and the dry-ring regime that produces) every
// time (design sec 2b, "Required details: learned-offset reseed").
// Cleared only by pl_drain_servo_bus_reset().
int32_t pl_drain_servo_learned_ppm(void);

// The EMA fill level this servo's P term evaluates against -- same
// finest-grained ~1ms sampling of the ring the old feedback loop did.
int32_t pl_drain_servo_fill_ema(void);

// WINDOWED minimum raw fill level -- DESTRUCTIVE ON READ, same convention
// as the old pl_usb_audio_fill_min(): answers "what was the true minimum
// fill since the last read", not "since boot". fault.c is the sole caller
// (same single-reader discipline the old accessor required -- two readers
// would steal each other's windows).
uint32_t pl_drain_servo_fill_min(void);

// Cumulative count of ticks on which the P+I output, or the integral
// accumulator's own anti-windup bound, clamped -- "does the servo still
// have authority to correct drift?" (backs the `FB RAIL` fault-strip key,
// fault.c).
uint32_t pl_drain_servo_rail_ticks(void);

// Call from a2dp.c's STREAM_STARTED handler and from the resync-trim
// COMPLETE phase (pl_a2dp_resync_complete) -- same two call sites the old
// pl_usb_audio_fb_reset() had. Reseeds the EMA to the ring's CURRENT fill
// (so the P term starts at its real operating point instead of coasting
// in from priming/a previous stream) and resets the windowed fill_min.
// Unlike the old reset, the integral accumulator is NOT zeroed here -- it
// is reseeded from the learned offset (pl_drain_servo_learned_ppm), so a
// fresh stream does not restart the ~11s P-only walk to steady state that
// the old loop paid on every reset (measured dry-ring regime, the
// 2026-08-31 crackle).
void pl_drain_servo_reset(void);

// Call from usb_pump.c's tud_mount_cb() -- fires on every real
// SET_CONFIGURATION/bus reset, i.e. whenever the host could be a
// different machine with a different crystal. Clears the learned offset
// entirely (a previous host's drift says nothing about a new one's), the
// live integral term, the published servo_ppm, and the accrual remainder.
void pl_drain_servo_bus_reset(void);

#endif // PICO_LINK_DRAIN_SERVO_H
