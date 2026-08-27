// pico-link-8v3.2.3: end-to-end proof that hal_time_ms (now backed by the
// RP2350 timer via Rust, see main.rs's `btstack_hal` module) and the
// btstack_run_loop_embedded timer mechanism actually work together.
//
// Sets a single recurring btstack_timer_source_t for 1000ms; every time it
// fires it re-arms itself for another 1000ms and bumps a counter. core1
// (see run_core1 in main.rs) polls that counter once per its own loop
// iteration and republishes it into a Rust atomic that core0's CDC console
// task prints once a second - see console_task in main.rs for the actual
// acceptance-criterion evidence.
//
// Kept in C rather than reimplemented in Rust FFI: the only thing this file
// touches is BTstack's own btstack_timer_source_t through BTstack's own C
// API (btstack_run_loop_set_timer_handler/set_timer/add_timer). Replicating
// that struct's field layout by hand in Rust FFI - to build a
// btstack_timer_source_t and set its `process` fn pointer from Rust - would
// be the actual risk of a silently-wrong field offset; letting C, which
// already has the real struct definition via btstack_run_loop.h, own the
// struct avoids that risk entirely. Rust only ever sees plain extern "C"
// functions with scalar signatures.
//
// pico-link-8v3.2.3 CODE REVIEW FOLLOW-UP: a live capture showed
// btstack_timer_fired jump by 138 in a single second (a burst, all inside
// one btstack_run_loop_base_process_timers(now) call - see that function:
// it snapshots `now` ONCE, then loops re-firing any timer whose freshly
// re-armed timeout is still <= that frozen snapshot). fire_log below exists
// to catch the raw hal_time_ms() readings taken during a burst like that,
// so the exact readings that produced it can be inspected instead of
// theorised about.
//
// MITIGATION, applied regardless of trigger: a 165s single-reader capture
// spanning a fresh boot did not reproduce the burst, so the root cause
// (one bad/stale hal_time_ms() reading, per the arithmetic in the review)
// is not confirmed - but the MECHANISM is confirmed by reading
// btstack_run_loop_base_process_timers (vendored, unpatched: it snapshots
// `now` once and loops uncapped) and is a real hazard independent of why
// any one reading might be off. Since we cannot patch that vendored loop
// without inheriting BTstack maintenance surface, the fix lives here: use
// pico_link_execute_once_epoch_get() (bumped by run_core1 on the Rust side
// once per execute_once() call) to detect a same-pass re-fire, and back
// off EXPONENTIALLY on each consecutive one. A normal ~1000ms-later fire
// always lands in a different epoch, so this changes nothing about the
// already-verified once-per-second behaviour; only a same-pass re-fire
// pays the escalating cost, and it terminates within roughly log2(gap)
// re-fires instead of one re-fire per millisecond of gap - e.g. even an
// hour-long bad reading would break out in about 12 iterations, not
// thousands.
#include <stdint.h>
#include "btstack_run_loop.h"
#include "hal_time_ms.h"

extern uint32_t pico_link_execute_once_epoch_get(void);

static btstack_timer_source_t timer_probe_source;
static volatile uint32_t timer_probe_fired_count = 0;
static uint32_t last_fire_epoch = 0xFFFFFFFFu; // sentinel: no fire yet
static uint32_t same_epoch_repeat = 0;

// Ring buffer of every hal_time_ms() reading used to re-arm the timer, in
// fire order (index = fire sequence number mod capacity). 512 is
// comfortably bigger than the 138-fire burst already observed once;
// timer_probe_fired_count_get() is the true (non-wrapping) fire count, so
// callers can tell how many of the CAP slots are valid and where the most
// recent burst starts (fired_count - burst_size).
#define FIRE_LOG_CAP 512
static uint32_t fire_log[FIRE_LOG_CAP];

static void timer_probe_fired(btstack_timer_source_t *ts) {
    uint32_t reading = hal_time_ms();
    fire_log[timer_probe_fired_count % FIRE_LOG_CAP] = reading;
    timer_probe_fired_count++;

    // Mirrors what btstack_run_loop_embedded_set_timer does internally for
    // HAVE_EMBEDDED_TIME_MS (ts->timeout = hal_time_ms() + timeout_in_ms +
    // 1) rather than calling btstack_run_loop_set_timer, which would read a
    // SECOND, possibly different, hal_time_ms() value instead of the one
    // just logged above - the whole point of this rewrite is that the
    // logged reading is provably the one that decided the re-arm.
    //
    // Same-pass re-fire mitigation: if this fire's epoch matches the last
    // fire's epoch, we are being re-invoked inside the SAME
    // btstack_run_loop_base_process_timers(now) pass as our own previous
    // fire - escalate the pushout exponentially instead of the normal
    // 1000ms so this can only loop a handful of times regardless of how
    // far ahead the pass's frozen `now` is. A genuine ~1000ms-later fire
    // always lands in a different epoch (run_core1 bumps it once per
    // execute_once() call, and fires are ~20 calls apart in normal
    // operation), so normal behaviour is untouched.
    uint32_t epoch = pico_link_execute_once_epoch_get();
    uint32_t backoff_ms = 1000;
    if (epoch == last_fire_epoch) {
        same_epoch_repeat++;
        for (uint32_t i = 0; i < same_epoch_repeat; i++) {
            backoff_ms *= 2;
        }
    } else {
        same_epoch_repeat = 0;
    }
    last_fire_epoch = epoch;

    ts->timeout = reading + backoff_ms + 1;
    btstack_run_loop_add_timer(ts);
}

void timer_probe_start(void) {
    btstack_run_loop_set_timer_handler(&timer_probe_source, &timer_probe_fired);
    btstack_run_loop_set_timer(&timer_probe_source, 1000);
    btstack_run_loop_add_timer(&timer_probe_source);
}

uint32_t timer_probe_fired_count_get(void) {
    return timer_probe_fired_count;
}

// Returns the hal_time_ms() reading used for the fire at 0-based sequence
// number `seq` (seq < timer_probe_fired_count_get()). Entries older than
// FIRE_LOG_CAP fires ago have been overwritten; callers must only ask for
// entries still inside that window.
uint32_t timer_probe_fire_log_get(uint32_t seq) {
    return fire_log[seq % FIRE_LOG_CAP];
}
