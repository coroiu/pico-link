// Pico Link firmware -- host-buildable test for bead pico-link-j5su: a
// pending-action-queue push that gets dropped for being full must NOT let
// the caller's own *_write_enqueued latch stay stuck true forever.
//
// MODEL test, not a link test -- same convention as
// test_preset_persistence_field_mask_and_lifecycle.c and every other
// firmware/tests/test_*.c file (firmware/src/bt.c and persist.c cannot be
// linked on host: BTstack, pico-sdk -- see pico-link-6cho, still open).
// The functions below are copied/simplified from the real ones cited
// below. If the real functions change, update both here and there --
// nothing enforces that they stay in sync (see LIMITATION below).
//
// Root cause (Ada, pico-link-ryw.14 review; confirmed reading
// firmware/src/bt.c:1199-1220 and firmware/src/persist.c:1314-1397):
// pl_bt_pending_push (bt.c) drops the entry and returns nothing when the
// queue (capacity PL_BT_PENDING_CAPACITY) is full. Every one of persist.c's
// six pl_persist_service producers latched its own s_*_write_enqueued flag
// to true unconditionally, right before calling the pl_bt_enqueue_*_write
// wrapper around that push. On a drop, the flag is now permanently true
// with nothing left in the queue to ever service it and clear it -- that
// write kind (e.g. SAVE_PRESET) never runs again until reboot.
//
// Fix under test: pl_bt_pending_push now returns bool (pushed vs dropped),
// threaded back through all six pl_bt_enqueue_*_write wrappers
// (firmware/src/bt.c, firmware/src/bt.h). persist.c's pl_persist_service
// now only latches its *_write_enqueued flag on true. On a drop the flag
// stays false, so the next pl_persist_service call (superloop, every
// iteration -- see main.c:894) retries the push instead of wedging.
//
// What this proves:
//   1. A push that fits succeeds, and the caller may latch its flag.
//   2. A push against a full queue is dropped (returns false) and does NOT
//      let the caller latch its flag.
//   3. A "stuck" model (the pre-fix behaviour: latch unconditionally) never
//      retries once one push is dropped -- this pins down the BUG so a
//      regression back to unconditional latching is caught.
//   4. The fixed model (latch only on success) retries on every
//      pl_persist_service-equivalent tick and eventually succeeds once the
//      queue drains, and the flag is false again once serviced.
//
// LIMITATION (same one test_preset_persistence_field_mask_and_lifecycle.c's
// own header documents): this is a hand-copied model, not a link test.
// Nothing here compiles or links firmware/src/bt.c or persist.c itself, so
// a revert of the real fix would NOT fail this test -- it only documents
// and pins down the intended behaviour of the copy. A real link test needs
// the firmware/tests infrastructure pico-link-6cho is tracking.
//
// Build + run (no CMake target -- same convention as the other firmware/
// tests/test_*.c files):
//   cc -std=c11 -Wall -Wextra \
//      firmware/tests/test_pending_queue_write_enqueued_recovery.c \
//      -o /tmp/test_pending_queue_write_enqueued_recovery && \
//      /tmp/test_pending_queue_write_enqueued_recovery
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

// --- Copied (shape), simplified from bt.c's s_bt_pending ring buffer
// (bt.c:1152-1161) -- a tiny capacity so the "full" case is reachable
// without needing PL_BT_PENDING_CAPACITY (8) real entries. Single tag is
// enough: this test is about the enqueue/latch contract, not about which
// BTstack call a given tag maps to. ---
#define MODEL_PENDING_CAPACITY 3
static int s_pending[MODEL_PENDING_CAPACITY];
static uint8_t s_pending_head;
static uint8_t s_pending_tail;
static uint32_t s_drop_count;

static void model_pending_reset(void) {
    memset(s_pending, 0, sizeof(s_pending));
    s_pending_head = 0;
    s_pending_tail = 0;
    s_drop_count = 0;
}

// Copied (behaviour) from bt.c's pl_bt_pending_push (bt.c:1199-1220,
// post-j5su): returns true if queued, false if the queue was full and the
// entry was dropped. No addr/tag payload needed for this model -- the
// entry itself doesn't matter, only occupancy.
static bool model_pending_push(int tag) {
    uint8_t head = s_pending_head;
    uint8_t next_head = (uint8_t)((head + 1) % MODEL_PENDING_CAPACITY);
    if (next_head == s_pending_tail) {
        s_drop_count++;
        return false;
    }
    s_pending[head] = tag;
    s_pending_head = next_head;
    return true;
}

// Copied (behaviour) from bt.c's pl_bt_pending_service (bt.c:1297-1382) --
// drains everything currently queued. Returns how many entries were
// drained, which the tests below use to confirm a supposedly-full queue
// actually drains before checking that the retry landed.
static int model_pending_service(void) {
    int drained = 0;
    while (s_pending_tail != s_pending_head) {
        s_pending_tail = (uint8_t)((s_pending_tail + 1) % MODEL_PENDING_CAPACITY);
        drained++;
    }
    return drained;
}

