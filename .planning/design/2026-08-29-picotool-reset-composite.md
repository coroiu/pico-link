# picotool BOOTSEL reset on the audio-composite firmware

Bead: `pico-link-l60`. Author: Ada (architect), 2026-08-29. Verified independently
by the orchestrator before dispatch.

## Mechanism (certain, found offline — no hardware needed)

`resetd_control_xfer_cb`'s BOOTSEL branch is **compiled out of our build**. The reset
interface is declared, enumerated and bound; the device answers picotool's request
with a STALL.

1. `reset_interface.c:119` gates `RESET_REQUEST_BOOTSEL` on
   `PICO_STDIO_USB_RESET_INTERFACE_SUPPORT_RESET_TO_BOOTSEL`; `:139` gates
   `RESET_REQUEST_FLASH`; `:174` gates the 1200-baud path on
   `PICO_STDIO_USB_ENABLE_RESET_VIA_BAUD_RATE`.
2. Those macros are defaulted **only** in `pico/stdio_usb.h:99-107`.
3. `reset_interface.c` never includes `pico/stdio_usb.h`. It reaches config through
   `tusb.h` -> `tusb_config.h`.
4. A stock build gets the defaults because the SDK's own
   `pico_stdio_usb/include/tusb_config.h:30` does `#include "pico/stdio_usb.h"`.
   **We replaced that file** with `firmware/src/tusb_config.h`, which includes only
   `usb_descriptors.h`. That dropped include is the whole bug.
5. Proof in the shipped binary, not just in theory: the dependency file
   `firmware/build/CMakeFiles/pico_link.dir/.../reset_interface.c.o.d` lists
   `pico/stdio_usb/reset_interface.h` and **not** `pico/stdio_usb.h`.

Undefined macro => `#if` evaluates 0 => both branches vanish => the handler reduces
to an empty `if` and `return false` => usbd stalls EP0.

The same defect silently killed the **1200-baud** fallback. We currently have zero
working software reset paths — that is why every flash costs a physical replug.

## Cleared by reading (do not re-investigate)

- **Descriptor assembly is correct.** `usb_descriptors.c:92-126`: bNumInterfaces =
  `ITF_NUM_TOTAL` = 5; audio IAD covers itf 0-1, CDC's own IAD covers 2-3, reset is
  itf 4 with class 0xFF / sub 0x00 / proto 0x01 matching `pico/usb_reset_interface.h:17-19`.
  The `_Static_assert` at `:125` already proves the total-length arithmetic.
  `STRID_RESET`=6 < `STRID_COUNT`=7.
- **Driver registration is intact.** `usbd.c:489-491` calls `usbd_app_driver_get_cb`;
  `reset_interface.c:168` provides it; `usbd.c:323-324` places app drivers first.
  `CFG_TUD_VENDOR 0` is correct and irrelevant — no competing claimant.
- **BOS / MS-OS-2.0 collision does not apply** — but for a different reason than
  `tusb_config.h:95-104` claims. The upstream default is **1**
  (`stdio_usb.h:110-112`); it is invisible to us for the same include reason. Ours
  ships `bcdUSB 0x0200` and no BOS, which is fine on macOS (MS OS 2.0 is Windows-only).
  **Do not enable that macro** — it pulls in `CFG_TUD_VENDOR 1`, a BOS descriptor and
  `bcdUSB 0x0210`: a real macOS enumeration risk for zero MVP benefit.

## Ranked mechanisms

| # | Mechanism | Is a defect | Explains the error text |
|---|---|---|---|
| M1 | BOOTSEL branch compiled out | **certain** | ~50% |
| M2 | picotool never classifies PID 0x000C as reset-capable | unknown | ~45% |
| M3 | descriptor / driver dispatch | ruled out | ~5% |
| M4 | macOS libusb cannot claim itf 4 | unlikely | <5% |

M2 survives because picotool's message — "No accessible RP-series devices in BOOTSEL
mode were found" — is the wording of a *discovery* failure, not a transfer failure.
picotool's source is not on this machine; this was NOT resolved by guessing. The
telemetry in (A) discriminates M1 from M2 on the first flash.

Orchestrator step-0 finding: picotool v2.2.0 exposes `--vid/--pid`, but they are
**filters** (narrowing), not allow-list wideners. The PID change (D) is therefore
NOT applied — it has real debt (it tells every tool we are the stock stdio device
while shipping an audio function) and the telemetry may make it unnecessary.

## The change — one build

**(A) Own the reset class driver.** New `firmware/src/usb_reset.c` + `.h`.
- Set `PICO_STDIO_USB_ENABLE_RESET_VIA_VENDOR_INTERFACE=0` in `CMakeLists.txt:104`
  (compiles `reset_interface.c` to an empty TU and frees the strong
  `usbd_app_driver_get_cb` symbol — two definitions would be a link error).
- Port `reset_interface.c:93-171` verbatim (MIT/BSD-3, same provenance-header
  convention as `usb_descriptors.c:1-15`) with the `#if` gates removed, both BOOTSEL
  and FLASH branches unconditionally present, `..._DISABLE_MASK` -> `0u`, no LED.
