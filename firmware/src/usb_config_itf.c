// Pico Link firmware -- bead pico-link-ryw.12.5: "Pico Link Config" vendor
// class driver -- see usb_config_itf.h for the module doc, wire protocol
// and IRQ-discipline contract this file implements.
//
// PROVENANCE: the class-driver skeleton (itf_num tracking,
// usbd_class_driver_t table, open/reset/xfer_cb shape) follows
// usb_reset.c's own pattern in this tree, which is itself ported from
// pico-sdk's reset_interface.c (BSD-3-Clause) -- see usb_reset.c's own
// provenance note. The control-transfer state machine
// (SETUP/DATA/ACK-stage handling via tud_control_xfer) follows the shape
// common to every TinyUSB class driver's control_xfer_cb (e.g.
// audiod_control_xfer_cb, hidd_control_xfer_cb in
// $PICO_SDK_PATH/lib/tinyusb/src/class -- MIT), not copied from any one
// of them.

#include "usb_config_itf.h"

#include <stdbool.h>
#include <stddef.h>
#include <string.h>

#include "tusb.h"

#include "hardware/sync.h"
#include "pico/time.h"

#include "pico_link_ui.h"
#include "usb_descriptors.h"
#include "usb_pump.h"

static uint8_t itf_num;

// The shared OUT mailbox, its length, and its pending flag -- written only
// by configd_control_xfer_cb (IRQ context) while `s_mailbox_pending` is
// false, read only by pl_config_itf_poll/pl_config_itf_poll_host_op
// (thread context) while `s_mailbox_pending` is true and `s_mailbox_kind`
// names the request that filled it. `s_mailbox_pending` is the single flag
// that makes those two windows disjoint -- SETUP for a new IMPORT_PRESET
// or HOST_OP stalls (see below) whenever it is already true, so the
// buffer is never written to while the thread-context side is reading it.
// Design comment on pico-link-ryw.12, section 1 ("wLength <= 1024") and
// pico-link-jyhk.21's design section 5 ("IMPORT_PRESET and HOST_OP share
// the one 1KB mailbox and its pending flag").
static uint8_t s_mailbox_buf[PL_CONFIG_MAILBOX_LEN];
static volatile uint16_t s_mailbox_len;
static volatile bool s_mailbox_pending;
// Valid only while `s_mailbox_pending` is true: which request filled
// `s_mailbox_buf` -- PL_CFG_REQ_IMPORT_PRESET or PL_CFG_REQ_HOST_OP.
static volatile uint8_t s_mailbox_kind;

// GET_STATUS's reply. No longer fits in one atomic word (widened past 4
// bytes to carry pl_ui_import_preset's real outcome/preset_id/error
// detail -- see usb_config_itf.h's struct comment), so the IRQ-context
// reader (GET_STATUS's SETUP handler) / thread-context writer
// (pl_config_itf_poll) boundary instead uses a short IRQ-disable critical
// section on the WRITER side only: configd_control_xfer_cb already runs
// with interrupts effectively excluded for the duration of its own
// handler (it IS the IRQ), so it can never observe a torn struct as long
// as the writer never leaves a partially-written s_status visible to an
// interrupt. save_and_disable_interrupts/restore_interrupts around the
// write is the same pattern bt.c/media_keys.c already use for this exact
// shape of problem.
static pl_cfg_status_wire_t s_status;

static void set_status(pl_cfg_status_wire_t wire) {
    uint32_t irq_state = save_and_disable_interrupts();
    s_status = wire;
    restore_interrupts(irq_state);
}

// Maps a PlImportResult's `code` (0 on success, else one of the small
// negative codes documented in ui-ffi/src/lib.rs's import_error_result)
// to GET_STATUS's single unsigned error byte.
static uint8_t import_error_byte(int32_t code) {
    if (code >= 0) {
        return 0;
    }
    // These codes are always small in magnitude (see import_error_result's
    // match arms: -1..-14) -- never anywhere near 256, so the narrowing
    // below never wraps.
    int32_t mag = -code;
    if (mag > 253) {
        mag = 253; // stay below the reserved PL_CFG_ERR_* range
    }
    return (uint8_t)mag;
}

// Truncates a PlImportResult u32 field (`line`/`band_index`, always tiny
// -- see PlImportResult's own doc comment) to the wire's u16.
static uint16_t import_u16(uint32_t value) {
    return (value > 0xFFFFu) ? 0xFFFFu : (uint16_t)value;
}

