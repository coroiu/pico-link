# TinyUSB owns the USB device controller; the embassy-usb CDC console is unwound

**Date:** 2026-08-27
**Status:** Accepted
**Relates to:** [2026-08-26 — Rust owns the firmware binary](2026-08-26-rust-owns-the-binary-no-usbpods-fork.md)

## Context

This is not a new decision. ADR 2026-08-26 already assigned "USB Audio Class 2 +
CDC + reset, one composite device" to TinyUSB, and its Consequences section
states plainly that because TinyUSB owns the USB controller, `embassy-usb`
cannot also be used, and that CDC logging and the picotool reset interface must
therefore be TinyUSB interfaces in the same composite device.

What happened between then and now is a **deviation**, taken knowingly and for
good reason. Bead `pico-link-8v3.2.2` set out to link TinyUSB's RP2350 device
controller driver and found the real hurdle: the dcd at
`portable/raspberrypi/rp2040` is the part that pulls in `pico.h` and the
hardware register headers, which is exactly what `firmware-spike/build.rs`
had so far avoided by compiling only TinyUSB's device-independent core. Rather
than solve that while the board had no observable output at all, bead
`pico-link-8v3.2.7` took a shortcut: an `embassy-usb` CDC console in pure Rust.

That shortcut worked, and it earned its keep. It gave the project a readable
boot log, a heartbeat, and — via a custom reset interface — `picotool reboot -f -u`
forcing BOOTSEL from software with no button press, which is what made
unattended flash-and-verify iteration possible at all. On a board with no
GPIO-addressable LED, it was the only instrument available.

The deviation was then briefly mistaken for a decision. When `pico-link-8v3.2.2`
was closed as superseded, its close reason asserted that the eventual sound card
"must be built on embassy-usb, not TinyUSB". That was an overreach by the
orchestrator, corrected the same day, and it contradicted the standing ADR.

## Decision

**Return to the 2026-08-26 architecture: TinyUSB owns the USB device controller.**
The `embassy-usb` USB device path, including the CDC console and its reset
interface, is unwound and replaced by a TinyUSB CDC + UAC2 + reset composite.

Andreas, 2026-08-27: *"CDC with embassy in rust was a temporary solution to get
something working. It does not remove the potential to use tinyusb if that's a
better/quicker way of getting audio over usb"*, and then: *"let the current work
complete and then start work on moving towards tinyusb. that's my decision im
making right now"*.

**Sequencing:** the swap is gated on C-spike gate 2 (`pico-link-8v3.2`)
completing. The `embassy-usb` console is the only instrument the cyw43 Bluetooth
bring-up has; removing it mid-bring-up would blind that work.

## Rationale

The reasons are unchanged from the original ADR, and nothing since has
weakened them:

- **The UAC2 class asymmetry is decisive.** TinyUSB ships a complete UAC2 device
  class in the SDK already vendored here — `$PICO_SDK_PATH/lib/tinyusb/src/class/audio/`
  (`audio_device.c`, `audio_device.h`, `audio.h`). `embassy-usb` has isochronous
  endpoints but **no UAC2 class**: the descriptors and the explicit-feedback
  clock loop would be ours to write, and as the original ADR put it, that loop
  is where amateur USB audio produces clicks. A driverless sound card is the
  entire product.
- **One stack owns the peripheral.** TinyUSB's dcd and `embassy-rp`'s USB driver
  cannot share the USB device controller. This is a swap, not a coexistence
  problem — TinyUSB takes CDC with it.
- **The door was deliberately left open.** `firmware-spike/build.rs:99-143`
  still compiles TinyUSB's device-independent core as a cross-compile probe,
  with a comment naming composite-device coexistence as the reason.

## Alternatives

**Keep `embassy-usb` and write UAC2 in Rust.** Rejected for now, deferred not
forbidden — the same position the 2026-08-26 ADR took. Writing the descriptors
and the explicit-feedback clock loop is real work with a high floor for getting
it subtly wrong, and it is not on the path to a working dongle. The original ADR
already names the seam for revisiting this: when Rust eventually takes over
UAC2, USB peripheral ownership is the boundary to cross.

**Run both stacks.** Not possible. Single peripheral.

## Consequences

**The `pico-sdk` runtime question is now forced, not deferred.** Linking the dcd
means pulling in `pico.h` and hardware register headers. The 2026-08-26 ADR's
verified claim — `grep -c 'runtime_init|pico_'` returning 0, `cortex_m_rt`
owning the vector table — is about to be tested for the first time. Bead
`pico-link-xkp.2` must choose a route (pico-sdk headers, a vendored trimmed
copy, or Rust register pokes), record it, and **say so explicitly if that claim
has to bend** rather than bending it quietly.

**The autonomous flash loop is a non-negotiable capability, not a nice-to-have.**
`picotool reboot -f -u` forcing BOOTSEL with no button press is what allows
hardware iteration without a human hand on the board. It currently rides on a
custom `embassy-usb` interface; TinyUSB has its own reset class, but it does not
carry over for free. `pico-link-xkp.3` must prove a full unattended cycle —
reboot, flash, read CDC from a fresh boot — on real hardware.

**Do not leave the tree without a console.** TinyUSB CDC must be printing before
`embassy-usb` is removed, even if both are briefly present in the source.

**`CFG_TUSB_MCU` stays `OPT_MCU_RP2040`.** Verified in the vendored tree at
`lib/tinyusb/src/common/tusb_mcu.h` (~line 410): it is the only macro this
TinyUSB version defines for the whole RP2 family; no `OPT_MCU_RP2350` exists.
Settled, do not revisit.

**The `embassy-usb` work is not wasted.** It bought observability during the
period when the board had none, and the CDC console it provided is what makes
the cyw43 bring-up debuggable today. A tactical shortcut that is retired on
schedule is not a mistake; one that silently becomes the architecture is.
