// Pico Link firmware -- bead pico-link-nli.2 (G1: multicore flash lockout).
//
// THE HAZARD: a flash write is flash_range_erase()/flash_range_program()
// executing out of XIP; nothing may fetch code or data from flash while
// that runs. Today (single core, core1 never launched) that only means
// "disable interrupts on core0". Once core1 exists (epic pico-link-nli, G3)
// and is running the LDAC encoder from XIP, a core0 flash write would race
// core1's own instruction fetch unless core1 is parked first. pico-sdk's
// sanctioned mechanism for that is multicore_lockout (pico/multicore.h):
// the victim core (core1) registers once via multicore_lockout_victim_init(),
// and the locker (core0) brackets the write with
// multicore_lockout_start_*/multicore_lockout_end_*.
//
// WHY THIS FILE EXISTS INSTEAD OF JUST LINKING pico_multicore AND CALLING
// IT DONE: pico-sdk's flash_safe_execute() (pico/flash.h), which every
// flash write in this firmware already funnels through (BTstack's own
// link-key writes AND persist.c's -- see below), gets its behaviour from
// get_flash_safety_helper(), declared weak. The instant pico_multicore is
// linked, PICO_FLASH_SAFE_EXECUTE_PICO_SUPPORT_MULTICORE_LOCKOUT flips on
// for the SDK's *default* helper (pico_flash/flash.c) tree-wide -- not just
// for calls we make ourselves. That default's enter_safe_zone treats
// "core1 has not called multicore_lockout_victim_init()" as an UNSAFE
// condition:
//   - PICO_FLASH_ASSERT_ON_UNSAFE defaults to 1 -> assert()/panic on the
//     very first flash write, on a single-core build, before core1 ever
//     exists. That is a crash this bead would introduce, not fix.
//   - Even with that off, enter_safe_zone returns PICO_ERROR_NOT_PERMITTED
//     BEFORE the mutation function runs. pico_flash_bank_erase/_write
//     (pico_btstack/btstack_flash_bank.c) discard flash_safe_execute's
//     return code ("currently we have no way to return an error to the
//     caller anyway") -- so the write would silently never happen. Pairing
//     would appear to succeed and vanish on reboot. Worse than a crash.
//
// So this module overrides get_flash_safety_helper() with a RUNTIME check
// (multicore_lockout_victim_is_initialized(1)) instead of trusting the
// SDK's compile-time knobs, and is therefore correct on the exact same
// binary both before and after G3 launches core1:
//
//   - core1 not a lockout victim (true for every build until G3 runs,
//     including this one) -> behave exactly like the pre-multicore
//     single-core build: disable interrupts on THIS core only, run the
//     mutation, re-enable. No lockout attempted, nothing to time out on,
//     nothing to hang on. THIS IS THE SHIPPING CASE for this bead.
//   - core1 IS a lockout victim (after G3) -> the real
//     multicore_lockout_start/end pair, TIMEOUT variants only (never the
//     _blocking ones), so a wedged core1 fails one save -- counted --
//     instead of hanging the device forever waiting for a core that will
//     never respond (design doc sec 6/7: "never hang").
//
// EVERY FLASH WRITE PATH IN THIS FIRMWARE, audited for this bead (grep for
// flash_range_program/flash_range_erase/flash_safe_execute across
// firmware/src and the vendored pico-sdk tree the build actually links):
//   1. pico_btstack/btstack_flash_bank.c's pico_flash_bank_erase() and
//      pico_flash_bank_write() -- the ONLY call sites of
//      flash_range_erase/flash_range_program reachable from this firmware.
//      Both go through flash_safe_execute(), which is what this file's
//      override intercepts.
//   2. persist.c's calls into btstack_tlv_flash_bank_store_tag/delete_tag
//      (persist.c:537,585,823 as of this bead) -- these call down into (1)
//      via the shared btstack_tlv_flash_bank_t / pico_flash_bank_instance()
//      plumbing; no separate flash access of their own.
//   3. BTstack's OWN link-key writes (hci.c's put_link_key, from inside its
//      HCI event dispatch) -- share the SAME btstack_tlv_flash_bank
//      instance as (2) (persist.h's module doc), so they ALSO funnel
//      through (1). Getting this override right protects BTstack's writes
//      too, not just ours -- there is no second path to separately guard.
//   4. Nothing else in firmware/src touches flash directly. panic_recorder.c,
//      watchdog_sup.c, usb_reset.c and main.c all mention "flash" only in
//      reflash/reboot-type prose, verified by reading each file; none calls
//      flash_range_program/flash_range_erase or flash_safe_execute.
//
// core_init_deinit is kept correct (calls multicore_lockout_victim_init()
// when invoked ON core1) even though nothing calls
// flash_safe_execute_core_init()/_deinit() anywhere in this codebase today
// -- G3's core1 entry point is expected to call
// pl_flash_lockout_core1_init() directly (see flash_lockout.h), but if a
// future change routes that through the SDK's own entry point instead,
// this must not silently do the wrong thing.

