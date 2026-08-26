# Pico Link — Roadmap

**Last updated:** 2026-08-26 (project founded; pivoted from a Bitwarden hardware-key prototype)

## Vision

**Pico Link is a USB-to-Bluetooth audio dongle with a screen.**

Plug it into any computer, console or handheld. The host sees a driverless USB
sound card. The dongle streams that audio to Bluetooth headphones over A2DP with
a hi-res codec — LDAC first. No app, no driver, no pairing dance on the host.

The thing that makes it ours is the **screen**. Comparable dongles are blind: a
button, an LED, and a serial console if you're lucky. Pico Link has a display and
a d-pad, so pairing, device switching, codec selection and live link status are
things you *see and control on the device*, with no terminal attached.

**MVP:** pair a fresh set of headphones and see the live codec and bitrate on
screen, driven entirely by the buttons, with no serial console.

## Hardware

- **Pimoroni Pico Plus 2 W** — RP2350B, dual Cortex-M33 @150MHz, 520KB SRAM,
  **8MB PSRAM**, 16MB flash, Raspberry Pi RM2 radio (CYW43439-class, Bluetooth
  5.2 **Classic + LE**), USB-C.
- **Waveshare Pico-LCD-1.3** — 240x240 IPS, ST7789, 4-wire SPI. Display on
  GP8 (DC), GP9 (CS), GP10 (SCK), GP11 (MOSI), GP12 (RST), GP13 (BL).
  Joystick on GP2 (up), GP18 (down), GP16 (left), GP20 (right), GP3 (press).
  Buttons A/B/X/Y on GP15/GP17/GP19/GP21.
- A stock **Raspberry Pi Pico 2 W** is kept as the known-good reference board.

The PSRAM is load-bearing, not a luxury: the render core holds a full RGB565
framebuffer (240x240x2 = 115KB) which must coexist with BTstack, TinyUSB's
isochronous buffers, LDAC encoder state and the audio ring buffers inside 520KB
of SRAM. PSRAM is what lets the framebuffer move off-chip and the render core
port unchanged.

## Why this chip

No single Espressif part can do this job. A2DP (and therefore LDAC) requires
Bluetooth Classic BR/EDR; the ESP32-S3 is BLE-only and Espressif closed the
BR/EDR request as "Won't Do". The original ESP32 has BR/EDR but no USB
peripheral, so it cannot be a USB sound card. RP2350 + CYW43439 is the
combination that has both: native USB device *and* Bluetooth Classic.

## Upstream

The audio and radio half is solved by **USBPods**
(github.com/wasdwasd0105/USBPods-Pico2W) — GPL-3, C, pico-sdk 2.1.1 + BTstack +
TinyUSB + Sony's Apache-2.0 libldac + FDK-AAC. It already does UAC1/UAC2 in and
SBC/AAC/AAC-ELD/LDAC/LHDC out on a Pico 2 W. It has no display; its UI is a
serial console.

**We do not fork it.** Pico Link is its own Rust firmware that links the same
underlying C libraries — BTstack, libldac, TinyUSB — directly. USBPods is a
reference to read for how they fit together; reading it is not copying it, so
its GPL-3 is not inherited. See
[ADR 2026-08-26](decisions/2026-08-26-rust-owns-the-binary-no-usbpods-fork.md).

## What we inherited

This repo began as a Bitwarden hardware-key prototype. Everything product-facing
was stripped; what survived is the part worth keeping:

- `core/render/*` — framebuffer, widget/focus model, screen + navigator, vertical
  list, menu, theme, chrome, message and confirm views. Verified to have **zero**
  coupling to the old product layer.
- `core/input.rs` — the semantic `NavIntent` input vocabulary.
- `core/platform.rs` — the `DisplaySurface`/`InputSource`/`Clock`/`Storage` trait
  seams that keep the core platform-free.
- `core/run.rs` — the shared run loop: frame budget, idle/sleep tiers, flush-error
  handling.
- `emulator/` — **the crown jewel.** Windowed (minifb), headless, and PNG-capture
  run modes, so UI work never blocks on hardware.

## Milestones

### A — Repo reset (in progress)
Strip to a generic UI-framework template, rewrite the docs, drop the ESP32 build
config, then squash into a single initial commit under the new identity.

### B — Core retarget
Host-side only; runs fully in parallel with C.
- **B1** `no_std` + `alloc` port of `core`. `std::time::Instant` becomes a
  core-defined clock type; `Rc`/`RefCell`/`Vec` come from `alloc`; the emulator
  supplies a host `Clock`. `embedded-graphics` is already `no_std`. *Largest
  single piece of work in the plan.*
- **B2** Retarget 320x170 landscape to 240x240 square. Not a resize — the chrome,
  list and menu layouts were designed for a wide strip and need rethinking.
- **B3** Replace the rotary-encoder `NavIntent` vocabulary with d-pad + 4 buttons.
  Strictly richer than the encoder it replaces.
- **B4** Keep all three run modes green throughout, at 240x240.

### C — Firmware: Rust owns the binary
Hardware-gated. **Not a fork** — see
[ADR 2026-08-26](decisions/2026-08-26-rust-owns-the-binary-no-usbpods-fork.md).
Cargo builds the `.elf`; BTstack, libldac and TinyUSB link in as C static
libraries. USBPods is a reference to read, not a base to patch.
- **C1** ~~Fork USBPods~~ **DONE 2026-08-26.** Stock USBPods flashed on the
  Pimoroni Pico Plus 2 W — the real target, not the Pico 2 W reference the
  roadmap assumed — and LDAC worked. Partially discharges C2 as well.
