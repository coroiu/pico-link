// Pico Link firmware -- see pl_log_ring.h's module doc for the why.
#include "pl_log_ring.h"

#include <string.h>

#include "hardware/sync.h"
#include "pico/time.h"
#include "tusb.h"

#include "usb_pump.h"

#define PL_LOG_RING_SIZE 4096u // power of two -- see the mask use below

static char s_buf[PL_LOG_RING_SIZE];
// Byte offsets, monotonically increasing (never wrapped themselves -- only
// the index into s_buf, via the mask, wraps). write is touched only inside
// the push critical section; read is touched only by the single drainer.
// Both are plain volatile, same "benign race, single writer per field"
// convention as every other counter in this firmware (see e.g.
// usb_audio.c's module doc).
static volatile uint32_t s_write;
static volatile uint32_t s_read;

static volatile uint32_t s_bytes_dropped;
static volatile uint32_t s_push_hold_us_total;
static volatile uint32_t s_push_hold_us_max;

// Bead pico-link-okx (F2). s_drain_skips: pl_log_ring_drain() found queued
// bytes but pl_usb_lock_try() failed (the 0xC0 worker held pl_usb_mutex) --
// the drain skipped this tick entirely rather than blocking. s_backlog_hwm:
// lifetime high-water mark of (s_write - s_read), i.e. how large the
// backlog ever got -- never reset, see this bead's design comment for why
// a lifetime max is more useful here than a windowed one. Both surfaced in
// usb_pump.c's "usb-pump-logring" report line.
static volatile uint32_t s_drain_skips;
static volatile uint32_t s_backlog_hwm;

void pl_log_ring_init(void) {
    s_write = 0;
    s_read = 0;
    s_bytes_dropped = 0;
    s_push_hold_us_total = 0;
    s_push_hold_us_max = 0;
    s_drain_skips = 0;
    s_backlog_hwm = 0;
}

void pl_log_ring_push(const char *data, uint32_t len) {
    if (len == 0) {
        return;
    }
    if (len > PL_LOG_RING_SIZE) {
        // Cannot ever fit, no matter how empty the ring is -- drop
        // outright, no critical section needed.
        s_bytes_dropped += len;
        return;
    }

    uint64_t t0 = time_us_64();
    uint32_t save = save_and_disable_interrupts();

    uint32_t used = s_write - s_read; // wraparound-safe: unsigned modular arithmetic
    uint32_t free_space = PL_LOG_RING_SIZE - used;
    if (len > free_space) {
        restore_interrupts(save);
        s_bytes_dropped += len;
        return;
    }

    uint32_t write_idx = s_write & (PL_LOG_RING_SIZE - 1);
    uint32_t first_chunk = PL_LOG_RING_SIZE - write_idx;
    if (first_chunk > len) {
        first_chunk = len;
    }
    memcpy(&s_buf[write_idx], data, first_chunk);
    if (len > first_chunk) {
        memcpy(&s_buf[0], data + first_chunk, len - first_chunk);
    }
    s_write += len;

    restore_interrupts(save);

    uint32_t hold_us = (uint32_t)(time_us_64() - t0);
    s_push_hold_us_total += hold_us;
    if (hold_us > s_push_hold_us_max) {
        s_push_hold_us_max = hold_us;
    }
}

// Bead pico-link-okx (F2): rewritten to be bounded and non-blocking -- see
// this bead's design comment (Ada, 2026-08-30) for the full root-cause
// chain this replaces. The old body called fwrite()+fflush() straight to
// stdout, which is pico_stdio_usb's stdio_usb_out_chars() underneath: a
// busy-wait whose escape condition (stdio_usb_connected(), which this
// build makes return tud_ready() -- see firmware/CMakeLists.txt's
// PICO_STDIO_USB_CONNECTION_WITHOUT_DTR comment) is HOST-paced, not
// firmware-bounded -- a host that isn't draining the CDC endpoint kept the
// superloop inside that call indefinitely, well past the 2000ms hardware
// watchdog window.
//
// New shape, no loop, no timeout, no tud_task() call from thread context,
// no dependence on the host whatsoever:
void pl_log_ring_drain(void) {
    // Single reader, no lock needed to observe s_write or advance s_read --
    // see this file's header doc.
    uint32_t write_snapshot = s_write;
    uint32_t available = write_snapshot - s_read;
    if (available > s_backlog_hwm) {
        s_backlog_hwm = available;
    }
    if (available == 0) {
        return;
    }

    // Never blocks: false means the 0xC0 worker currently holds
    // pl_usb_mutex (tud_task() mid-call) -- skip this tick, the bytes stay
    // in the ring for the next one. tud_task() is not reentrant, so this
    // gate cannot be removed even though F1 already stopped logging from
    // contending on it.
    if (!pl_usb_lock_try()) {
        s_drain_skips++;
        return;
    }

    if (!tud_ready()) {
        // Not enumerated / suspended -- nothing to write to yet.
        pl_usb_unlock();
        return;
    }

    // cdc_device.c sets the CDC TX FIFO OVERWRITABLE at reset and only
    // clears that on a DTR assert -- which this build's host (macOS,
    // PICO_STDIO_USB_CONNECTION_WITHOUT_DTR=1) never performs (see
    // firmware/CMakeLists.txt and CLAUDE.md's DTR notes). tud_cdc_write()
    // therefore never returns short here; it SILENTLY OVERWRITES unsent
    // bytes instead. Gating on tud_cdc_write_available() before writing is
    // therefore load-bearing for correctness, not an optimisation -- it is
    // the only thing standing between this drain and quietly corrupting
    // its own output.
    uint32_t room = tud_cdc_write_available();
    if (room == 0) {
        pl_usb_unlock();
        return;
    }

    uint32_t read_idx = s_read & (PL_LOG_RING_SIZE - 1);
    uint32_t first_chunk = PL_LOG_RING_SIZE - read_idx;
    if (first_chunk > available) {
        first_chunk = available;
    }
    // Bound to this call's contiguous run of the ring; a wrapped remainder
    // (available > first_chunk) is picked up by the NEXT call, same as any
    // other partial drain -- there is no requirement that one call drains
    // everything queued.
    uint32_t take = available;
    if (take > room) {
        take = room;
    }
    if (take > first_chunk) {
        take = first_chunk;
    }

    // Advance by what tud_cdc_write() ACTUALLY accepted, not by `take` --
    // this is instrument defect #1 from this bead's design comment: the
    // pre-F2 code advanced s_read by `available` unconditionally regardless
    // of what the underlying write accepted, silently discarding whatever
    // stdio_usb's own internal timeout gave up on. tud_cdc_write() itself
    // is documented to accept up to `bufsize` immediately (it is a FIFO
    // copy, not a blocking call), so w == take is the expected case, but
    // trusting the return value rather than the request is what makes this
    // correct even if that ever changes.
    uint32_t w = tud_cdc_write(&s_buf[read_idx], take);
    s_read += w;
    tud_cdc_write_flush();

    pl_usb_unlock();
}

uint32_t pl_log_ring_bytes_dropped(void) {
    return s_bytes_dropped;
}

uint32_t pl_log_ring_push_hold_us_total(void) {
    return s_push_hold_us_total;
}

uint32_t pl_log_ring_push_hold_us_max(void) {
    return s_push_hold_us_max;
}

uint32_t pl_log_ring_drain_skips(void) {
    return s_drain_skips;
}

uint32_t pl_log_ring_backlog_hwm(void) {
    return s_backlog_hwm;
}
