// Pico Link firmware -- lock-free-drain SPSC-style console byte ring (bead
// pico-link-okx, F1).
//
// WHY THIS EXISTS: before this bead, pl_log() (usb_pump.c) held pl_usb_mutex
// across vprintf() -> stdio_usb_out_chars(), which can spin for up to
// PICO_STDIO_USB_STDOUT_TIMEOUT_US (2ms, firmware/CMakeLists.txt) waiting on
// tud_cdc_write_available(). A degrading/dribbling CDC reader can keep that
// loop alive far longer than one call's worth of slack (last_avail_time is
// refreshed on every successful partial write, so the 2ms bound only catches
// a HARD stall, not a slow drain) -- and every microsecond of that hold is a
// microsecond the 0xC0 USB-servicing worker's own mutex_try_enter(&pl_usb_mutex)
// (usb_pump.c) fails and skips its tick entirely, which starves TinyUSB's
// 784-byte ISO-OUT FIFO. See bd pico-link-okx's design comments (Ada,
// 2026-08-29) for the full chain -- this was the LEADING hypothesis for the
// USB ingestion collapse before the EP1-OUT arm/complete race (F4) overtook
// it; still worth fixing regardless of which one is the actual cause,
// because the coupling is real today and would silently eat ISO-OUT packets
// the next time ANYTHING logs a burst (a panic dump, a BTstack error burst,
// future UI event logging).
//
// THE FIX: pl_log()/pl_log_locked() (usb_pump.c) format into a small stack
// buffer, then push those bytes into this ring under a brief
// save_and_disable_interrupts() critical section -- bounded, O(len), no I/O,
// no blocking -- and return immediately. Only the superloop
// (pl_log_ring_drain(), main.c, thread context, NEVER an IRQ, NEVER the 0xC0
// worker) reads the ring back out and performs the actual (possibly slow)
// stdio write. pl_usb_mutex never enters the picture for logging any more.
//
// Not a textbook lock-free MPSC ring (that would need per-slot sequence
// numbers/CAS to let concurrent producers interleave safely): this project
// has THREE producer contexts (thread, the 0xC0 worker IRQ, and the 0xFF
// cyw43/BTstack background IRQ), all on one core, so a brief
// interrupts-disabled critical section around the push is the simplest
// correct way to serialize them -- it cannot deadlock (nothing it does can
// block) and is O(message length), not O(I/O latency), which is the actual
// property this bead needs. The DRAIN side has exactly one reader (the
// superloop) and needs no lock at all to advance its own read index.
//
// A push that won't fit is dropped WHOLE (never partially), so a drained
// line is always a complete line -- this is also what makes the console
// line-splice corruption (bd pico-link-pbv/okx measured 23 of them in one
// capture) structurally impossible from here on: a single drainer can never
// interleave two producers' bytes.
#ifndef PICO_LINK_LOG_RING_H
#define PICO_LINK_LOG_RING_H

#include <stdint.h>

// Zeroes the ring. Call once, at the very top of main(), before ANY
// pl_log()/pl_log_locked() call anywhere in this firmware (including ones
// made before pl_usb_pump_init()/stdio_init_all()).
void pl_log_ring_init(void);

// Pushes `len` already-formatted bytes. Callable from any context (thread,
// any IRQ priority, nested or not) -- see this file's module doc for why
// that's safe. Drops the whole message (counted, see
// pl_log_ring_bytes_dropped) if it would not fit rather than write a
// partial line.
void pl_log_ring_push(const char *data, uint32_t len);

// Drains whatever is currently queued directly to the CDC endpoint
// (tud_cdc_write()/tud_cdc_write_flush()) -- NOT stdio/fwrite any more, see
// bead pico-link-okx (F2). THREAD CONTEXT ONLY -- call from the superloop,
// never from an IRQ or the 0xC0 worker.
//
// Bead pico-link-okx (F2): this is now BOUNDED and NON-BLOCKING -- no loop,
// no timeout, no dependence on the host. It takes pl_usb_mutex via
// pl_usb_lock_try() (usb_pump.h) for the duration of one bounded
// memcpy-sized write and returns immediately either way. This replaces the
// pre-F2 body, which called fwrite()+fflush() on stdout -- underneath,
// pico_stdio_usb's stdio_usb_out_chars(), whose escape condition is
// HOST-paced (see this bead's design comment, Ada, 2026-08-30, for the full
// chain) -- that call could stall the superloop indefinitely on a host that
// wasn't draining the CDC endpoint, which is the root cause this bead
// exists to fix.
void pl_log_ring_drain(void);

// Cumulative bytes dropped because a push did not fit. Surfaced in
// usb-pump's report line as log_drops (bead pico-link-okx D-series;
// replaces the pre-F1 counter of the same report field, which counted
// contended-mutex drops -- that failure mode no longer exists).
uint32_t pl_log_ring_bytes_dropped(void);

// Bead pico-link-okx D3 (repointed from "pl_log's mutex hold" -- there is
// no mutex here any more -- to what actually replaced it): cumulative and
// max microseconds spent inside this ring's interrupts-disabled push
// critical section. Expected to be a handful of microseconds, always --
// if this ever climbs, the push itself (not I/O) has become the hazard.
uint32_t pl_log_ring_push_hold_us_total(void);
uint32_t pl_log_ring_push_hold_us_max(void);

// Bead pico-link-okx (F2): cumulative count of pl_log_ring_drain() calls
// that found queued bytes but skipped this tick because pl_usb_lock_try()
// failed (the 0xC0 worker held pl_usb_mutex). Expected to be small and
// occasional; if it climbs, the worker is holding the lock long enough to
// be worth investigating on its own (see usb_pump.c's pump_ticks_skipped,
// the worker-side symmetric counter). Surfaced in usb-pump-logring.
uint32_t pl_log_ring_drain_skips(void);

// Lifetime high-water mark of (s_write - s_read), i.e. the largest backlog
// this ring has ever held. Never reset -- deltas aren't meaningful here,
// only "how bad did it ever get" is. Surfaced in usb-pump-logring.
uint32_t pl_log_ring_backlog_hwm(void);

#endif // PICO_LINK_LOG_RING_H