void pl_config_itf_poll(struct PlUi *ui) {
    if (!s_mailbox_pending || s_mailbox_kind != PL_CFG_REQ_IMPORT_PRESET) {
        return;
    }

    uint16_t len = s_mailbox_len;
    // BUSY is already set by configd_control_xfer_cb's ACK stage (see its
    // comment) -- no need to set it again here.

    if (len < 2) {
        pl_log("usb-config: IMPORT_PRESET rejected -- header too short (len=%u)\r\n", (unsigned)len);
        set_status((pl_cfg_status_wire_t){ .state = PL_CFG_STATE_REJECTED, .error = PL_CFG_ERR_MALFORMED_HEADER });
        s_mailbox_pending = false;
        return;
    }

    uint8_t proto = s_mailbox_buf[0];
    uint8_t name_len = s_mailbox_buf[1];
    size_t header_len = (size_t)2 + (size_t)name_len;
    if (proto != 1 || name_len > PL_CONFIG_NAME_MAX || header_len > (size_t)len) {
        pl_log(
            "usb-config: IMPORT_PRESET rejected -- bad header (proto=%u name_len=%u len=%u)\r\n", (unsigned)proto,
            (unsigned)name_len, (unsigned)len
        );
        set_status((pl_cfg_status_wire_t){ .state = PL_CFG_STATE_REJECTED, .error = PL_CFG_ERR_MALFORMED_HEADER });
        s_mailbox_pending = false;
        return;
    }

    // Real save path (pico-link-ryw.12.4): one call, whole document. On
    // success this has ALREADY queued a Command::SavePreset before
    // returning (see pl_ui_import_preset's own doc comment) -- so
    // PL_CFG_STATE_SAVED below is only ever reported once that queueing
    // has happened, never before.
    const uint8_t *name = s_mailbox_buf + 2;
    const uint8_t *text = s_mailbox_buf + header_len;
    size_t text_len = (size_t)len - header_len;

    PlImportResult result = pl_ui_import_preset(ui, text, text_len, name, (size_t)name_len);
    if (result.code != 0) {
        pl_log(
            "usb-config: IMPORT_PRESET rejected code=%ld line=%lu band=%lu\r\n", (long)result.code,
            (unsigned long)result.line, (unsigned long)result.band_index
        );
        set_status((pl_cfg_status_wire_t){
            .state = PL_CFG_STATE_REJECTED,
            .error = import_error_byte(result.code),
            .line = import_u16(result.line),
            .band_index = import_u16(result.band_index),
            .value = result.value,
        });
        s_mailbox_pending = false;
        return;
    }

    pl_log(
        "usb-config: IMPORT_PRESET queued for saving (name_len=%u, %u text bytes, preset_id=%u, outcome=%u)\r\n",
        (unsigned)name_len, (unsigned)text_len, (unsigned)result.preset_id, (unsigned)result.outcome
    );
    set_status((pl_cfg_status_wire_t){
        .state = PL_CFG_STATE_SAVED,
        .outcome = result.outcome,
        .preset_id = result.preset_id,
    });
    s_mailbox_pending = false;
}

// --- bead pico-link-jyhk.21: HOST_OP (0x06) / GET_OP_STATUS (0x07) ---
// design section 4/11 Task 4.
//
// s_op_status_buf/s_op_status_len are published by pl_config_itf_poll_host_op
// (thread context) under save_and_disable_interrupts and read directly by
// configd_control_xfer_cb's GET_OP_STATUS SETUP handler (IRQ context) --
// the same publish pattern s_telemetry_buf/s_library_buf already use.
// Zero-initialized by static storage duration, which matches
// `HostOpStatus::default()`'s all-zero `state == 0` ("none") -- a
// GET_OP_STATUS before any HOST_OP has ever run correctly reports "no
// result yet", not a stale/garbage one.
static uint8_t s_op_status_buf[PL_CFG_OP_STATUS_MAX_LEN];
static volatile uint16_t s_op_status_len;

static void publish_op_status(const uint8_t *buf, size_t len) {
    size_t copy_len = (len > PL_CFG_OP_STATUS_MAX_LEN) ? PL_CFG_OP_STATUS_MAX_LEN : len;
    uint32_t irq_state = save_and_disable_interrupts();
    memcpy(s_op_status_buf, buf, copy_len);
    s_op_status_len = (uint16_t)copy_len;
    restore_interrupts(irq_state);
}

