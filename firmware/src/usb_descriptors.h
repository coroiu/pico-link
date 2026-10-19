// Pico Link firmware -- M3: TinyUSB composite descriptor (CDC console + UAC2
// speaker, adaptive, no feedback EP + pico-sdk's driverless-reset vendor
// interface).
//
// PROVENANCE: this file combines pieces derived from THREE MIT/BSD-3-Clause
// pico-sdk / TinyUSB sources (all already vendored in this tree, see
// $PICO_SDK_PATH/lib/tinyusb -- nothing GPL, nothing from USBPods):
//
//   1. The UAC2_ENTITY_*, TUD_AUDIO_SPEAKER_STEREO_ADAPTIVE_DESC_LEN and
//      TUD_AUDIO_SPEAKER_STEREO_ADAPTIVE_DESCRIPTOR() macros are derived
//      from lib/tinyusb/examples/device/uac2_speaker_fb/src/usb_descriptors.h
//      (MIT, Copyright 2023 HiFiPhile), originally the ASYNC-mode,
//      explicit-feedback-endpoint speaker -- the one place this bead flagged
//      as having a high floor for getting subtly wrong, so it was copied
//      rather than reinvented. Trimmed to speaker-only, single fixed PCM
//      format (no mic path, no multi-format switching -- M3's scope
//      boundary is enumeration and a clean stream, not format negotiation).
//      Bead pico-link-ycm2.7.2 (design comment on pico-link-ycm2.7) then
//      dropped the feedback endpoint entirely and switched the ISO OUT
//      endpoint attribute ASYNCHRONOUS -> ADAPTIVE: no feedback endpoint
//      means no feedback-format byte-count disagreement between hosts
//      (sec 1/4 of that design).
//   2. TUD_RPI_RESET_DESC_LEN and TUD_RPI_RESET_DESCRIPTOR() are derived
//      from pico-sdk's own
//      src/rp2_common/pico_stdio_usb/stdio_usb_descriptors.c (MIT,
//      Copyright 2020 Raspberry Pi (Trading) Ltd / Damien P. George). That
//      file's own descriptor generation is compiled OUT once this project
//      links `tinyusb_device` (LIB_TINYUSB_DEVICE=1, see CMakeLists.txt) --
//      the interface bytes are reproduced here so `picotool reboot -f -u`
//      keeps working against a descriptor WE now own. The vendor class
//      driver itself (reset_interface.c, registered via
//      usbd_app_driver_get_cb) is untouched pico-sdk code, still linked in
//      by pico_enable_stdio_usb -- only its descriptor bytes are
//      reproduced here.
//   3. CDC's own descriptor comes from TinyUSB's own TUD_CDC_DESCRIPTOR()
//      macro (tusb.h) -- no local reproduction needed, it is a stable
//      public macro.
//
// Endpoint map (see usb_descriptors.c for the numeric assignment): EP1 OUT
// carries audio data. Bead pico-link-ycm2.7.2 removed the feedback IN
// endpoint that used to share EP1's number (0x81 stays unused, not
// renumbered onto anything else). EP2 IN is CDC's notification endpoint;
// EP3 is CDC's data OUT/IN pair. The vendor reset interface has no
// endpoints (control-transfer only).

#ifndef PICO_LINK_USB_DESCRIPTORS_H
#define PICO_LINK_USB_DESCRIPTORS_H

// Unit numbers are arbitrarily selected, matching the upstream example.
#define UAC2_ENTITY_CLOCK           0x04
#define UAC2_ENTITY_INPUT_TERMINAL  0x01
#define UAC2_ENTITY_FEATURE_UNIT    0x02
#define UAC2_ENTITY_OUTPUT_TERMINAL 0x03

// ITF_NUM_RESET MUST STAY AT INDEX 4 -- FROZEN. The hand-run press-free
// BOOTSEL snippet (memory note "bootsel-over-cdc-command"; also
// picotool's `-f` path) does `claim_interface(d, 4)` then
// `ctrl_transfer(0x21, 0x01, wIndex=4, ...)` with the literal 4 baked in,
// not looked up by class code. If RESET's index ever moves, that transfer
// silently retargets whatever interface now sits at 4 -- on HID that is
// class request 0x01 = SET_REPORT, which TinyUSB will accept or stall with
// NO error saying "wrong interface". The board just stops rebooting on
// command, with no diagnostic, on exactly the recovery path you need when
// something else is already broken. See
// .planning/design/2026-09-02-media-keys.md section 2.4. Any new interface
// (HID included) MUST be appended AFTER ITF_NUM_RESET, never inserted
// before it.
// ITF_NUM_CONFIG (bead pico-link-ryw.12.5, design comment on
// pico-link-ryw.12): "Pico Link Config", a zero-endpoint vendor interface
// for host->device preset import (see usb_config_itf.h). Appended AFTER
// HID, same "append after RESET, never insert before it" rule that
// governed HID's own placement -- ITF_NUM_RESET's frozen index is what
// matters, not what comes after it.
enum {
    ITF_NUM_AUDIO_CONTROL = 0,
    ITF_NUM_AUDIO_STREAMING,
    ITF_NUM_CDC,
    ITF_NUM_CDC_DATA,
    ITF_NUM_RESET,   // FROZEN at 4 -- see comment above.
    ITF_NUM_HID,     // Consumer-control (media keys). Appended after RESET.
    ITF_NUM_CONFIG,  // Pico Link Config (preset import). Appended after HID.
    ITF_NUM_TOTAL
};

