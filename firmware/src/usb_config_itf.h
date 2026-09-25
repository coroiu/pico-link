// Pico Link firmware -- bead pico-link-ryw.12.5: "Pico Link Config", a
// zero-endpoint TinyUSB vendor class driver on ITF_NUM_CONFIG (see
// usb_descriptors.h) that lets a host tool push an Equalizer APO preset
// into the running device over plain USB control transfers, present in
// BOTH debug and release builds. Design: pico-link-ryw.12's DESIGN
// comment, section "1. Transport".
//
// Why this exists instead of reusing PL_DEBUG_REMOTE's CDC console
// (pico-link-cd3/debug_remote.c): that whole module is compiled OUT of a
// release build (its own module doc explains why -- a debug-only NavIntent
// injection channel has no business existing in what ships), and CDC's
// other release-mode path is the tty, which memory note "CDC tty can
// kernel-panic this Mac" rules out for anything routine. A dedicated
// control-only vendor interface needs no tty, no CDC line discipline, and
// costs nothing when idle (no endpoints, no polling).
//
// SAFETY / SCOPE (the property this file exists to hold, not just
// document): there is no write-arbitrary-flash, erase, reboot, or
// memory-read request on this interface. The two requests below can only
// (a) copy up to 1KB into a static buffer, gated so at most one import is
// in flight, and (b) report the last result. The one flash side effect --
// saving a preset -- happens inside pl_config_itf_poll()'s Rust call,
// which today (this bead) routes through the SAME debug-override session
// API pico-link-ryw.11 already proved end to end
// (pl_ui_debug_eq_command); ryw.12.2/12.4 replace that call with a real
// named-preset save behind this exact function, which is the one seam
// meant to change.
//
// IRQ DISCIPLINE (pico-link-6o2 / pico-link-tfj): tud_task() runs in the
// 0xC0 user-IRQ worker in this project, so BOTH configd_control_xfer_cb
// (below, via the TinyUSB class driver table) and TinyUSB's own
// usbd_control_xfer_cb that invokes it run in IRQ context. Neither may
// call into Rust. configd_control_xfer_cb only copies bytes into
// s_import_buf (via tud_control_xfer, which does the copying itself) and
// sets a flag; pl_config_itf_poll() -- called from main.c's superloop,
// thread context only, same call site as pl_link_input_poll /
// pl_debug_remote_poll -- is the one place that flag is consumed and the
// one place pl_ui_debug_eq_command() is called.
//
// WIRE PROTOCOL (vendor class, interface recipient, wIndex ==
// ITF_NUM_CONFIG). Exactly two requests, matching tools/usb-console/
// pl_eq_import.py:
//   0x01 IMPORT_PRESET, OUT, wLength 2..1024:
//     byte 0: proto version, must be 1
//     byte 1: name_len, <= PL_CONFIG_NAME_MAX (16)
//     bytes [2, 2+name_len): name, UTF-8 (not yet consumed by this bead's
//       debug-override target -- validated and available for ryw.12.4)
//     bytes [2+name_len, wLength): Equalizer APO text, one filter/preamp
//       line per '\n' (or "\r\n")-terminated line, fed line by line to the
//       SAME parser BEGIN/<line>/END drives over the CDC console.
//     SETUP stalls (returns false) if an import is already pending, or
//     wLength is 0 or exceeds the 1KB buffer -- see PL_CONFIG_IMPORT_BUF_LEN.
//   0x02 GET_STATUS, IN, exactly sizeof(pl_cfg_status_wire_t) (4) bytes:
//     {u8 state, u8 error, u16 line} -- see the enum and struct below.
#ifndef PICO_LINK_USB_CONFIG_ITF_H
#define PICO_LINK_USB_CONFIG_ITF_H

#include <stdint.h>

#include "device/usbd_pvt.h"

struct PlUi;

// bRequest values, interface recipient, wIndex == ITF_NUM_CONFIG.
#define PL_CFG_REQ_IMPORT_PRESET 0x01
#define PL_CFG_REQ_GET_STATUS    0x02

// Matches the DESIGN comment's name_len bound ("name bytes up to 16").
#define PL_CONFIG_NAME_MAX 16

// GET_STATUS's `state` byte. No PL_CFG_STATE_AWAITING_USER: Andreas's
// 2026-09-25 ruling on pico-link-ryw.12 is "no on-device confirmation --
// imports save immediately", so this bead's state machine only ever
// produces IDLE, BUSY (transiently, while pl_config_itf_poll() is
// running), SAVED or REJECTED. DISCARDED is reserved for a future
// confirmation flow the ruling explicitly ruled out; kept in the enum so
// the wire's state byte has a stable meaning if one is ever added, not
// because this firmware emits it.
enum {
    PL_CFG_STATE_IDLE = 0,
    PL_CFG_STATE_BUSY = 1,
    PL_CFG_STATE_SAVED = 2,
    PL_CFG_STATE_DISCARDED = 3,
    PL_CFG_STATE_REJECTED = 4,
};

// GET_STATUS's `error` byte when state == REJECTED: the magnitude of
// pico_link_core::dsp::EqApoError / DebugEqCommandError's negative `code`
// (see ui-ffi/src/lib.rs's eq_apo_error_code / debug_eq_command_error_result
// -- 2..9), or one of these transport-level codes for failures that never
// reach the parser.
#define PL_CFG_ERR_MALFORMED_HEADER 254

// The 4-byte GET_STATUS reply, wire-exact (no padding -- packed).
typedef struct __attribute__((packed)) {
    uint8_t state;
    uint8_t error;
    uint16_t line;
} pl_cfg_status_wire_t;

// Returns the TinyUSB class driver for ITF_NUM_CONFIG, for
// usb_reset.c's usbd_app_driver_get_cb to return alongside resetd.
usbd_class_driver_t const *pl_configd_driver(void);

// Thread-context only (main.c's superloop, unconditional -- NOT gated on
// PL_DEBUG_REMOTE, unlike pl_debug_remote_poll: this transport must exist
// in release builds). Drains at most one pending IMPORT_PRESET into `ui`
// and updates the GET_STATUS state. A no-op when nothing is pending.
void pl_config_itf_poll(struct PlUi *ui);

#endif // PICO_LINK_USB_CONFIG_ITF_H
