// Pico Link firmware -- M3: TinyUSB composite descriptor callbacks.
//
// PROVENANCE: structure (device descriptor, config descriptor assembly,
// string descriptor callback) follows the pattern common to ALL of
// pico-sdk's vendored TinyUSB device examples (MIT) -- most directly
// lib/tinyusb/examples/device/cdc_uac2/src/usb_descriptors.c (MIT,
// Copyright 2020 Jerzy Kasenberg / 2022 Angel Molina) for the CDC+audio
// composite shape, with the speaker's own descriptor macro swapped for the
// adaptive, no-feedback-EP one from uac2_speaker_fb, trimmed per bead
// pico-link-ycm2.7.2 (see usb_descriptors.h's provenance note). The reset
// interface's serial-number-from-flash-ID trick is pico-sdk's own
// (stdio_usb_descriptors.c). No USBPods code.
//
// This file is the ONE thing this project's USB device topology is: audio
// control + audio streaming (speaker, adaptive, no feedback EP) + CDC
// (console) + vendor reset. See usb_descriptors.h for interface/string
// index layout.

#include <string.h>

#include "tusb.h"

#include "pico/stdio_usb/reset_interface.h"
#include "pico/unique_id.h"

#include "usb_descriptors.h"

//--------------------------------------------------------------------+
// Endpoint numbers
//--------------------------------------------------------------------+
// EP1 OUT carries audio data. Bead pico-link-ycm2.7.2: the feedback IN
// endpoint that used to share EP1's number (0x81) is gone -- no feedback EP
// at all, see usb_descriptors.h's macro doc comment. 0x81 stays unused
// rather than being renumbered onto something else, per the design's
// "do NOT renumber other EPs" instruction. EP2 IN is CDC's notification
// endpoint. EP3 is CDC's data OUT/IN pair.
#define EPNUM_AUDIO_OUT (0x01)
#define EPNUM_CDC_NOTIF (0x82)
#define EPNUM_CDC_OUT   (0x03)
#define EPNUM_CDC_IN    (0x83)
// HID consumer-control (media keys), interrupt IN only -- see bead
// pico-link-47z.1 / .planning/design/2026-09-02-media-keys.md.
#define EPNUM_HID_IN    (0x84)
// UAC2 feature-unit status interrupt endpoint (AC interface, not HID) --
// bead pico-link-2ue (VT4a). EP5 IN: the next free address after HID's EP4.
#define EPNUM_AUDIO_INT (0x85)

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
    // Bumped 0x0200 -> 0x0210 -- bead pico-link-ycm2.3 (design comment on
    // pico-link-ycm2, section 1): a host only asks GET_DESCRIPTOR(BOS) for
    // bcdUSB >= 0x0201, and BOS is how Windows discovers the MS OS 2.0
    // descriptor set below (no BOS -> no WinUSB on iface 4/6).
    .bcdUSB = 0x0210,
    .bDeviceClass = TUSB_CLASS_MISC,
    .bDeviceSubClass = MISC_SUBCLASS_COMMON,
    .bDeviceProtocol = MISC_PROTOCOL_IAD,
    .bMaxPacketSize0 = CFG_TUD_ENDPOINT0_SIZE,
    .idVendor = USBD_VID,
    .idProduct = USBD_PID,
    // Bumped 0x0100 -> 0x0101 for the HID interface addition (bead
    // pico-link-47z.1), then 0x0101 -> 0x0102 for the Pico Link Config
    // vendor interface (bead pico-link-ryw.12.5, design comment on
    // pico-link-ryw.12: "bump bcdDevice ... so hosts drop cached
    // descriptors"), then 0x0102 -> 0x0103 for the BOS/MS OS 2.0 addition
    // (bead pico-link-ycm2.3, same reasoning: a cached pre-BOS devnode must
    // not be reused), then 0x0103 -> 0x0105 for the adaptive/no-feedback-EP
    // descriptor shape (bead pico-link-ycm2.7.2). 0x0104 is deliberately
    // SKIPPED: Windows holds an errored REV_0104 devnode cached from the
    // ycm2.5 alt-enum test (and 0x01F0 from the ycm2.4 diagnostic build),
    // and bumping past it avoids any chance of Windows reusing that
    // confirmed-bad cache entry. PID deliberately NOT bumped -- see
    // .planning/design/2026-09-02-media-keys.md section 2.5.
    .bcdDevice = 0x0105,
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
    + TUD_AUDIO_SPEAKER_STEREO_ADAPTIVE_DESC_LEN \
    + TUD_CDC_DESC_LEN \
    + TUD_RPI_RESET_DESC_LEN \
    + TUD_HID_DESC_LEN \
    + TUD_PL_CONFIG_DESC_LEN)

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

    // Interface number, string index, bytes/sample, bits/sample, EP out, EP out size, AC interrupt EP
    // 16-bit stereo @ up to 48kHz: TUD_AUDIO_EP_SIZE rounds up for rate
    // variance so the endpoint can carry a burst without dropping frames.
    //
    // Bead pico-link-ycm2.7.2: no feedback endpoint, no feedback-format
    // byte-count question -- see usb_descriptors.h's macro doc comment for
    // why (the 3-byte/4-byte macOS-vs-Windows disagreement this used to
    // document disappears structurally once there is no feedback EP).
    TUD_AUDIO_SPEAKER_STEREO_ADAPTIVE_DESCRIPTOR(ITF_NUM_AUDIO_CONTROL, STRID_AUDIO,
        CFG_TUD_AUDIO_FUNC_1_N_BYTES_PER_SAMPLE_RX, CFG_TUD_AUDIO_FUNC_1_RESOLUTION_RX,
        EPNUM_AUDIO_OUT, CFG_TUD_AUDIO_FUNC_1_EP_OUT_SZ_MAX, EPNUM_AUDIO_INT),

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

    // HID consumer-control (media keys) -- after RESET, before CONFIG.
    // Interface number, string index, protocol, report descriptor len, EP In addr, size, polling interval (ms).
    // NOTE: TUD_HID_DESCRIPTOR's own expansion ends in a trailing comma
    // (same as TUD_RPI_RESET_DESCRIPTOR above), so none is added here.
    TUD_HID_DESCRIPTOR(ITF_NUM_HID, STRID_HID, HID_ITF_PROTOCOL_NONE, sizeof(desc_hid_report), EPNUM_HID_IN, 8, 10),

    // Pico Link Config -- bead pico-link-ryw.12.5: zero-endpoint vendor
    // interface for host->device preset import (control transfers only,
    // see usb_config_itf.h). MUST stay last -- appended after every other
    // interface, same append-only rule ITF_NUM_HID followed for RESET.
    TUD_PL_CONFIG_DESCRIPTOR(ITF_NUM_CONFIG, STRID_CONFIG)
};

