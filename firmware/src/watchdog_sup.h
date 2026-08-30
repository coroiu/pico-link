// Pico Link firmware -- software watchdog supervisor (bead pico-link-ufh).
//
// Design of record: .planning/design/2026-08-30-watchdog.md (Ada,
// 2026-08-30). Read it before changing anything here -- in particular the
// "Where the feed lives" and "Reset behaviour" sections, which this header
// only summarizes.
//
// Two mechanisms, one feed site:
//   1. Per-subsystem progress counters (this module). Each producer calls
//      pl_wdt_kick() from wherever it proves it is still doing real work;
//      pl_wdt_service() checks each ENABLED subsystem's counter against a
//      deadline and, on staleness, records a breadcrumb and reboots via
//      panic_recorder's dedicated watchdog entry point.
//   2. The RP2350 hardware watchdog as the backstop for "the superloop
//      stopped executing at all" -- armed by pl_wdt_arm(), fed by
//      pl_wdt_service()'s call to watchdog_update() when nothing is stale.
//
// LOAD-BEARING CONSTRAINT: pl_wdt_service() must be called EXACTLY ONCE, in
// the superloop, from thread context -- never from a timer or an IRQ. The
// 1ms USB pump timer and the 0xC0 worker (usb_pump.c) and the BTstack
// background IRQ all keep running when the superloop itself is wedged; a
// feed from any of those would reduce the watchdog to "is a peripheral IRQ
// still firing", which is exactly the class of failure this project has
// actually suffered (a wedged superloop with peripherals still ticking).
// See the design doc's "Where the feed lives" section -- this is not a
// style preference, it is the whole point of the design.
//
// pl_wdt_kick() is a single `volatile uint32_t` increment and nothing else.
// No 64-bit timestamps are written from IRQ/producer context: a
// `time_us_64()` stamp written from IRQ and read from thread context can
// tear on this architecture. Exactly one producer writes each subsystem's
// counter; pl_wdt_service() is the only reader.
#ifndef PICO_LINK_WATCHDOG_SUP_H
#define PICO_LINK_WATCHDOG_SUP_H

#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef enum {
    PL_WDT_USB_TIMER = 0, // the 1ms timer -> 0xC0 IRQ path is still firing
    PL_WDT_USB_TASK,       // tud_task() is actually being reached
    PL_WDT_BTSTACK,        // the BTstack run loop is still servicing timers
    PL_WDT_MEDIA,          // the A2DP media timer is ticking (conditional)
    PL_WDT_COUNT
} pl_wdt_subsys_t;

// One volatile uint32_t increment. Safe to call from IRQ or thread context
// -- exactly one producer per subsystem, see this file's module doc.
void pl_wdt_kick(pl_wdt_subsys_t s);

// Enables/disables staleness checking for one subsystem. Enabling RE-BASES
// the deadline to now, so a subsystem that was legitimately idle (MEDIA
// before any stream) is never judged stale against history from before it
// was enabled.
void pl_wdt_set_enabled(pl_wdt_subsys_t s, bool enabled);

// Arms the RP2350 hardware watchdog (PL_WDT_TIMEOUT_MS, pause_on_debug =
// true) and re-bases every subsystem's deadline to now. Call once, after
// pl_bt_init() and before the superloop -- see main.c and the design doc's
// implementation checklist step 4.
void pl_wdt_arm(void);

// THE feed site. Call exactly once per superloop iteration, from thread
// context, after pl_a2dp_report() and before the pacing sleep. For each
// enabled subsystem: if its counter changed since the last call, re-base
// its deadline; if not, and it has been stale longer than its deadline,
// record the breadcrumb and reboot (or, under PL_WDT_OBSERVE_ONLY, just log
// and keep feeding). If nothing is stale, feeds the hardware watchdog.
void pl_wdt_service(void);

// For the cz0.6.1 flash-write path: call begin() before disabling
// interrupts and end() after re-enabling them. end() re-bases every
// deadline to now, since the IRQ-driven subsystems (USB_TIMER/USB_TASK/
// BTSTACK) genuinely do not progress while interrupts are off, and without
// this a save would be a guaranteed false trip. Not wired into anything by
// this bead -- cz0.6.1 owns the call sites.
void pl_wdt_blackout_begin(void);
void pl_wdt_blackout_end(void);

// Once-per-second instrumentation: logs, per subsystem, the worst staleness
// observed since the last report against its deadline, then resets that
// max. Call from the superloop, same as pl_wdt_service() -- cheap to call
// every iteration, rate-limits itself.
void pl_wdt_report(void);

// Classifies and logs *why* the board booted, using the raw
// watchdog_hw->reason register and scratch[4] -- see the design doc's
// "Distinguishing reset causes at boot" section for the full decision
// table. MUST be called before pl_wdt_arm(): arming stamps scratch[4] with
// the SDK's own arm marker and destroys the value this function reads.
// Does not touch or clear scratch[0..3] -- that is panic_recorder's job
// (pl_panic_report_and_clear(), which must also run before pl_wdt_arm()).
void pl_wdt_report_boot_reason(void);

// Shared subsystem-id -> name lookup, used by both this module's own
// reporting and panic_recorder.c's WDT breadcrumb case.
const char *pl_wdt_subsys_name(pl_wdt_subsys_t s);

#ifdef __cplusplus
}
#endif

#endif // PICO_LINK_WATCHDOG_SUP_H
