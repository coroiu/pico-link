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

// Design comment on pico-link-ryw.12, section 1: "wLength <= 1024".
#define PL_CONFIG_IMPORT_BUF_LEN 1024

static uint8_t itf_num;

// The pending-import buffer and its length, written only by
// configd_control_xfer_cb (IRQ context) while `s_import_pending` is
// false, read only by pl_config_itf_poll (thread context) while
// `s_import_pending` is true. `s_import_pending` is the single flag that
// makes those two windows disjoint -- SETUP for a new IMPORT_PRESET
// stalls (see below) whenever it is already true, so the buffer is never
// written to while the thread-context side is reading it.
static uint8_t s_import_buf[PL_CONFIG_IMPORT_BUF_LEN];
static volatile uint16_t s_import_len;
static volatile bool s_import_pending;

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
    if (!s_import_pending) {
        return;
    }

    uint16_t len = s_import_len;
    // BUSY is already set by configd_control_xfer_cb's ACK stage (see its
    // comment) -- no need to set it again here.

    if (len < 2) {
        pl_log("usb-config: IMPORT_PRESET rejected -- header too short (len=%u)\r\n", (unsigned)len);
        set_status((pl_cfg_status_wire_t){ .state = PL_CFG_STATE_REJECTED, .error = PL_CFG_ERR_MALFORMED_HEADER });
        s_import_pending = false;
        return;
    }

    uint8_t proto = s_import_buf[0];
    uint8_t name_len = s_import_buf[1];
    size_t header_len = (size_t)2 + (size_t)name_len;
    if (proto != 1 || name_len > PL_CONFIG_NAME_MAX || header_len > (size_t)len) {
        pl_log(
            "usb-config: IMPORT_PRESET rejected -- bad header (proto=%u name_len=%u len=%u)\r\n", (unsigned)proto,
            (unsigned)name_len, (unsigned)len
        );
        set_status((pl_cfg_status_wire_t){ .state = PL_CFG_STATE_REJECTED, .error = PL_CFG_ERR_MALFORMED_HEADER });
        s_import_pending = false;
        return;
    }

    // Real save path (pico-link-ryw.12.4): one call, whole document. On
    // success this has ALREADY queued a Command::SavePreset before
    // returning (see pl_ui_import_preset's own doc comment) -- so
    // PL_CFG_STATE_SAVED below is only ever reported once that queueing
    // has happened, never before.
    const uint8_t *name = s_import_buf + 2;
    const uint8_t *text = s_import_buf + header_len;
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
        s_import_pending = false;
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
    s_import_pending = false;
}

// --- bead pico-link-jyhk.4: GET_TELEMETRY (0x03) / GET_INFO (0x04) ---
// ADA DESIGN comment on pico-link-jyhk.1, sections 3-5.
//
// s_telemetry_buf/s_telemetry_len are published by
// pl_config_itf_poll_telemetry (thread context, main.c's superloop) under
// save_and_disable_interrupts and read directly by
// configd_control_xfer_cb's GET_TELEMETRY SETUP handler (IRQ context) --
// the same "writer excludes the IRQ, IRQ needs no lock" pattern as
// s_status/set_status above. Both are zero-initialized by static storage
// duration, which is exactly the pre-first-generation "not ready" state
// (core's snap_seq field reads 0 until the first successful encode --
// see App::telemetry_snapshot's doc comment): the reply is simply
// however-many-bytes (possibly zero) have ever been published, with no
// separate "reset to not-ready after further idle" step. That is a
// deliberate simplification of design section 3 flow (c) -- once a page
// has attached at least once, serving its last real (if stale) snapshot
// rather than a synthesized not-ready header matches the design's own
// "self-heals, not a delta log" philosophy (section 2) more closely than
// inventing a second not-ready representation would.
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

static void configd_init(void) {
    s_import_pending = false;
    s_import_len = 0;
    set_status((pl_cfg_status_wire_t){ .state = PL_CFG_STATE_IDLE });
    s_telemetry_len = 0;
    s_telemetry_last_poll_us = 0;
    s_telemetry_last_gen_us = 0;
    s_telemetry_requested_page = PL_TELEMETRY_PAGE_HOME;
}

