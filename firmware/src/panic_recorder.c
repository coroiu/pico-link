// Panic recorder -- bd pico-link-gap.
//
// GOAL (from the bead): a panic handler that records what happened,
// survives the reset, and reports it over CDC on the next boot -- without
// the recording ever being able to compromise recovery.
//
// ORDERING IS THE WHOLE POINT (the bead's own words -- kept verbatim as the
// contract every entry point below has to honour):
//   1. Arm the RP2350 hardware watchdog FIRST, at the top of the handler,
//      with a short timeout. Recovery is then guaranteed by silicon before
//      any recording code runs. If the recording panics, hangs or faults,
//      the watchdog still fires. The recording is strictly best-effort by
//      construction.
//   2. Set a panic-in-progress flag before touching anything else, so a
//      recursive panic is detected rather than looped on.
//   3. Only then record, with the dumbest possible code: no allocation, no
//      locks, no interrupts, no generic machinery that could itself fail.
//   4. Then reboot.
//
// C-FIRST TRANSLATION NOTE: this bead was written 2026-08-27 against the
// embassy-usb / Rust-owns-main() spike (pico-link-8v3.2.7), which the
// 2026-08-27 C-first ADR replaced. Two design specifics needed translating:
//
//   - "an .uninit RAM buffer that cortex-m-rt does not zero at startup"
//     becomes pico-sdk's own `.uninitialized_data` section, reached via the
//     `__uninitialized_ram(group)` macro (pico/platform/sections.h). PROVEN
//     by reading, not assumed: pico_crt0/rp2350/memmap_default.ld places
//     `.uninitialized_data` as a NOLOAD section in RAM, entirely separate
//     from `.data`/`.bss`; pico_crt0/crt0.S's boot-time zero loop (the
//     "Zero out the BSS" block) walks only `__bss_start__` .. `__bss_end__`
//     and the preceding `data_cpy_table` copies only `.data`/`.tdata` from
//     flash -- neither touches `.uninitialized_data` at all. So a variable
//     placed there is untouched by the C runtime's startup path, exactly
//     the property the original Rust design needed from `.uninit`.
//
//   - The watchdog scratch registers carry over unchanged -- RP2350
//     silicon, not Rust-runtime-specific. This is "the contract" tier per
//     the bead, so it gets the strongest guarantee: no allocation, no
//     runtime dependency, works whether the reporting stage is Rust, C, or
//     nothing at all.
//
// STORAGE TIER ADJUSTMENT (also from reading, not assuming): the bead
// specs "8 x 32 bits" of scratch for our payload. hardware_watchdog's own
// watchdog_enable()/watchdog_reboot() (see the SDK's watchdog.c) claim
// scratch[4] as a magic marker and, when watchdog_reboot() is given a
// non-zero pc, scratch[5..7] as a pc/sp handshake -- and this file calls
// both of those functions on every panic. Using scratch[4..7] for our own
// payload would have it silently overwritten by the very calls this file
// makes to arm/trigger the reboot. So the payload tier here is scratch[0..3]
// -- 4 registers, not 8, which happens to be exactly the 4 fields the bead
// actually lists: magic, fault-or-PC address, line number (or CFSR for a
// hardfault -- disambiguated by which magic is set), retry counter.
// LEARNED and logged to the bead.
//
// NOTHING HERE CALLS INTO RUST (pico-link-5am/6o2): this file is reachable
// from interrupt context (the HardFault vector) and must never become a
// second violation of "nothing calls into Rust from interrupt context" --
// it is deliberately pure C, touching only watchdog scratch registers, a
// RAM buffer, and stdio.
#include "panic_recorder.h"

#include <assert.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>

#include "hardware/structs/watchdog.h"
#include "hardware/watchdog.h"
#include "pico.h"
#include "pico/bootrom.h"
#include "pico/platform/sections.h"

// For pl_wdt_subsys_name() only (the WDT breadcrumb case below) -- no
// circular dependency: watchdog_sup.h does not include this file.
#include "usb_pump.h"
#include "watchdog_sup.h"

