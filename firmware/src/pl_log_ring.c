// Pico Link firmware -- see pl_log_ring.h's module doc for the why.
#include "pl_log_ring.h"

#include <stdio.h>
#include <string.h>

#include "hardware/sync.h"
#include "pico/time.h"

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

void pl_log_ring_init(void) {
    s_write = 0;
    s_read = 0;
    s_bytes_dropped = 0;
    s_push_hold_us_total = 0;
    s_push_hold_us_max = 0;
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

void pl_log_ring_drain(void) {
    // Single reader, no lock needed to observe s_write or advance s_read --
    // see this file's header doc.
    uint32_t write_snapshot = s_write;
    uint32_t available = write_snapshot - s_read;
    if (available == 0) {
        return;
    }

    uint32_t read_idx = s_read & (PL_LOG_RING_SIZE - 1);
    uint32_t first_chunk = PL_LOG_RING_SIZE - read_idx;
    if (first_chunk > available) {
        first_chunk = available;
    }
    fwrite(&s_buf[read_idx], 1, first_chunk, stdout);
    if (available > first_chunk) {
        fwrite(&s_buf[0], 1, available - first_chunk, stdout);
    }
    fflush(stdout);

    s_read += available;
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
