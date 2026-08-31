// Pico Link firmware -- see pl_log_ring.h's module doc for the why.
#include "pl_log_ring.h"

#include <stdbool.h>
#include <string.h>

#include "hardware/sync.h"
#include "pico/time.h"
#include "tusb.h"

#include "usb_pump.h"

#define PL_LOG_RING_SIZE 4096u // power of two -- see the mask use below

// Bead pico-link-okx (F3b): s_buf/s_write/s_read live in .uninitialized_data
// (NOLOAD -- a watchdog reset does not clear SRAM, same mechanism
// watchdog_sup.c's s_loop_trace/s_loop_ring/s_boot_seq already use and that
// is proven to survive by that file's own reset path) so a reset that cut a
// drain off mid-backlog does not destroy the very evidence of what the
// board was saying as it died -- "the debug instrument is implicated in the
// bug it was installed to find" (this bead's design comment, Ada,
// 2026-08-30) no longer means the evidence is lost too. s_ring_magic
// distinguishes a real prior session (keep s_write/s_read, let the next
// drain emit the old backlog followed by this boot's own logging, in
// strict FIFO order -- no separate "read the old tail" path is needed) from
// a cold boot / a power cycle that did not preserve SRAM (start both at 0;
// s_buf's actual garbage content is never read in that case, since
// available = write - read = 0 until this session's own pushes advance
// write past it).
#define PL_LOG_RING_MAGIC 0x504c4c52u // "PLLR"
static volatile uint32_t s_ring_magic __attribute__((section(".uninitialized_data.pl_log_ring_magic")));
static char s_buf[PL_LOG_RING_SIZE] __attribute__((section(".uninitialized_data.pl_log_ring_buf")));
// Byte offsets, monotonically increasing (never wrapped themselves -- only
// the index into s_buf, via the mask, wraps). write is touched only inside
// the push critical section; read is touched only by the single drainer.
// Both are plain volatile, same "benign race, single writer per field"
// convention as every other counter in this firmware (see e.g.
// usb_audio.c's module doc).
static volatile uint32_t s_write __attribute__((section(".uninitialized_data.pl_log_ring_write")));
static volatile uint32_t s_read __attribute__((section(".uninitialized_data.pl_log_ring_read")));

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

// Set by pl_log_ring_init() -- true iff a valid previous session's
// backlog was found and preserved (not persistence-defeating-reset).
// pl_log_ring_recovered_backlog_bytes() lets main.c log that fact once,
// before pushing its own boot banner into the same ring.
static bool s_recovered;
static uint32_t s_recovered_bytes;

void pl_log_ring_init(void) {
    // Bead pico-link-okx (F3b): THE persistence check -- this must NOT
    // unconditionally zero s_write/s_read (the pre-F3b body did exactly
    // that, which would have silently defeated persistence the moment
    // s_buf/s_write/s_read moved to NOLOAD: the bytes would still be
    // sitting in SRAM but nothing would ever know to read them).
    if (s_ring_magic == PL_LOG_RING_MAGIC) {
        s_recovered = true;
        s_recovered_bytes = s_write - s_read; // unsigned modular arithmetic, same convention as everywhere else in this file
        // s_write/s_read intentionally NOT reset here -- see the module
        // doc above.
    } else {
        // Cold boot, or a true power cycle that did not preserve SRAM.
        s_write = 0;
        s_read = 0;
        s_recovered = false;
        s_recovered_bytes = 0;
    }
    s_ring_magic = PL_LOG_RING_MAGIC;

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

bool pl_log_ring_recovered_backlog(void) {
    return s_recovered;
}

uint32_t pl_log_ring_recovered_backlog_bytes(void) {
    return s_recovered_bytes;
}
