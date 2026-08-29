// Pico Link firmware -- M3: TinyUSB composite descriptor (CDC console + UAC2
// speaker-with-feedback + pico-sdk's driverless-reset vendor interface).
//
// PROVENANCE: this file combines pieces derived from THREE MIT/BSD-3-Clause
// pico-sdk / TinyUSB sources (all already vendored in this tree, see
// $PICO_SDK_PATH/lib/tinyusb -- nothing GPL, nothing from USBPods):
//
//   1. The UAC2_ENTITY_*, TUD_AUDIO_SPEAKER_STEREO_FB_DESC_LEN and
//      TUD_AUDIO_SPEAKER_STEREO_FB_DESCRIPTOR() macros are derived from
//      lib/tinyusb/examples/device/uac2_speaker_fb/src/usb_descriptors.h
//      (MIT, Copyright 2023 HiFiPhile). That example is the ASYNC-mode,
//      explicit-feedback-endpoint speaker -- the one place this bead flagged
//      as having a high floor for getting subtly wrong, so it is copied
//      rather than reinvented. Trimmed to speaker-only, single fixed PCM
//      format (no mic path, no multi-format switching -- M3's scope
//      boundary is enumeration and a clean stream, not format negotiation).
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
// Endpoint map (see usb_descriptors.c for the numeric assignment): EP1 is
// shared OUT (audio data) / IN (audio feedback) -- RP2040/RP2350's USB
// controller allows a same-numbered EP pair with opposite directions (see
// tusb_mcu.h: OPT_MCU_RP2040 does NOT define
// TUD_ENDPOINT_ONE_DIRECTION_ONLY), so this is the same pattern TinyUSB's
// own RP2040 example CMakeLists selects by default. EP2 IN is CDC's
// notification endpoint; EP3 is CDC's data OUT/IN pair. The vendor reset
// interface has no endpoints (control-transfer only).

#ifndef PICO_LINK_USB_DESCRIPTORS_H
#define PICO_LINK_USB_DESCRIPTORS_H

// Unit numbers are arbitrarily selected, matching the upstream example.
#define UAC2_ENTITY_CLOCK           0x04
#define UAC2_ENTITY_INPUT_TERMINAL  0x01
#define UAC2_ENTITY_FEATURE_UNIT    0x02
#define UAC2_ENTITY_OUTPUT_TERMINAL 0x03

enum {
    ITF_NUM_AUDIO_CONTROL = 0,
    ITF_NUM_AUDIO_STREAMING,
    ITF_NUM_CDC,
    ITF_NUM_CDC_DATA,
    ITF_NUM_RESET,
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
    STRID_COUNT
};

// --- (1) Derived from uac2_speaker_fb/src/usb_descriptors.h (MIT) ---
// Speaker-only (no mic path), ASYNC isochronous OUT with an explicit
// feedback IN endpoint -- the correct pattern for a device whose sample
// clock is NOT derived from the USB host's SOF (ours eventually comes from
// the Bluetooth/LDAC side, out of scope for M3, but the endpoint topology
// has to be right from the start since it can't change without breaking
// host enumeration caches).
#define TUD_AUDIO_SPEAKER_STEREO_FB_DESC_LEN (TUD_AUDIO_DESC_IAD_LEN\
    + TUD_AUDIO_DESC_STD_AC_LEN\
    + TUD_AUDIO_DESC_CS_AC_LEN\
    + TUD_AUDIO_DESC_CLK_SRC_LEN\
    + TUD_AUDIO_DESC_INPUT_TERM_LEN\
    + TUD_AUDIO_DESC_OUTPUT_TERM_LEN\
    + TUD_AUDIO_DESC_FEATURE_UNIT_TWO_CHANNEL_LEN\
    + TUD_AUDIO_DESC_STD_AS_INT_LEN\
    + TUD_AUDIO_DESC_STD_AS_INT_LEN\
    + TUD_AUDIO_DESC_CS_AS_INT_LEN\
    + TUD_AUDIO_DESC_TYPE_I_FORMAT_LEN\
    + TUD_AUDIO_DESC_STD_AS_ISO_EP_LEN\
    + TUD_AUDIO_DESC_CS_AS_ISO_EP_LEN\
    + TUD_AUDIO_DESC_STD_AS_ISO_FB_EP_LEN)