// --- Tier 1: the contract -- watchdog scratch[0..3]. ---

// Three panic kinds, one field to hold them because the magic itself
// disambiguates: which kind determines how scratch[2] ("diag") and
// scratch[1] ("address") are meant to be read back.
#define PL_PANIC_MAGIC_RUST 0x504c5250u      // "PLRP" -- Rust panic via pl_ui_panic_hook
#define PL_PANIC_MAGIC_C 0x504c4343u         // "PLCC" -- C-side panic() (PICO_PANIC_FUNCTION)
#define PL_PANIC_MAGIC_HARDFAULT 0x504c4846u // "PLHF" -- a real Cortex-M HardFault exception
#define PL_PANIC_MAGIC_ASSERT 0x504c4153u    // "PLAS" -- plain assert() (bd pico-link-itf)

// Short enough that a wedged recording path still recovers well inside the
// "a bare *** PANIC *** cost a whole session" territory this bead exists to
// avoid; long enough that the bounded, allocation-free recording below
// (a handful of scratch writes plus a capped byte copy) always finishes
// first in the non-recursive case.
#define PL_PANIC_WATCHDOG_TIMEOUT_MS 500u
// Reboot delay for the *deliberate* watchdog_reboot() call in step 4 --
// short, just enough to let the write actually land before reset.
#define PL_PANIC_REBOOT_DELAY_MS 10u

// --- Tier 2: best-effort -- uninitialized RAM, may be clobbered by bootrom
// code if the BOOTSEL fallback path is ever taken. Never relied on for
// recovery; only for the message text on the next normal boot.
#define PL_PANIC_MSG_CAP 220u
static char __uninitialized_ram(s_panic_msg)[PL_PANIC_MSG_CAP];

// --- Recording (step 3) -- no allocation, no locks, no interrupts. ---

static void pl_panic_record_message(const uint8_t *msg, uintptr_t len) {
    uintptr_t n = len;
    if (n > PL_PANIC_MSG_CAP - 1) {
        n = PL_PANIC_MSG_CAP - 1;
    }
    for (uintptr_t i = 0; i < n; i++) {
        s_panic_msg[i] = (char)msg[i];
    }
    s_panic_msg[n] = '\0';
}

// Steps 1+2+3+4, shared by every entry point below. `magic` selects the
// panic kind; `address` is the fault/PC address (0 if not applicable);
// `diag` is the line number (Rust) or CFSR (HardFault) -- meaning depends
// on `magic`. `msg`/`len` may be NULL/0 (the HardFault path has no
// formatted message, only the register dump already folded into
// `address`/`diag`).
static void __attribute__((noreturn))
pl_panic_arm_record_and_reboot(uint32_t magic, uint32_t address, uint32_t diag, const uint8_t *msg, uintptr_t len) {
    // Step 1 -- FIRST, before anything else is touched: arm the watchdog.
    // From this instant, recovery is guaranteed by silicon within
    // PL_PANIC_WATCHDOG_TIMEOUT_MS regardless of what happens below --
    // including if the code below itself panics, hangs, or faults.
    watchdog_enable(PL_PANIC_WATCHDOG_TIMEOUT_MS, /*pause_on_debug=*/false);

    // Step 2 -- the panic-in-progress flag IS the retry counter: it is read
    // before anything else is touched, and a non-zero value means a
    // previous panic's record was never reported-and-cleared by
    // pl_panic_report_and_clear() on the boot that followed it -- i.e. this
    // is a recursive/unresolved panic, not a fresh one.
    uint32_t retry = watchdog_hw->scratch[3];
    if (retry != 0) {
        // Unresolved. Don't attempt to record or warm-reboot a second time
        // -- go straight to the USB bootloader so the board degrades to
        // flashable instead of looping forever between panic and re-panic.
        // reset_usb_boot() is noreturn and does not depend on anything this
        // file has touched; the watchdog armed above is a moot backstop
        // here (reset_usb_boot should not itself hang) but arming it first
        // cost nothing and stays correct if it somehow did.
        reset_usb_boot(0, 0);
        while (true) {
            tight_loop_contents();
        }
    }
    watchdog_hw->scratch[3] = 1;

    // Step 3 -- record. Plain scratch writes, then a capped byte copy.
    watchdog_hw->scratch[0] = magic;
    watchdog_hw->scratch[1] = address;
    watchdog_hw->scratch[2] = diag;
    if (msg != NULL && len > 0) {
        pl_panic_record_message(msg, len);
    } else {
        s_panic_msg[0] = '\0';
    }

    // Step 4 -- reboot. pc=0 means "normal boot", i.e. the regular flash
    // boot path back into our own firmware (see watchdog_reboot()'s doc
    // comment) -- NOT BOOTSEL -- so pl_panic_report_and_clear() gets to run
    // on the way back up and the record actually gets reported.
    watchdog_reboot(0, 0, PL_PANIC_REBOOT_DELAY_MS);
    while (true) {
        // watchdog_reboot() re-arms the watchdog with the delay above;
        // spin here rather than returning until it fires. The watchdog
        // armed in step 1 is still ticking underneath as a backstop if
        // this loop is somehow reached without watchdog_reboot() having
        // taken effect.
        tight_loop_contents();
    }
}

