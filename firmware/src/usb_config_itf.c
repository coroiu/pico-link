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

// GET_STATUS's reply, packed into one 32-bit word so a single aligned
// load/store is atomic between the IRQ-context reader (GET_STATUS's SETUP
// handler) and the thread-context writer (pl_config_itf_poll) without
// needing an IRQ-disable critical section -- Cortex-M word-aligned
// word accesses are indivisible.
static volatile uint32_t s_status_word;

static void set_status(uint8_t state, uint8_t error, uint16_t line) {
    pl_cfg_status_wire_t wire = { .state = state, .error = error, .line = line };
    uint32_t word;
    memcpy(&word, &wire, sizeof(word));
    s_status_word = word;
}

// Maps a PlEqCommandResult's `code` (0 on success, else one of the small
// negative EqApoError/DebugEqCommandError codes documented in
// ui-ffi/src/lib.rs) to GET_STATUS's single unsigned error byte.
static uint8_t eq_error_byte(int32_t code) {
    if (code >= 0) {
        return 0;
    }
    // These codes are always small in magnitude (see
    // debug_eq_command_error_result's match arms: -1..-9) -- never
    // anywhere near 256, so the narrowing below never wraps.
    int32_t mag = -code;
    if (mag > 253) {
        mag = 253; // stay below the reserved PL_CFG_ERR_* range
    }
    return (uint8_t)mag;
}

// Truncates a PlEqCommandResult's `line` (a usize widened to u32 on the
// Rust side, always tiny -- see its own doc comment) to the wire's u16.
static uint16_t eq_line_u16(uint32_t line) {
    return (line > 0xFFFFu) ? 0xFFFFu : (uint16_t)line;
}

void pl_config_itf_poll(struct PlUi *ui) {
    if (!s_import_pending) {
        return;
    }

    uint16_t len = s_import_len;
    set_status(PL_CFG_STATE_BUSY, 0, 0);

    if (len < 2) {
        pl_log("usb-config: IMPORT_PRESET rejected -- header too short (len=%u)\r\n", (unsigned)len);
        set_status(PL_CFG_STATE_REJECTED, PL_CFG_ERR_MALFORMED_HEADER, 0);
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
        set_status(PL_CFG_STATE_REJECTED, PL_CFG_ERR_MALFORMED_HEADER, 0);
        s_import_pending = false;
        return;
    }

    // ryw.12.5 seam: this bead proves the transport end to end by routing
    // the imported APO text through the SAME debug-override session API
    // (pl_ui_debug_eq_command) the "EQ BEGIN"/"EQ <line>"/"EQ END" CDC
    // console commands drive (pico-link-ryw.11). The name bytes above are
    // already extracted and length-validated for the real save path;
    // ryw.12.2/12.4 replace the body below with a call that uses them --
    // the surrounding header parse, buffer/flag handshake and GET_STATUS
    // reporting do not change.
    const uint8_t *text = s_import_buf + header_len;
    size_t text_len = (size_t)len - header_len;

    PlEqCommandResult result = pl_ui_debug_eq_command(ui, (const uint8_t *)"BEGIN", 5);
    if (result.code != 0) {
        pl_log("usb-config: IMPORT_PRESET BEGIN failed code=%ld\r\n", (long)result.code);
        set_status(PL_CFG_STATE_REJECTED, eq_error_byte(result.code), eq_line_u16(result.line));
        s_import_pending = false;
        return;
    }

    bool failed = false;
    PlEqCommandResult fail_result = { .code = 0, .line = 0 };
    size_t i = 0;
    while (i < text_len) {
        size_t start = i;
        while (i < text_len && text[i] != '\n') {
            i++;
        }
        size_t end = i;
        if (end > start && text[end - 1] == '\r') {
            end--;
        }
        if (i < text_len) {
            i++; // skip the '\n'
        }
        if (end == start) {
            continue; // blank line -- same tolerance feed_line() gives a trimmed empty line
        }
        result = pl_ui_debug_eq_command(ui, text + start, end - start);
        if (result.code != 0) {
            failed = true;
            fail_result = result;
            break;
        }
    }

    if (failed) {
        // Best-effort: leave no half-built session active. Its own result
        // is not reported -- the ORIGINAL parse failure above is what the
        // host needs to see.
        (void)pl_ui_debug_eq_command(ui, (const uint8_t *)"OFF", 3);
        pl_log(
            "usb-config: IMPORT_PRESET line %lu failed code=%ld\r\n", (unsigned long)fail_result.line,
            (long)fail_result.code
        );
        set_status(PL_CFG_STATE_REJECTED, eq_error_byte(fail_result.code), eq_line_u16(fail_result.line));
        s_import_pending = false;
        return;
    }

    result = pl_ui_debug_eq_command(ui, (const uint8_t *)"END", 3);
    if (result.code != 0) {
        pl_log("usb-config: IMPORT_PRESET END failed code=%ld\r\n", (long)result.code);
        set_status(PL_CFG_STATE_REJECTED, eq_error_byte(result.code), eq_line_u16(result.line));
        s_import_pending = false;
        return;
    }

    pl_log("usb-config: IMPORT_PRESET applied ok (name_len=%u, %u text bytes)\r\n", (unsigned)name_len, (unsigned)text_len);
    set_status(PL_CFG_STATE_SAVED, 0, 0);
    s_import_pending = false;
}

static void configd_init(void) {
    s_import_pending = false;
    s_import_len = 0;
    set_status(PL_CFG_STATE_IDLE, 0, 0);
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
    if (request->bmRequestType_bit.type != TUSB_REQ_TYPE_VENDOR ||
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
            // Single aligned word read -- see s_status_word's own comment
            // for why this needs no lock against pl_config_itf_poll's
            // (thread-context) writes.
            uint32_t word = s_status_word;
            static pl_cfg_status_wire_t reply;
            memcpy(&reply, &word, sizeof(reply));
            return tud_control_xfer(rhport, request, &reply, sizeof(reply));
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
