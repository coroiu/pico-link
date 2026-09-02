// Pico Link firmware -- PCM ring implementation. See pcm_ring.h's module
// doc for ownership rules; this file just implements them.
#include "pcm_ring.h"

#include <string.h>

#include "hardware/sync.h" // __dmb() -- see pcm_ring.h's module doc on why
                            // ordering barriers replace the old IRQ-nesting
                            // argument once the consumer can be core1.

// PL_PCM_RING_CAPACITY is a power of two (checked below), so masking
// replaces modulo for both indexing and the wraparound distance
// computation (head - tail) & PL_PCM_RING_MASK.
#define PL_PCM_RING_MASK (PL_PCM_RING_CAPACITY - 1u)

_Static_assert((PL_PCM_RING_CAPACITY & PL_PCM_RING_MASK) == 0u,
               "PL_PCM_RING_CAPACITY must be a power of two");
_Static_assert((PL_PCM_RING_CAPACITY % PL_PCM_FRAME_BYTES) == 0u,
               "PL_PCM_RING_CAPACITY must be a whole number of frames");

// SRAM, not PSRAM -- see pcm_ring.h.
static uint8_t s_ring[PL_PCM_RING_CAPACITY];
static volatile uint32_t s_head; // producer-owned; index into s_ring, [0, PL_PCM_RING_CAPACITY)
static volatile uint32_t s_tail; // consumer-owned; index into s_ring, [0, PL_PCM_RING_CAPACITY)
static volatile uint32_t s_overrun_frames;
static volatile uint32_t s_misaligned;

void pl_pcm_push(const uint8_t *data, uint32_t len) {
    if (len == 0) {
        return;
    }
    if ((len % PL_PCM_FRAME_BYTES) != 0u) {
        // Reject wholesale -- see pcm_ring.h's doc comment on why a partial
        // accept is worse than a full reject.
        s_misaligned++;
        return;
    }

    uint32_t head = s_head; // producer's own index; only this side writes it
    uint32_t tail = s_tail; // single aligned word read of the consumer's index; safe without a lock

    // Capacity-1 usable, same convention as input.c/bt.c's rings: one slot
    // stays permanently empty so a full ring is distinguishable from an
    // empty one without a separate flag.
    uint32_t used = (head - tail) & PL_PCM_RING_MASK;
    uint32_t free_bytes = (PL_PCM_RING_CAPACITY - 1u) - used;
    // Keep the usable region frame-aligned so `used` stays a multiple of
    // PL_PCM_FRAME_BYTES on both sides.
    free_bytes -= free_bytes % PL_PCM_FRAME_BYTES;

    uint32_t accept_bytes = len;
    if (accept_bytes > free_bytes) {
        // Overflow: keep the front of this batch (already-elapsed audio),
        // drop the NEWEST whole frames at the end of it, and count them.
        // Never touch `tail` -- see pcm_ring.h's doc comment.
        uint32_t dropped_bytes = accept_bytes - free_bytes;
        s_overrun_frames += dropped_bytes / PL_PCM_FRAME_BYTES;
        accept_bytes = free_bytes;
    }
    if (accept_bytes == 0) {
        return;
    }

    uint32_t head_idx = head & PL_PCM_RING_MASK;
    uint32_t first_chunk = PL_PCM_RING_CAPACITY - head_idx;
    if (first_chunk > accept_bytes) {
        first_chunk = accept_bytes;
    }
    memcpy(&s_ring[head_idx], data, first_chunk);
    if (accept_bytes > first_chunk) {
        memcpy(&s_ring[0], data + first_chunk, accept_bytes - first_chunk);
    }

    // Publish-after-write: make the memcpy's stores visible to the other
    // core BEFORE the index update that tells it there is new data to
    // read. Without this a core1 consumer could observe the new `s_head`
    // and read stale/torn bytes out of s_ring -- the two are otherwise
    // unordered with respect to each other across cores.
    __dmb();
    s_head = (head + accept_bytes) & PL_PCM_RING_MASK;
}

uint32_t pl_pcm_read(uint8_t *out, uint32_t max) {
    max -= max % PL_PCM_FRAME_BYTES;
    if (max == 0) {
        return 0;
    }

    uint32_t tail = s_tail; // consumer's own index; only this side writes it
    uint32_t head = s_head; // single aligned word read of the producer's index; safe without a lock

    uint32_t used = (head - tail) & PL_PCM_RING_MASK;
    uint32_t n = used < max ? used : max;
    n -= n % PL_PCM_FRAME_BYTES;
    if (n == 0) {
        return 0;
    }

    // Read-after-acquire: `head` above is the signal that the producer's
    // memcpy into s_ring already happened (see the __dmb() at its publish
    // site in pl_pcm_push). Order that acquire read before this side's own
    // reads of s_ring so a core1 consumer cannot observe a fresh `head`
    // paired with stale bytes still in flight from the other core.
    __dmb();
    uint32_t tail_idx = tail & PL_PCM_RING_MASK;
    uint32_t first_chunk = PL_PCM_RING_CAPACITY - tail_idx;
    if (first_chunk > n) {
        first_chunk = n;
    }
    memcpy(out, &s_ring[tail_idx], first_chunk);
    if (n > first_chunk) {
        memcpy(out + first_chunk, &s_ring[0], n - first_chunk);
    }

    s_tail = (tail + n) & PL_PCM_RING_MASK;
    return n;
}

uint32_t pl_pcm_fill_bytes(void) {
    uint32_t head = s_head;
    uint32_t tail = s_tail;
    return (head - tail) & PL_PCM_RING_MASK;
}

uint32_t pl_pcm_reset(void) {
    // Consumer side only -- reads the producer's head, writes only its own
    // tail. Drops everything currently buffered. Bead pico-link-pbv (C2-6):
    // report how many whole frames that was, so the caller can count it.
    uint32_t head = s_head;
    uint32_t tail = s_tail;
    uint32_t used = (head - tail) & PL_PCM_RING_MASK;
    s_tail = head;
    return used / PL_PCM_FRAME_BYTES;
}

uint32_t pl_pcm_trim_to(uint32_t target_bytes) {
    // Consumer side only -- same read-head/write-tail discipline as
    // pl_pcm_reset() above, just a partial drop instead of a full one.
    target_bytes -= target_bytes % PL_PCM_FRAME_BYTES;

    uint32_t head = s_head;
    uint32_t tail = s_tail;
    uint32_t used = (head - tail) & PL_PCM_RING_MASK;
    if (used <= target_bytes) {
        return 0; // already at or under target -- the common case
    }

    uint32_t drop_bytes = used - target_bytes;
    s_tail = (tail + drop_bytes) & PL_PCM_RING_MASK;
    return drop_bytes / PL_PCM_FRAME_BYTES;
}

uint32_t pl_pcm_overrun_frames(void) {
    return s_overrun_frames;
}

uint32_t pl_pcm_misaligned(void) {
    return s_misaligned;
}
