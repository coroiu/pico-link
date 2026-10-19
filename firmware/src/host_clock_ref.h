// Pico Link firmware -- independent host-clock-offset reference (bead
// pico-link-ycm2.7.1.1, follow-up to pico-link-ycm2.7.1's drain-rate
// servo).
//
// Why this exists: the drain servo's servo_learned_ppm settled at -2..+8
// ppm on hardware (bead pico-link-ycm2.7.1's HARDWARE comment), but a
// 2026-08-31 measurement put this Mac's host-vs-crystal offset at ~220ppm.
// Either the offset genuinely shrank since, or the servo is not actually
// matching rates -- and servo_learned_ppm cannot tell the difference from
// itself, because it IS the thing under test. This module is a second,
// independent instrument: it counts USB start-of-frame events (paced by
// the HOST's own crystal, one per 1ms on this full-speed-only USB
// controller -- see usb_pump.c's pl_usb_sof_isr_sample, the real-ISR-
// context counter this module reads) against THIS BOARD's own timer
// (time_us_64) over a fixed window. host_ppm never touches pcm_ring,
// drain_servo, or a2dp.c's credit clock -- it cannot be wrong for the same
// reason the servo might be.
//
// Sign convention (direct, not flipped): host_ppm is the host's SOF rate
// as measured against our timer, expressed as a deviation from nominal
// 1000 SOF/s. host_ppm > 0 means the host's crystal is sending SOFs
// FASTER than our timer expects -- the host believes more time has passed
// than our crystal does, so a host-paced 48kHz PCM stream delivers bytes
// faster than nominal too, and the ring tends to run HIGH. That is
// EXACTLY drain_servo.h's "ring running high, drain faster" condition,
// which drain_servo.h documents as POSITIVE servo_ppm/servo_learned_ppm.
// So for a healthy servo: servo_learned_ppm should converge to
// approximately +host_ppm (same sign, same rough magnitude) -- design sec
// 6's acceptance bar is agreement within +/-20ppm.
//
// Pure/testable by construction (same discipline as drain_servo.c): takes
// its clock and SOF-count inputs as parameters rather than reading
// time_us_64()/pl_usb_pump_sof_count() itself, so it has zero pico-sdk or
// tinyusb dependency and can be exercised host-side with synthetic input
// sequences (see firmware/tests/test_host_clock_ref.c). main.c's shared
// ~1Hz report block is the real call site, passing time_us_64() and
// pl_usb_pump_sof_count() each time.
#ifndef PICO_LINK_HOST_CLOCK_REF_H
#define PICO_LINK_HOST_CLOCK_REF_H

#include <stdint.h>

// Sentinel returned by pl_host_clock_ref_ppm() before the first full
// window has elapsed (there is no plausible real ppm reading anywhere
// near this value, so a caller printing it raw cannot mistake it for "no
// drift" the way a 0 placeholder could).
#define PL_HOST_CLOCK_PPM_NOT_READY INT32_MIN

// Call once per ~1s tick (same cadence as pl_usb_pump_report/
// pl_a2dp_report's shared report clock, main.c) with the CURRENT
// time_us_64() and the CURRENT pl_usb_pump_sof_count(). Internally gates
// itself to a much coarser ~10s window (PL_HOST_CLOCK_WINDOW_US in the
// .c file) -- being called more often than that only costs a few cheap
// comparisons, it does not shorten the window or change the answer.
//
// On the first call, or the first call after pl_host_clock_ref_bus_reset,
// this only records the window's starting snapshot (no measurement is
// possible from a single point) -- pl_host_clock_ref_ppm() keeps
// returning PL_HOST_CLOCK_PPM_NOT_READY until a full window has passed.
void pl_host_clock_ref_poll(uint64_t now_us, uint32_t sof_count);

// The most recently completed window's measurement, held constant between
// windows (same "cached snapshot" convention as
// pl_drain_servo_learned_ppm) until the next one completes.
// PL_HOST_CLOCK_PPM_NOT_READY before the first window completes.
int32_t pl_host_clock_ref_ppm(void);

// Call from usb_pump.c's tud_mount_cb(), same call site as
// pl_drain_servo_bus_reset() -- a new enumeration may be a different host
// with a different crystal, so restart the window rather than let a
// pre-reset sof_count/time_us_64 pair span the boundary and blend two
// hosts' offsets into one reading. Returns pl_host_clock_ref_ppm() to
// PL_HOST_CLOCK_PPM_NOT_READY until the next window completes.
void pl_host_clock_ref_bus_reset(void);

#endif // PICO_LINK_HOST_CLOCK_REF_H