_Static_assert(sizeof(usbd_desc_cfg) == USBD_DESC_LEN,
    "usbd_desc_cfg's actual byte count does not match USBD_DESC_LEN's macro arithmetic");

const uint8_t *tud_descriptor_configuration_cb(uint8_t index) {
    (void)index;
    return usbd_desc_cfg;
}

//--------------------------------------------------------------------+
// BOS Descriptor + MS OS 2.0 descriptor set -- bead pico-link-ycm2.3
//--------------------------------------------------------------------+
// DESIGN (comment on pico-link-ycm2, section 1, "Windows WebUSB (step 3)"):
// Windows never asks for MS OS 2.0 without a BOS descriptor advertising the
// MS OS 2.0 platform capability; without it iface 4 (reset) and iface 6
// (Pico Link Config, the web companion's target) enumerate driverless on
// Windows and the web companion cannot claim iface 6 via WebUSB. No WebUSB
// platform capability is exposed -- Chrome does not need it to open a
// device (landing-page URL only, no longer surfaced by Chrome), and adding
// one would need a GET_URL vendor-code handler for zero function.
//
// PROVENANCE: byte-for-byte shape follows TinyUSB's own
// examples/device/webusb_serial/src/usb_descriptors.c (MIT, Copyright 2019
// Ha Thach) -- one configuration subset, but TWO function subsets instead
// of its one (we have two control-only vendor interfaces that want WinUSB,
// iface 4 and iface 6, not one), each with its own WINUSB compatible ID and
// its own fresh DeviceInterfaceGUIDs registry property (type REG_MULTI_SZ,
// double-NUL terminated).
//
// This vendor code (0x01) is used ONLY for the MS OS 2.0 descriptor-set
// request below -- it lives in the VENDOR bmRequestType namespace, which
// usb_reset.c and usb_config_itf.c both deliberately avoid (they dispatch
// on type==CLASS so a genuine VENDOR request never reaches them; see their
// own provenance notes), so there is no collision.
#define PL_MS_VENDOR_CODE 0x01

