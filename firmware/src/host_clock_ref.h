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
// independent instrument: it compares USB start-of-frame EDGE TIMESTAMPS
// (paced by the HOST's own crystal, one per 1ms on this full-speed-only
// USB controller -- see usb_pump.c's pl_usb_sof_isr_sample, the real-ISR-
// context counter+timestamp this module reads via
// pl_usb_pump_sof_snapshot()) against THIS BOARD's own timer (time_us_64).
// host_ppm never touches pcm_ring, drain_servo, or a2dp.c's credit clock
// -- it cannot be wrong for the same reason the servo might be.
//
// Edge timestamps, not a fixed time window (the first cut of this bead):
// the first cut counted SOF edges inside an externally-chosen,
// arbitrary-phase time window (the thread's own ~1Hz poll instant). That
// has an inherent +/-1-SOF phase ambiguity -- the window's start/end do
// not land exactly ON an edge, so the "true" continuous rate implied by
// an exact integer edge count is only known to within one edge's worth of
// uncertainty. At ~10,000 edges in a 10s window, that is a ~100ppm
// quantization FLOOR (1/10,000 * 1e6ppm) -- coarse enough to be unable to
// tell 0ppm from 220ppm, let alone check a +/-20ppm bar. This version
// instead snaps BOTH window boundaries to actual captured SOF edges
// (pl_usb_pump_sof_snapshot's (count, time_us) pair, time_us read inside
// the ISR at that exact edge): the count delta between two real edges is
// then paired with the EXACT elapsed time between those same two edges,
// with no phase ambiguity left -- only ISR-latency jitter (low
// microseconds) on a ~10,000,000us span, i.e. ~0.1ppm resolution.
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
// its SOF-edge (count, time_us) snapshots as parameters rather than
// calling pl_usb_pump_sof_snapshot() itself, so it has zero pico-sdk or
// tinyusb dependency and can be exercised host-side with synthetic input
// sequences (see firmware/tests/test_host_clock_ref.c). main.c's shared
// ~1Hz report block is the real call site, passing
// pl_usb_pump_sof_snapshot()'s output each time.
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
// pl_usb_pump_sof_snapshot() output: sof_time_us is that snapshot's edge
// timestamp (NOT the caller's own time_us_64()), sof_count its paired
// count. Internally gates itself to a much coarser ~10s window
// (PL_HOST_CLOCK_WINDOW_US in the .c file) measured between SOF edges --
// being called more often than that only costs a cheap comparison, it
// does not shorten the window or change the answer.
//
// On the first call, or the first call after pl_host_clock_ref_bus_reset,
// this only records the window's starting edge snapshot (no measurement
// is possible from a single point) -- pl_host_clock_ref_ppm() keeps
// returning PL_HOST_CLOCK_PPM_NOT_READY until a full window has passed.
//
// If the SOF edge has not advanced since the last call (sof_count
// unchanged -- e.g. the bus is not enumerated, or streaming stopped and
// SOF itself stopped being serviced), this call is a safe no-op: it does
// not restart or corrupt the window, since nothing new to measure
// occurred.
void pl_host_clock_ref_poll(uint64_t sof_time_us, uint32_t sof_count);

// The most recently completed window's measurement, held constant between
// windows (same "cached snapshot" convention as
// pl_drain_servo_learned_ppm) until the next one completes.
// PL_HOST_CLOCK_PPM_NOT_READY before the first window completes.
int32_t pl_host_clock_ref_ppm(void);

// Call from usb_pump.c's tud_mount_cb(), same call site as
// pl_drain_servo_bus_reset() -- a new enumeration may be a different host
// with a different crystal, so restart the window rather than let a
// pre-reset edge snapshot span the boundary and blend two hosts' offsets
// into one reading. Returns pl_host_clock_ref_ppm() to
// PL_HOST_CLOCK_PPM_NOT_READY until the next window completes.
void pl_host_clock_ref_bus_reset(void);

#endif // PICO_LINK_HOST_CLOCK_REF_H
