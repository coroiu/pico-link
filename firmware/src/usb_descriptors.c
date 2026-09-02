// Pico Link firmware -- M3: TinyUSB composite descriptor callbacks.
//
// PROVENANCE: structure (device descriptor, config descriptor assembly,
// string descriptor callback) follows the pattern common to ALL of
// pico-sdk's vendored TinyUSB device examples (MIT) -- most directly
// lib/tinyusb/examples/device/cdc_uac2/src/usb_descriptors.c (MIT,
// Copyright 2020 Jerzy Kasenberg / 2022 Angel Molina) for the CDC+audio
// composite shape, with the speaker's own descriptor macro swapped for the
// explicit-feedback one from uac2_speaker_fb (see usb_descriptors.h's
// provenance note). The reset interface's serial-number-from-flash-ID
// trick is pico-sdk's own (stdio_usb_descriptors.c). No USBPods code.
//
// This file is the ONE thing this project's USB device topology is: audio
// control + audio streaming (speaker, async + feedback) + CDC (console) +
// vendor reset. See usb_descriptors.h for interface/string index layout.

#include <string.h>

#include "tusb.h"

#include "pico/stdio_usb/reset_interface.h"
#include "pico/unique_id.h"

#include "usb_descriptors.h"

//--------------------------------------------------------------------+
// Endpoint numbers
//--------------------------------------------------------------------+
// EP1 is shared: OUT for audio data, IN for the explicit feedback value --
// RP2040/RP2350's dcd allows a same-numbered EP pair with opposite
// directions (tusb_mcu.h does NOT define TUD_ENDPOINT_ONE_DIRECTION_ONLY
// for OPT_MCU_RP2040). EP2 IN is CDC's notification endpoint. EP3 is CDC's
// data OUT/IN pair.
#define EPNUM_AUDIO_OUT (0x01)
#define EPNUM_AUDIO_FB  (0x81)
#define EPNUM_CDC_NOTIF (0x82)
#define EPNUM_CDC_OUT   (0x03)
#define EPNUM_CDC_IN    (0x83)
// HID consumer-control (media keys), interrupt IN only -- see bead
// pico-link-47z.1 / .planning/design/2026-09-02-media-keys.md.
#define EPNUM_HID_IN    (0x84)

//--------------------------------------------------------------------+
// Device Descriptor
//--------------------------------------------------------------------+
// VID kept as pico-sdk's own "Raspberry Pi" allocation (0x2E8A, same as the
// CDC-only M1b/M2 firmware) since RP2350 is Raspberry Pi silicon; PID
// changed from the CDC-only defaults (0x0009/0x000A) to 0x000C to mark this
// as the M3 composite topology -- see tools/usb-console/cdc_reader.py's
// DEFAULT_PID, updated to match.
#ifndef USBD_VID
#define USBD_VID (0x2E8A)
#endif
#ifndef USBD_PID
#define USBD_PID (0x000C)
#endif

static const tusb_desc_device_t usbd_desc_device = {
    .bLength = sizeof(tusb_desc_device_t),
    .bDescriptorType = TUSB_DESC_DEVICE,
    .bcdUSB = 0x0200,
    .bDeviceClass = TUSB_CLASS_MISC,
    .bDeviceSubClass = MISC_SUBCLASS_COMMON,
    .bDeviceProtocol = MISC_PROTOCOL_IAD,
    .bMaxPacketSize0 = CFG_TUD_ENDPOINT0_SIZE,
    .idVendor = USBD_VID,
    .idProduct = USBD_PID,
    // Bumped 0x0100 -> 0x0101 for the HID interface addition (bead
    // pico-link-47z.1). PID deliberately NOT bumped -- see
    // .planning/design/2026-09-02-media-keys.md section 2.5.
    .bcdDevice = 0x0101,
    .iManufacturer = STRID_MANUFACTURER,
    .iProduct = STRID_PRODUCT,
    .iSerialNumber = STRID_SERIAL,
    .bNumConfigurations = 1,
};

const uint8_t *tud_descriptor_device_cb(void) {
    return (const uint8_t *)&usbd_desc_device;
}

