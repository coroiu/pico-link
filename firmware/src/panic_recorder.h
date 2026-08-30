// Panic recorder -- bd pico-link-gap.
//
// DEV AFFORDANCE, NOT FOR SHIP AS-IS: this rewires panics and hardfaults to
// reboot the board (warm on the first one, into the USB bootloader on an
// unresolved second one) so the failure is diagnosable without a physical
// power-cycle. A shipped audio dongle that reboots into BOOTSEL on a field
// fault looks completely bricked to the user. No compile-time gate exists
// yet (there is no PL_DIAG_DISABLE_PANIC_RECORDER or similar today) --
// this needs one built before anything ships, same as the watchdog and
// VID/PID dev affordances it sits next to.
//
// See the bead for the full design and its ordering (arm the watchdog
// first, then a panic-in-progress flag, then record, then reboot) and
// panic_recorder.c's module doc for the C-first translation notes.
#ifndef PL_PANIC_RECORDER_H
#define PL_PANIC_RECORDER_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// Call once, early in main() -- after stdio_init_all() and the CDC
// attach-grace sleep, so a host terminal has a chance to be listening.
// If the watchdog scratch registers hold a valid, watchdog-caused-reboot
// panic record, prints a report over the CDC console and clears the
// record. No-op (including no scratch-register writes) otherwise.
void pl_panic_report_and_clear(void);

// The C-side half of the one Rust -> C call (see ui-ffi's pl_ui_panic_hook
// and main.c). `msg` points to `len` bytes of the formatted PanicInfo text
// (already includes Rust's file:line); valid only for the duration of this
// call. Arms the watchdog, records, and reboots -- never returns.
void pl_panic_record_rust(const uint8_t *msg, uintptr_t len);

// --- Watchdog supervisor breadcrumb (bead pico-link-ufh) ---
//
// "PLWD" -- a distinct magic from the four panic kinds above, so
// pl_panic_report_and_clear() can tell a supervised stale-subsystem trip
// apart from an actual panic/hardfault/assert on the next boot.
#define PL_PANIC_MAGIC_WDT 0x504c5744u

// Called ONLY from watchdog_sup.c's pl_wdt_service(), on a subsystem stale
// past its deadline. Deliberately a SEPARATE entry point from
// pl_panic_arm_record_and_reboot() (panic_recorder.c) -- it must not touch
// scratch[3] (the panic recorder's own retry/BOOTSEL-escalation flag) and
// must never call reset_usb_boot: a watchdog trip must always come back
// through the regular flash boot path, never into BOOTSEL in the field.
// Writes scratch[0..2] only, then watchdog_reboot(0, 0, ...). Never returns.
void pl_panic_record_watchdog_stale(uint32_t subsys, uint32_t stale_ms) __attribute__((noreturn));

#ifdef __cplusplus
}
#endif

#endif
