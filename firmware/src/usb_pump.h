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
// firmware.
//
// Bead pico-link-okx (F1): pl_log() no longer calls vprintf/stdio directly
// (and therefore no longer calls tud_task() re-entrantly via
// stdio_usb_out_chars) -- it pushes formatted bytes into pl_log_ring.h's
// ring and returns; only the superloop's pl_log_ring_drain() ever touches
// stdio. pl_usb_mutex below is kept purely as the worker's own
// tud_task()-reentrancy guard (mutex_try_enter around the whole tick) in
// case any future caller reaches tud_task() from outside this file; it is
// no longer contended by logging.
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

// The ONLY console entry point in this firmware -- printf-style, backed by
// vsnprintf into a stack scratch buffer. Every bare printf() in
// firmware/src/*.c must go through this instead (bead pico-link-tfj).
//
// Bead pico-link-okx (F1): formats into a stack buffer, then pushes the
// result into pl_log_ring.h's byte ring and returns -- NO mutex, no I/O, no
// blocking, callable from thread context or any IRQ priority (including
// bt.c's BTstack packet handler). pl_usb_mutex is no longer involved in
// logging at all; a message that would not fit in the ring is dropped
// whole (counted -- see pl_log_ring_bytes_dropped, surfaced in
// pl_usb_pump_report's log_drops field) rather than blocking any producer.
// The actual (possibly slow) write to stdio happens only in
// pl_log_ring_drain(), called from the superloop.
void pl_log(const char *fmt, ...) __attribute__((format(printf, 1, 2)));

// Bead pico-link-l60 introduced this for callers already running inside the
// 0xC0 worker IRQ with pl_usb_mutex held (usb_reset.c's
// resetd_open/resetd_control_xfer_cb) -- calling plain pl_log() from there
// used to be a guaranteed no-op, since pl_usb_mutex's ownership check is by
// CORE NUMBER, not call depth (pico/lock_core.h).
//
// Bead pico-link-okx (F1): that hazard no longer exists -- pl_log() does
// not take pl_usb_mutex any more (see its own doc comment above), so this
// function is now IDENTICAL to pl_log() and safe to call from anywhere,
// nested or not. Kept as a distinct, separately-named entry point only so
// existing callers (usb_reset.c) don't need to change and so a future
// reader who finds pl_log_locked() knows exactly why it's still here.
void pl_log_locked(const char *fmt, ...) __attribute__((format(printf, 1, 2)));

// The PCM ring itself (M3's producer-only ring, hardened for M4) lives in
// pcm_ring.h/.c, not here -- it is the USB/Bluetooth seam and belongs to
// neither side. usb_audio.c's pl_usb_audio_task(), which this worker calls,
// pushes into it via pl_pcm_push() directly. See pcm_ring.h.

// Bead pico-link-okx (D11): report_dt_us is the actual elapsed microseconds
// since the previous call, computed ONCE in main.c's superloop and shared
// with pl_a2dp_report -- NOT independently re-derived here. Before this,
// this function and pl_a2dp_report each ran their own ~1s rate-limit clock,
// so every cross-report rate comparison (sof_isr/s vs packets/s vs
// enc_frames_total/s) divided by two different, unsynchronized ~1.02s
// windows -- worth ~1 percent by construction, and this bead's whole
// discriminator (miss/s = sof_isr/s - packets/s) needs better than that.
// Pass 0 on the very first call (no prior sample to diff against, matches
// pl_a2dp_report's existing convention) -- this function does NOT gate
// itself any more; the caller (main.c) decides when a second has elapsed
// and calls both report functions together.
//
// Once-per-second instrumentation snapshot: logs the cumulative audio
// packet count (from usb_audio.c's tud_audio_rx_done_pre_read_cb hook),
// the tud_audio_available() high-water mark against the 784-byte software
// FIFO, the worst observed interval between worker invocations, and the
// PCM-ring/log drop counts. These distinguish "packets never arriving",
// "packets arriving but the FIFO not draining fast enough", and "the fix
// didn't take" in one capture -- see bd pico-link-tfj's design comment.
// Extended by bd pico-link-okx (D1-D13, see usb_pump.c) with the
// arm/complete-race discriminator counters: pump_ticks_run/skipped,
// ep_out_idle_ticks, ep_out_state_flipped_in_task, sof_isr, and the
// sof_phase_hist[8] SOF-to-worker-tick phase histogram. Call from the
// superloop.
void pl_usb_pump_report(uint32_t report_dt_us);

#endif // PICO_LINK_USB_PUMP_H
