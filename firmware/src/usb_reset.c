// Pico Link firmware -- bead pico-link-l60: this project's own driverless-
// reset (picotool BOOTSEL) TinyUSB class driver. See usb_reset.h for why
// this file exists instead of pico-sdk's reset_interface.c.
//
// PROVENANCE: ported near-verbatim from pico-sdk's
// src/rp2_common/pico_stdio_usb/reset_interface.c (BSD-3-Clause, Copyright
// 2021 Raspberry Pi (Trading) Ltd) -- see usb_reset.h's module doc for
// exactly what changed (the #if gates removed, no activity LED, two pl_log
// telemetry lines added).

#include "tusb.h"

#include "pico/bootrom.h"
#include "pico/stdio_usb/reset_interface.h"
#include "hardware/watchdog.h"
#include "device/usbd_pvt.h"

#include "usb_pump.h"

static uint8_t itf_num;

static void resetd_init(void) {
}

static void resetd_reset(uint8_t __unused rhport) {
    itf_num = 0;
}

static uint16_t resetd_open(uint8_t __unused rhport, tusb_desc_interface_t const *itf_desc, uint16_t max_len) {
    TU_VERIFY(TUSB_CLASS_VENDOR_SPECIFIC == itf_desc->bInterfaceClass &&
              RESET_INTERFACE_SUBCLASS == itf_desc->bInterfaceSubClass &&
              RESET_INTERFACE_PROTOCOL == itf_desc->bInterfaceProtocol, 0);

    uint16_t const drv_len = sizeof(tusb_desc_interface_t);
    TU_VERIFY(max_len >= drv_len, 0);

    itf_num = itf_desc->bInterfaceNumber;

    // Bead pico-link-l60 telemetry: proves the driver bound to an
    // interface at all, before picotool ever sends a control request --
    // its absence on the one verification flash means M3/M4 (descriptor /
    // driver dispatch), not the compiled-out-branch bug this file fixes.
    pl_log_locked("usb-reset: itf bound itf=%u\r\n", (unsigned)itf_num);

    return drv_len;
}

// Support for parameterized reset via vendor interface control request.
// Unlike upstream pico-sdk's reset_interface.c, both the BOOTSEL and
// FLASH-boot branches below are UNCONDITIONAL -- see usb_reset.h for why
// gating them on PICO_STDIO_USB_RESET_INTERFACE_SUPPORT_* silently compiled
// them out in this project's build.
static bool resetd_control_xfer_cb(uint8_t __unused rhport, uint8_t stage, tusb_control_request_t const * request) {
    // Bead pico-link-l60 telemetry: logged for EVERY setup-stage control
    // transfer on this interface (not just the ones we recognize), so a
    // hardware run can tell "picotool never asked" (M2) apart from
    // "picotool asked and our handler was wrong" (M1) even if the request
    // doesn't match RESET_REQUEST_BOOTSEL/RESET_REQUEST_FLASH.
    if (stage == CONTROL_STAGE_SETUP) {
        pl_log_locked(
            "usb-reset: ctrl bmReq=0x%02x bReq=0x%02x wValue=0x%04x wIndex=%u\r\n",
            (unsigned)request->bmRequestType,
            (unsigned)request->bRequest,
            (unsigned)request->wValue,
            (unsigned)request->wIndex
        );
    }

    // nothing to do with DATA & ACK stage
    if (stage != CONTROL_STAGE_SETUP) return true;

    if (request->wIndex == itf_num) {
        if (request->bRequest == RESET_REQUEST_BOOTSEL) {
            // No activity LED wired on this board.
            int gpio = -1;
            bool active_low = false;
            rom_reset_usb_boot_extra(gpio, (request->wValue & 0x7f) | 0u, active_low);
            // does not return, otherwise we'd return true
        }

        if (request->bRequest == RESET_REQUEST_FLASH) {
            watchdog_reboot(0, 0, PICO_STDIO_USB_RESET_RESET_TO_FLASH_DELAY_MS);
            return true;
        }
    }
    return false;
}

static bool resetd_xfer_cb(uint8_t __unused rhport, uint8_t __unused ep_addr, xfer_result_t __unused result, uint32_t __unused xferred_bytes) {
    return true;
}

static usbd_class_driver_t const _resetd_driver =
{
#if CFG_TUSB_DEBUG >= 2
    .name = "RESET",
#endif
    .init             = resetd_init,
    .reset            = resetd_reset,
    .open             = resetd_open,
    .control_xfer_cb  = resetd_control_xfer_cb,
    .xfer_cb          = resetd_xfer_cb,
    .sof              = NULL
};

// Implement callback to add our custom driver. This is the strong
// usbd_app_driver_get_cb symbol -- pico-sdk's reset_interface.c must NOT
// also define it (see CMakeLists.txt:
// PICO_STDIO_USB_ENABLE_RESET_VIA_VENDOR_INTERFACE=0 compiles that whole
// translation unit down to nothing, freeing the symbol for this file).
usbd_class_driver_t const *usbd_app_driver_get_cb(uint8_t *driver_count) {
    *driver_count = 1;
    return &_resetd_driver;
}