#define TUD_AUDIO_SPEAKER_STEREO_FB_DESCRIPTOR(_itfnum, _stridx, _nBytesPerSample, _nBitsUsedPerSample, _epout, _epoutsize, _epfb, _epfbsize) \
    /* Standard Interface Association Descriptor (IAD) */\
    TUD_AUDIO_DESC_IAD(/*_firstitf*/ _itfnum, /*_nitfs*/ 0x02, /*_stridx*/ 0x00),\
    /* Standard AC Interface Descriptor(4.7.1) */\
    TUD_AUDIO_DESC_STD_AC(/*_itfnum*/ _itfnum, /*_nEPs*/ 0x00, /*_stridx*/ _stridx),\
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
    /* Standard AS Interface Descriptor(4.9.1) */\
    /* Interface 1, Alternate 0 - default alternate setting with 0 bandwidth */\
    TUD_AUDIO_DESC_STD_AS_INT(/*_itfnum*/ (uint8_t)((_itfnum) + 1), /*_altset*/ 0x00, /*_nEPs*/ 0x00, /*_stridx*/ 0x00),\
    /* Standard AS Interface Descriptor(4.9.1) */\
    /* Interface 1, Alternate 1 - alternate interface for data streaming */\
    TUD_AUDIO_DESC_STD_AS_INT(/*_itfnum*/ (uint8_t)((_itfnum) + 1), /*_altset*/ 0x01, /*_nEPs*/ 0x02, /*_stridx*/ 0x00),\
    /* Class-Specific AS Interface Descriptor(4.9.2) */\
    TUD_AUDIO_DESC_CS_AS_INT(/*_termid*/ UAC2_ENTITY_INPUT_TERMINAL, /*_ctrl*/ AUDIO_CTRL_NONE, /*_formattype*/ AUDIO_FORMAT_TYPE_I, /*_formats*/ AUDIO_DATA_FORMAT_TYPE_I_PCM, /*_nchannelsphysical*/ 0x02, /*_channelcfg*/ AUDIO_CHANNEL_CONFIG_NON_PREDEFINED, /*_stridx*/ 0x00),\
    /* Type I Format Type Descriptor(2.3.1.6 - Audio Formats) */\
    TUD_AUDIO_DESC_TYPE_I_FORMAT(_nBytesPerSample, _nBitsUsedPerSample),\
    /* Standard AS Isochronous Audio Data Endpoint Descriptor(4.10.1.1) */\
    TUD_AUDIO_DESC_STD_AS_ISO_EP(/*_ep*/ _epout, /*_attr*/ (uint8_t) ((uint8_t)TUSB_XFER_ISOCHRONOUS | (uint8_t)TUSB_ISO_EP_ATT_ASYNCHRONOUS | (uint8_t)TUSB_ISO_EP_ATT_DATA), /*_maxEPsize*/ _epoutsize, /*_interval*/ 0x01),\
    /* Class-Specific AS Isochronous Audio Data Endpoint Descriptor(4.10.1.2) */\
    TUD_AUDIO_DESC_CS_AS_ISO_EP(/*_attr*/ AUDIO_CS_AS_ISO_DATA_EP_ATT_NON_MAX_PACKETS_OK, /*_ctrl*/ AUDIO_CTRL_NONE, /*_lockdelayunit*/ AUDIO_CS_AS_ISO_DATA_EP_LOCK_DELAY_UNIT_MILLISEC, /*_lockdelay*/ 0x0001),\
    /* Standard AS Isochronous Feedback Endpoint Descriptor(4.10.2.1) */\
    TUD_AUDIO_DESC_STD_AS_ISO_FB_EP(/*_ep*/ _epfb, /*_epsize*/ _epfbsize, /*_interval*/ TUD_OPT_HIGH_SPEED ? 4 : 1)

// --- (2) Derived from pico_stdio_usb/stdio_usb_descriptors.c (MIT) ---
// Just the interface descriptor bytes for the driverless-reset vendor
// interface -- the class DRIVER (reset_interface.c) is untouched pico-sdk
// code and stays linked in via pico_enable_stdio_usb.
#define TUD_RPI_RESET_DESC_LEN 9
#define TUD_RPI_RESET_DESCRIPTOR(_itfnum, _stridx) \
    9, TUSB_DESC_INTERFACE, _itfnum, 0, 0, TUSB_CLASS_VENDOR_SPECIFIC, RESET_INTERFACE_SUBCLASS, RESET_INTERFACE_PROTOCOL, _stridx,

#endif // PICO_LINK_USB_DESCRIPTORS_H