void pl_panic_record_rust(const uint8_t *msg, uintptr_t len) {
    pl_panic_arm_record_and_reboot(PL_PANIC_MAGIC_RUST, 0, 0, msg, len);
}

// --- Watchdog supervisor breadcrumb (bead pico-link-ufh) ---
//
// Deliberately does NOT call pl_panic_arm_record_and_reboot() above: that
// function writes scratch[3] (the panic recorder's retry/BOOTSEL-escalation
// flag) and, on a non-zero retry, calls reset_usb_boot(). A watchdog trip
// must never poison scratch[3] -- doing so would misdiagnose the NEXT
// genuine panic as "recursive, unresolved" and route it to BOOTSEL in a
// user's hands -- and must never itself reach BOOTSEL either. So this is a
// separate, narrower entry point: scratch[0..2] only, plain
// watchdog_reboot(0, 0, ...) (pc=0 -> regular flash boot, not BOOTSEL).
void __attribute__((noreturn)) pl_panic_record_watchdog_stale(uint32_t subsys, uint32_t stale_ms) {
    watchdog_hw->scratch[0] = PL_PANIC_MAGIC_WDT;
    watchdog_hw->scratch[1] = subsys;
    watchdog_hw->scratch[2] = stale_ms;
    // scratch[3] deliberately untouched.
    watchdog_reboot(0, 0, PL_PANIC_REBOOT_DELAY_MS);
    while (true) {
        tight_loop_contents();
    }
}