enum {
    STRID_LANGID = 0,
    STRID_MANUFACTURER,
    STRID_PRODUCT,
    STRID_SERIAL,
    STRID_AUDIO,
    STRID_CDC,
    STRID_RESET,
    STRID_HID,
    STRID_CONFIG,
    STRID_COUNT
};

// --- (1) Derived from uac2_speaker_fb/src/usb_descriptors.h (MIT) ---
// Speaker-only (no mic path), ADAPTIVE isochronous OUT with NO feedback
// endpoint -- bead pico-link-ycm2.7.2 (design comment on pico-link-ycm2.7,
// sec 1/3/4). The original async+explicit-feedback shape (the MIT
// uac2_speaker_fb example this is still derived from) forced a 3-byte vs
// 4-byte feedback-format disagreement between macOS and Windows with no
// correct answer for both (pico-link-icb/ycm2.4/ycm2.5); dropping the
// feedback endpoint removes the disagreement structurally -- one descriptor
// set for every host. Rate matching moved to drain_servo.c servoing the
// A2DP credit clock (pico-link-ycm2.7.1), not a USB-side feedback loop.
// Bead pico-link-2ue (VT4a): + TUD_AUDIO_DESC_STD_AC_INT_EP_LEN for the AC
// interrupt IN endpoint (see CFG_TUD_AUDIO_ENABLE_INTERRUPT_EP in
// tusb_config.h). 7 bytes, TinyUSB's own fixed-size macro
// (usbd.h:418-420) -- no local reproduction needed, unlike the two
// PROVENANCE items at the top of this file.
#define TUD_AUDIO_SPEAKER_STEREO_ADAPTIVE_DESC_LEN (TUD_AUDIO_DESC_IAD_LEN\
    + TUD_AUDIO_DESC_STD_AC_LEN\
    + TUD_AUDIO_DESC_CS_AC_LEN\
    + TUD_AUDIO_DESC_CLK_SRC_LEN\
    + TUD_AUDIO_DESC_INPUT_TERM_LEN\
    + TUD_AUDIO_DESC_OUTPUT_TERM_LEN\
    + TUD_AUDIO_DESC_FEATURE_UNIT_TWO_CHANNEL_LEN\
    + TUD_AUDIO_DESC_STD_AC_INT_EP_LEN\
    + TUD_AUDIO_DESC_STD_AS_INT_LEN\
    + TUD_AUDIO_DESC_STD_AS_INT_LEN\
    + TUD_AUDIO_DESC_CS_AS_INT_LEN\
    + TUD_AUDIO_DESC_TYPE_I_FORMAT_LEN\
    + TUD_AUDIO_DESC_STD_AS_ISO_EP_LEN\
    + TUD_AUDIO_DESC_CS_AS_ISO_EP_LEN)

