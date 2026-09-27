// Pico Link firmware -- host-buildable test for bead pico-link-j5su: a
// pending-action-queue push that gets dropped for being full, OR races the
// IRQ-context drain, must NOT let the caller's own *_write_enqueued latch
// get stuck true forever.
//
// MODEL test, not a link test -- same convention as
// test_preset_persistence_field_mask_and_lifecycle.c and every other
// firmware/tests/test_*.c file (firmware/src/bt.c and persist.c cannot be
// linked on host: BTstack, pico-sdk -- see pico-link-6cho, still open).
// The functions below are copied/simplified from the real ones cited
// below. If the real functions change, update both here and there --
// nothing enforces that they stay in sync (see LIMITATION below).
//
// Round 1 root cause (Ada, pico-link-ryw.14 review; confirmed reading
// firmware/src/bt.c:1199-1220 and firmware/src/persist.c:1314-1397):
// pl_bt_pending_push (bt.c) drops the entry and returns nothing when the
// queue (capacity PL_BT_PENDING_CAPACITY) is full. Every one of persist.c's
// six pl_persist_service producers latched its own s_*_write_enqueued flag
// to true unconditionally, right before calling the pl_bt_enqueue_*_write
// wrapper around that push. On a drop, the flag was permanently true with
// nothing left in the queue to ever service it and clear it.
// Round-1 fix: latch only on a successful push (assign the push's return
// value directly). That closed the drop case but opened a NEW, narrower
// race (round 2 below).
//
// Round 2 root cause (code review, pico-link-j5su reopened): the round-1
// fix was `s_write_enqueued = pl_bt_enqueue_persist_write();` -- push
// FIRST, latch on its return value SECOND. But pl_bt_pending_push (bt.c)
// re-enables interrupts before it returns, and the drain
// (pl_bt_pending_service) runs from a btstack timer in
// async_context_threadsafe_background IRQ context -- it can preempt and
// run to completion *during* the push call, before push's return value
// ever reaches the caller. If that happens: the entry gets enqueued AND
// drained/serviced (with the flag still false, since the caller hasn't
// latched it yet, so the drain's clear-on-service is a no-op) all before
// push returns -- then the caller's post-return assignment stomps the flag
// to true anyway, even though the write already completed and nothing is
// left in the queue to ever clear it again. Same wedge as round 1, reached
// a different way.
//
// Round 3 fix (this test, current): latch the flag TRUE *before* calling
// the push, and only claw it back to false if the push reports the entry
// was dropped (queue full). Now any drain that preempts during or after
// the push sees a flag that is already true and can correctly clear it;
// a drop can't have raced the drain, since nothing was ever queued for the
// drain to touch.
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

// Copied (behaviour) from bt.c's pl_bt_pending_service (bt.c:1297-1382) --
// drains everything currently queued. `flag`, if non-NULL, models the
// IRQ-context completion handler (persist.c's execute_pending_*_write)
// clearing the caller's *_write_enqueued latch as each entry is serviced --
// exactly what races the caller's own post-push assignment in the round-2
// bug. Returns how many entries were drained.
static int model_pending_service(bool *flag) {
    int drained = 0;
    while (s_pending_tail != s_pending_head) {
        s_pending_tail = (uint8_t)((s_pending_tail + 1) % MODEL_PENDING_CAPACITY);
        drained++;
        if (flag != NULL) {
            *flag = false;
        }
    }
    return drained;
}

