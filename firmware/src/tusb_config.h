// Pico Link firmware -- M3: TinyUSB device stack configuration.
//
// This project now owns TinyUSB's descriptors and config directly (see
// CMakeLists.txt: linking `tinyusb_device` sets LIB_TINYUSB_DEVICE=1, which
// compiles OUT pico_stdio_usb's own default tusb_config.h and descriptor
// generation -- pico_stdio_usb's stdio_usb.c and reset_interface.c stay
// linked in for the console I/O glue and the reset vendor-interface driver,
// but we drive tud_init()/tud_task() ourselves from main.c now).
//
// PROVENANCE: the shape of this file (which knobs exist, in what order)
// follows lib/tinyusb/examples/device/uac2_speaker_fb/src/tusb_config.h
// (MIT, Copyright 2020 Ha Thach / Jerzy Kasenberg) for the audio class
// knobs, with CDC added back in (CFG_TUD_CDC=1) since that example is
// audio-only. No USBPods code.

#ifndef PICO_LINK_TUSB_CONFIG_H
#define PICO_LINK_TUSB_CONFIG_H

// Bead pico-link-l60: this file REPLACES pico_stdio_usb's own
// pico_stdio_usb/include/tusb_config.h, which is the ONLY place
// PICO_STDIO_USB_* defaults (PICO_STDIO_USB_RESET_INTERFACE_SUPPORT_*,
// PICO_STDIO_USB_RESET_RESET_TO_FLASH_DELAY_MS, etc.) come from -- that
// header's own #include "pico/stdio_usb.h" is what a stock build relies on,
// and losing it here is what silently compiled the BOOTSEL/FLASH reset
// branches out of the build (see usb_reset.c/.h and
// .planning/decisions -- .planning/design/2026-08-29-picotool-reset-composite.md
// for the full mechanism). pico/stdio_usb.h defines only PICO_STDIO_USB_*
// macros and prototypes -- no CFG_TUD_* -- so it cannot fight this file's
// own class config below, and every default in it is #ifndef-guarded, so
// this project's own -D overrides (CMakeLists.txt) still win.
#include "pico/stdio_usb.h"

#include "usb_descriptors.h"

