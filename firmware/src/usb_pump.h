// Pico Link firmware -- interrupt-driven USB servicing pump (bead
// pico-link-tfj).
//
// Why this exists: main.c used to call tud_task()/pl_usb_audio_task() once
// per superloop iteration (~55ms with render+blit+BT-poll overhead), but
// TinyUSB's audio class driver only re-arms the ISO OUT endpoint from
// inside tud_task() (audio_device.c's audiod_xfer_cb), and the isochronous
// OUT software FIFO is only 784 bytes (~4ms at 48kHz/16-bit/stereo). A
// >4ms gap between tud_task() calls guarantees an overflow, and a single
// overflow permanently kills the endpoint (audio_device.c's TU_VERIFY on a
// full FIFO returns before the re-arm). See bd pico-link-tfj's design
// comment (Ada, 2026-08-28) for the full chain, including why this also
// explains the total EP0-killing hang (usbd.c's 16-deep event queue drops
// ~100 events, including SETUP, over a 55ms gap).
//
// This module services tud_task() (and drains the audio FIFO) from a 1ms
// pico-sdk repeating timer instead. It does NOT call tud_task() from the
// timer callback itself -- pico-sdk's default alarm pool runs at
// PICO_DEFAULT_IRQ_PRIORITY (0x80), the SAME priority as USBCTRL_IRQ, so
// the real DCD interrupt could never preempt a tud_task() call made there.
// Instead the timer only pends a claimed user IRQ running at priority
// 0xC0 -- strictly between USBCTRL_IRQ (0x80) and the cyw43/BTstack
// background IRQ (PICO_LOWEST_IRQ_PRIORITY, 0xFF) -- and that IRQ is where
// tud_task() actually runs.
//
// tud_task() is not reentrant, and before this bead it was already called
// from three uncoordinated places: main.c's superloop, EVERY printf (via
// pico_stdio_usb's stdio_usb_out_chars, which calls tud_task() itself even
// with LIB_TINYUSB_DEVICE=1 -- that call site is outside the
// !LIB_TINYUSB_DEVICE guard), and bt.c's BTstack packet handler running in
// IRQ context. pl_log() below is now the ONLY console entry point in this
// firmware, serialized against the worker via the same mutex, specifically
// to close that reentrancy window -- see its doc comment.
//
// Constraint that MUST NOT be undone (established by pico-link-5am): this
// module never calls into Rust (pl_ui_*) from interrupt context. The
// worker below touches only TinyUSB and plain C buffers.
#ifndef PICO_LINK_USB_PUMP_H
#define PICO_LINK_USB_PUMP_H

#include <stdint.h>

// Initializes the console mutex, claims a user IRQ at priority 0xC0,
// installs it as the TinyUSB servicing worker, and starts the 1ms
// repeating timer that pends it. Call once, after tusb_init() (the worker
// calls tud_task(), which asserts the stack is initialized) and before
// ANY printf/pl_log call in this firmware (pl_log's mutex must exist
// first) -- see main.c.
void pl_usb_pump_init(void);

// The ONLY console entry point in this firmware -- printf-style, backed
// by vprintf. Every bare printf() in firmware/src/*.c must go through this
// instead (bead pico-link-tfj). Uses mutex_try_enter, NOT a blocking
// enter: this is called from both thread context (main.c) and IRQ context
// (bt.c's BTstack packet handler), and pico/mutex.h's own module doc calls
// blocking mutex calls from an IRQ handler "generally a bad idea". Under
// contention this silently drops the message (counted -- see
// pl_usb_pump_report) rather than ever risking one context blocking on
// the other.
void pl_log(const char *fmt, ...) __attribute__((format(printf, 1, 2)));

// Bead pico-link-l60: for callers that are ALREADY running inside the 0xC0
// worker IRQ with pl_usb_mutex held -- concretely, usb_reset.c's
// resetd_open/resetd_control_xfer_cb, which are only ever reached from
// tud_task(), which is only ever called from pl_usb_pump_worker_irq()
// while it holds pl_usb_mutex (see usb_pump.c:97101). Calling plain
// pl_log() from there is a guaranteed-every-time no-op: pl_usb_mutex is a
// plain non-recursive mutex_t whose ownership check is by CORE NUMBER, not
// call depth (pico/lock_core.h), so mutex_try_enter() returns false for
// the very core that already owns it, and the message is silently counted
// as a drop and never printed. This variant skips mutex_try_enter/exit
// entirely and goes straight to vprintf -- correct ONLY when the caller
// can prove it already holds the lock. Do NOT call this from anywhere
// that isn't already inside the worker's critical section, and do NOT
// make pl_usb_mutex recursive to paper over a future misuse -- the
// non-reentrancy guard at usb_pump.c's mutex_try_enter is load-bearing
// (see this file's module doc).
void pl_log_locked(const char *fmt, ...) __attribute__((format(printf, 1, 2)));

// Producer-side push into the pump's SRAM (NOT PSRAM -- an IRQ-context
// write through the QMI XIP path is not a latency you want against a 1ms
// deadline) SPSC PCM ring. Called only from usb_audio.c's
// pl_usb_audio_task() while that runs inside the 0xC0 worker IRQ. Drops
// bytes (counted) once the ring is full rather than overwrite an
// undrained region -- same policy as input.c's debounce ring. Nothing
// drains this ring yet in M3: the milestone's proof is that PCM arrives
// and is read out of TinyUSB's FIFO fast enough to keep the ISO OUT
// endpoint alive, not that anything downstream consumes it. M4's LDAC/I2S
// consumer is the first real reader, via pl_usb_pump_read_pcm below.
void pl_usb_pump_push_pcm(const uint8_t *data, uint32_t len);

// Consumer-side drain of the PCM ring, for whatever thread-context code
// eventually wants it (M4). Writes at most `max` bytes into `out` and
// returns how many were written. Not called anywhere yet in M3.
uint32_t pl_usb_pump_read_pcm(uint8_t *out, uint32_t max);

// Once-per-second instrumentation snapshot (rate-limits itself, so it's
// cheap to call every superloop iteration): logs the cumulative audio
// packet count (from usb_audio.c's tud_audio_rx_done_pre_read_cb hook),
// the tud_audio_available() high-water mark against the 784-byte software
// FIFO, the worst observed interval between worker invocations, and the
// PCM-ring/log drop counts. These distinguish "packets never arriving",
// "packets arriving but the FIFO not draining fast enough", and "the fix
// didn't take" in one capture -- see bd pico-link-tfj's design comment.
// Call from the superloop.
void pl_usb_pump_report(void);

#endif // PICO_LINK_USB_PUMP_H