#define PL_MS_OS_20_COMPATID_LEN      0x0014 // header(4) + "WINUSB"(8) + sub-compatible(8)
#define PL_MS_OS_20_REGPROP_LEN       0x0084 // header(8) + name "DeviceInterfaceGUIDs\0"(42) + datalen(2) + data(80)
#define PL_MS_OS_20_FUNC_SUBSET_LEN   (0x0008 + PL_MS_OS_20_COMPATID_LEN + PL_MS_OS_20_REGPROP_LEN) // 160
#define PL_MS_OS_20_CONFIG_SUBSET_LEN (0x0008 + 2 * PL_MS_OS_20_FUNC_SUBSET_LEN) // 328
#define PL_MS_OS_20_DESC_LEN          (0x000A + PL_MS_OS_20_CONFIG_SUBSET_LEN)   // 338

// Sized by the initializer list (same discipline as usbd_desc_cfg above):
// a mismatch between this macro arithmetic and the literal byte sequence
// below is a compile-time array-size mismatch via the _Static_assert, not
// a silent truncation that only shows up as Windows rejecting the set.
static const uint8_t desc_ms_os_20[] = {
    // Set header: length, type, dwWindowsVersion, total length of the set
    U16_TO_U8S_LE(0x000A), U16_TO_U8S_LE(MS_OS_20_SET_HEADER_DESCRIPTOR),
    U32_TO_U8S_LE(0x06030000), U16_TO_U8S_LE(PL_MS_OS_20_DESC_LEN),

    // Configuration subset header: length, type, configuration INDEX
    // (despite the field's name -- we have one configuration, index 0),
    // reserved, subset length (everything after this header)
    U16_TO_U8S_LE(0x0008), U16_TO_U8S_LE(MS_OS_20_SUBSET_HEADER_CONFIGURATION),
    0x00, 0x00, U16_TO_U8S_LE(PL_MS_OS_20_CONFIG_SUBSET_LEN),

    // --- Function subset: ITF_NUM_CONFIG (iface 6, "Pico Link Config" --
    // the web companion's WebUSB/WinUSB target) ---
    U16_TO_U8S_LE(0x0008), U16_TO_U8S_LE(MS_OS_20_SUBSET_HEADER_FUNCTION),
    ITF_NUM_CONFIG, 0x00, U16_TO_U8S_LE(PL_MS_OS_20_FUNC_SUBSET_LEN),
    // Compatible ID descriptor: length, type, compatible ID, sub-compatible ID
    U16_TO_U8S_LE(PL_MS_OS_20_COMPATID_LEN), U16_TO_U8S_LE(MS_OS_20_FEATURE_COMPATBLE_ID),
    'W', 'I', 'N', 'U', 'S', 'B', 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    // Registry property descriptor: length, type, wPropertyDataType
    // (0x0007 == REG_MULTI_SZ), wPropertyNameLength + PropertyName
    // "DeviceInterfaceGUIDs\0" (UTF-16LE)
    U16_TO_U8S_LE(PL_MS_OS_20_REGPROP_LEN), U16_TO_U8S_LE(MS_OS_20_FEATURE_REG_PROPERTY),
    U16_TO_U8S_LE(0x0007), U16_TO_U8S_LE(0x002A),
    'D', 0x00, 'e', 0x00, 'v', 0x00, 'i', 0x00, 'c', 0x00, 'e', 0x00, 'I', 0x00, 'n', 0x00, 't', 0x00, 'e', 0x00,
    'r', 0x00, 'f', 0x00, 'a', 0x00, 'c', 0x00, 'e', 0x00, 'G', 0x00, 'U', 0x00, 'I', 0x00, 'D', 0x00, 's', 0x00, 0x00, 0x00,
    // wPropertyDataLength + PropertyData: "{GUID}" (UTF-16LE), double-NUL
    // terminated (REG_MULTI_SZ). Freshly generated for this interface --
    // see this file's module header.
    U16_TO_U8S_LE(0x0050),
    '{', 0x00, '8', 0x00, '1', 0x00, 'B', 0x00, '8', 0x00, '0', 0x00, '1', 0x00, 'B', 0x00, 'B', 0x00, '-', 0x00,
    '2', 0x00, 'E', 0x00, 'B', 0x00, '2', 0x00, '-', 0x00, '4', 0x00, '6', 0x00, 'D', 0x00, '9', 0x00, '-', 0x00,
    'A', 0x00, '5', 0x00, 'A', 0x00, 'D', 0x00, '-', 0x00, 'F', 0x00, 'E', 0x00, '8', 0x00, '5', 0x00, '4', 0x00,
    '3', 0x00, '4', 0x00, '9', 0x00, '6', 0x00, '9', 0x00, '1', 0x00, 'B', 0x00, '}', 0x00, 0x00, 0x00, 0x00, 0x00,

    // --- Function subset: ITF_NUM_RESET (iface 4, driverless-reset --
    // recommended target for picotool/pyusb tools on Windows per the
    // design) ---
    U16_TO_U8S_LE(0x0008), U16_TO_U8S_LE(MS_OS_20_SUBSET_HEADER_FUNCTION),
    ITF_NUM_RESET, 0x00, U16_TO_U8S_LE(PL_MS_OS_20_FUNC_SUBSET_LEN),
    U16_TO_U8S_LE(PL_MS_OS_20_COMPATID_LEN), U16_TO_U8S_LE(MS_OS_20_FEATURE_COMPATBLE_ID),
    'W', 'I', 'N', 'U', 'S', 'B', 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    U16_TO_U8S_LE(PL_MS_OS_20_REGPROP_LEN), U16_TO_U8S_LE(MS_OS_20_FEATURE_REG_PROPERTY),
    U16_TO_U8S_LE(0x0007), U16_TO_U8S_LE(0x002A),
    'D', 0x00, 'e', 0x00, 'v', 0x00, 'i', 0x00, 'c', 0x00, 'e', 0x00, 'I', 0x00, 'n', 0x00, 't', 0x00, 'e', 0x00,
    'r', 0x00, 'f', 0x00, 'a', 0x00, 'c', 0x00, 'e', 0x00, 'G', 0x00, 'U', 0x00, 'I', 0x00, 'D', 0x00, 's', 0x00, 0x00, 0x00,
    U16_TO_U8S_LE(0x0050),
    // A DIFFERENT GUID from iface 6's above -- each WinUSB-bearing
    // interface must publish its own DeviceInterfaceGUIDs value.
    '{', 0x00, '3', 0x00, 'A', 0x00, '8', 0x00, 'D', 0x00, '1', 0x00, 'E', 0x00, '9', 0x00, '3', 0x00, '-', 0x00,
    '1', 0x00, 'D', 0x00, 'A', 0x00, '5', 0x00, '-', 0x00, '4', 0x00, 'E', 0x00, 'A', 0x00, 'B', 0x00, '-', 0x00,
    '8', 0x00, '8', 0x00, 'D', 0x00, '4', 0x00, '-', 0x00, '9', 0x00, '5', 0x00, '1', 0x00, '7', 0x00, '1', 0x00,
    '9', 0x00, '1', 0x00, 'C', 0x00, 'E', 0x00, '6', 0x00, 'D', 0x00, '4', 0x00, '}', 0x00, 0x00, 0x00, 0x00, 0x00,
};

