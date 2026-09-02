// Pico Link firmware -- host-buildable test for bead pico-link-nli.3 (G2:
// prepare pcm_ring/tx-ring for cross-core use, still single-core).
// `.planning/decisions/2026-09-03-ldac-encoder-on-core1.md` sec 3.1.
//
// Links the REAL firmware/src/pcm_ring.c unmodified. pcm_ring.c has no
// BTstack/pico-sdk dependency beyond hardware/sync.h's __dmb(), so on host
// we substitute a trivial stub (a full compiler barrier is a superset of
// what __dmb() gives us on-target, and correctness here is about the
// head/tail arithmetic, not the barrier's hardware effect, which cannot be
// observed from a single-threaded host test anyway).
//
// Build + run (no CMake target exists for this -- firmware has no host test
// harness; this is a standalone host binary, same convention as
// test_codec_id_stability.c). pico-sdk's OWN host stub
// (src/host/hardware_sync/include/hardware/sync.h) does not define
// __dmb(), so make a one-line stub first:
//   mkdir -p /tmp/hostinc/hardware
//   printf 'static inline void __dmb(void) { __sync_synchronize(); }\n' \
//     > /tmp/hostinc/hardware/sync.h
//   cc -std=c11 -Wall -Wextra -I firmware/src -I /tmp/hostinc \
//      firmware/tests/test_pcm_ring_cross_core.c \
//      firmware/src/pcm_ring.c \
//      -o /tmp/test_pcm_ring_cross_core && /tmp/test_pcm_ring_cross_core
//
// What this proves: push/read roundtrip correctness, data integrity across
// many wraps of the internal head/tail indices past PL_PCM_RING_CAPACITY,
// and the drop-newest overflow policy -- all with the __dmb() calls added
// by this bead in place. What it does NOT prove: that the barriers are
// sufficient on real cross-core hardware (a single-threaded host test
// cannot observe a store-ordering bug) -- that is hardware acceptance,
// out of scope for this bead per its own instructions.
#include <assert.h>
#include <stdint.h>
#include <stdio.h>

#include "pcm_ring.h"

int main(void) {
    uint8_t buf_in[8] = {1, 2, 3, 4, 5, 6, 7, 8};
    uint8_t buf_out[64];

    uint32_t n = pl_pcm_read(buf_out, sizeof(buf_out));
    assert(n == 0); // empty at boot

    pl_pcm_push(buf_in, 8);
    assert(pl_pcm_fill_bytes() == 8);
    n = pl_pcm_read(buf_out, 4);
    assert(n == 4);
    assert(buf_out[0] == 1 && buf_out[1] == 2 && buf_out[2] == 3 && buf_out[3] == 4);
    assert(pl_pcm_fill_bytes() == 4);

    n = pl_pcm_read(buf_out, 64);
    assert(n == 4);
    assert(buf_out[0] == 5);
    assert(pl_pcm_fill_bytes() == 0);

    // Wraparound: push/read repeatedly so the internal head/tail indices
    // wrap past PL_PCM_RING_CAPACITY many times over, verifying data
    // integrity survives every lap.
    static uint8_t pattern[4096];
    for (int i = 0; i < 4096; i++) {
        pattern[i] = (uint8_t)(i & 0xFF);
    }
    uint32_t total_pushed = 0, total_read = 0;
    static uint8_t check_out[4096];
    for (int iter = 0; iter < 2000; iter++) {
        pl_pcm_push(pattern, sizeof(pattern));
        total_pushed += sizeof(pattern);
        uint32_t got = pl_pcm_read(check_out, sizeof(check_out));
        total_read += got;
        for (uint32_t i = 0; i < got; i++) {
            assert(check_out[i] == pattern[i % sizeof(pattern)]);
        }
    }
    assert(total_pushed == total_read); // reads kept up -- no drops expected
    assert(pl_pcm_overrun_frames() == 0);
    assert(pl_pcm_misaligned() == 0);

    // Overflow: push far more than capacity in one call. Drop-newest
    // policy must still hold and the drop must be counted.
    static uint8_t big[40000];
    for (int i = 0; i < 40000; i++) {
        big[i] = (uint8_t)(i & 0xFF);
    }
    uint32_t before_overrun = pl_pcm_overrun_frames();
    pl_pcm_push(big, sizeof(big));
    assert(pl_pcm_overrun_frames() > before_overrun);

    uint32_t dropped = pl_pcm_reset();
    assert(dropped > 0);
    assert(pl_pcm_fill_bytes() == 0);

    printf("test_pcm_ring_cross_core: ALL PASSED (total_pushed=%u)\n", total_pushed);
    return 0;
}