#ifdef __cplusplus
extern "C" {
#endif

//--------------------------------------------------------------------+
// Board / rhport
//--------------------------------------------------------------------+
#ifndef BOARD_TUD_RHPORT
#define BOARD_TUD_RHPORT 0
#endif
#ifndef BOARD_TUD_MAX_SPEED
#define BOARD_TUD_MAX_SPEED OPT_MODE_DEFAULT_SPEED
#endif

//--------------------------------------------------------------------+
// Common
//--------------------------------------------------------------------+
#ifndef CFG_TUSB_MCU
#error CFG_TUSB_MCU must be defined (should come from linking tinyusb_device -- see CMakeLists.txt)
#endif

#ifndef CFG_TUSB_OS
#define CFG_TUSB_OS OPT_OS_NONE
#endif

#ifndef CFG_TUSB_DEBUG
#define CFG_TUSB_DEBUG 0
#endif

#define CFG_TUD_ENABLED 1
#define CFG_TUD_MAX_SPEED BOARD_TUD_MAX_SPEED

#ifndef CFG_TUSB_MEM_SECTION
#define CFG_TUSB_MEM_SECTION
#endif
#ifndef CFG_TUSB_MEM_ALIGN
#define CFG_TUSB_MEM_ALIGN __attribute__((aligned(4)))
#endif

#ifndef CFG_TUD_ENDPOINT0_SIZE
#define CFG_TUD_ENDPOINT0_SIZE 64
#endif

//--------------------------------------------------------------------+
// Class enables
//--------------------------------------------------------------------+
#define CFG_TUD_CDC    1
#define CFG_TUD_MSC    0
#define CFG_TUD_HID    0
#define CFG_TUD_MIDI   0
#define CFG_TUD_AUDIO  1
// Vendor class is NOT used for the reset interface -- that is a hand-rolled
// usbd_class_driver_t (pico-sdk's reset_interface.c), registered via
// usbd_app_driver_get_cb, independent of CFG_TUD_VENDOR.
#define CFG_TUD_VENDOR 0

// CDC FIFO sizes -- 256B gives the console some slack under audio-streaming
// load without costing meaningful RAM (RP2350B has 520KB SRAM).
#define CFG_TUD_CDC_RX_BUFSIZE 256
#define CFG_TUD_CDC_TX_BUFSIZE 256

//--------------------------------------------------------------------+
// Audio class (speaker, stereo, 16-bit, async + explicit feedback)
//--------------------------------------------------------------------+
#define CFG_TUD_AUDIO_FUNC_1_DESC_LEN TUD_AUDIO_SPEAKER_STEREO_FB_DESC_LEN

// M3 scope boundary: a single fixed format/rate is enough to prove
// enumeration and a clean, measurably-correct stream. Multi-rate switching
// and clock-domain matching against Bluetooth/LDAC are M4+ concerns.
#define CFG_TUD_AUDIO_FUNC_1_MAX_SAMPLE_RATE 48000
#define CFG_TUD_AUDIO_FUNC_1_N_CHANNELS_RX 2
#define CFG_TUD_AUDIO_FUNC_1_N_BYTES_PER_SAMPLE_RX 2
#define CFG_TUD_AUDIO_FUNC_1_RESOLUTION_RX 16

// CFG_TUD_AUDIO_ENABLE_FEEDBACK_FORMAT_CORRECTION is NOT the OS-guessing quirk
// (quirk_os_guessing.c upstream, which needs a BOS descriptor to host-sniff
// the OS and would collide with reset_interface.c's BOS ownership when
// PICO_STDIO_USB_RESET_INTERFACE_SUPPORT_MS_OS_20_DESCRIPTOR is set -- it is
// not, here, so that collision doesn't even apply). That reasoning is correct
// about the *quirk* and was wrongly used to justify disabling this flag too.
// This flag is a plain compile-time switch inside audiod_fb_send
// (audio_device.c:1187-1214): it converts the feedback value to 10.14 and
// sends 3 bytes instead of 4. It touches no BOS descriptor and nothing
// reset_interface.c owns.
//
// Full-speed macOS REQUIRES the 3-byte 10.14 feedback format (USB 2.0
// Sec.5.12.4.2) -- TinyUSB's own compatibility matrix at
// audio_device.c:1200-1214 lists OSX only in the packetSize==3 rows, never in
// the packetSize==4 (16.16) rows we were shipping. Confirmed empirically on
// this device: with this flag at 0 (16.16/4 bytes, the Windows/Linux row),
// macOS silently discarded the whole alt setting 1 that carries the feedback
// endpoint and SET_INTERFACE(1, 1) was never issued -- see bead pico-link-icb.
//
// The trade this makes: TinyUSB's own uac2_speaker_fb example (see
// usb_descriptors.c:162, "OS X needs 3 bytes feedback endpoint on FS") only
// sends 3 bytes when CFG_QUIRK_OS_GUESSING detects OSX at runtime, and keeps
// sending 4 bytes to Windows (whose UAC2 driver has a documented bug
// requiring 16.16 -- audio_device.h:501-502). We have no Windows test rig and
// macOS is the MVP host, so we hardcode 3 bytes unconditionally rather than
// wire up the quirk sniffer. This is a known, deliberate Windows-compatibility
// regression. The durable fix, if Windows support is ever needed, is exactly
// CFG_QUIRK_OS_GUESSING -- an isolated swap in tud_descriptor_configuration_cb
// that picks the descriptor variant per detected host, not a reason to revert
// this flag.
#define CFG_TUD_AUDIO_ENABLE_FEEDBACK_FORMAT_CORRECTION 1

#define CFG_TUD_AUDIO_ENABLE_EP_OUT 1
#define CFG_TUD_AUDIO_FUNC_1_EP_OUT_SZ_MAX \
    TUD_AUDIO_EP_SIZE(CFG_TUD_AUDIO_FUNC_1_MAX_SAMPLE_RATE, CFG_TUD_AUDIO_FUNC_1_N_BYTES_PER_SAMPLE_RX, CFG_TUD_AUDIO_FUNC_1_N_CHANNELS_RX)
// Full-speed only device (RP2350's USB controller has no high-speed PHY) --
// 4x the max packet size gives ~4ms of slack between audio_task() drains.
#define CFG_TUD_AUDIO_FUNC_1_EP_OUT_SW_BUF_SZ (4 * CFG_TUD_AUDIO_FUNC_1_EP_OUT_SZ_MAX)

#define CFG_TUD_AUDIO_ENABLE_FEEDBACK_EP 1

// Number of Standard AS Interface Descriptors (one alt-streaming interface
// here: alt 0 = idle/zero-bandwidth, alt 1 = streaming).
#define CFG_TUD_AUDIO_FUNC_1_N_AS_INT 1

#define CFG_TUD_AUDIO_FUNC_1_CTRL_BUF_SZ 64

// Bead pico-link-2ap.6: EXPERIMENTAL, default OFF. When 1, sdk-patches/
// 04-tinyusb-audio-iso-out-isr.patch wires audiod's ISO-OUT re-arm
// (usbd_edpt_xfer on audio->ep_out) into TRUE USB ISR context via a new
// xfer_isr class-driver hook, instead of task context via tud_task()'s
// queued DCD_EVENT_XFER_COMPLETE (the stock 0.18.0 path, unchanged when
// this is 0). Backports the mechanism of upstream TinyUSB PR #3150 ("Move
// ISO transfers into xfer_isr", landed in 0.19.0) without the surrounding
// 0.18->0.19 refactor. Motivation: our re-arm's phase within the USB frame
// is currently set by pl_usb_pump_worker_irq's free-running 1ms timer, not
// the bus -- a candidate mechanism for the pico-link-2ap halving. See
// firmware/sdk-patches/README.md and bead pico-link-2ap.6.
//
// DEFAULT ON as of bead pico-link-2ap.6 (2026-09-01): this is the fix for the
// pico-link-2ap halving, verified on hardware over ~960s of continuous
// streaming across two independent soaks (786 clean one-second intervals,
// minimum packets-per-SOF 0.9706 and 0.9990, nothing below 0.95, against a
// pre-fix control that sat at 0.4989-0.5018). Set to 0 to fall back to the
// stock 0.18.0 task-context re-arm, which reintroduces the halving.
#ifndef PL_USB_ISO_XFER_ISR
#define PL_USB_ISO_XFER_ISR 1
#endif

#ifdef __cplusplus
}
#endif

#endif // PICO_LINK_TUSB_CONFIG_H