// --- C-side panic() entry point (PICO_PANIC_FUNCTION, wired in
// CMakeLists.txt) -- this is the path that produced the bare "*** PANIC ***"
// with zero further context on 2026-08-28 (pico_platform_panic/panic.c:65).
// Fixing that exact failure mode is this function's whole job. ---
void __attribute__((noreturn)) __printflike(1, 0) pl_panic_c_hook(const char *fmt, ...) {
    // Step 1 -- FIRST statement, before fmt/args are touched at all: arm
    // the watchdog. `fmt` and any %s arguments are exactly what a
    // memory-corruption panic can hand us as bad pointers, and vsnprintf
    // below walks them -- code review finding (2026-08-28): this must not
    // run before the watchdog is armed, or a bad pointer here hangs with no
    // recovery guarantee at all. The second watchdog_enable() inside
    // pl_panic_arm_record_and_reboot() is idempotent and harmless.
    watchdog_enable(PL_PANIC_WATCHDOG_TIMEOUT_MS, /*pause_on_debug=*/false);

    // A fixed stack buffer, not anything allocator-backed -- a panic is
    // exactly the moment the allocator (if this build even has a C one;
    // ui-ffi's is Rust-side and unrelated) might be the thing that's
    // broken. vsnprintf itself performs no allocation.
    char buf[PL_PANIC_MSG_CAP];
    int n = 0;
    if (fmt != NULL) {
        va_list args;
        va_start(args, fmt);
        n = vsnprintf(buf, sizeof(buf), fmt, args);
        va_end(args);
        if (n < 0) {
            n = 0;
        } else if ((size_t)n > sizeof(buf) - 1) {
            n = (int)sizeof(buf) - 1;
        }
    }
    // The caller's return address (i.e. roughly where in pico-sdk/our code
    // panic() was invoked from) is the closest thing to a "PC" available
    // here -- pico-sdk's panic() is a naked asm forwarder, so this is its
    // own return address, not a deeper frame, but it is still more than
    // the bare banner gave us before.
    uint32_t caller = (uint32_t)(uintptr_t)__builtin_return_address(0);
    pl_panic_arm_record_and_reboot(PL_PANIC_MAGIC_C, caller, 0, (const uint8_t *)buf, (uintptr_t)n);
}

// --- assert() entry point (bd pico-link-itf) -- newlib's __assert_func is
// WEAK (pico_clib_interface/newlib_interface.c:163) and this strong
// definition overrides it at link time. Unlike panic()/PICO_PANIC_FUNCTION,
// plain assert() does NOT go through pl_panic_c_hook above at all -- it is
// a wholly separate newlib code path. Left unoverridden it prints once and
// calls _exit(1), which (PICO_ENTER_USB_BOOT_ON_EXIT is not defined in this
// project) spins forever in a bare __breakpoint() loop with NO watchdog
// ever armed -- the exact silent-hang failure mode this file exists to
// close.
//
// CORRECTION (2026-09-01, bead pico-link-at3): this comment used to claim
// NDEBUG is not defined anywhere in firmware/CMakeLists.txt and that
// assert() is therefore live in the shipped (default) build. THAT IS
// WRONG -- verified 2026-08-29 by rebuilding bare and reading flags.make.
// firmware/CMakeLists.txt sets no CMAKE_BUILD_TYPE, and pico-sdk's
// pico_pre_load_toolchain.cmake:5-11 forces CMAKE_BUILD_TYPE=Release
// whenever the caller leaves it unset. A Release build defines NDEBUG, so
// assert() IS ELIDED PROJECT-WIDE in the default build -- ours,
// pico-sdk's, BTstack's and TinyUSB's alike -- and this override is DEAD
// CODE there. The override still matters: it is what protects a Debug
// build (`cmake -DCMAKE_BUILD_TYPE=Debug`), which is the one you reach for
// when debugging on device and where assert() is genuinely reachable.
// Signature must match newlib's exactly (see <assert.h>) or this override
// does not link against the weak symbol's call sites. ---
void __attribute__((noreturn)) __assert_func(const char *file, int line, const char *func, const char *failedexpr) {
    // Step 1 -- FIRST statement, before file/func/failedexpr are touched at
    // all: arm the watchdog. Same reasoning as pl_panic_c_hook above --
    // these are exactly the pointers a memory-corruption-triggered assert
    // can hand us as garbage, and the formatting below walks them. Treat
    // every one of them as untrusted and bound every read.
    watchdog_enable(PL_PANIC_WATCHDOG_TIMEOUT_MS, /*pause_on_debug=*/false);

    // A fixed stack buffer, not anything allocator-backed, same as
    // pl_panic_c_hook. snprintf itself performs no allocation and is
    // bounded by sizeof(buf) regardless of how long (or how bogus) the
    // input strings are -- it will still walk a bad pointer until it finds
    // a NUL or hits the field width, same residual risk pl_panic_c_hook
    // already accepts for panic()'s fmt/args, and the watchdog armed above
    // is exactly the backstop for that.
    char buf[PL_PANIC_MSG_CAP];
    int n = snprintf(buf, sizeof(buf), "assert \"%s\" failed: %s:%d%s%s",
                      failedexpr != NULL ? failedexpr : "?",
                      file != NULL ? file : "?",
                      line,
                      func != NULL ? " in " : "",
                      func != NULL ? func : "");
    if (n < 0) {
        n = 0;
    } else if ((size_t)n > sizeof(buf) - 1) {
        n = (int)sizeof(buf) - 1;
    }

    // Same "closest thing to a PC available here" rationale as
    // pl_panic_c_hook -- the caller's return address, i.e. roughly where
    // assert() was invoked from.
    uint32_t caller = (uint32_t)(uintptr_t)__builtin_return_address(0);
    pl_panic_arm_record_and_reboot(PL_PANIC_MAGIC_ASSERT, caller, (uint32_t)line, (const uint8_t *)buf, (uintptr_t)n);
}