- Add `pl_log` telemetry — this is the point of doing it this way:
  - `resetd_open`, after the class/subclass/protocol `TU_VERIFY`:
    `usb-reset: itf bound itf=%u`
  - top of `resetd_control_xfer_cb`, SETUP stage only:
    `usb-reset: ctrl bmReq=0x%02x bReq=0x%02x wValue=0x%04x wIndex=%u`
- Control transfers only; no endpoints, no per-frame cost.

Why own it rather than add `-D` flags: we already own the descriptors and
`tusb_config.h`. Leaving one SDK file compiled against a config header it
structurally cannot see is the blurred seam that produced this bug, and it will
produce the next one. It is also the only way to get telemetry that makes a failed
one-shot worth the replug.

**(A') Fallback only if (A) will not build cleanly** — add to `CMakeLists.txt:103-153`:
`PICO_STDIO_USB_RESET_INTERFACE_SUPPORT_RESET_TO_BOOTSEL=1`,
`..._RESET_TO_FLASH_BOOT=1`, `PICO_STDIO_USB_ENABLE_RESET_VIA_BAUD_RATE=1`.
Cheap and right as a fix, but blind — no telemetry. Never flash A' and A separately.

**(B) Make the trap non-repeatable.** Add to the top of `firmware/src/tusb_config.h`:
`#include "pico/stdio_usb.h"`, with a comment explaining that this file REPLACES
pico_stdio_usb's own `tusb_config.h`, the only place `PICO_STDIO_USB_*` defaults come
from. That header defines only `PICO_STDIO_USB_*` macros and prototypes — no
`CFG_TUD_*` — so it cannot fight our class config, and every default is `#ifndef`
guarded so our `-D` overrides still win. If include order fights back, replace with an
`#error` guard asserting the three macros are defined; do not silently drop it.

**(C) Hardware-independent escape hatch — hold X+Y ~1s = BOOTSEL.** The insurance that
makes a failed attempt cost nothing further. In the `main.c:297` superloop beside
`pl_link_input_poll` (`:300`), read raw GPIOs directly — `pl_link_input_poll` is
press-edge-only by design (`input.h:4-8,41-51`) and cannot express a hold. Pull-ups,
so 0 == pressed; ~60 frames at the 16ms budget ~= 1s; then `reset_usb_boot(0, 0)`
from `pico/bootrom.h`. Two simultaneous buttons are not a UI gesture, so it cannot
collide with navigation, and it runs in thread context — safe w.r.t. the 0xC0 worker.

**(D) PID change — NOT APPLIED.** See step-0 finding above.

## Verify on hardware, in this order

1. `system_profiler SPUSBDataType | grep -A8 "Pico Link"` -> `Product ID: 0x000c`.
2. **Start the console FIRST and leave it running** —
   `python3 tools/usb-console/cdc_reader.py` (never the tty path). Expect
   `usb-reset: itf bound itf=4` within a second of enumeration. Absence => M3/M4.
3. **Audio non-regression, BEFORE touching picotool** — play audio with Pico Link
   selected. Console must show a non-zero `measured=` rate (`main.c:344-350`).
   `usb-audio: idle` is a FAIL. A zero reading is not a pass.
4. **The test** — `picotool reboot -f -u`.
   - Pass: "The device was rebooted into BOOTSEL mode", `/Volumes/RP2350` mounts
     within ~2s, console shows `ctrl ... bReq=0x01 wIndex=4` just before it dies.
   - Fail 1 — `ctrl bReq=0x01` line present, no reboot: M1 necessary but not
     sufficient; our handler is wrong. Inspect `reset_usb_boot` args / disable_mask.
   - Fail 2 — NO `ctrl` line, picotool repeats the same error: **M2 confirmed**,
     picotool never asked. Fix is the device-selection/PID route, now known rather
     than guessed.
   - Fail 3 — no `itf bound` line either: M3/M4; dump the full config descriptor
     with `system_profiler SPUSBDataType -detailLevel full` and compare interface 4
     against `usb_descriptors.h:131`.
5. **Escape hatch** — hold X+Y ~1s, board must enter BOOTSEL. Verify even on a pass:
   it is what guarantees no future replug regardless of picotool.

## Must not regress

- `CFG_TUD_AUDIO_ENABLE_FEEDBACK_FORMAT_CORRECTION 1` (`tusb_config.h:125`) and the
  3-byte feedback endpoint (`usb_descriptors.c:115`). macOS refuses alt setting 1
  without them (pico-link-icb). Step 3 gates this before picotool is touched.
- CDC console + `PICO_STDIO_USB_CONNECTION_WITHOUT_DTR=1` +
  `PICO_STDIO_USB_STDOUT_TIMEOUT_US=2000` (`CMakeLists.txt:118,129`).
- `bcdUSB` stays `0x0200`, no BOS, `CFG_TUD_VENDOR 0`.
- `CFG_TUSB_DEBUG 0` stays 0 (the `usbd.c:356-360` dropped-event issue is its own bead).
- The 0xC0 worker / 1ms `tud_task` timing (pico-link-tfj). The reset driver is
  control-transfer-only; the X+Y chord adds two `gpio_get`s per 16ms frame.
- Interface numbering and the endpoint map — changing them invalidates the macOS
  enumeration cache and risks step 3 for no reason.