_Static_assert(sizeof(desc_ms_os_20) == PL_MS_OS_20_DESC_LEN,
    "desc_ms_os_20's actual byte count does not match PL_MS_OS_20_DESC_LEN's macro arithmetic");

// USB 2.0 Extension Device Capability (bLength, bDescriptorType,
// bDevCapabilityType, bmAttributes[4]) -- explicit no-LPM (bmAttributes=0),
// expected whenever bcdUSB >= 0x0201 (there is no TUD_BOS_* template for
// this one, unlike WebUSB/MS-OS-2.0, so it is hand-written per the USB 2.0
// spec sec. 9.6.2.1 / Link Power Management ECN).
#define PL_BOS_USB2_EXT_LEN 7

#define USBD_BOS_TOTAL_LEN (TUD_BOS_DESC_LEN + PL_BOS_USB2_EXT_LEN + TUD_BOS_MICROSOFT_OS_DESC_LEN)

static const uint8_t desc_bos[] = {
    // Total length, number of device capabilities
    TUD_BOS_DESCRIPTOR(USBD_BOS_TOTAL_LEN, 2),

    // USB 2.0 Extension Capability -- bmAttributes = 0 (no LPM)
    PL_BOS_USB2_EXT_LEN, TUSB_DESC_DEVICE_CAPABILITY, DEVICE_CAPABILITY_USB20_EXTENSION, 0x00, 0x00, 0x00, 0x00,

    // Microsoft OS 2.0 Platform Capability: descriptor-set total length, vendor code
    TUD_BOS_MS_OS_20_DESCRIPTOR(PL_MS_OS_20_DESC_LEN, PL_MS_VENDOR_CODE),
};