// --- The pre-fix (buggy) latch contract: sets the flag unconditionally,
// ignoring the push's return value -- copied (behaviour) from every
// pl_persist_service producer block as they stood before pico-link-j5su
// (e.g. persist.c:1360-1371's SAVE_PRESET block, pre-fix). ---
static bool model_service_tick_unconditional_latch(bool *write_enqueued, int tag) {
    if (!*write_enqueued) {
        *write_enqueued = true;
        return model_pending_push(tag);
    }
    return false;
}

// --- The fixed latch contract: only latches on a successful push -- copied
// (behaviour) from persist.c's pl_persist_service post-j5su (e.g. the
// SAVE_PRESET block at persist.c:1360-1371 after the fix). ---
static bool model_service_tick_fixed_latch(bool *write_enqueued, int tag) {
    if (!*write_enqueued) {
        bool pushed = model_pending_push(tag);
        if (pushed) {
            *write_enqueued = true;
        }
        return pushed;
    }
    return false;
}

static void test_push_that_fits_succeeds(void) {
    model_pending_reset();
    bool write_enqueued = false;
    bool pushed = model_service_tick_fixed_latch(&write_enqueued, 1);
    assert(pushed);
    assert(write_enqueued);
    printf("test_push_that_fits_succeeds: PASS\n");
}

static void test_push_against_full_queue_is_dropped(void) {
    model_pending_reset();
    // Fill the queue to capacity-1 (a ring buffer with N slots holds N-1
    // entries -- same head==tail-means-empty convention as bt.c's).
    assert(model_pending_push(100));
    assert(model_pending_push(101));
    bool ok = model_pending_push(102); // queue now full
    assert(!ok);
    assert(s_drop_count == 1);
    printf("test_push_against_full_queue_is_dropped: PASS\n");
}

// Pins down the BUG: unconditional latching means a single drop wedges the
// write kind forever -- no amount of retrying the service tick ever
// re-attempts the push, because the flag is already (wrongly) true.
static void test_unconditional_latch_wedges_after_one_drop(void) {
    model_pending_reset();
    assert(model_pending_push(100));
    assert(model_pending_push(101)); // queue full now (capacity 3, 2 held)

    bool write_enqueued = false;
    bool pushed = model_service_tick_unconditional_latch(&write_enqueued, 200);
    assert(!pushed); // dropped
    assert(write_enqueued); // BUG: latched anyway

    // Drain the real queue -- there's nothing to drain for tag 200, it was
    // never queued.
    int drained = model_pending_service();
    assert(drained == 2);

    // Even though the queue is now completely empty, a "later tick" never
    // retries: the flag is still (wrongly) true, so the write is lost
    // until reboot.
    pushed = model_service_tick_unconditional_latch(&write_enqueued, 200);
    assert(!pushed); // model_service_tick_* only pushes when flag is false
    assert(write_enqueued);
    printf("test_unconditional_latch_wedges_after_one_drop: PASS\n");
}

// Proves the fix: same drop, but the fixed latch contract retries on every
// subsequent tick (persist.c's pl_persist_service runs every superloop
// iteration -- main.c:894) and succeeds once the queue has room again.
static void test_fixed_latch_retries_and_recovers(void) {
    model_pending_reset();
    assert(model_pending_push(100));
    assert(model_pending_push(101)); // queue full now

    bool write_enqueued = false;
    bool pushed = model_service_tick_fixed_latch(&write_enqueued, 200);
    assert(!pushed); // dropped
    assert(!write_enqueued); // FIX: NOT latched, so a retry is possible

    // Simulate the heartbeat draining the two entries that were already
    // queued, same as a real IRQ-context pl_bt_pending_service tick.
    int drained = model_pending_service();
    assert(drained == 2);

    // Next superloop iteration's pl_persist_service-equivalent tick
    // retries the same write kind and this time it lands.
    pushed = model_service_tick_fixed_latch(&write_enqueued, 200);
    assert(pushed);
    assert(write_enqueued);

    // The heartbeat drains it; a real caller would clear write_enqueued
    // from the IRQ-context execute_pending_*_write function once the write
    // actually lands (persist.c's pattern, e.g. persist.c:1578) -- model
    // that here too, to show the flag returns to false and the write kind
    // is fully available again.
    drained = model_pending_service();
    assert(drained == 1);
    write_enqueued = false;
    assert(!write_enqueued);
    printf("test_fixed_latch_retries_and_recovers: PASS\n");
}

int main(void) {
    test_push_that_fits_succeeds();
    test_push_against_full_queue_is_dropped();
    test_unconditional_latch_wedges_after_one_drop();
    test_fixed_latch_retries_and_recovers();
    printf("All tests passed.\n");
    return 0;
}