// --- HardFault entry point (weak isr_hardfault, pico_crt0/crt0.S) -- a
// real Cortex-M exception, not an assert. Deliberately does NOT call into
// pl_panic_arm_record_and_reboot()'s vsnprintf-using sibling above: this
// runs in interrupt/handler context off whatever stack the fault interrupted,
// so the C part below (pl_hardfault_record) sticks to the same
// register-writes-only recording the bead specifies, no library calls. ---

// Naked trampoline: figures out which stack (MSP or PSP) held the
// exception frame from bit 2 of EXC_RETURN in LR, then hands its base to
// pl_hardfault_record as r0. Standard ARMv8-M idiom for reading the
// hardware-stacked frame (r0-r3, r12, lr, pc, xpsr) from a fault handler.
void __attribute__((naked)) isr_hardfault(void) {
    __asm volatile(
        "movs r0, #4        \n"
        "mov r1, lr         \n"
        "tst r0, r1         \n"
        "beq 1f             \n"
        "mrs r0, psp        \n"
        "b 2f               \n"
        "1: mrs r0, msp     \n"
        "2: b pl_hardfault_record \n");
}

// CFSR (Configurable Fault Status Register) -- SCB->CFSR, address fixed by
// the Cortex-M architecture, read directly rather than pulling in a CMSIS
// device header this project doesn't otherwise use.
#define PL_SCB_CFSR (*(volatile uint32_t *)0xE000ED28u)

// `frame` points at the hardware-stacked {r0, r1, r2, r3, r12, lr, pc,
// xpsr}; the stacked pc (frame[6]) is the faulting instruction address.
// No stdio, no printf, no format buffer -- register reads and scratch
// writes only, per the bead's "dumbest possible" requirement for code that
// runs this close to a real fault.
void __attribute__((noreturn)) pl_hardfault_record(uint32_t *frame) {
    // Step 1 -- FIRST statement, before frame is dereferenced: arm the
    // watchdog. Code review finding (2026-08-28): a HardFault caused by
    // stack corruption is exactly the case where `frame` itself may be
    // invalid, and a second fault at HardFault priority is a Cortex-M
    // lockup -- with no watchdog armed yet, silicon does not guarantee
    // recovery from that. This call itself only touches fixed MMIO/scratch
    // registers, never `frame`, so it is safe to run before frame[6] below.
    watchdog_enable(PL_PANIC_WATCHDOG_TIMEOUT_MS, /*pause_on_debug=*/false);

    uint32_t faulting_pc = frame[6];
    uint32_t cfsr = PL_SCB_CFSR;
    pl_panic_arm_record_and_reboot(PL_PANIC_MAGIC_HARDFAULT, faulting_pc, cfsr, NULL, 0);
}

// --- Reporting, next boot (main() calls this early) ---

