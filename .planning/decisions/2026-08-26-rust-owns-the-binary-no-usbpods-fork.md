# Rust owns the firmware binary; C libraries are linked in, not forked

**Date:** 2026-08-26
**Status:** Accepted

## Context

The roadmap's original Milestone C was "fork USBPods" — take
`github.com/wasdwasd0105/USBPods-Pico2W` (GPL-3, pico-sdk 2.1.1 + BTstack +
TinyUSB + Sony libldac) as the firmware base, patch it, and migrate downward into
Rust over time from the top.

Andreas flashed stock USBPods onto the Pimoroni Pico Plus 2 W — the real target
board, not the stock Pico 2 W the roadmap assumed as the reference — and LDAC
worked. He then reopened the decision: *"I DON'T WANT A FORK. I want a rust core
with C/C++ bolted on to do the things rust can't do."*

## Decision

**Our own firmware crate. Cargo builds the `.elf`. Rust owns `main()`, the vector
table and crt0.** C libraries are linked underneath as plain static libraries and
own nothing above themselves.

| Layer | Language |
|---|---|
| App, screens, navigation (`core`) | Rust |
| ST7789 display, joystick, buttons | Rust |
| HCI transport to the RM2 | Rust (`cyw43`) |
| A2DP host stack — L2CAP, SDP, AVDTP, AVRCP | **C — BTstack** |
| LDAC encoder | **C — libldac** |
| USB Audio Class 2 + CDC + reset, one composite device | **C — TinyUSB** |

USBPods becomes a **reference to read**, not a base to fork or patch. Copying
its code would inherit GPL-3; reading it for understanding does not.

## Rationale

Verified empirically in spike `pico-link-8v3.1` before committing to it. The
resulting ELF was inspected directly rather than trusted:

```
arm-none-eabi-objdump -f    elf32-littlearm, armv8-m.main
arm-none-eabi-nm | grep -c  76 BTstack/HCI/L2CAP/SDP text symbols
arm-none-eabi-nm -u         EMPTY — zero undefined symbols
Reset, __pre_init           provided by cortex_m_rt — RUST owns the vector table
grep -c 'runtime_init|pico_' 0 — pico-sdk's runtime is NOT in the binary
```

BTstack and TinyUSB coexist in one Cargo-built binary. The architecture is
viable; the C-owns-`main()` alternative is not required.

Two supporting findings:

- `~/pico-sdk/lib/cyw43-driver/firmware/cyw43_btfw_43439.h` — the CYW43 Bluetooth
  firmware blob is an ordinary C header we can link. An earlier analysis claimed
  BR/EDR bring-up was pico-sdk-exclusive; it is not.
- `cyw43` 0.7.0's `bluetooth` feature implements `bt_hci::transport::Transport`
  and carries a matching firmware-patch format — the Rust half of the HCI bridge
  exists and does Bluetooth Classic, not only BLE.

## Alternatives considered

**Fork USBPods** (the original plan). Rejected: it puts C in charge of the
program and leaves us permanently downstream of someone else's architecture,
which is precisely what Andreas rejected. Note it was *not* rejected on
licensing.

**Pure Rust, no C at all.** Not possible today. `cyw43` provides HCI *transport*;
an A2DP **source host stack** — L2CAP, SDP, AVDTP, AVRCP plus codec integration —
does not exist in Rust. The Rust BLE host it pairs with (TrouBLE) is BLE-only.
Writing a BR/EDR host stack is a larger project than this dongle.

**Rust UAC2 via `embassy-usb`.** Deferred, not rejected. `embassy-usb` has
isochronous endpoints but no UAC2 class; the descriptors and the explicit-feedback
clock loop would be ours to write, and that loop is where amateur USB audio
produces clicks. Andreas: *"You can bolt on TinyUSB's UAC2 in C for now as long
as the core is still rust. We can write our own later down the line."* TinyUSB is
therefore the one **provisionally-C** library on the list.

## Consequences

**USB is a single peripheral and cannot be shared.** Because TinyUSB owns the USB
controller, `embassy-usb` cannot also be used. USB CDC logging and the picotool
reset interface must therefore be TinyUSB interfaces in the same composite device
(UAC2 + CDC + reset). This makes the C side slightly larger than the theoretical
minimum but removes the ownership-arbitration problem entirely.

**Keep the audio data path behind a narrow interface.** Do not let TinyUSB
callbacks reach up into app code. When Rust eventually takes over UAC2, USB
peripheral ownership is the seam to cross.

**Licensing.** Andreas confirmed he does not intend Pico Link's own code to be
closed, so this closes as a risk. Recorded for completeness: BTstack's commercial
terms, the LDAC trademark/certification programme and FDK-AAC patent licensing
apply **identically whether or not USBPods is forked** — they were never a
differentiator between the options, and only become live if the project goes
commercial. Not forking means our own code is not obliged to be GPL-3; that is
the only licensing delta.

**Still unproven.** The spike is a link-seam probe. Nothing has executed on
hardware: the HCI transport is a stub, `hal_time_ms()` returns 0, and the RP2350
USB device controller is not linked. Bead `pico-link-8v3.2` takes it to first
execution.
