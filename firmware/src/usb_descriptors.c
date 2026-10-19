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
    // not be reused), then 0x0103 -> 0x0104 for the MS OS 2.0 alternate
    // enumeration support below (bead pico-link-ycm2.5: bAltEnumCode goes
    // from 0 to nonzero in the BOS capability, a cached devnode from
    // before that must not be reused either). PID deliberately NOT bumped
    // -- see .planning/design/2026-09-02-media-keys.md section 2.5.
    .bcdDevice = 0x0104,
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
        (TUD_OPT_HIGH_SPEED ? 4 : 3), EPNUM_AUDIO_INT),

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

// Windows-only variant of the config descriptor above -- bead
// pico-link-ycm2.5 (root cause proven on hardware in pico-link-ycm2.4):
// Windows' in-box usbaudio2.sys refuses to START a full-speed UAC2
// function whose feedback endpoint claims wMaxPacketSize 3 (an
// undocumented driver bug, not a USB-transport error -- see that bead's
// DESIGN comment for the decode of the Code 10 / 0xC0440025 status and
// the Microsoft driver source citation). macOS requires the opposite:
// it silently drops the whole alternate setting if that same endpoint
// claims 4 bytes (bead pico-link-icb). No single descriptor satisfies
// both hosts, so this is the SAME macro call as usbd_desc_cfg above with
// only _epfbsize changed from 3 to the literal 4 Windows needs --
// identical total length (TUD_AUDIO_DESC_STD_AS_ISO_FB_EP's size field is
// fixed-width regardless of the value it carries, so USBD_DESC_LEN is
// unchanged) -- served only to a host that opted in by sending
// MS_OS_20_SET_ALT_ENUMERATION (see tud_vendor_control_xfer_cb below);
// macOS and Linux never send that request and always get usbd_desc_cfg
// above, byte-for-byte unchanged.
static const uint8_t usbd_desc_cfg_win[] = {
    TUD_CONFIG_DESCRIPTOR(1, ITF_NUM_TOTAL, STRID_LANGID, USBD_DESC_LEN, 0x00, USBD_MAX_POWER_MA),

    TUD_AUDIO_SPEAKER_STEREO_FB_DESCRIPTOR(ITF_NUM_AUDIO_CONTROL, STRID_AUDIO,
        CFG_TUD_AUDIO_FUNC_1_N_BYTES_PER_SAMPLE_RX, CFG_TUD_AUDIO_FUNC_1_RESOLUTION_RX,
        EPNUM_AUDIO_OUT, CFG_TUD_AUDIO_FUNC_1_EP_OUT_SZ_MAX, EPNUM_AUDIO_FB,
        4, EPNUM_AUDIO_INT),

    TUD_CDC_DESCRIPTOR(ITF_NUM_CDC, STRID_CDC, EPNUM_CDC_NOTIF, 8, EPNUM_CDC_OUT, EPNUM_CDC_IN, 64),

    TUD_RPI_RESET_DESCRIPTOR(ITF_NUM_RESET, STRID_RESET)

    TUD_HID_DESCRIPTOR(ITF_NUM_HID, STRID_HID, HID_ITF_PROTOCOL_NONE, sizeof(desc_hid_report), EPNUM_HID_IN, 8, 10),

    TUD_PL_CONFIG_DESCRIPTOR(ITF_NUM_CONFIG, STRID_CONFIG)
};

_Static_assert(sizeof(usbd_desc_cfg_win) == USBD_DESC_LEN,
    "usbd_desc_cfg_win's actual byte count does not match USBD_DESC_LEN's macro arithmetic -- "
    "the two config descriptor variants must stay byte-length-identical");

// Set once by tud_vendor_control_xfer_cb's MS_OS_20_SET_ALT_ENUMERATION
// handler below, read here AND by usb_audio.c's
// tud_audio_feedback_format_correction_cb override (see
// pl_usb_win_alt_enum_active, usb_descriptors.h). Cleared ONLY by a
// power-on reset (a plain static initializer): see that handler's own
// comment for why a same-session USB bus reset must NOT clear it, and why
// "plug into a different host" is covered anyway.
static volatile bool s_win_alt_enum_active = false;

const uint8_t *tud_descriptor_configuration_cb(uint8_t index) {
    (void)index;
    return s_win_alt_enum_active ? usbd_desc_cfg_win : usbd_desc_cfg;
}