// Bead pico-link-2ue (VT4a): _epint added. _nEPs on the AC interface goes
// 0x00 -> 0x01 (audiod_open() requires exactly this: bNumEndpoints==1 iff
// CFG_TUD_AUDIO_ENABLE_INTERRUPT_EP, audio_device.c:1017-1019/1649), and
// the interrupt EP descriptor is appended right after the last AC unit
// descriptor (Feature Unit), before the AS interfaces -- same position
// TinyUSB's own uac2_headset example uses (the only upstream example that
// enables this EP).
//
// Bead pico-link-ycm2.7.2: alt 1 bNumEndpoints 2 -> 1 (no feedback EP);
// _epfb/_epfbsize params dropped.
#define TUD_AUDIO_SPEAKER_STEREO_ADAPTIVE_DESCRIPTOR(_itfnum, _stridx, _nBytesPerSample, _nBitsUsedPerSample, _epout, _epoutsize, _epint) \
    /* Standard Interface Association Descriptor (IAD) */\
    TUD_AUDIO_DESC_IAD(/*_firstitf*/ _itfnum, /*_nitfs*/ 0x02, /*_stridx*/ 0x00),\
    /* Standard AC Interface Descriptor(4.7.1) */\
    TUD_AUDIO_DESC_STD_AC(/*_itfnum*/ _itfnum, /*_nEPs*/ 0x01, /*_stridx*/ _stridx),\
    /* Class-Specific AC Interface Header Descriptor(4.7.2) */\
    TUD_AUDIO_DESC_CS_AC(/*_bcdADC*/ 0x0200, /*_category*/ AUDIO_FUNC_DESKTOP_SPEAKER, /*_totallen*/ TUD_AUDIO_DESC_CLK_SRC_LEN+TUD_AUDIO_DESC_INPUT_TERM_LEN+TUD_AUDIO_DESC_OUTPUT_TERM_LEN+TUD_AUDIO_DESC_FEATURE_UNIT_TWO_CHANNEL_LEN, /*_ctrl*/ AUDIO_CS_AS_INTERFACE_CTRL_LATENCY_POS),\
    /* Clock Source Descriptor(4.7.2.1) */\
    TUD_AUDIO_DESC_CLK_SRC(/*_clkid*/ UAC2_ENTITY_CLOCK, /*_attr*/ AUDIO_CLOCK_SOURCE_ATT_INT_PRO_CLK, /*_ctrl*/ (AUDIO_CTRL_RW << AUDIO_CLOCK_SOURCE_CTRL_CLK_FRQ_POS), /*_assocTerm*/ UAC2_ENTITY_INPUT_TERMINAL,  /*_stridx*/ 0x00),\
    /* Input Terminal Descriptor(4.7.2.4) */\
    TUD_AUDIO_DESC_INPUT_TERM(/*_termid*/ UAC2_ENTITY_INPUT_TERMINAL, /*_termtype*/ AUDIO_TERM_TYPE_USB_STREAMING, /*_assocTerm*/ 0x00, /*_clkid*/ UAC2_ENTITY_CLOCK, /*_nchannelslogical*/ 0x02, /*_channelcfg*/ AUDIO_CHANNEL_CONFIG_NON_PREDEFINED, /*_idxchannelnames*/ 0x00, /*_ctrl*/ 0 * (AUDIO_CTRL_R << AUDIO_IN_TERM_CTRL_CONNECTOR_POS), /*_stridx*/ 0x00),\
    /* Output Terminal Descriptor(4.7.2.5) */\
    TUD_AUDIO_DESC_OUTPUT_TERM(/*_termid*/ UAC2_ENTITY_OUTPUT_TERMINAL, /*_termtype*/ AUDIO_TERM_TYPE_OUT_DESKTOP_SPEAKER, /*_assocTerm*/ UAC2_ENTITY_INPUT_TERMINAL, /*_srcid*/ UAC2_ENTITY_FEATURE_UNIT, /*_clkid*/ UAC2_ENTITY_CLOCK, /*_ctrl*/ 0x0000, /*_stridx*/ 0x00),\
    /* Feature Unit Descriptor(4.7.2.8) */\
    TUD_AUDIO_DESC_FEATURE_UNIT_TWO_CHANNEL(/*_unitid*/ UAC2_ENTITY_FEATURE_UNIT, /*_srcid*/ UAC2_ENTITY_INPUT_TERMINAL, /*_ctrlch0master*/ AUDIO_CTRL_RW << AUDIO_FEATURE_UNIT_CTRL_MUTE_POS | AUDIO_CTRL_RW << AUDIO_FEATURE_UNIT_CTRL_VOLUME_POS, /*_ctrlch1*/ AUDIO_CTRL_RW << AUDIO_FEATURE_UNIT_CTRL_MUTE_POS | AUDIO_CTRL_RW << AUDIO_FEATURE_UNIT_CTRL_VOLUME_POS, /*_ctrlch2*/ AUDIO_CTRL_RW << AUDIO_FEATURE_UNIT_CTRL_MUTE_POS | AUDIO_CTRL_RW << AUDIO_FEATURE_UNIT_CTRL_VOLUME_POS,/*_stridx*/ 0x00),\
    /* Standard AC Interrupt Endpoint Descriptor(4.8.2.1) -- bead pico-link-2ue (VT4a) */\
    TUD_AUDIO_DESC_STD_AC_INT_EP(/*_ep*/ _epint, /*_interval*/ 0x01),\
    /* Standard AS Interface Descriptor(4.9.1) */\
    /* Interface 1, Alternate 0 - default alternate setting with 0 bandwidth */\
    TUD_AUDIO_DESC_STD_AS_INT(/*_itfnum*/ (uint8_t)((_itfnum) + 1), /*_altset*/ 0x00, /*_nEPs*/ 0x00, /*_stridx*/ 0x00),\
    /* Standard AS Interface Descriptor(4.9.1) */\
    /* Interface 1, Alternate 1 - alternate interface for data streaming */\
    /* Bead pico-link-ycm2.7.2: _nEPs 0x02 -> 0x01, no feedback EP. */\
    TUD_AUDIO_DESC_STD_AS_INT(/*_itfnum*/ (uint8_t)((_itfnum) + 1), /*_altset*/ 0x01, /*_nEPs*/ 0x01, /*_stridx*/ 0x00),\
    /* Class-Specific AS Interface Descriptor(4.9.2) */\
    TUD_AUDIO_DESC_CS_AS_INT(/*_termid*/ UAC2_ENTITY_INPUT_TERMINAL, /*_ctrl*/ AUDIO_CTRL_NONE, /*_formattype*/ AUDIO_FORMAT_TYPE_I, /*_formats*/ AUDIO_DATA_FORMAT_TYPE_I_PCM, /*_nchannelsphysical*/ 0x02, /*_channelcfg*/ AUDIO_CHANNEL_CONFIG_NON_PREDEFINED, /*_stridx*/ 0x00),\
    /* Type I Format Type Descriptor(2.3.1.6 - Audio Formats) */\
    TUD_AUDIO_DESC_TYPE_I_FORMAT(_nBytesPerSample, _nBitsUsedPerSample),\
    /* Standard AS Isochronous Audio Data Endpoint Descriptor(4.10.1.1) */\
    /* Bead pico-link-ycm2.7.2: ASYNCHRONOUS -> ADAPTIVE (design sec 1). */\
    TUD_AUDIO_DESC_STD_AS_ISO_EP(/*_ep*/ _epout, /*_attr*/ (uint8_t) ((uint8_t)TUSB_XFER_ISOCHRONOUS | (uint8_t)TUSB_ISO_EP_ATT_ADAPTIVE | (uint8_t)TUSB_ISO_EP_ATT_DATA), /*_maxEPsize*/ _epoutsize, /*_interval*/ 0x01),\
    /* Class-Specific AS Isochronous Audio Data Endpoint Descriptor(4.10.1.2) */\
    TUD_AUDIO_DESC_CS_AS_ISO_EP(/*_attr*/ AUDIO_CS_AS_ISO_DATA_EP_ATT_NON_MAX_PACKETS_OK, /*_ctrl*/ AUDIO_CTRL_NONE, /*_lockdelayunit*/ AUDIO_CS_AS_ISO_DATA_EP_LOCK_DELAY_UNIT_MILLISEC, /*_lockdelay*/ 0x0001)

