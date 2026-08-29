// Pico Link firmware -- bead pico-link-l60: this project's own driverless-
// reset (picotool BOOTSEL) TinyUSB class driver.
//
// WHY THIS FILE EXISTS instead of pico-sdk's own
// src/rp2_common/pico_stdio_usb/reset_interface.c: that file gates BOTH its
// BOOTSEL and FLASH-boot request handlers on
// PICO_STDIO_USB_RESET_INTERFACE_SUPPORT_RESET_TO_BOOTSEL /
// _RESET_TO_FLASH_BOOT, which are `#ifndef`-defaulted only in
// pico/stdio_usb.h -- a header reset_interface.c never includes itself (it
// reaches config via tusb.h -> tusb_config.h). Stock pico_stdio_usb builds
// get the defaults for free because the SDK's OWN
// pico_stdio_usb/include/tusb_config.h includes pico/stdio_usb.h. This
// project replaced that file with our own firmware/src/tusb_config.h (see
// its module doc), which includes only usb_descriptors.h -- so those two
// macros were silently undefined, both `#if` branches compiled out, and
// `picotool reboot -f -u` stalled EP0 forever. Full mechanism:
// .planning/design/2026-08-29-picotool-reset-composite.md.
//
// Rather than leave reset_interface.c linked in against a config header it
// structurally cannot see (the blurred seam that produced this bug), this
// project now owns the reset class driver directly, unconditionally, with
// both branches always compiled in -- see CMakeLists.txt, where
// PICO_STDIO_USB_ENABLE_RESET_VIA_VENDOR_INTERFACE is set to 0 to free the
// reset_interface.c translation unit down to nothing (including the strong
// usbd_app_driver_get_cb symbol this file now provides instead).
//
// PROVENANCE: resetd_open/resetd_control_xfer_cb/resetd_reset/resetd_xfer_cb
// and the _resetd_driver table below are ported near-verbatim from
// pico-sdk's src/rp2_common/pico_stdio_usb/reset_interface.c (BSD-3-Clause,
// Copyright 2021 Raspberry Pi (Trading) Ltd), with the `#if
// PICO_STDIO_USB_RESET_INTERFACE_SUPPORT_*` gates removed (both branches are
// now unconditional), the activity-LED path dropped (this board doesn't
// wire one), and PICO_STDIO_USB_RESET_BOOTSEL_INTERFACE_DISABLE_MASK
// replaced with the literal 0u it always defaulted to. Two `pl_log_locked` lines
// added for bead pico-link-l60's one-shot hardware verification -- see
// usb_reset.c.
#ifndef PICO_LINK_USB_RESET_H
#define PICO_LINK_USB_RESET_H

#include "device/usbd_pvt.h"

// The TinyUSB class driver table for the driverless-reset vendor interface.
// Registered via usbd_app_driver_get_cb (usb_reset.c) -- nothing outside
// usb_reset.c needs to touch this directly.

#endif // PICO_LINK_USB_RESET_H
