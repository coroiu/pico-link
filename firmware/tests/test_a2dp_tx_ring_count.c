// Pico Link firmware -- host-buildable test for bead pico-link-nli.3 (G2:
// prepare pcm_ring/tx-ring for cross-core use, still single-core).
// `.planning/decisions/2026-09-03-ldac-encoder-on-core1.md` sec 3.1.
//
// This is a MODEL test, not a link test: firmware/src/a2dp.c cannot be
// linked on host without dragging in BTstack, pico-sdk's pico/time.h,
// ldacBT.h, and this project's own bt.c/codec_*/usb_audio/usb_pump/
// watchdog_sup -- none of which exist on host and none of which this
// bead touched. What IS this bead's own new risk, and what actually
// needs coverage, is the arithmetic itself:
//
//   pl_a2dp_tx_count() == (tx_head - tx_tail) & PL_A2DP_TX_QUEUE_MASK
//
// with PL_A2DP_TX_QUEUE_SLOTS a power of two (8), tx_head/tx_tail each
// advanced independently mod SLOTS by producer/consumer, and one slot
// permanently reserved so count==0 (empty) and count==SLOTS (impossible
// to reach, by construction) never alias -- the usable ceiling is
// SLOTS - 1 (7 of 8), exactly the pico-link-r44 lesson FIX 2 of this
// bead's review re-applied to a2dp.c:1982's D2 tripwire.
//
// This model is copied verbatim from a2dp.c's real `#define
// PL_A2DP_TX_QUEUE_SLOTS`/`PL_A2DP_TX_QUEUE_MASK` and the one-line body
// of `pl_a2dp_tx_count()` -- if either changes, update both here and
// there. It replaces the standalone, uncommitted model-check the G2
// bead comment mentioned (and which got the formula wrong once during
// self-testing) with a permanent, committed one, per G2 review FIX 3.
//
// Build + run (no CMake target -- same convention as
// test_pcm_ring_cross_core.c and test_codec_id_stability.c):
//   cc -std=c11 -Wall -Wextra firmware/tests/test_a2dp_tx_ring_count.c \
//      -o /tmp/test_a2dp_tx_ring_count && /tmp/test_a2dp_tx_ring_count
#include <assert.h>
#include <stdint.h>
#include <stdio.h>

#define PL_A2DP_TX_QUEUE_SLOTS 8u
#define PL_A2DP_TX_QUEUE_MASK (PL_A2DP_TX_QUEUE_SLOTS - 1u)

static uint32_t model_tx_count(uint32_t head, uint32_t tail) {
    return (head - tail) & PL_A2DP_TX_QUEUE_MASK;
}

