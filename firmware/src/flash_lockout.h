#ifndef PL_FLASH_LOCKOUT_H
#define PL_FLASH_LOCKOUT_H

#include <stdint.h>

// Pico Link firmware -- bead pico-link-nli.2 (epic pico-link-nli, "LDAC
// encoder on core1", G1). Design of record:
// .planning/decisions/2026-09-03-ldac-encoder-on-core1.md sec 6, which
// narrows the older pico-link-15n: only the MULTICORE LOCKOUT half is a
// prerequisite for launching core1 (G3); the RAM-resident USB ISO ISR half
// stays deferred with 15n.
//
// This module makes every flash write in the firmware safe against a
// concurrently-running core1 fetching code/data from the very flash bank
// being erased/programmed, WITHOUT requiring core1 to exist yet. See
// flash_lockout.c's module doc for the full mechanism and why the pico-sdk
// default flash_safety_helper_t is unsuitable here.

// Call once, from core1's entry point, before it does anything else that
// might race a core0 flash write. Registers core1 as a multicore_lockout
// victim (pico/multicore.h) so core0's flash writes can park it safely.
//
// NOT CALLED ANYWHERE YET in this bead -- core1 is not launched until G3
// (pico-link-nli.4). Exists now so G3's core1 entry point has a single,
// correct call to make; wiring it in is that bead's job, not this one.
//
// Precondition this function does NOT and CANNOT enforce, so G3 must:
// core1 must keep interrupts enabled and service its own SIO FIFO IRQ
// promptly for the rest of its life after calling this -- any
// interrupts-off window on core1 can hang a core0 flash write forever
// (mitigated on core0's side by the timeout variants this module uses, but
// a hung core1 is still a dead encoder either way).
void pl_flash_lockout_core1_init(void);

// Diagnostics, read-only, monotonic, never reset. Bumped whenever a
// lockout start/end handshake with core1 timed out (core1 wedged or not
// yet servicing its FIFO IRQ). Zero on a single-core build/boot, and
// necessarily zero until G3 launches core1 -- there is no "other core" to
// time out on yet. Intended for G3/G5's reporter, not wired into one here
// (out of this bead's scope).
uint32_t pl_flash_lockout_timeout_count(void);

// Diagnostics: how many times the REAL multicore-lockout handshake ran
// (as opposed to the single-core interrupts-only fallback). Zero until G3
// launches core1 and calls pl_flash_lockout_core1_init() -- exists so a
// future reporter/test can distinguish "no flash writes happened" from
// "flash writes happened but core1 wasn't up yet to be locked out".
uint32_t pl_flash_lockout_active_count(void);

#endif
