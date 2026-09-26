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
// `pl_ui_import_preset` (bead pico-link-ryw.12.4): on success it queues a
// `Command::SavePreset` IMMEDIATELY (Andreas's ruling: no on-device
// confirm), so `PL_CFG_STATE_SAVED` below is only ever reported after that
// queueing has already happened -- never before, never speculatively.
// (An earlier revision of this bead routed through
// `pl_ui_debug_eq_command`'s live-only audition override, which is never
// persisted; that was a review finding, fixed by switching to the real
// `pl_ui_import_preset` call below.)
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
// one place pl_ui_import_preset() is called.
//
// WIRE PROTOCOL (bmRequestType type==CLASS, recipient==INTERFACE, wIndex ==
// ITF_NUM_CONFIG -- NOT type==VENDOR: pico-sdk 2.1.1's TinyUSB usbd.c routes
// every VENDOR-type request to tud_vendor_control_xfer_cb and never to a
// class driver, so a real vendor request would always stall here; CLASS is
// the same convention usb_reset.c's resetd already uses). Exactly two
// requests, matching tools/usb-console/pl_eq_import.py:
//   0x01 IMPORT_PRESET, OUT, wLength 2..1024:
//     byte 0: proto version, must be 1
//     byte 1: name_len, <= PL_CONFIG_NAME_MAX (16)
//     bytes [2, 2+name_len): name, UTF-8, passed to `pl_ui_import_preset`
//       as the fallback name (used only if the document itself has no
//       `Name:` line -- see that function's own doc comment)
//     bytes [2+name_len, wLength): Equalizer APO / AutoEQ text, passed to
//       `pl_ui_import_preset` whole (not split into a BEGIN/<line>/END
//       debug session -- that was an interim seam, replaced).
//     SETUP stalls (returns false) if an import is already pending, or
//     wLength is 0 or exceeds the 1KB buffer -- see PL_CONFIG_IMPORT_BUF_LEN.
//   0x02 GET_STATUS, IN, exactly sizeof(pl_cfg_status_wire_t) bytes:
//     {u8 state, u8 error, u8 outcome, u8 reserved, u16 preset_id,
//      u16 line, u16 band_index, f32 value} -- see the enums and struct
//     below for which fields are valid in which state.
//
// Bead pico-link-jyhk.4 adds two more requests on the SAME interface, SAME
// bRequest namespace (ADA DESIGN comment on pico-link-jyhk.1, sections 3-5
// -- the durable design; see also .planning/design/2026-09-26-web-home-
// telemetry.md if a scribe has since filed it). Both are CLASS, recipient
// INTERFACE, wIndex == ITF_NUM_CONFIG, same as the two above -- not VENDOR,
// for the same reason documented above (pico-sdk 2.1.1's TinyUSB usbd.c
// routes VENDOR-type requests to tud_vendor_control_xfer_cb, never here).
//   0x03 GET_TELEMETRY, IN, wValue = page id (only page 0, "Home", exists
//     today): replies with the most recently generated telemetry snapshot
//     for that page, or a shorter/all-zero reply if none has been
//     generated yet (host reads the payload's own `len` field and treats
//     `snap_seq == 0` as "not ready" -- see core/src/app/telemetry.rs for
//     the wire layout, which this file copies opaquely and never parses).
//     Stalls if `wValue` names an unsupported page. Generation is gated in
//     pl_config_itf_poll_telemetry (thread context, main.c's superloop):
//     it runs only while a GET_TELEMETRY SETUP has arrived within the last
//     second, and at most once every 20ms -- see that function's own
//     comment. A page that never attaches costs nothing.
//   0x04 GET_INFO, IN, exactly sizeof(pl_cfg_info_wire_t) bytes: a static
//     capability/version handshake -- no Rust call, no dynamic state. The
//     host calls this once, first, to learn which proto versions and
//     telemetry pages this firmware build supports.
#ifndef PICO_LINK_USB_CONFIG_ITF_H
#define PICO_LINK_USB_CONFIG_ITF_H

#include <stdint.h>

#include "device/usbd_pvt.h"

struct PlUi;

// bRequest values, interface recipient, wIndex == ITF_NUM_CONFIG.
#define PL_CFG_REQ_IMPORT_PRESET  0x01
#define PL_CFG_REQ_GET_STATUS     0x02
#define PL_CFG_REQ_GET_TELEMETRY  0x03
#define PL_CFG_REQ_GET_INFO       0x04

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

// GET_STATUS's `outcome` byte, valid only when state == SAVED. Mirrors
// `pico_link_core::dsp::ImportOutcome` via `PlImportResult::outcome`
// (ui-ffi/src/lib.rs).
enum {
    PL_CFG_OUTCOME_CREATED = 0,
    PL_CFG_OUTCOME_REPLACED = 1,
    PL_CFG_OUTCOME_RENAMED = 2,
};

