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

#include "tusb.h"

#include "hardware/sync.h"

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
    set_status((pl_cfg_status_wire_t){ .state = PL_CFG_STATE_BUSY });

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

static void configd_init(void) {
    s_import_pending = false;
    s_import_len = 0;
    set_status((pl_cfg_status_wire_t){ .state = PL_CFG_STATE_IDLE });
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
            // Plain read -- see s_status's own comment for why this needs
            // no lock here: the writer (pl_config_itf_poll, thread
            // context) excludes this IRQ for the duration of its own
            // write, so whatever is in s_status right now is always a
            // complete, non-torn snapshot.
            static pl_cfg_status_wire_t reply;
            reply = s_status;
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
