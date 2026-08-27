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
// struct avoids that risk entirely. Rust only ever sees two plain
// extern "C" functions with scalar signatures.
#include <stdint.h>
#include "btstack_run_loop.h"

static btstack_timer_source_t timer_probe_source;
static volatile uint32_t timer_probe_fired_count = 0;

static void timer_probe_fired(btstack_timer_source_t *ts) {
    timer_probe_fired_count++;
    // btstack_run_loop_embedded timers are one-shot: re-arm for the next
    // 1000ms from inside the fire callback itself, same pattern BTstack's
    // own periodic users (e.g. hci.c's shutdown timer) follow.
    btstack_run_loop_set_timer(ts, 1000);
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
