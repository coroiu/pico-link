// Panic recorder -- bd pico-link-gap.
//
// DEV AFFORDANCE, NOT FOR SHIP AS-IS: this rewires panics and hardfaults to
// reboot the board (warm on the first one, into the USB bootloader on an
// unresolved second one) so the failure is diagnosable without a physical
// power-cycle. A shipped audio dongle that reboots into BOOTSEL on a field
// fault looks completely bricked to the user -- gate this out (see
// PL_DIAG_DISABLE_PANIC_RECORDER in CMakeLists.txt/main.c) before anything
// ships, same as the watchdog and VID/PID dev affordances it sits next to.
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

#ifdef __cplusplus
}
#endif

#endif