- **C-spike** **DONE.** Proved Cargo can own the binary with BTstack + TinyUSB
  linked: 76 BTstack symbols, zero undefined, `cortex_m_rt` owns the vector
  table, no pico-sdk runtime. Link-seam only — nothing ran on hardware.
- **C2** Board config for the Pico Plus 2 W — RM2 pin setup, RP2350B, 16MB flash,
  PSRAM init, in Rust via `embassy-rp`/`rp-hal`.
- **C3** ~~Link Rust into their CMake build~~ — inverted. The linking question is
  answered in the other direction and the spike settled it.
- **C4** PSRAM framebuffer + ST7789 driver on GP8-13. **Verify PSRAM write and
  DMA-to-SPI bandwidth sustains UI framerates** — the whole render approach rests
  on this assumption, so prove it before building on it. Note there is no mature
  Rust support for RP2350 QMI XIP PSRAM; expect to write the init.
- **C5** Joystick and buttons to `NavIntent`. The vocabulary already exists
  (Epic B3) — this is the firmware `InputSource` impl. Use pull-**ups**: RP2350
  erratum E9 affects pull-downs.

### D — UI integration (the MVP)
- **D1** The `extern "C"` seam. Rust owns SPI, DMA and the framebuffer directly
  now that Rust owns the binary — the C side is BTstack, libldac and TinyUSB
  only. **To validate at C2/C4:** put the C real-time audio/Bluetooth loop on
  **core1** and the Rust app/UI on **core0**, communicating over a lock-free
  command/event queue. A data contract rather than a call contract: it keeps the
  FFI surface enumerable and structurally stops a slow SPI flush or PSRAM stall
  from starving audio. Check how BTstack's run loop wants to be driven before
  committing.
- **D2** A narrow, versioned C header **we author** — commands in (scan, pair,
  connect, disconnect, slot select, codec select, volume), events out (link
  state, negotiated codec, bitrate, device names). Contract-first, not
  discovery-first.
- **D3** Screens: status (device, codec, bitrate, link quality), device list with
  real names instead of slots `1`/`2`, pairing flow, settings.
- **D4** On-hardware acceptance: pair fresh headphones using only the screen.

### E — Rust migration downward (post-MVP)
One subsystem at a time, each step ending on a working dongle: settings and
persistence, then the control layer, then USB (TinyUSB is the one
**provisionally**-C library — replacing it means writing a UAC2 class and an
explicit-feedback clock loop in `embassy-rp`, and taking over USB peripheral
ownership), then the audio pipeline
and resampler. **BTstack and libldac stay C indefinitely** — wrapping them well is
the Rust work, not replacing them. The real-time path goes last, deliberately.

## Sequencing

```
A ──────────────────────────────► (squash, new identity)
   ↘ B1 → B2, B3 → B4                    (host, no hardware)
C1 → C2 → C3 → C4, C5                    (hardware)
              B4 + C5 → D1 → D2 → D3 → D4  ◄── MVP
                                        → E
```

## Live risks

- **PSRAM framebuffer bandwidth** (C4) — the assumption everything visual rests
  on. Prove it first.
- **RM2 board config** (C2) — mitigated by keeping a stock Pico 2 W as reference.
- ~~**Rust/pico-sdk linking** (C3)~~ — RETIRED. The C-spike linked BTstack and
  TinyUSB into a Cargo-owned binary with zero undefined symbols.
- ~~**`no_std` port scope** (B1)~~ — RETIRED. Done; `core` cross-compiles for
  `thumbv8m.main-none-eabihf`.
- ~~**Upstream drift**~~ — RETIRED by the no-fork decision. We track the C
  libraries directly, not someone's fork of them.
- **Rewriting working audio** — the real-time USB/resample/encode/A2DP path is
  genuine engineering and we are now writing our own rather than inheriting
  USBPods'. Mitigated by keeping TinyUSB's UAC2 in C rather than writing a Rust
  one, so the clock-feedback loop is proven code. Do not rewrite that loop until
  the dongle works.
- **`hal_time_ms` and friends are stubs** — the C-spike shimmed BTstack's HAL to
  get a link. BTstack timers cannot work until they are real (`pico-link-8v3.2`).

## Settled decisions

- **Rust, not C, for everything above the codecs.** BTstack and libldac stay C
  behind FFI; there is no Rust Bluetooth Classic host stack and writing one is not
  a project.
- **240x240 is fixed.** The larger 2" panel was considered and dropped, so the
  resolution is a constant, not a parameter.
- **No rotary encoder.** The joystick and four buttons are the input model.
- **No USBPods fork.** Rust owns the binary; BTstack, libldac and TinyUSB link
  in as C libraries. TinyUSB is provisionally C — a Rust UAC2 is a later option,
  not a commitment. Confirmed by the C-spike, 2026-08-26.
- **Licensing is open by choice, not obligation.** Not forking means our code is
  not required to be GPL-3; Andreas confirmed open is fine regardless. BTstack's
  commercial terms, the LDAC trademark and FDK-AAC patents apply the same either
  way and only matter if this ever goes commercial.