void pl_panic_report_and_clear(void) {
    // Gated on our own magic alone, NOT watchdog_caused_reboot() -- code
    // review finding (2026-08-28): on RP2350 that also requires
    // rom_get_last_boot_type() == BOOT_TYPE_NORMAL (hardware_watchdog/
    // watchdog.c), which is false on the very first boot of freshly
    // reflashed firmware (that boot's type is BOOT_TYPE_FLASH_UPDATE) --
    // exactly the boot that follows this file's own reset_usb_boot()
    // recovery path. Gating on it meant returning early without clearing
    // scratch[3], leaving the retry flag set to 1 so the *next* genuine,
    // unrelated panic was misdiagnosed as recursive and skipped recording
    // entirely -- the very failure mode this bead exists to fix. Magic
    // alone is still safe: the watchdog scratch registers live in the
    // always-on domain and are NOT retained across an actual power-on
    // reset (only across warm/watchdog/software resets and a BOOTSEL
    // reflash cycle, none of which are power cycles), so a genuine cold
    // boot reads scratch[0] as 0, which matches none of our magics.
    uint32_t magic = watchdog_hw->scratch[0];
    if (magic != PL_PANIC_MAGIC_RUST && magic != PL_PANIC_MAGIC_C && magic != PL_PANIC_MAGIC_HARDFAULT &&
        magic != PL_PANIC_MAGIC_ASSERT && magic != PL_PANIC_MAGIC_WDT) {
        return;
    }

    uint32_t address = watchdog_hw->scratch[1];
    uint32_t diag = watchdog_hw->scratch[2];

    // Bead pico-link-okx (F2b): converted from raw printf() to pl_log() --
    // printf() went straight to stdio, blocking on a host that hadn't
    // attached yet (exactly the case right after boot). pl_log() pushes
    // into the byte ring instead and returns immediately; the superloop's
    // pl_log_ring_drain() (now bounded and non-blocking itself, F2) writes
    // it out whenever a host is actually listening. This is also why the
    // #define printf poison macro below usb_pump.h's include doesn't fire
    // here any more.
    pl_log("\r\n=== pico_link PANIC RECORD (previous boot) ===\r\n");
    switch (magic) {
        case PL_PANIC_MAGIC_RUST:
            pl_log("kind: Rust panic (ui-ffi)\r\n");
            pl_log("message: %s\r\n", s_panic_msg);
            break;
        case PL_PANIC_MAGIC_C:
            pl_log("kind: C panic()\r\n");
            pl_log("caller return address: 0x%08lx\r\n", (unsigned long)address);
            pl_log("message: %s\r\n", s_panic_msg);
            break;
        case PL_PANIC_MAGIC_ASSERT:
            pl_log("kind: assert()\r\n");
            pl_log("caller return address: 0x%08lx\r\n", (unsigned long)address);
            pl_log("line: %lu\r\n", (unsigned long)diag);
            pl_log("message: %s\r\n", s_panic_msg);
            break;
        case PL_PANIC_MAGIC_HARDFAULT:
            pl_log("kind: HardFault\r\n");
            pl_log("faulting PC: 0x%08lx\r\n", (unsigned long)address);
            pl_log("CFSR: 0x%08lx\r\n", (unsigned long)diag);
            break;
        case PL_PANIC_MAGIC_WDT:
            pl_log("kind: watchdog supervisor -- stale subsystem\r\n");
            pl_log("subsystem: %s (id %lu)\r\n", pl_wdt_subsys_name((pl_wdt_subsys_t)address), (unsigned long)address);
            pl_log("observed staleness: %lums\r\n", (unsigned long)diag);
            break;
        default:
            break;
    }
    pl_log("=== end panic record ===\r\n\r\n");

    // Clear the record -- both tiers -- so a genuinely new, unrelated panic
    // later is treated as a fresh one (retry counter back to 0) rather than
    // immediately routed to BOOTSEL as "unresolved".
    watchdog_hw->scratch[0] = 0;
    watchdog_hw->scratch[1] = 0;
    watchdog_hw->scratch[2] = 0;
    watchdog_hw->scratch[3] = 0;
    s_panic_msg[0] = '\0';
}