// Copied (behaviour) from the ring-buffer enqueue at the heart of bt.c's
// pl_bt_pending_push (bt.c:1199-1220): returns true if queued, false if
// the queue was full and the entry was dropped. No drain simulation here
// -- this is the raw enqueue, used by tests that need to fill the queue
// without also triggering a drain.
static bool model_pending_enqueue(int tag) {
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

// Copied (behaviour) from bt.c's pl_bt_pending_push (bt.c:1199-1220), PLUS
// the race window it opens: pl_bt_pending_push re-enables interrupts
// before returning, so the real IRQ-context drain can run to completion
// *inside* this call, before control ever returns to the caller.
// `flag_racing_drain`, if non-NULL, models the caller's own
// *_write_enqueued flag and lets a test observe exactly what state the
// drain leaves it in before the caller's own post-push logic runs.
static bool model_pending_push(int tag, bool *flag_racing_drain) {
    bool queued = model_pending_enqueue(tag);
    if (!queued) {
        return false;
    }
    // Simulate the IRQ-context drain preempting here, between the enqueue
    // and this function's return -- the exact window bt.c's re-enabling of
    // interrupts opens.
    model_pending_service(flag_racing_drain);
    return true;
}

// --- Round-1 (buggy) latch contract: push first, latch on the return
// value second -- copied (behaviour) from persist.c's producer blocks as
// they stood after the round-1 fix, before this bead was reopened. ---
static bool model_service_tick_post_latch(bool *write_enqueued, int tag) {
    if (*write_enqueued) {
        return false;
    }
    bool pushed = model_pending_push(tag, write_enqueued);
    *write_enqueued = pushed;
    return pushed;
}

// --- Round-3 (fixed) latch contract: latch true BEFORE the push, claw
// back to false only if the push reports a drop -- copied (behaviour)
// from persist.c's pl_persist_service producer blocks post-fix. ---
static bool model_service_tick_pre_latch(bool *write_enqueued, int tag) {
    if (*write_enqueued) {
        return false;
    }
    *write_enqueued = true;
    bool pushed = model_pending_push(tag, write_enqueued);
    if (!pushed) {
        *write_enqueued = false;
    }
    return pushed;
}

static void test_push_that_fits_succeeds(void) {
    model_pending_reset();
    bool write_enqueued = false;
    bool pushed = model_service_tick_pre_latch(&write_enqueued, 1);
    assert(pushed);
    // The push's own (simulated) drain already serviced the only entry in
    // the queue, so the flag is correctly back to false -- there's nothing
    // left to service.
    assert(!write_enqueued);
    printf("test_push_that_fits_succeeds: PASS\n");
}

static void test_push_against_full_queue_is_dropped(void) {
    model_pending_reset();
    // Fill the queue to capacity-1 (a ring buffer with N slots holds N-1
    // entries -- same head==tail-means-empty convention as bt.c's).
    assert(model_pending_enqueue(100));
    assert(model_pending_enqueue(101));
    bool ok = model_pending_enqueue(102); // queue now full
    assert(!ok);
    assert(s_drop_count == 1);
    printf("test_push_against_full_queue_is_dropped: PASS\n");
}

// Pins down the ROUND-2 BUG: latching on the push's return value, after
// the call, loses a race against the IRQ-context drain that can run
// (and clear the not-yet-latched flag) inside the push call itself. This
// must FAIL against the fixed (pre-latch) contract's guarantee -- it is
// here specifically to prove the old ordering is wrong, not to pass.
static void test_post_latch_wedges_on_drain_race_during_push(void) {
    model_pending_reset();
    bool write_enqueued = false;

    // The push succeeds and, per the model, the IRQ-context drain
    // preempts and services this exact entry before push returns -- the
    // drain "clears" write_enqueued, but it was never set true yet, so
    // that's a no-op, and the entry is now gone from the queue forever.
    bool pushed = model_service_tick_post_latch(&write_enqueued, 200);
    assert(pushed);

    // BUG: the caller's post-push assignment now stomps the flag back to
    // true, even though the write already completed and nothing remains
    // in the queue to ever service and clear it again.
    assert(write_enqueued);
    int drained = model_pending_service(&write_enqueued);
    assert(drained == 0); // nothing left -- it was already drained above
    assert(write_enqueued); // stuck true forever: this write kind is dead
    printf("test_post_latch_wedges_on_drain_race_during_push: PASS (bug reproduced)\n");
}

// Proves the ROUND-3 FIX: latching true before the push means the
// in-push drain race sees a flag that is already true and can correctly
// clear it, so the flag ends up false with the write fully serviced --
// no wedge, and nothing left to retry.
static void test_pre_latch_survives_drain_race_during_push(void) {
    model_pending_reset();
    bool write_enqueued = false;

    bool pushed = model_service_tick_pre_latch(&write_enqueued, 200);
    assert(pushed);
    // Correctly serviced and cleared during the push itself.
    assert(!write_enqueued);
    int drained = model_pending_service(&write_enqueued);
    assert(drained == 0); // nothing left to drain -- already serviced
    assert(!write_enqueued);
    printf("test_pre_latch_survives_drain_race_during_push: PASS\n");
}

// Proves the fix also still covers the round-1 drop case: same drop, but
// the pre-latch contract claws the flag back to false on a reported drop,
// so a later tick retries and eventually succeeds once the queue drains.
static void test_pre_latch_retries_and_recovers_from_drop(void) {
    model_pending_reset();
    assert(model_pending_enqueue(100));
    assert(model_pending_enqueue(101)); // queue full now

    bool write_enqueued = false;
    bool pushed = model_service_tick_pre_latch(&write_enqueued, 200);
    assert(!pushed); // dropped
    assert(!write_enqueued); // FIX: clawed back to false, so a retry is possible

    // Simulate the heartbeat draining the two entries that were already
    // queued, same as a real IRQ-context pl_bt_pending_service tick.
    int drained = model_pending_service(NULL);
    assert(drained == 2);

    // Next superloop iteration's pl_persist_service-equivalent tick
    // retries the same write kind and this time it lands (and its own
    // in-push drain immediately services and clears it, same as the
    // no-contention case above).
    pushed = model_service_tick_pre_latch(&write_enqueued, 200);
    assert(pushed);
    assert(!write_enqueued);
    printf("test_pre_latch_retries_and_recovers_from_drop: PASS\n");
}

int main(void) {
    test_push_that_fits_succeeds();
    test_push_against_full_queue_is_dropped();
    test_post_latch_wedges_on_drain_race_during_push();
    test_pre_latch_survives_drain_race_during_push();
    test_pre_latch_retries_and_recovers_from_drop();
    printf("All tests passed.\n");
    return 0;
}