#include "flash_lockout.h"

#include "hardware/sync.h"
#include "pico/flash.h"
#include "pico/multicore.h"
#include "pico/platform.h"

// Per-phase timeout for the real lockout handshake. Generous on purpose:
// this only matters once G3 exists, and the failure mode of "too long" is
// a slower save; the failure mode of "too short" is a spurious lost save
// under normal jitter. 5s is far above any plausible core1 scheduling
// delay and far below "the user would notice a hang".
#define PL_FLASH_LOCKOUT_TIMEOUT_US (5u * 1000u * 1000u)

static uint32_t s_saved_irq;
static uint32_t s_timeout_count;
static uint32_t s_active_count;

static bool pl_flash_helper_core_init_deinit(bool init) {
    if (get_core_num() == 1) {
        if (init) {
            multicore_lockout_victim_init();
        }
        // No victim_deinit() call: this design's core1 lifecycle (design
        // doc sec 4.2) never stops core1 once launched, so there is
        // nothing meaningful to undo.
        return true;
    }
    // core0 is always the locker in this design, never the victim; no
    // per-core init needed on its side.
    return true;
}

static int pl_flash_helper_enter_safe_zone(uint32_t timeout_ms) {
    (void)timeout_ms;  // this module owns its own timeout, see above.

    // Runtime, not compile-time: true only once G3 has launched core1 AND
    // core1 has run pl_flash_lockout_core1_init(). False for every build
    // and every boot until then -- including this one.
    if (multicore_lockout_victim_is_initialized(1)) {
        s_active_count++;
        if (!multicore_lockout_start_timeout_us(PL_FLASH_LOCKOUT_TIMEOUT_US)) {
            s_timeout_count++;
            // Do NOT disable interrupts or proceed: the caller
            // (flash_safe_execute) skips the mutation function entirely
            // when enter_safe_zone returns non-PICO_OK, matching the
            // design's "skip the write, keep the RAM-staged value, never
            // hang" failure response.
            return PICO_ERROR_TIMEOUT;
        }
    }
    s_saved_irq = save_and_disable_interrupts();
    return PICO_OK;
}

static int pl_flash_helper_exit_safe_zone(uint32_t timeout_ms) {
    (void)timeout_ms;
    restore_interrupts_from_disabled(s_saved_irq);
    if (multicore_lockout_victim_is_initialized(1)) {
        if (!multicore_lockout_end_timeout_us(PL_FLASH_LOCKOUT_TIMEOUT_US)) {
            s_timeout_count++;
            return PICO_ERROR_TIMEOUT;
        }
    }
    return PICO_OK;
}

static flash_safety_helper_t s_pl_flash_helper = {
    .core_init_deinit = pl_flash_helper_core_init_deinit,
    .enter_safe_zone_timeout_ms = pl_flash_helper_enter_safe_zone,
    .exit_safe_zone_timeout_ms = pl_flash_helper_exit_safe_zone,
};

// Overrides pico-sdk's weak pico_flash/flash.c definition.
flash_safety_helper_t *get_flash_safety_helper(void) {
    return &s_pl_flash_helper;
}

void pl_flash_lockout_core1_init(void) {
    multicore_lockout_victim_init();
}

uint32_t pl_flash_lockout_timeout_count(void) {
    return s_timeout_count;
}

uint32_t pl_flash_lockout_active_count(void) {
    return s_active_count;
}