bool pl_usb_win_alt_enum_active(void) {
    return s_win_alt_enum_active;
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

// Bead pico-link-ycm2.5: nonzero bAltEnumCode in the MS OS 2.0 platform
// capability below (bos descriptor) is how a device OPTS IN to the MS OS
// 2.0 "alternate enumeration" mechanism -- the stock TUD_BOS_MS_OS_20_DESCRIPTOR
// macro (usbd.h:207-208) hardcodes this field to 0 (no alternate
// enumeration), so this project defines its own expansion below
// (PL_BOS_MS_OS_20_DESCRIPTOR) rather than patch the vendored macro.
// Value is otherwise arbitrary (any nonzero byte) -- it is not looked up
// against anything, it only gates whether Windows considers sending
// MS_OS_20_SET_ALT_ENUMERATION (tud_vendor_control_xfer_cb below) at all.
#define PL_MS_ALT_ENUM_CODE 0x01

// wIndex value this project's vendor code, 0x0008, means on a VENDOR
// control request: Microsoft's "Set Alternate Enumeration" command (the
// MS OS 2.0 spec's other defined use of bMS_VendorCode, alongside 0x0007
// for the descriptor-set read above). PROVENANCE CAVEAT: Microsoft's own
// MS_OS_2_0_desc.docx (gated behind a click-through license, not
// reproducible here) is the only normative source for this constant; no
// open-source USB stack this project checked (TinyUSB, Zephyr, libusbk)
// implements the DEVICE side of this request, only the same bare
// constant value -- real-world adoption of alternate enumeration is thin
// (see pico-link-ycm2's DESIGN comment, section 2). This project treats
// it as host-to-device with wLength 0 (a bare command, no data stage,
// ack'd with a status-stage ZLP via tud_control_status) and does not gate
// on wValue's documented bPageNumber -- this device has exactly one page.
#define PL_MS_OS_20_SET_ALT_ENUMERATION 0x0008

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

// Hand-written variant of TUD_BOS_MS_OS_20_DESCRIPTOR (usbd.h:207-208) --
// that stock macro's last field (bAltEnumCode) is hardcoded 0, so a
// device that wants alternate enumeration needs its own expansion of the
// same TUD_BOS_PLATFORM_DESCRIPTOR fields the stock macro uses: the
// MS OS 2.0 UUID, dwWindowsVersion, wMSOSDescriptorSetTotalLength,
// bMS_VendorCode, bAltEnumCode.
#define PL_BOS_MS_OS_20_DESCRIPTOR(_desc_set_len, _vendor_code, _alt_enum_code) \
    TUD_BOS_PLATFORM_DESCRIPTOR(TUD_BOS_MS_OS_20_UUID, U32_TO_U8S_LE(0x06030000), U16_TO_U8S_LE(_desc_set_len), _vendor_code, _alt_enum_code)

static const uint8_t desc_bos[] = {
    // Total length, number of device capabilities
    TUD_BOS_DESCRIPTOR(USBD_BOS_TOTAL_LEN, 2),

    // USB 2.0 Extension Capability -- bmAttributes = 0 (no LPM)
    PL_BOS_USB2_EXT_LEN, TUSB_DESC_DEVICE_CAPABILITY, DEVICE_CAPABILITY_USB20_EXTENSION, 0x00, 0x00, 0x00, 0x00,

    // Microsoft OS 2.0 Platform Capability: descriptor-set total length,
    // vendor code, alt-enum code (bead pico-link-ycm2.5: was
    // TUD_BOS_MS_OS_20_DESCRIPTOR's hardcoded 0, now PL_MS_ALT_ENUM_CODE)
    PL_BOS_MS_OS_20_DESCRIPTOR(PL_MS_OS_20_DESC_LEN, PL_MS_VENDOR_CODE, PL_MS_ALT_ENUM_CODE),
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

    // MS OS 2.0 "Set Alternate Enumeration" (bead pico-link-ycm2.5) -- see
    // PL_MS_OS_20_SET_ALT_ENUMERATION's own comment above for what this
    // constant is and the provenance caveat on its exact wire shape. No
    // data stage (wLength 0): ack immediately with a status-stage ZLP and
    // latch the flag tud_descriptor_configuration_cb and usb_audio.c's
    // tud_audio_feedback_format_correction_cb both read.
    if (request->bRequest == PL_MS_VENDOR_CODE && request->wIndex == PL_MS_OS_20_SET_ALT_ENUMERATION) {
        s_win_alt_enum_active = true;
        return tud_control_status(rhport, request);
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
