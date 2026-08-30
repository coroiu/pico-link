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
    f.write_text(src.replace(STOCK, PATCHED, 1))
    print(f"APPLIED: 01-tinyusb-rp2040-double-arm -> {f}")
else:
    sys.exit("FAIL: rp2040_usb.c matches neither the stock nor the patched form.\n"
             "Unknown SDK version -- inspect it by hand before proceeding.")
PY