//--------------------------------------------------------------------+
// Configuration Descriptor
//--------------------------------------------------------------------+
#define USBD_MAX_POWER_MA (250)

#define USBD_DESC_LEN (TUD_CONFIG_DESC_LEN \
    + TUD_AUDIO_SPEAKER_STEREO_FB_DESC_LEN \
    + TUD_CDC_DESC_LEN \
    + TUD_RPI_RESET_DESC_LEN \
    + TUD_HID_DESC_LEN)

// HID consumer-control report descriptor -- single 16-bit usage field, no
// report ID (TinyUSB's stock consumer-control template). Covers play/pause
// (0x00CD), scan next (0x00B5) and scan previous (0x00B6), which is what
// T2/T3 (bead pico-link-47z.2/.3) will send. T1 sends nothing; this bead is
// descriptors/enumeration only.
static const uint8_t desc_hid_report[] = {
    TUD_HID_REPORT_DESC_CONSUMER()
};

// Sized by the initializer list, not by USBD_DESC_LEN directly: this way a
// mismatch between USBD_DESC_LEN's macro arithmetic and what the
// TUD_*_DESCRIPTOR macros actually expand to is a compile-time array-size
// mismatch (via the _Static_assert below) instead of a silent
// implicit-zero-fill truncation that would only surface as a host-side
// enumeration failure.
static const uint8_t usbd_desc_cfg[] = {
    // Config number, interface count, string index, total length, attribute, power in mA
    TUD_CONFIG_DESCRIPTOR(1, ITF_NUM_TOTAL, STRID_LANGID, USBD_DESC_LEN, 0x00, USBD_MAX_POWER_MA),

    // Interface number, string index, bytes/sample, bits/sample, EP out, EP out size, EP feedback, feedback EP size
    // 16-bit stereo @ up to 48kHz: TUD_AUDIO_EP_SIZE rounds up for rate
    // variance so the endpoint can carry a burst without dropping frames.
    //
    // Feedback EP size: high-speed devices use 4-byte 16.16 fixed-point;
    // full-speed devices must use 3-byte 10.14 (USB 2.0 Sec.5.12.4.2) -- macOS
    // (AppleUSBAudio) rejects the whole alternate setting if a full-speed
    // feedback endpoint claims 4 bytes, per TinyUSB's own compatibility
    // matrix (audio_device.c:1200-1214, OSX only in the 3-byte rows) and its
    // uac2_speaker_fb example (usb_descriptors.c:162, "OS X needs 3 bytes
    // feedback endpoint on FS"). RP2350 has no high-speed PHY so
    // TUD_OPT_HIGH_SPEED is always false here, but this is written
    // speed-conditional to document why, and to match upstream's shape.
    // Paired with CFG_TUD_AUDIO_ENABLE_FEEDBACK_FORMAT_CORRECTION in
    // tusb_config.h, which must be 1 for this 3-byte size to actually carry
    // 10.14-converted values -- see bead pico-link-icb.
    TUD_AUDIO_SPEAKER_STEREO_FB_DESCRIPTOR(ITF_NUM_AUDIO_CONTROL, STRID_AUDIO,
        CFG_TUD_AUDIO_FUNC_1_N_BYTES_PER_SAMPLE_RX, CFG_TUD_AUDIO_FUNC_1_RESOLUTION_RX,
        EPNUM_AUDIO_OUT, CFG_TUD_AUDIO_FUNC_1_EP_OUT_SZ_MAX, EPNUM_AUDIO_FB,
        (TUD_OPT_HIGH_SPEED ? 4 : 3)),

    // CDC: interface number, string index, EP notif addr & size, EP data out/in addr & size.
    TUD_CDC_DESCRIPTOR(ITF_NUM_CDC, STRID_CDC, EPNUM_CDC_NOTIF, 8, EPNUM_CDC_OUT, EPNUM_CDC_IN, 64),

    // Driverless-reset vendor interface -- keeps `picotool reboot -f -u`
    // working (see reset_interface.c, still linked via pico_enable_stdio_usb).
    // FROZEN at ITF_NUM_RESET == 4. Nothing may be inserted between this
    // entry and the HID entry below -- see usb_descriptors.h's comment on
    // ITF_NUM_RESET for why.
    // NOTE: TUD_RPI_RESET_DESCRIPTOR's own expansion (usb_descriptors.h)
    // already ends in a trailing comma, so no comma is added here.
    TUD_RPI_RESET_DESCRIPTOR(ITF_NUM_RESET, STRID_RESET)

    // HID consumer-control (media keys) -- MUST stay last, after RESET.
    // Interface number, string index, protocol, report descriptor len, EP In addr, size, polling interval (ms).
    TUD_HID_DESCRIPTOR(ITF_NUM_HID, STRID_HID, HID_ITF_PROTOCOL_NONE, sizeof(desc_hid_report), EPNUM_HID_IN, 8, 10)
};