void pl_config_itf_poll_host_op(struct PlUi *ui) {
    if (!s_mailbox_pending || s_mailbox_kind != PL_CFG_REQ_HOST_OP) {
        return;
    }

    uint16_t len = s_mailbox_len;
    uint8_t tmp[PL_CFG_OP_STATUS_MAX_LEN];
    // One call runs the op AND encodes the GET_OP_STATUS reply (see
    // pl_ui_host_op's own doc comment) -- `pl_ui_host_op` never panics on a
    // malformed/truncated request (App::host_op's own contract), so `len`
    // is passed through unvalidated here, same discipline
    // pl_ui_import_preset's caller uses.
    size_t written = pl_ui_host_op(ui, s_mailbox_buf, (size_t)len, tmp, sizeof(tmp));
    if (written == 0) {
        // Structurally impossible here (sizeof(tmp) == PL_CFG_OP_STATUS_MAX_LEN,
        // the worst case pl_ui_host_op can ever encode) -- see its own doc
        // comment: "0 ... C must not publish anything new". Leave whatever
        // was previously published in place, same as a too-small
        // pl_ui_telemetry/pl_ui_library call.
        pl_log("usb-config: HOST_OP produced no status (unexpected -- out_cap should always suffice)\r\n");
        s_mailbox_pending = false;
        return;
    }

    publish_op_status(tmp, written);
    s_mailbox_pending = false;
}

// --- bead pico-link-jyhk.4: GET_TELEMETRY (0x03) / GET_INFO (0x04) ---
// ADA DESIGN comment on pico-link-jyhk.1, sections 3-5.
//
// s_telemetry_buf/s_telemetry_len are published by
// pl_config_itf_poll_telemetry (thread context, main.c's superloop) under
// save_and_disable_interrupts and read directly by
// configd_control_xfer_cb's GET_TELEMETRY SETUP handler (IRQ context) --
// the same "writer excludes the IRQ, IRQ needs no lock" pattern as
// s_status/set_status above. configd_init (below) seeds both with a
// synthesized proto-1, snap_seq-0 "not ready" header at the full current
// wire length -- NOT the zero-length reply plain static zero-init would
// give (bead pico-link-s6hh: a 0-byte reply looked like a short/malformed
// transfer to the web companion for the ~10s before the first real
// snapshot, rather than the documented not-ready state; core's snap_seq
// field reads 0 until the first successful encode -- see
// App::telemetry_snapshot's doc comment). After that seed, the reply is
// simply however-many-bytes have ever been published, with no separate
// "reset to not-ready after further idle" step -- a deliberate
// simplification of design section 3 flow (c): once a page has attached
// at least once, serving its last real (if stale) snapshot rather than
// re-synthesizing not-ready matches the design's own "self-heals, not a
// delta log" philosophy (section 2) more closely than inventing a second
// not-ready representation would.
#define PL_TELEMETRY_POLL_RECENCY_US (1000ull * 1000ull) // 1s, design sec 3 flow (b)
#define PL_TELEMETRY_GEN_INTERVAL_US (20ull * 1000ull)   // 20ms, design sec 3 flow (b)

static uint8_t s_telemetry_buf[PL_CONFIG_TELEMETRY_BUF_LEN];
static volatile uint16_t s_telemetry_len;
// Stamped by the IRQ-context GET_TELEMETRY SETUP handler on every poll;
// read by pl_config_itf_poll_telemetry (thread context) to gate
// generation on "polled within the last second". A torn 64-bit read on
// this 32-bit core can only misjudge staleness by at most one loop
// iteration -- never a correctness hazard, same reasoning pl_prio.h/
// pl_loop_prof.h already rely on for their own newest-snapshot counters.
static volatile uint64_t s_telemetry_last_poll_us;
// Which page the last SETUP asked for (only PL_TELEMETRY_PAGE_HOME exists
// today, but this keeps a future page-1 poll driving generation of the
// page actually requested, not always page 0).
static volatile uint8_t s_telemetry_requested_page;
// Thread-context only (both read and written exclusively from
// pl_config_itf_poll_telemetry, which main.c calls from a single
// superloop on core 0) -- no lock needed, same single-writer/single-reader
// reasoning as pl_loop_prof.h's own module doc.
static uint64_t s_telemetry_last_gen_us;