// GET_STATUS's `error` byte when state == REJECTED: the magnitude of
// `PlImportResult::code` (ui-ffi/src/lib.rs's `import_error_result` --
// 1..14, reusing `eq_apo_error_code`'s 2..8 for parse errors), or one of
// these transport-level codes for failures that never reach the parser.
#define PL_CFG_ERR_MALFORMED_HEADER 254

// The GET_STATUS reply, wire-exact (no padding -- packed). Wider than the
// original 4-byte reply (pico-link-ryw.12.5's review finding: the transport
// needs to carry `pl_ui_import_preset`'s real outcome/preset_id/error
// detail, not just state+error+line) -- see `pl_config_itf_poll`'s own
// comment for how the wider struct stays consistent across the
// IRQ-context reader / thread-context writer boundary despite no longer
// fitting in one atomic word.
typedef struct __attribute__((packed)) {
    uint8_t state;
    // Valid only when state == REJECTED: PL_CFG_ERR_* or an
    // import-error magnitude (see above).
    uint8_t error;
    // Valid only when state == SAVED: PL_CFG_OUTCOME_*.
    uint8_t outcome;
    uint8_t reserved;
    // Valid only when state == SAVED: the imported/updated preset's id.
    uint16_t preset_id;
    // Valid only when state == REJECTED and the failure is a parse
    // error: the 1-based source line, else 0.
    uint16_t line;
    // Valid only when state == REJECTED and the failure is a
    // range-check error (gain/freq/Q/preamp out of range): the 1-based
    // `Filter N:` band index, else 0.
    uint16_t band_index;
    // Valid only when state == REJECTED and the failure is a
    // range-check error: the offending value (dB, Hz or Q, matching
    // `error`), else 0.0.
    float value;
} pl_cfg_status_wire_t;

// --- GET_TELEMETRY (0x03) / GET_INFO (0x04) -- bead pico-link-jyhk.4 ---

// Only page implemented today; matches
// `pico_link_core::app::telemetry::TELEMETRY_PAGE_HOME`.
#define PL_TELEMETRY_PAGE_HOME 0

// Working/publish buffer size for a telemetry snapshot. Sized with
// headroom over `HOME_SNAPSHOT_LEN` (163 bytes as of proto 1, owned by
// core/src/app/telemetry.rs -- not duplicated here) for a future page's
// growth within the same append-only proto; `pl_ui_telemetry` itself
// enforces the real per-page length and returns 0 rather than overflow
// this buffer.
#define PL_CONFIG_TELEMETRY_BUF_LEN 256

// GET_INFO's static version numbers -- bump the relevant one whenever the
// corresponding wire layout changes (ADA DESIGN section 5: "each request
// is versioned on its own").
#define PL_CFG_INFO_VERSION            1
#define PL_CFG_IMPORT_PROTO_VERSION    1
#define PL_CFG_STATUS_VERSION          1
#define PL_CFG_TELEMETRY_PROTO_VERSION 1

// Bitmask of supported telemetry pages, bit N == page N. Only Home (page
// 0) exists today.
#define PL_CFG_TELEMETRY_PAGE_MASK (1u << PL_TELEMETRY_PAGE_HOME)

// Firmware version string reported by GET_INFO. Overridable at compile
// time (e.g. from CMake via a git describe); no such wiring exists yet, so
// this is a placeholder until one is added on its own bead.
#ifndef PL_FW_VERSION_STRING
#define PL_FW_VERSION_STRING "dev"
#endif

// The GET_INFO reply, wire-exact (packed, fixed size regardless of the
// real version string length -- `version_len` tells the host how many of
// `version`'s bytes are meaningful; the rest are zero-padded).
typedef struct __attribute__((packed)) {
    uint8_t info_ver;
    uint8_t import_proto;
    uint8_t status_ver;
    uint8_t telemetry_proto;
    uint32_t telemetry_page_mask;
    uint8_t version_len;
    uint8_t version[32];
} pl_cfg_info_wire_t;

// Returns the TinyUSB class driver for ITF_NUM_CONFIG, for
// usb_reset.c's usbd_app_driver_get_cb to return alongside resetd.
usbd_class_driver_t const *pl_configd_driver(void);

// Thread-context only (main.c's superloop, unconditional -- NOT gated on
// PL_DEBUG_REMOTE, unlike pl_debug_remote_poll: this transport must exist
// in release builds). Drains at most one pending IMPORT_PRESET into `ui`
// and updates the GET_STATUS state. A no-op when nothing is pending.
void pl_config_itf_poll(struct PlUi *ui);

// Thread-context only, called from main.c's superloop right after
// pl_dsp_service() (ADA DESIGN section 3: "the snapshot carries this
// iteration's levels and bitrate"). Regenerates the published telemetry
// snapshot from `ui` (a read-only borrow -- never touches dirty, damage or
// idle state) at most once every 20ms, and only while a GET_TELEMETRY
// SETUP has arrived within the last second; otherwise a no-op, so an
// unattached page costs nothing. The one place `pl_ui_telemetry` is
// called from C.
void pl_config_itf_poll_telemetry(struct PlUi *ui);

#endif // PICO_LINK_USB_CONFIG_ITF_H
