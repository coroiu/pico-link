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
//
// pico-link-8v3.2.3 CODE REVIEW FOLLOW-UP, RESOLVED: a code review flagged a
// live capture where the printed btstack_timer_fired jumped by 138 in one
// heartbeat line, which looked like the vendored, unpatched
// btstack_run_loop_base_process_timers()'s uncapped same-pass re-fire loop
// (it snapshots `now` once per execute_once() call and re-fires any timer
// still <= that snapshot) melting down from a bad hal_time_ms() reading. An
// exponential-backoff mitigation was added and briefly lived here, keyed off
// an execute-once epoch counter to detect same-pass re-fires.
//
// It was removed after a controlled disconnect experiment on the
// PRE-mitigation commit (f35de01) disproved the premise: closing the CDC
// reader for 45s and reopening showed timer_probe_fired_count had climbed
// steadily and correctly the entire time it was disconnected (~1/s, no
// burst) - the apparent "jump" was two stale heartbeat lines that had been
// queued behind a BLOCKED `class.write_packet().await` (embassy-usb backs
// up when nothing drains the endpoint) draining back-to-back the instant the
// host reconnected, immediately followed by one genuinely fresh, correct
// reading. Both the original 138x jump and this rewired capture are fully
// explained by that console-side artifact - not by hal_time_ms, not by
// btstack_run_loop_embedded, not by the vendored process_timers loop.
// Confirming evidence the backoff mitigation was actively harmful, not
// neutral: WITH it applied, the very first same-pass re-fire it "caught"
// (itself a false positive from this same artifact) escalated the timer's
// real next-fire schedule out by however many doublings had accumulated -
// self-inflicting the exact "timer barely advances for tens of seconds"
// symptom it was written to prevent, which is what the code review's
// disconnect-test follow-up actually measured. See pico-link-8v3.2.3's
// bead comments for the full capture data. Net: hal_time_ms and this timer
// need no defence here; if a REAL same-pass burst is ever confirmed via
// instrumentation that cannot itself be fooled by a blocked write (e.g. a
// debug channel that never backs up), reach for a simple fires-per-pass
// cap, not open-ended exponential backoff.
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
