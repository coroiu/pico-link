#!/bin/bash
# Idempotent applier for the vendored pico-sdk patches (bead pico-link-06m).
#
# Uses exact-string replacement rather than patch(1) on purpose: it is
# idempotent, it can distinguish "already applied" from "unknown SDK version"
# (patch(1) reports both as a failed hunk), and it needs no fuzz. The .patch
# files in firmware/sdk-patches/ remain the human-readable source of truth.
set -euo pipefail
SDK="${PICO_SDK_PATH:-$HOME/.pico-sdk/sdk/2.1.1}"
python3 - "$SDK" <<'PY'
import sys, pathlib
sdk = pathlib.Path(sys.argv[1])
f = sdk / "lib/tinyusb/src/portable/raspberrypi/rp2040/rp2040_usb.c"
if not f.exists():
    sys.exit(f"FAIL: not found: {f}\nIs PICO_SDK_PATH correct? (got {sdk})")
src = f.read_text()

STOCK = '        panic("ep %02X was already available", ep->ep_addr);'
MARK  = "pl_ep_double_arm_count"
PATCHED = """        // pico-link (bead pico-link-06m): was
        //   panic("ep %02X was already available", ep->ep_addr);
        // panic() is a pico-sdk function -- NDEBUG does NOT elide it -- called
        // from an IRQ path and ending in __breakpoint(), i.e. a hard lockup
        // needing a physical BOOTSEL press. USBPods comments this out; we
        // COUNT it instead so the arm/complete race stays measurable.
        // Defined in firmware/src/usb_pump.c. See firmware/sdk-patches/README.md.
        { extern volatile unsigned int pl_ep_double_arm_count[32];
          pl_ep_double_arm_count[((ep->ep_addr & 0x0fu) << 1) | ((ep->ep_addr & 0x80u) ? 1u : 0u)]++; }"""

if MARK in src:
    print("ok: 01-tinyusb-rp2040-double-arm already applied")
elif STOCK in src:
    src = src.replace(STOCK, PATCHED, 1)
    f.write_text(src)
    print(f"APPLIED: 01-tinyusb-rp2040-double-arm -> {f}")
else:
    sys.exit("FAIL: rp2040_usb.c matches neither the stock nor the patched form.\n"
             "Unknown SDK version -- inspect it by hand before proceeding.")

# --- 02: stale completion on an inactive endpoint ------------------------
# hw_endpoint_xfer_continue() panics if a buffer-status completion arrives for
# an endpoint with ep->active == 0 -- exactly what an abrupt alt0 teardown
# produces. Same arm/complete disorder as 01, different fatal exit.
#
# Recovery is NOT "count and fall through": _hw_endpoint_xfer_sync() would run
# against torn-down state, and returning true would hand the stack a bogus
# dcd_event_xfer_complete() with a stale xferred_len. The correct action is to
# treat it as a stale completion -- release the lock taken on entry (the panic
# never had to, we do) and return false, so hw_handle_buff_status() neither
# completes nor resets the transfer. The buf_status bit is already cleared by
# the caller before this call (dcd_rp2040.c:157), so returning changes nothing
# about interrupt acknowledgement and cannot cause a storm.
STOCK2 = '  // Part way through a transfer\n  if (!ep->active) {\n    panic("Can\'t continue xfer on inactive ep %02X", ep->ep_addr);\n  }'
MARK2 = "pl_ep_inactive_xfer_count"
PATCHED2 = """  // Part way through a transfer
  if (!ep->active) {
    // pico-link (bead pico-link-06m): was
    //   panic("Can't continue xfer on inactive ep %02X", ep->ep_addr);
    // A stale completion for an endpoint torn down mid-transfer (abrupt alt0).
    // Counting and RETURNING is the correct recovery -- falling through would
    // sync against torn-down state, and returning true would deliver a bogus
    // xfer_complete with a stale xferred_len. Must release the lock taken on
    // entry above, which the panic never needed to. See sdk-patches/README.md.
    { extern volatile unsigned int pl_ep_inactive_xfer_count[32];
      pl_ep_inactive_xfer_count[((ep->ep_addr & 0x0fu) << 1) | ((ep->ep_addr & 0x80u) ? 1u : 0u)]++; }
    hw_endpoint_lock_update(ep, -1);
    return false;
  }"""

src = f.read_text()
if MARK2 in src:
    print("ok: 02-tinyusb-rp2040-inactive-xfer already applied")
elif STOCK2 in src:
    f.write_text(src.replace(STOCK2, PATCHED2, 1))
    print(f"APPLIED: 02-tinyusb-rp2040-inactive-xfer -> {f}")
else:
    sys.exit("FAIL: hw_endpoint_xfer_continue matches neither stock nor patched form.")

# --- 03: sample the ISO-OUT AVAIL bit in TRUE ISR context (bead pico-link-wbq) ---
# usbd.c's DCD_EVENT_SOF case in dcd_event_handler() runs in real ISR context,
# but only calls the app's tud_sof_cb() by RE-QUEUING an event that is later
# drained by tud_task() -- on this firmware, from inside the 0xC0
# pl_usb_pump_worker_irq. Sampling from there measures AVAIL up to ~1ms after
# the real start of frame, at a phase set by our own 1ms timer, not the bus.
# This patch calls pl_usb_sof_isr_sample() (firmware/src/usb_pump.c) directly
# from inside the ISR case, before any re-queuing happens.
f3 = sdk / "lib/tinyusb/src/device/usbd.c"
if not f3.exists():
    sys.exit(f"FAIL: not found: {f3}\nIs PICO_SDK_PATH correct? (got {sdk})")
src3 = f3.read_text()

STOCK3 = """    case DCD_EVENT_SOF:
      // SOF driver handler in ISR context
      for (uint8_t i = 0; i < TOTAL_DRIVER_COUNT; i++) {"""
MARK3 = "pl_usb_sof_isr_sample"
PATCHED3 = """    case DCD_EVENT_SOF:
      // SOF driver handler in ISR context
      // pico-link (bead pico-link-wbq): sample the raw ISO-OUT AVAIL bit
      // HERE, in true ISR context, before this event is (maybe) re-queued
      // for tud_task() below. Defined in firmware/src/usb_pump.c. See
      // firmware/sdk-patches/README.md.
      { extern void pl_usb_sof_isr_sample(uint32_t frame_count);
        pl_usb_sof_isr_sample(event->sof.frame_count); }
      for (uint8_t i = 0; i < TOTAL_DRIVER_COUNT; i++) {"""

if MARK3 in src3:
    print("ok: 03-tinyusb-usbd-sof-isr-sample already applied")
elif STOCK3 in src3:
    f3.write_text(src3.replace(STOCK3, PATCHED3, 1))
    print(f"APPLIED: 03-tinyusb-usbd-sof-isr-sample -> {f3}")
else:
    sys.exit("FAIL: usbd.c's DCD_EVENT_SOF case matches neither stock nor patched form.\n"
             "Unknown SDK version -- inspect it by hand before proceeding.")
PY
