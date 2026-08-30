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
//  - Reads happen from the MAIN LOOP only, via getchar_timeout_us(0) called
//    from main.c's superloop -- never from interrupt context. This module
//    calls into Rust (indirectly, via the PlIntent values main.c feeds to
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
// An unrecognized or malformed line is logged and ignored -- never fatal,
// never wedges the poll loop. See tools/usb-console/cdc_sender.py for the
// host-side counterpart.
#ifndef PICO_LINK_DEBUG_REMOTE_H
#define PICO_LINK_DEBUG_REMOTE_H

#include <stddef.h>

#include "pico_link_ui.h"

// Drains whatever bytes are currently available on the CDC console's RX
// side (non-blocking -- getchar_timeout_us(0) per byte, bounded per call),
// accumulates them into a line buffer, and parses any complete lines seen
// this call into `out`. Writes at most `max` intents and returns how many
// were written. Cheap and safe to call every superloop iteration alongside
// pl_link_input_poll -- most calls read zero bytes and emit nothing.
//
// Call from main.c's superloop only.
size_t pl_debug_remote_poll(PlIntent *out, size_t max);

#endif // PICO_LINK_DEBUG_REMOTE_H