// --- (2) Derived from pico_stdio_usb/stdio_usb_descriptors.c (MIT) ---
// Just the interface descriptor bytes for the driverless-reset vendor
// interface -- the class DRIVER (reset_interface.c) is untouched pico-sdk
// code and stays linked in via pico_enable_stdio_usb.
#define TUD_RPI_RESET_DESC_LEN 9
#define TUD_RPI_RESET_DESCRIPTOR(_itfnum, _stridx) \
    9, TUSB_DESC_INTERFACE, _itfnum, 0, 0, TUSB_CLASS_VENDOR_SPECIFIC, RESET_INTERFACE_SUBCLASS, RESET_INTERFACE_PROTOCOL, _stridx,

// --- (3) Pico Link Config -- bead pico-link-ryw.12.5 ---
// Same shape as TUD_RPI_RESET_DESCRIPTOR above (one plain interface
// descriptor, zero endpoints, control-transfer only) but its own
// subclass/protocol pair so it can never be mistaken for the reset
// interface by a host that dispatches on those fields instead of the
// interface number. See usb_config_itf.h for the class driver and wire
// protocol this descriptor announces.
#define PL_CONFIG_INTERFACE_SUBCLASS 0x01
#define PL_CONFIG_INTERFACE_PROTOCOL 0x01

#define TUD_PL_CONFIG_DESC_LEN 9
#define TUD_PL_CONFIG_DESCRIPTOR(_itfnum, _stridx) \
    9, TUSB_DESC_INTERFACE, _itfnum, 0, 0, TUSB_CLASS_VENDOR_SPECIFIC, PL_CONFIG_INTERFACE_SUBCLASS, PL_CONFIG_INTERFACE_PROTOCOL, _stridx,

#endif // PICO_LINK_USB_DESCRIPTORS_H