_Static_assert(sizeof(desc_bos) == USBD_BOS_TOTAL_LEN,
    "desc_bos's actual byte count does not match USBD_BOS_TOTAL_LEN's macro arithmetic");

const uint8_t *tud_descriptor_bos_cb(void) {
    return desc_bos;
}

// Strong tud_vendor_control_xfer_cb -- CFG_TUD_VENDOR stays 0 (tusb_config.h);
// this is reached via usbd.c's process_control_request, which routes EVERY
// bmRequestType.type==VENDOR control request here regardless of CFG_TUD_VENDOR
// (that macro only gates the bulk vendor class driver, not this callback).
// The only VENDOR request this device understands is the MS OS 2.0
// descriptor-set read (bRequest == PL_MS_VENDOR_CODE, wIndex == 7, the fixed
// index the MS OS 2.0 spec reserves for this request); everything else
// stalls. usb_reset.c (iface 4) and usb_config_itf.c (iface 6) both dispatch
// on bmRequestType.type == CLASS, never VENDOR, so there is no collision
// with their existing requests.
bool tud_vendor_control_xfer_cb(uint8_t rhport, uint8_t stage, tusb_control_request_t const *request) {
    if (stage != CONTROL_STAGE_SETUP) {
        return true;
    }

    if (request->bmRequestType_bit.type != TUSB_REQ_TYPE_VENDOR) {
        return false;
    }

    if (request->bRequest == PL_MS_VENDOR_CODE && request->wIndex == 0x0007) {
        return tud_control_xfer(rhport, request, (void *)(uintptr_t)desc_ms_os_20, PL_MS_OS_20_DESC_LEN);
    }

    return false;
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
    [STRID_CONFIG] = "Pico Link Config",
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
