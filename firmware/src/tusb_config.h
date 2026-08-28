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

// DIAGNOSTIC ONLY -- bead pico-link-icb (macOS never enters the audio
// streaming alt-setting; is SET_INTERFACE even arriving?). Level 2 turns
// on usbd.c's TU_LOG_USBD() calls in process_control_request(), which log
// every standard/class control request by name -- exactly the visibility
// needed to see whether SET_INTERFACE (or anything else) for the audio
// streaming interface reaches the device at all. Checked the noise budget
// before flipping this on: audio_device.c has zero TU_LOG_DRV call sites
// (grep-verified) so nothing logs per-ISO-packet, and dcd_rp2040.c has
// exactly one TU_LOG call, at init only -- so this does not reintroduce
// the >1ms-per-tick risk pico-link-tfj just fixed. Routed through
// pl_tusb_trace_printf (usb_pump.c), NOT pl_log, because TU_LOG fires from
// inside tud_task() while the 0xC0 worker already holds pl_usb_mutex --
// see usb_pump.h's doc comment on pl_tusb_trace_printf for why pl_log
// would silently drop these. STRIP before this instrumentation is
// considered permanent; not meant to reach main as-is.
#ifndef CFG_TUSB_DEBUG
#define CFG_TUSB_DEBUG 2
#endif
#define CFG_TUSB_DEBUG_PRINTF pl_tusb_trace_printf

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

// The full-speed-OSX 3-byte-feedback quirk (quirk_os_guessing.c upstream)
// is deliberately NOT wired in here: it requires a BOS descriptor solely to
// host-sniff the OS, its own reference tusb_config.h ships it DISABLED by
// default even when the sniffer is compiled in, and current macOS does not
// need it. Skipping it keeps the composite's device/BOS descriptor surface
// simple, which matters because reset_interface.c also wants ownership of
// tud_descriptor_bos_cb when PICO_STDIO_USB_RESET_INTERFACE_SUPPORT_MS_OS_20_DESCRIPTOR
// is set (it is not, here) -- two BOS-descriptor owners would conflict.
#define CFG_TUD_AUDIO_ENABLE_FEEDBACK_FORMAT_CORRECTION 0

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

#ifdef __cplusplus
}
#endif

#endif // PICO_LINK_TUSB_CONFIG_H
