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
//   SKIPTICKS <K>    -- bead pico-link-fhf, test A (injection): one-shot,
//                        the NEXT <K> calls to a2dp.c's media timer
//                        handler skip the drain (pl_a2dp_fill()) entirely,
//                        so the PCM ring gains fill at the full 192 B/ms
//                        rate for that duration -- proving the
//                        hysteresis-banded resync trim actually fires
//                        rather than passing a soak test by doing nothing.
//                        Dispatched directly to a2dp.c's
//                        pl_a2dp_debug_skip_media_ticks(), not a NavIntent.
//   VOL GET          -- bead pico-link-4v2.1 (VT1, volume-sync risk gate,
//                        .planning/design/2026-09-02-volume-sync.md sec 9):
//                        dumps usb_audio.c's stored fu_volume[]/fu_mute[]
//                        per channel plus fu_set_calls/fu_get_calls.
//   VOL WATCH        -- toggles logging of every feature-unit SET (channel,
//                        selector, raw value). Polled from this file's
//                        thread-context superloop call, which watches
//                        usb_audio.c's fu_set_calls() counter and publishes
//                        the fu_volume[]/fu_mute[] snapshot via pl_prio.h's
//                        slot 4 -- logging directly from the 0xC0 worker
//                        IRQ via plain pl_log() was tried first and found
//                        unreliable under this firmware's background
//                        log-ring congestion (see VT1's LEARNED comment on
//                        bead pico-link-4v2.1). Second call turns it back
//                        off.
//   VOL SET n        -- bead pico-link-4v2.2 (VT2, .planning/design/
//                        2026-09-02-volume-sync.md sec 9): sets volume.c's
//                        canonical value (0..127) through the SAME loop
//                        rule and circuit breaker the real host/sink edges
//                        will use once T3/T4 wire them, and logs what it
//                        would emit to each peer via pl_prio.h's slot 4.
//                        Emits nothing for real -- nothing reads the
//                        outbound latches yet.
//   EQ BEGIN         -- bead pico-link-ryw.11: starts a fresh Equalizer
//                        APO import session (discards any prior
//                        never-`EQ END`ed one). Dispatched to Rust over
//                        pl_ui_debug_eq_command(ui, "BEGIN", ...).
//   EQ <line>        -- feeds one Equalizer APO text line (a "Preamp: <n>
//                        dB" or "Filter N: ON|OFF LS|PK|HS Fc <n> Hz Gain
//                        <n> dB (BW Oct <n>|Q <n>)" line, pasted verbatim)
//                        into the in-progress session. Requires EQ BEGIN
//                        first. See tools/usb-console/eq_import.py for the
//                        host-side sender.
//   EQ END           -- finishes the session and, on success, installs it
//                        as a non-persisted debug DSP override that
//                        core::app::App::dsp_program returns ahead of
//                        everything else (the connected device's assigned
//                        preset, or an open effects editor's preview).
//                        Logs the resulting band count/preamp via
//                        pl_ui_debug_eq_status() on success.
//   EQ OFF           -- clears the debug override (and any in-progress
//                        session), reverting to normal resolution. Also
//                        the only way to clear it besides a reboot.
//   EQ STATUS        -- logs whether an override is currently active and,
//                        if so, its band count/explicit preamp -- does not
//                        change anything.
//   DSPPROG <n>      -- bead pico-link-ryw.1, design .planning/design/
//                        2026-09-25-dsp-effects-stage.md sec 1.4: loads
//                        one of 4 canned DSP programs (0=Off,
//                        1=crossfeed only, 2=10 peaking bands + crossfeed
//                        (worst case), 3=+9dB low shelf clip test) into
//                        dsp.c's core1 realtime kernel via
//                        pl_dsp_debug_load_program(). Dispatched directly,
//                        not a NavIntent -- same pattern as ABR FLOOR/LDAC
//                        RUNG above. Exists to measure the DSP stage
//                        (ryw.2's M0-M3 hardware gate) before any Rust
//                        preset UI does.
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
// `ui` (bead pico-link-ryw.11) is needed only for the "EQ ..." family of
// commands, which dispatch straight to pl_ui_debug_eq_command()/
// pl_ui_debug_eq_status() -- every other command here still bypasses `ui`
// the same way it always has (direct dispatch to the owning C module, or
// PlIntent values appended to `out` for the caller's own pl_ui_input()
// call).
//
// Call from main.c's superloop only.
size_t pl_debug_remote_poll(struct PlUi *ui, PlIntent *out, size_t max);

#endif // PICO_LINK_DEBUG_REMOTE_H
