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
    // Bead pico-link-nli.4 (G3, epic pico-link-nli): fed from core1's
    // s_enc_heartbeat advancing -- NEVER fed by core1 itself calling this
    // function directly (core1 must never call anything that could touch
    // pl_log's mutex or any core0-only state; see a2dp.c's core1 section).
    // Core0's superloop is the sole caller, same single-producer-per-slot
    // discipline every other subsystem here already has -- it just reads
    // core1's heartbeat counter and re-kicks this slot when it has moved.
    // Enabled only while streaming (STREAM_STARTED/SUSPENDED/RELEASED),
    // same lifecycle as PL_WDT_MEDIA above, so an idle encoder is never
    // judged stale.
    PL_WDT_ENCODER,
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
// --- Superloop checkpoint trace (bead pico-link-okx round 3) --------------
//
// WHY THIS EXISTS. The 2026-08-30 soak took a HARDWARE watchdog expiry whose
// boot line read "UNATTRIBUTED", and the reason is structural, not bad luck:
//
//   * pl_wdt_service() -- the ONLY caller of watchdog_update() -- runs at
//     main.c's superloop (THREAD context).
//   * All four pl_wdt_kick() sites run in INTERRUPT context: usb_pump.c's
//     0xC0 user IRQ pended by the 1ms hardware timer (which PREEMPTS the
//     superloop), bt.c's BTstack handler, a2dp.c's media timer.
//
// So when the superloop stalls, every subsystem counter keeps advancing
// perfectly while watchdog_update() is never reached. The supervisor cannot
// attribute a superloop stall BY CONSTRUCTION -- and a superloop stall is
// exactly what it caught. Measured: the stall exceeded the 2000ms watchdog
// budget while NO instrument on the board recorded anything above ~13ms.
//
// This is the missing half: a checkpoint id stamped as the loop walks its
// body, kept in .uninitialized_data (NOLOAD -- survives a watchdog reset,
// which does not clear SRAM) and read back on the next boot. It is the
// loop's program counter by proxy. Deliberately NOT in watchdog scratch:
// panic_recorder.c owns scratch[0..3] and pico-sdk's watchdog_enable/
// watchdog_reboot own scratch[4..7], so all eight are already spoken for.
//
// PL_WDT_CP_BLIT_DMA_WAIT and PL_WDT_CP_BLIT_SPI_DRAIN exist to separate the
// two UNBOUNDED waits found inside st7789_blit_framebuffer():
// dma_channel_wait_for_finish_blocking() and the bare while(spi_is_busy())
// spin. Neither has a timeout, both run every frame, and no counter watches
// either. If the next expiry names one of them, that is the answer.
typedef enum {
    PL_WDT_CP_NONE = 0,
    PL_WDT_CP_LOOP_TOP,
    PL_WDT_CP_INPUT_POLL,
    PL_WDT_CP_UI_INPUT,
    PL_WDT_CP_DEBUG_REMOTE,
    PL_WDT_CP_BT_DRAIN,
    PL_WDT_CP_UI_TICK,
    PL_WDT_CP_UI_RENDER,
    PL_WDT_CP_BLIT_ENTER,
    PL_WDT_CP_BLIT_DMA_WAIT,
    PL_WDT_CP_BLIT_SPI_DRAIN,
    PL_WDT_CP_BLIT_EXIT,
    PL_WDT_CP_BT_POLL_CMDS,
    PL_WDT_CP_BT_POLL_FFI,
    PL_WDT_CP_BT_POLL_DISPATCH,
    // Bead pico-link-okx round 4: one mark per command.tag inside the C
    // dispatch switch of pl_bt_poll_commands(), plus a "returned" mark after
    // each handler. Round 3 named BT_POLL_DISPATCH as the last checkpoint
    // before a >2s stall (n=1); these say WHICH tag's handler was entered and
    // not returned from. CMD_NONE is stamped on the overwhelmingly common
    // no-command path so that "reached the switch, dispatched nothing" is
    // distinguishable from "stalled before the switch" -- without it, a stall
    // with tag NONE is indistinguishable from the round-3 signature.
    PL_WDT_CP_CMD_NONE,
    PL_WDT_CP_CMD_SCAN_CALL,
    PL_WDT_CP_CMD_SCAN_RET,
    PL_WDT_CP_CMD_CONNECT_ENTER,   // case entered: pl_log + push_link_state ahead
    PL_WDT_CP_CMD_CONNECT_A2DP,    // immediately before pl_a2dp_connect()
    PL_WDT_CP_CMD_CONNECT_RET,
    PL_WDT_CP_CMD_CANCEL_SCAN_CALL,
    PL_WDT_CP_CMD_CANCEL_SCAN_RET,
    PL_WDT_CP_CMD_OTHER,           // a tag with no case (e.g. CANCEL_CONNECT=4)
    // Round 4, second finding: PL_WDT_CP_REPORT existed in this enum and in
    // the name table but had NO call site, so the ~80 lines between
    // pl_bt_poll_commands() returning and pl_wdt_mark(WDT_SERVICE) were
    // entirely unmarked -- the once-a-second usb-audio/usb-pump/a2dp report
    // (about 15 pl_log calls) and pl_log_ring_drain(), which pushes to the
    // CDC console. A stall anywhere in there stamps BT_POLL_DISPATCH as
    // LAST, which is exactly the round-3 soak3 signature. These four marks
    // partition that region so the dispatch switch and the report block can
    // be told apart.
    PL_WDT_CP_REPORT,          // stamped the instant pl_bt_poll_commands returns
    PL_WDT_CP_REPORT_FRAME,    // per-60-frame timing pl_log
    PL_WDT_CP_REPORT_SHARED,   // pl_usb_pump_report + pl_a2dp_report
    PL_WDT_CP_LOG_DRAIN,       // pl_log_ring_drain()
    PL_WDT_CP_WDT_SERVICE,
    PL_WDT_CP_COUNT
} pl_wdt_checkpoint_t;

// Stamps the current checkpoint. Cheap by design (a handful of stores) --
// it runs ~14x per frame and must never be the thing that slows the loop.
void pl_wdt_mark(pl_wdt_checkpoint_t cp);

// Bead pico-link-okx (F4): snapshots watchdog_hw->reason/scratch[0]/
// scratch[4] into module-private state and bumps the NOLOAD boot_seq
// counter. Call as the LITERAL FIRST STATEMENT of main() -- before
// pl_log_ring_init(), before anything -- so no other code in this firmware
// can run first and disturb the registers this reads. Replaces the old
// "pl_wdt_report_boot_reason() MUST run before pl_wdt_arm()" landmine
// (enforced only by a comment) with a snapshot taken before there is
// anything left to race against. Also must run before
// pl_panic_report_and_clear() clears scratch[0..3], for the same reason the
// old single function did.
void pl_wdt_capture_boot_reason(void);

// Formats the snapshot pl_wdt_capture_boot_reason() took -- does not touch
// watchdog_hw itself any more. Safe to call any time after the capture
// call above; its position in main() is no longer load-bearing for
// correctness, only for boot-log ordering. Also reports the persisted
// checkpoint loop-trace ring (moved to run LAST within this function, bead
// pico-link-okx F4, so the higher-priority boot-reason line survives even
// if the console is still degraded when the longer ring dump runs).
void pl_wdt_report_boot_reason(void);

// Shared subsystem-id -> name lookup, used by both this module's own
// reporting and panic_recorder.c's WDT breadcrumb case.
const char *pl_wdt_subsys_name(pl_wdt_subsys_t s);

#ifdef __cplusplus
}
#endif

#endif // PICO_LINK_WATCHDOG_SUP_H