// Copies `len` bytes (capped to the buffer) into s_telemetry_buf under a
// short IRQ-disable critical section, then publishes the new length --
// same set_status pattern as above (usb_config_itf.c:59-63 in the module's
// original numbering), so configd_control_xfer_cb's SETUP handler (which
// runs as the excluded IRQ) can never observe a torn buffer/length pair.
static void publish_telemetry(const uint8_t *buf, size_t len) {
    size_t copy_len = (len > PL_CONFIG_TELEMETRY_BUF_LEN) ? PL_CONFIG_TELEMETRY_BUF_LEN : len;
    uint32_t irq_state = save_and_disable_interrupts();
    memcpy(s_telemetry_buf, buf, copy_len);
    s_telemetry_len = (uint16_t)copy_len;
    restore_interrupts(irq_state);
}

void pl_config_itf_poll_telemetry(struct PlUi *ui) {
    uint64_t now_us = time_us_64();

    // Design sec 3 flow (b): "run only if the last poll was under 1s ago."
    // No page attached (or it stopped polling) -- zero cost, no Rust call.
    uint64_t last_poll_us = s_telemetry_last_poll_us;
    if (last_poll_us == 0 || (now_us - last_poll_us) > PL_TELEMETRY_POLL_RECENCY_US) {
        return;
    }

    // "...and at least 20ms have passed since the last generation."
    if ((now_us - s_telemetry_last_gen_us) < PL_TELEMETRY_GEN_INTERVAL_US) {
        return;
    }
    s_telemetry_last_gen_us = now_us;

    uint8_t page = s_telemetry_requested_page;
    uint8_t tmp[PL_CONFIG_TELEMETRY_BUF_LEN];
    size_t written = pl_ui_telemetry(ui, page, tmp, sizeof(tmp));
    if (written == 0) {
        // Unsupported page, or (structurally impossible here, since
        // sizeof(tmp) is PL_CONFIG_TELEMETRY_BUF_LEN) too small a buffer
        // -- see pl_ui_telemetry's own doc comment: "0 means don't
        // publish this poll." Leave whatever was previously published in
        // place.
        return;
    }

    publish_telemetry(tmp, written);
}

// --- bead pico-link-jyhk.21: GET_LIBRARY (0x05) ---
// design section 3/11 Task 4. Same publish/poll-recency shape as
// GET_TELEMETRY just above -- see that section's comment for the
// "self-heals, not a delta log" reasoning, which applies unchanged here.
static uint8_t s_library_buf[PL_CONFIG_LIBRARY_BUF_LEN];
static volatile uint16_t s_library_len;
// Stamped by the IRQ-context GET_LIBRARY SETUP handler on every poll --
// own recency tracker, independent of s_telemetry_last_poll_us, since a
// host could poll one page without the other.
static volatile uint64_t s_library_last_poll_us;
// Thread-context only, same single-writer/single-reader reasoning as
// s_telemetry_last_gen_us above.
static uint64_t s_library_last_gen_us;

static void publish_library(const uint8_t *buf, size_t len) {
    size_t copy_len = (len > PL_CONFIG_LIBRARY_BUF_LEN) ? PL_CONFIG_LIBRARY_BUF_LEN : len;
    uint32_t irq_state = save_and_disable_interrupts();
    memcpy(s_library_buf, buf, copy_len);
    s_library_len = (uint16_t)copy_len;
    restore_interrupts(irq_state);
}

void pl_config_itf_poll_library(struct PlUi *ui) {
    uint64_t now_us = time_us_64();

    uint64_t last_poll_us = s_library_last_poll_us;
    if (last_poll_us == 0 || (now_us - last_poll_us) > PL_TELEMETRY_POLL_RECENCY_US) {
        return;
    }

    if ((now_us - s_library_last_gen_us) < PL_TELEMETRY_GEN_INTERVAL_US) {
        return;
    }
    s_library_last_gen_us = now_us;

    uint8_t tmp[PL_CONFIG_LIBRARY_BUF_LEN];
    size_t written = pl_ui_library(ui, tmp, sizeof(tmp));
    if (written == 0) {
        // Too-small buffer (structurally impossible here) OR the freshly
        // encoded library_rev is unchanged from the last publish -- see
        // pl_ui_library's own doc comment. Either way, leave whatever was
        // previously published in place.
        return;
    }

    publish_library(tmp, written);
}