int main(void) {
    // --- 0-vs-full disambiguation ---
    // Empty: head == tail (any equal pair, not just 0,0) must read 0.
    assert(model_tx_count(0, 0) == 0);
    assert(model_tx_count(5, 5) == 0);
    assert(model_tx_count(0xFFFFFFFFu, 0xFFFFFFFFu) == 0);

    // The masked derivation can NEVER produce SLOTS (8) for any head/tail
    // pair -- (head-tail)&MASK is always in [0, MASK] = [0, 7]. The
    // reserved slot exists precisely so callers never need count==8 to
    // mean "full": the real full state is count == SLOTS-1 == 7, reached
    // when head is exactly one slot behind wrapping onto tail.
    for (uint32_t tail = 0; tail < PL_A2DP_TX_QUEUE_SLOTS; tail++) {
        uint32_t head_one_behind = (tail + PL_A2DP_TX_QUEUE_SLOTS - 1u) & PL_A2DP_TX_QUEUE_MASK;
        assert(model_tx_count(head_one_behind, tail) == PL_A2DP_TX_QUEUE_SLOTS - 1u);
    }

    // --- every count 0..SLOTS-1 is reachable and exactly one head/tail
    //     delta produces it, for a fixed tail ---
    for (uint32_t tail = 0; tail < PL_A2DP_TX_QUEUE_SLOTS; tail++) {
        for (uint32_t delta = 0; delta < PL_A2DP_TX_QUEUE_SLOTS; delta++) {
            uint32_t head = (tail + delta) & PL_A2DP_TX_QUEUE_MASK;
            assert(model_tx_count(head, tail) == delta);
        }
    }

    // --- wrap through the mask across many mod-8 cycles ---
    // Simulate a producer/consumer pair each independently incrementing
    // their index by 1 every tick (producer seals a slot, consumer sends
    // one), starting offset by 3 (a steady-state queue depth of 3), and
    // walk it through many multiples of SLOTS so the raw indices wrap
    // past 2^32/SLOTS-style laps many times over (mod arithmetic on a
    // uint32_t, not the tiny SLOTS modulus -- see the rollover case
    // below for the real 2^32 boundary).
    {
        uint32_t head = 3, tail = 0;
        for (uint32_t tick = 0; tick < 100000; tick++) {
            assert(model_tx_count(head, tail) == 3);
            head = (head + 1u) & PL_A2DP_TX_QUEUE_MASK;
            tail = (tail + 1u) & PL_A2DP_TX_QUEUE_MASK;
        }
    }

    // --- SLOTS-1 full boundary held across a full push/drain cycle ---
    // Fill to the usable ceiling (SLOTS-1 == 7), verify it reads exactly
    // that and not SLOTS, then drain to empty, verifying every
    // intermediate count along the way.
    {
        uint32_t head = 0, tail = 0;
        for (uint32_t i = 0; i < PL_A2DP_TX_QUEUE_SLOTS - 1u; i++) {
            head = (head + 1u) & PL_A2DP_TX_QUEUE_MASK;
            assert(model_tx_count(head, tail) == i + 1u);
        }
        assert(model_tx_count(head, tail) == PL_A2DP_TX_QUEUE_SLOTS - 1u);
        for (uint32_t i = PL_A2DP_TX_QUEUE_SLOTS - 1u; i > 0; i--) {
            assert(model_tx_count(head, tail) == i);
            tail = (tail + 1u) & PL_A2DP_TX_QUEUE_MASK;
        }
        assert(model_tx_count(head, tail) == 0);
    }

    // --- uint32_t rollover boundary ---
    // Both indices are `volatile uint32_t` that increment forever
    // (never reset except at stream restart) and wrap at 2^32, not at
    // SLOTS -- the SLOTS-modulus reduction only happens in the final
    // `& PL_A2DP_TX_QUEUE_MASK`. Unsigned subtraction wraps modulo
    // 2^32-per-the-standard, so `head - tail` is well-defined and equal
    // to the true forward distance even when head has rolled over and
    // tail has not. Walk the boundary itself: tail just below UINT32_MAX,
    // head having wrapped a few ticks past 0.
    {
        uint32_t tail = 0xFFFFFFFEu; // UINT32_MAX - 1
        uint32_t head = tail;        // start empty at the boundary
        assert(model_tx_count(head, tail) == 0);

        head = head + 1u; // == 0xFFFFFFFF, no wrap yet
        assert(model_tx_count(head, tail) == 1);

        head = head + 1u; // wraps: 0xFFFFFFFF + 1 == 0 (uint32_t)
        assert(head == 0u);
        assert(model_tx_count(head, tail) == 2);

        head = head + 5u; // == 5, tail still 0xFFFFFFFE
        assert(model_tx_count(head, tail) == 7);
        assert(model_tx_count(head, tail) == PL_A2DP_TX_QUEUE_SLOTS - 1u); // at usable ceiling, straddling the rollover

        // One more producer step here would be the classic "queue full,
        // do not seal" case straddling the exact 2^32 wrap -- confirm it
        // is NOT misread as empty (the aliasing bug this reserved slot
        // exists to prevent).
        assert(model_tx_count(head, tail) != 0);
    }

    // --- both indices freshly reset (stream restart) still behave ---
    assert(model_tx_count(0, 0) == 0);

    printf("test_a2dp_tx_ring_count: ALL PASSED\n");
    return 0;
}