static void configd_reset(uint8_t __unused rhport) {
    itf_num = 0;
    s_import_pending = false;
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
// s_import_buf) and flips flags/status words -- never calls into Rust.
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
        if (request->bRequest == PL_CFG_REQ_IMPORT_PRESET &&
            request->bmRequestType_bit.direction == TUSB_DIR_OUT) {
            // Stall (return false) while an import is already pending, or
            // on an out-of-range length -- see usb_config_itf.h's module
            // doc: "SETUP stalls if an import is already pending or
            // wLength is out of range." This is the only gate that keeps
            // s_import_buf's IRQ-writer/thread-reader windows disjoint.
            if (s_import_pending || request->wLength == 0 || request->wLength > PL_CONFIG_IMPORT_BUF_LEN) {
                pl_log_locked(
                    "usb-config: IMPORT_PRESET SETUP stalled (pending=%d wLength=%u)\r\n", (int)s_import_pending,
                    (unsigned)request->wLength
                );
                return false;
            }
            return tud_control_xfer(rhport, request, s_import_buf, request->wLength);
        }

        if (request->bRequest == PL_CFG_REQ_GET_STATUS &&
            request->bmRequestType_bit.direction == TUSB_DIR_IN &&
            request->wLength >= sizeof(pl_cfg_status_wire_t)) {
            // Plain read -- see s_status's own comment for why this needs
            // no lock here: the writer (pl_config_itf_poll, thread
            // context) excludes this IRQ for the duration of its own
            // write, so whatever is in s_status right now is always a
            // complete, non-torn snapshot.
            static pl_cfg_status_wire_t reply;
            reply = s_status;
            return tud_control_xfer(rhport, request, &reply, sizeof(reply));
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
            static uint8_t reply[PL_CONFIG_TELEMETRY_BUF_LEN];
            uint16_t reply_len = s_telemetry_len;
            memcpy(reply, s_telemetry_buf, reply_len);

            uint16_t xfer_len = reply_len;
            if (xfer_len > request->wLength) {
                xfer_len = request->wLength;
            }
            // A short (possibly zero-length) reply before the first
            // successful generation is a legal control transfer, not a
            // stall -- see this file's telemetry state comment above for
            // why "not ready" is realised as "however much has ever been
            // published" rather than a synthesized header.
            return tud_control_xfer(rhport, request, reply, xfer_len);
        }

        if (request->bRequest == PL_CFG_REQ_GET_INFO &&
            request->bmRequestType_bit.direction == TUSB_DIR_IN) {
            // Static configuration only -- no Rust call, no dynamic
            // state, no lock needed (design sec 5).
            static pl_cfg_info_wire_t reply;
            const char *version = PL_FW_VERSION_STRING;
            size_t version_len = strnlen(version, sizeof(reply.version));

            reply.info_ver = PL_CFG_INFO_VERSION;
            reply.import_proto = PL_CFG_IMPORT_PROTO_VERSION;
            reply.status_ver = PL_CFG_STATUS_VERSION;
            reply.telemetry_proto = PL_CFG_TELEMETRY_PROTO_VERSION;
            reply.telemetry_page_mask = PL_CFG_TELEMETRY_PAGE_MASK;
            reply.version_len = (uint8_t)version_len;
            memset(reply.version, 0, sizeof(reply.version));
            memcpy(reply.version, version, version_len);

            uint16_t xfer_len = (uint16_t)sizeof(reply);
            if (xfer_len > request->wLength) {
                xfer_len = request->wLength;
            }
            return tud_control_xfer(rhport, request, &reply, xfer_len);
        }

        return false;
    }

    if (stage == CONTROL_STAGE_ACK) {
        if (request->bRequest == PL_CFG_REQ_IMPORT_PRESET) {
            // DATA-stage completion: TinyUSB has already copied
            // request->wLength bytes into s_import_buf (tud_control_xfer's
            // own doing, still IRQ context). Nothing else happens here --
            // just record the length and flip the flag the superloop
            // polls. See pl_config_itf_poll for what happens next, in
            // thread context.
            s_import_len = request->wLength;
            // Set BUSY here, in IRQ context, not just s_import_pending: the
            // ACK completes before the host can issue its next SETUP, so a
            // GET_STATUS that lands in the gap before pl_config_itf_poll
            // (thread context) picks the import up must not see the
            // *previous* import's final status (idle on a fresh board looks
            // like a false failure; saved/rejected from a prior import looks
            // like a false success for this one). Safe from IRQ context:
            // set_status disables interrupts around its own write, and it is
            // the only other writer of s_status.
            set_status((pl_cfg_status_wire_t){ .state = PL_CFG_STATE_BUSY });
            s_import_pending = true;
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