// --- bead pico-link-jyhk.21: preview lease -- design section 7 ---
//
// Stamped by configd_control_xfer_cb on EVERY iface-6 SETUP (all seven
// requests, not just GET_TELEMETRY), thread-context read only by
// pl_config_itf_poll_preview_lease. No lock: same "at most one loop
// iteration of slop, never a correctness hazard" reasoning as
// s_telemetry_last_poll_us above -- ending a preview one iteration late
// or early is not a safety issue.
#define PL_HOST_PREVIEW_LEASE_US (2ull * 1000ull * 1000ull) // 2s, design sec 7

static volatile uint64_t s_last_host_setup_us;
// True once pl_ui_host_preview_end has already been called for the
// CURRENT idle period -- reset to false by every fresh SETUP stamp, so a
// long idle stretch calls it exactly once, not every superloop iteration.
static volatile bool s_preview_lease_notified;

static void stamp_host_setup(void) {
    s_last_host_setup_us = time_us_64();
    s_preview_lease_notified = false;
}

void pl_config_itf_poll_preview_lease(struct PlUi *ui) {
    uint64_t last = s_last_host_setup_us;
    // last == 0: no iface-6 SETUP has EVER landed -- nothing to expire.
    if (last == 0 || s_preview_lease_notified) {
        return;
    }
    if ((time_us_64() - last) > PL_HOST_PREVIEW_LEASE_US) {
        pl_ui_host_preview_end(ui);
        s_preview_lease_notified = true;
    }
}

// Builds the "not ready" GET_TELEMETRY reply -- a proto-1 header
// (proto/page/len) with snap_seq (and everything after it) zeroed, at the
// full current wire length. Published by configd_init below in place of
// s_telemetry_len == 0 (bead pico-link-s6hh: replying zero bytes for ~10s
// after boot looked like a short/malformed reply to the web companion
// rather than the documented "not ready" state -- ADA DESIGN comment on
// pico-link-jyhk.1, core/src/app/telemetry.rs's module doc: "snap_seq ...
// `0` means not ready"). No Rust call: this is plain bytes, matching the
// same "C only copies opaque bytes" discipline the SETUP handler itself
// already follows.
static void publish_telemetry_not_ready(void) {
    uint8_t tmp[PL_CFG_HOME_SNAPSHOT_LEN];
    memset(tmp, 0, sizeof(tmp));
    tmp[PL_CFG_TELEMETRY_OFF_PROTO] = PL_CFG_TELEMETRY_PROTO_VERSION;
    tmp[PL_CFG_TELEMETRY_OFF_PAGE] = PL_TELEMETRY_PAGE_HOME;
    tmp[PL_CFG_TELEMETRY_OFF_LEN] = (uint8_t)(PL_CFG_HOME_SNAPSHOT_LEN & 0xFFu);
    tmp[PL_CFG_TELEMETRY_OFF_LEN + 1] = (uint8_t)((PL_CFG_HOME_SNAPSHOT_LEN >> 8) & 0xFFu);
    publish_telemetry(tmp, sizeof(tmp));
}

static void configd_init(void) {
    s_mailbox_pending = false;
    s_mailbox_len = 0;
    set_status((pl_cfg_status_wire_t){ .state = PL_CFG_STATE_IDLE });
    publish_telemetry_not_ready();
    s_telemetry_last_poll_us = 0;
    s_telemetry_last_gen_us = 0;
    s_telemetry_requested_page = PL_TELEMETRY_PAGE_HOME;
    s_library_len = 0;
    s_library_last_poll_us = 0;
    s_library_last_gen_us = 0;
    s_op_status_len = 0;
    s_last_host_setup_us = 0;
    s_preview_lease_notified = false;
}

static void configd_reset(uint8_t __unused rhport) {
    itf_num = 0;
    s_mailbox_pending = false;
}

static uint16_t configd_open(uint8_t __unused rhport, tusb_desc_interface_t const *itf_desc, uint16_t max_len) {
    TU_VERIFY(TUSB_CLASS_VENDOR_SPECIFIC == itf_desc->bInterfaceClass &&
              PL_CONFIG_INTERFACE_SUBCLASS == itf_desc->bInterfaceSubClass &&
              PL_CONFIG_INTERFACE_PROTOCOL == itf_desc->bInterfaceProtocol, 0);

    uint16_t const drv_len = sizeof(tusb_desc_interface_t);
    TU_VERIFY(max_len >= drv_len, 0);

    itf_num = itf_desc->bInterfaceNumber;
    pl_log_locked("usb-config: itf bound itf=%u\r\n", (unsigned)itf_num);

    return drv_len;
}

