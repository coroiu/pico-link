// Pico Link firmware -- debug-only NavIntent injection over the CDC console
// (bead pico-link-cd3).
//
// Why this exists: code review on pico-link-cz0.5.2 established there was
// no software path to trigger scan/connect -- every on-device acceptance
// item needed a human physically pressing the d-pad. This module lets a
// host-side script drive the SAME event path the GPIO button scan feeds
// (input.c -> pl_ui_input), by parsing simple text commands off the
// existing CDC console and turning them into `PlIntent` values.
//
// HARD CONSTRAINTS (see bead pico-link-cd3 and CLAUDE.md):
//  - Reads happen from the MAIN LOOP only, via tud_cdc_read() under
//    usb_pump.h's pl_usb_lock_try() seam (bead pico-link-okx F2b -- see
//    that bead's design comment for why getchar_timeout_us(0) was replaced:
//    it called tud_task() re-entrantly from thread context), called from
//    main.c's superloop -- never from interrupt context. This module calls
//    into Rust (indirectly, via the PlIntent values main.c feeds to
//    pl_ui_input) only through that same thread-context call site; nothing
//    here itself touches Rust or IRQ state.
//  - Compile-gated behind the PL_DEBUG_REMOTE CMake option (OFF by
//    default -- see firmware/CMakeLists.txt). This entire module is
//    excluded from a shipping build.
//  - Reuses the existing NavIntent/PlIntentTag vocabulary (pico_link_ui.h,
//    generated from core/src/input.rs) rather than inventing a parallel
//    input route -- the whole point is exercising the same code the
//    buttons do.
//
// Wire protocol: one command per line (LF or CRLF), ASCII, case-sensitive:
//   NAV UP / NAV DOWN / NAV LEFT / NAV RIGHT
//   NAV SELECT / NAV BACK / NAV X / NAV Y
//   NAV JUMP <signed-int>
//   CONNECT <addr>   -- bead pico-link-g48: <addr> is 6 bytes of hex,
//                        optionally ':'/'-'-separated (e.g. "AABBCCDDEEFF"
//                        or "AA:BB:CC:DD:EE:FF"). Bypasses GAP inquiry
//                        entirely and calls straight into bt.c's
//                        pl_bt_debug_connect -- NOT a NavIntent, dispatched
//                        directly rather than added to `out`. The address
//                        is a per-call, host-supplied value only; it is
//                        never stored as a constant anywhere in this
//                        codebase (see bt.h's doc comment).
//   BOOTSEL          -- bead pico-link-vu4: logs the reason, then calls
//                        reset_usb_boot(0, 0) (noreturn) to drop the board
//                        straight into the USB mass-storage bootloader, so
//                        a flash-verify loop no longer needs a human to
//                        hold BOOTSEL. This routes around pico-link-d74
//                        (the vendor CONTROL transfer on interface 4 that
//                        STALLs on a healthy board) because it rides the
//                        CDC BULK data path instead -- a different endpoint,
//                        a different code path.
//
//                        THIS IS NOT A REPLACEMENT FOR PHYSICAL BOOTSEL AND
//                        MUST NOT BE TREATED AS ONE. It only works because
//                        pl_debug_remote_poll() is reached from the running
//                        main loop -- a board that is wedged, panicking, or
//                        stuck before this poll call is reached is
//                        completely unreachable through this command, and
//                        still needs a human at the desk holding the
//                        physical BOOTSEL button. Do not trust this during
//                        a hang hunt: if BOOTSEL-over-CDC doesn't work,
//                        that is not new information about the hang, it's
//                        the expected outcome of a wedged board.
//   VOL GET          -- bead pico-link-4v2.1 (VT1, volume-sync risk gate,
//                        .planning/design/2026-09-02-volume-sync.md sec 9):
//                        dumps usb_audio.c's stored fu_volume[]/fu_mute[]
//                        per channel plus fu_set_calls/fu_get_calls.
//   VOL WATCH        -- toggles logging of every feature-unit SET (channel,
//                        selector, raw value) as it happens, from inside
//                        the 0xC0 worker IRQ (pl_log() is IRQ-safe). Second
//                        call turns it back off.
//   VOL HOSTUP [n]   -- pushes n (default 1, max 100) HID Consumer Volume
//   VOL HOSTDOWN [n]    Increment/Decrement taps via media_keys.c's
//                        already-proven ring, to measure whether it moves
//                        macOS's own output-volume slider (design sec 6,
//                        mechanism M2) and whether macOS then writes our FU
//                        back in response.
// An unrecognized or malformed line is logged and ignored -- never fatal,
// never wedges the poll loop. See tools/usb-console/cdc_sender.py for the
// host-side counterpart.
#ifndef PICO_LINK_DEBUG_REMOTE_H
#define PICO_LINK_DEBUG_REMOTE_H

#include <stddef.h>

#include "pico_link_ui.h"

// Drains whatever bytes are currently available on the CDC console's RX
// side (non-blocking -- tud_cdc_read() under pl_usb_lock_try(), bounded per
// call; skips the whole call if the lock is unavailable, see usb_pump.h),
// accumulates them into a line buffer, and parses any complete lines seen
// this call into `out`. Writes at most `max` intents and returns how many
// were written. Cheap and safe to call every superloop iteration alongside
// pl_link_input_poll -- most calls read zero bytes and emit nothing.
//
// Call from main.c's superloop only.
size_t pl_debug_remote_poll(PlIntent *out, size_t max);

#endif // PICO_LINK_DEBUG_REMOTE_H