_Static_assert(sizeof(usbd_desc_cfg) == USBD_DESC_LEN,
    "usbd_desc_cfg's actual byte count does not match USBD_DESC_LEN's macro arithmetic");

const uint8_t *tud_descriptor_configuration_cb(uint8_t index) {
    (void)index;
    return usbd_desc_cfg;
}

//--------------------------------------------------------------------+
// String Descriptors
//--------------------------------------------------------------------+
static char usbd_serial_str[PICO_UNIQUE_BOARD_ID_SIZE_BYTES * 2 + 1];

static const char *const usbd_desc_str[STRID_COUNT] = {
    [STRID_MANUFACTURER] = "Pico Link",
    [STRID_PRODUCT] = "Pico Link Audio Dongle",
    [STRID_SERIAL] = usbd_serial_str,
    [STRID_AUDIO] = "Pico Link Speaker",
    [STRID_CDC] = "Pico Link Console",
    [STRID_RESET] = "Reset",
    [STRID_HID] = "Pico Link Media Keys",
};

const uint16_t *tud_descriptor_string_cb(uint8_t index, uint16_t langid) {
    (void)langid;

#define USBD_DESC_STR_MAX (32)
    static uint16_t desc_str[USBD_DESC_STR_MAX];

    if (!usbd_serial_str[0]) {
        pico_get_unique_board_id_string(usbd_serial_str, sizeof(usbd_serial_str));
    }

    uint8_t len;
    if (index == STRID_LANGID) {
        desc_str[1] = 0x0409; // English
        len = 1;
    } else {
        if (index >= STRID_COUNT || usbd_desc_str[index] == NULL) {
            return NULL;
        }
        const char *str = usbd_desc_str[index];
        for (len = 0; len < USBD_DESC_STR_MAX - 1 && str[len]; ++len) {
            desc_str[1 + len] = (uint16_t)str[len];
        }
    }

    desc_str[0] = (uint16_t)((TUSB_DESC_STRING << 8) | (2 * len + 2));
    return desc_str;
}

//--------------------------------------------------------------------+
// HID class callbacks
//--------------------------------------------------------------------+
// T1 (bead pico-link-47z.1) only: descriptors and enumeration. No reports
// are ever sent from this bead -- that is T2/T3 (media_keys.c and the
// AVRCP passthrough handler).

uint8_t const *tud_hid_descriptor_report_cb(uint8_t instance) {
    (void)instance;
    return desc_hid_report;
}

// Host GET_REPORT (e.g. polling current key state): we are output-only, so
// report nothing.
uint16_t tud_hid_get_report_cb(uint8_t instance, uint8_t report_id, hid_report_type_t report_type,
                                uint8_t *buffer, uint16_t reqlen) {
    (void)instance;
    (void)report_id;
    (void)report_type;
    (void)buffer;
    (void)reqlen;
    return 0;
}

// Host SET_REPORT (e.g. output/feature reports): consumer control has none
// we accept; no-op.
void tud_hid_set_report_cb(uint8_t instance, uint8_t report_id, hid_report_type_t report_type,
                            uint8_t const *buffer, uint16_t bufsize) {
    (void)instance;
    (void)report_id;
    (void)report_type;
    (void)buffer;
    (void)bufsize;
}