// IRQ context (tud_task runs in the 0xC0 user-IRQ worker in this project
// -- see usb_config_itf.h's module doc). Only copies bytes (via
// tud_control_xfer, which TinyUSB itself performs synchronously into
// s_mailbox_buf) and flips flags/status words -- never calls into Rust.
// Shared IN-reply staging buffer, used by every IN request's SETUP handler
// below (GET_STATUS, GET_TELEMETRY, GET_INFO, GET_LIBRARY, GET_OP_STATUS)
// to copy from that request's own published source (s_status/
// s_telemetry_buf/s_library_buf/s_op_status_buf, or GET_INFO's static
// fields) just before tud_control_xfer -- design section 5: "EP0
// serialises control transfers, so all IN requests share ONE static reply
// buffer sized for the largest (1536B), replacing telemetry's separate
// 256B reply." Safe to share across requests despite each being copied
// inside this same IRQ-context callback: EP0 serialises control
// transfers, so there is never a second SETUP in flight reusing this
// buffer before the first's tud_control_xfer has consumed it.
static uint8_t s_reply_buf[PL_CONFIG_LIBRARY_BUF_LEN];

static bool configd_control_xfer_cb(uint8_t rhport, uint8_t stage, tusb_control_request_t const *request) {
    if (request->wIndex != itf_num) {
        return false;
    }
    // pico-sdk 2.1.1's TinyUSB usbd.c routes every type==VENDOR control
    // request straight to tud_vendor_control_xfer_cb (never to a class
    // driver's control_xfer_cb), so a real VENDOR request can never reach
    // this callback and would always stall. type==CLASS + recipient
    // INTERFACE IS routed here by wIndex -- the same convention usb_reset.c's
    // resetd already relies on for its BOOTSEL request. Accept CLASS, not
    // VENDOR.
    if (request->bmRequestType_bit.type != TUSB_REQ_TYPE_CLASS ||
        request->bmRequestType_bit.recipient != TUSB_REQ_RCPT_INTERFACE) {
        return false;
    }

    if (stage == CONTROL_STAGE_SETUP) {
        // Design section 7: "C stamps s_last_host_setup_us on EVERY
        // iface-6 SETUP" -- every request that reaches this point has
        // already passed the wIndex/type/recipient filter above, so this
        // covers all seven bRequests, including ones that go on to stall
        // for their own reasons (a present, if momentarily confused, host
        // still counts as "present" for the preview lease).
        stamp_host_setup();

        if (request->bRequest == PL_CFG_REQ_IMPORT_PRESET &&
            request->bmRequestType_bit.direction == TUSB_DIR_OUT) {
            // Stall (return false) while the shared mailbox is already
            // pending (an IMPORT_PRESET or a HOST_OP), or on an
            // out-of-range length -- see usb_config_itf.h's module doc:
            // "SETUP stalls if an import is already pending or wLength is
            // out of range." This is the only gate that keeps
            // s_mailbox_buf's IRQ-writer/thread-reader windows disjoint.
            if (s_mailbox_pending || request->wLength == 0 || request->wLength > PL_CONFIG_MAILBOX_LEN) {
                pl_log_locked(
                    "usb-config: IMPORT_PRESET SETUP stalled (pending=%d wLength=%u)\r\n", (int)s_mailbox_pending,
                    (unsigned)request->wLength
                );
                return false;
            }
            return tud_control_xfer(rhport, request, s_mailbox_buf, request->wLength);
        }

        if (request->bRequest == PL_CFG_REQ_GET_STATUS &&
            request->bmRequestType_bit.direction == TUSB_DIR_IN) {
            // Plain read -- see s_status's own comment for why this needs
            // no lock here: the writer (pl_config_itf_poll, thread
            // context) excludes this IRQ for the duration of its own
            // write, so whatever is in s_status right now is always a
            // complete, non-torn snapshot. bead pico-link-jyhk.21 review
            // fix: min(wLength, sizeof) rather than requiring
            // wLength >= sizeof -- a struct growth must never stall an
            // old host that only asks for the old size (same contract
            // GET_TELEMETRY/GET_INFO already had).
            memcpy(s_reply_buf, &s_status, sizeof(s_status));
            uint16_t xfer_len = (uint16_t)sizeof(s_status);
            if (xfer_len > request->wLength) {
                xfer_len = request->wLength;
            }
            return tud_control_xfer(rhport, request, s_reply_buf, xfer_len);
        }

        if (request->bRequest == PL_CFG_REQ_GET_TELEMETRY &&
            request->bmRequestType_bit.direction == TUSB_DIR_IN) {
            uint8_t page = (uint8_t)(request->wValue & 0xFFu);
            if (page != PL_TELEMETRY_PAGE_HOME) {
                // Only page 0 exists today (design sec 4's versioning
                // note: "F3 diagnostics becomes page 1 under the same
                // header" -- not yet built). Stall rather than reply with
                // the wrong page's data.
                return false;
            }

            // Stamp poll-recency + requested page BEFORE copying the
            // reply, matching design sec 3 flow (a): "stamp
            // s_telemetry_last_poll_us, copy s_telemetry into a static
            // reply buffer, then tud_control_xfer." NO pl_log in this
            // path (design sec 3: "NO pl_log in this path").
            s_telemetry_last_poll_us = time_us_64();
            s_telemetry_requested_page = page;

            // Copying s_telemetry_buf here, inside SETUP, means a
            // concurrent publish_telemetry (thread context, under
            // save_and_disable_interrupts) can never tear this reply --
            // this callback runs AS the excluded IRQ, same reasoning as
            // GET_STATUS's reply just above.
            uint16_t reply_len = s_telemetry_len;
            memcpy(s_reply_buf, s_telemetry_buf, reply_len);

            uint16_t xfer_len = reply_len;
            if (xfer_len > request->wLength) {
                xfer_len = request->wLength;
            }
            // A short (possibly zero-length) reply before the first
            // successful generation is a legal control transfer, not a
            // stall -- see this file's telemetry state comment above for
            // why "not ready" is realised as "however much has ever been
            // published" rather than a synthesized header.
            return tud_control_xfer(rhport, request, s_reply_buf, xfer_len);
        }

        if (request->bRequest == PL_CFG_REQ_GET_INFO &&
            request->bmRequestType_bit.direction == TUSB_DIR_IN) {
            // Static configuration only -- no Rust call, no dynamic
            // state, no lock needed (design sec 5).
            pl_cfg_info_wire_t reply;
            memset(&reply, 0, sizeof(reply));
            const char *version = PL_FW_VERSION_STRING;
            size_t version_len = strnlen(version, sizeof(reply.version));

            reply.info_ver = PL_CFG_INFO_VERSION;
            reply.import_proto = PL_CFG_IMPORT_PROTO_VERSION;
            reply.status_ver = PL_CFG_STATUS_VERSION;
            reply.telemetry_proto = PL_CFG_TELEMETRY_PROTO_VERSION;
            reply.telemetry_page_mask = PL_CFG_TELEMETRY_PAGE_MASK;
            reply.version_len = (uint8_t)version_len;
            memcpy(reply.version, version, version_len);
            // info_ver 2 append (bead pico-link-jyhk.21, design sec 8).
            reply.lib_proto = PL_CFG_LIB_PROTO_VERSION;
            reply.op_proto = PL_CFG_OP_PROTO_VERSION;
            reply.mailbox_len = PL_CONFIG_MAILBOX_LEN;
            reply.op_mask = PL_CFG_OP_MASK;
            reply.library_max_len = PL_CFG_LIBRARY_MAX_LEN;

            memcpy(s_reply_buf, &reply, sizeof(reply));
            uint16_t xfer_len = (uint16_t)sizeof(reply);
            if (xfer_len > request->wLength) {
                xfer_len = request->wLength;
            }
            return tud_control_xfer(rhport, request, s_reply_buf, xfer_len);
        }

        if (request->bRequest == PL_CFG_REQ_GET_LIBRARY &&
            request->bmRequestType_bit.direction == TUSB_DIR_IN) {
            // Stamp poll-recency BEFORE copying the reply, same shape as
            // GET_TELEMETRY above (design section 3 applies to both
            // published-snapshot reads identically).
            s_library_last_poll_us = time_us_64();

            uint16_t reply_len = s_library_len;
            memcpy(s_reply_buf, s_library_buf, reply_len);

            uint16_t xfer_len = reply_len;
            if (xfer_len > request->wLength) {
                xfer_len = request->wLength;
            }
            // A short (possibly zero-length) reply before the first
            // successful generation is a legal control transfer, not a
            // stall -- same "self-heals" reasoning as GET_TELEMETRY.
            return tud_control_xfer(rhport, request, s_reply_buf, xfer_len);
        }

        if (request->bRequest == PL_CFG_REQ_HOST_OP &&
            request->bmRequestType_bit.direction == TUSB_DIR_OUT) {
            // Same shared-mailbox stall gate as IMPORT_PRESET -- design
            // section 5: "IMPORT_PRESET and HOST_OP share the one 1KB
            // mailbox and its pending flag: SETUP of either stalls while
            // one is pending."
            if (s_mailbox_pending || request->wLength == 0 || request->wLength > PL_CONFIG_MAILBOX_LEN) {
                pl_log_locked(
                    "usb-config: HOST_OP SETUP stalled (pending=%d wLength=%u)\r\n", (int)s_mailbox_pending,
                    (unsigned)request->wLength
                );
                return false;
            }
            return tud_control_xfer(rhport, request, s_mailbox_buf, request->wLength);
        }

        if (request->bRequest == PL_CFG_REQ_GET_OP_STATUS &&
            request->bmRequestType_bit.direction == TUSB_DIR_IN) {
            // Plain read of already-published bytes -- no Rust call here,
            // same shape as GET_STATUS/GET_TELEMETRY/GET_LIBRARY. The
            // publish happened synchronously inside
            // pl_config_itf_poll_host_op (thread context), which is the
            // only writer, under save_and_disable_interrupts.
            uint16_t reply_len = s_op_status_len;
            memcpy(s_reply_buf, s_op_status_buf, reply_len);

            uint16_t xfer_len = reply_len;
            if (xfer_len > request->wLength) {
                xfer_len = request->wLength;
            }
            return tud_control_xfer(rhport, request, s_reply_buf, xfer_len);
        }

        return false;
    }

    if (stage == CONTROL_STAGE_ACK) {
        if (request->bRequest == PL_CFG_REQ_IMPORT_PRESET) {
            // DATA-stage completion: TinyUSB has already copied
            // request->wLength bytes into s_mailbox_buf (tud_control_xfer's
            // own doing, still IRQ context). Nothing else happens here --
            // just record the length/kind and flip the flag the superloop
            // polls. See pl_config_itf_poll for what happens next, in
            // thread context.
            s_mailbox_len = request->wLength;
            // Set BUSY here, in IRQ context, not just s_mailbox_pending: the
            // ACK completes before the host can issue its next SETUP, so a
            // GET_STATUS that lands in the gap before pl_config_itf_poll
            // (thread context) picks the import up must not see the
            // *previous* import's final status (idle on a fresh board looks
            // like a false failure; saved/rejected from a prior import looks
            // like a false success for this one). Safe from IRQ context:
            // set_status disables interrupts around its own write, and it is
            // the only other writer of s_status.
            set_status((pl_cfg_status_wire_t){ .state = PL_CFG_STATE_BUSY });
            s_mailbox_kind = PL_CFG_REQ_IMPORT_PRESET;
            s_mailbox_pending = true;
        } else if (request->bRequest == PL_CFG_REQ_HOST_OP) {
            // Same DATA-stage completion shape as IMPORT_PRESET, but no
            // GET_STATUS-style BUSY write: HOST_OP has no such status --
            // the host polls GET_OP_STATUS instead (design section 4:
            // "This removes the need for C to write a BUSY status at
            // ACK").
            s_mailbox_len = request->wLength;
            s_mailbox_kind = PL_CFG_REQ_HOST_OP;
            s_mailbox_pending = true;
        }
        return true;
    }

    // DATA stage: nothing to do (TinyUSB itself moves the bytes).
    return true;
}

static bool configd_xfer_cb(uint8_t __unused rhport, uint8_t __unused ep_addr, xfer_result_t __unused result, uint32_t __unused xferred_bytes) {
    return true;
}

static usbd_class_driver_t const _configd_driver = {
#if CFG_TUSB_DEBUG >= 2
    .name = "PL-CONFIG",
#endif
    .init             = configd_init,
    .reset            = configd_reset,
    .open             = configd_open,
    .control_xfer_cb  = configd_control_xfer_cb,
    .xfer_cb          = configd_xfer_cb,
    .sof              = NULL
};

usbd_class_driver_t const *pl_configd_driver(void) {
    return &_configd_driver;
}
