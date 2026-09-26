# Pico Link — Roadmap

**Last updated:** 2026-08-28 (C-first migration executed, gate-2 radio
bring-up achieved on real hardware — see `decisions/2026-08-27-c-first-pico-sdk-owns-main.md`
and `.planning/progress.md`; project founded 2026-08-26, pivoted from a
Bitwarden hardware-key prototype)

## Vision

**Pico Link is a USB-to-Bluetooth audio dongle with a screen.**

Plug it into any computer, console or handheld. The host sees a driverless USB
sound card. The dongle streams that audio to Bluetooth headphones over A2DP with
a hi-res codec — LDAC first. No app, no driver, no pairing dance on the host.
An optional web companion adds power tools the screen can't do well; the dongle
never needs it.

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

USBPods is a reference to read for how these C libraries fit together on this
exact hardware; reading it is not copying it, so its GPL-3 is not inherited by
that alone. **As of [ADR 2026-08-27](decisions/2026-08-27-c-first-pico-sdk-owns-main.md),
pico-sdk owns `main()` and `runtime_init` — the same boot path USBPods uses —
and Rust (`core/`) is a staticlib called from C over FFI, not the owner of the
binary.** **DECIDED (same-day update to that ADR): Route B — we write our own
C against pico-sdk. USBPods stays a reference to read, never to vendor or
fork.** GPL-3 is viral across the link boundary, so there is no partial fork;
the hard parts (UAC2 + explicit-feedback clock loop, A2DP/AVDTP, LDAC) already
have licence-clean first-party references vendored in the SDK itself. The
earlier "no fork" position
([ADR 2026-08-26](decisions/2026-08-26-rust-owns-the-binary-no-usbpods-fork.md),
now superseded) is reaffirmed under the new architecture, not reopened.

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

### C — Firmware: C-first, pico-sdk owns `main()`
Hardware-gated. **Architecture flipped 2026-08-27** — see
[ADR 2026-08-27](decisions/2026-08-27-c-first-pico-sdk-owns-main.md), which
supersedes the "Rust owns the binary"
[ADR 2026-08-26](decisions/2026-08-26-rust-owns-the-binary-no-usbpods-fork.md).
pico-sdk owns `main()` and `runtime_init`; `core/` is a `no_std` + `alloc`
staticlib called from C over a narrow FFI for rendering only. **The
fork/no-fork question is decided: Route B, we write our own C against
pico-sdk, USBPods stays read-only reference** (same-day update to the ADR
above).

**The C-first migration executed overnight, 2026-08-27 into 2026-08-28, all
merged to `main`.** Detail in `.planning/progress.md`.
- **C1** ~~Fork USBPods~~ **DONE 2026-08-26.** Stock USBPods flashed on the
  Pimoroni Pico Plus 2 W — the real target, not the Pico 2 W reference the
  roadmap assumed — and LDAC worked. Partially discharges C2 as well.
- **C-spike** **DONE, then superseded.** Proved Cargo *could* link BTstack +
  TinyUSB into a Rust-owned binary: 76 BTstack symbols, zero undefined,
  `cortex_m_rt` owns the vector table, no pico-sdk runtime present in the
  binary. That was a real result, but it verified linking, not running — three
  sessions of gate-2 bring-up against it found a HardFault (CPACR never
  enabled) and a hang in `cyw43_spi_init`, both traceable to pico-sdk's
  `runtime_init` never running. Superseded by the C-first ADR.
- **M1a DONE** (`pico-link-cz0.1`) — new `firmware/` CMake project on pico-sdk
  2.1.1, board `pimoroni_pico_plus2_w_rp2350`, CDC console, unattended
  `picotool reboot -f -u` reflashing.
- **M1b DONE** (`pico-link-cz0.2`) — the architecture proof: Rust `core`
  renders, C blits, over the FFI seam, on real hardware, no crashes.
- **M2 DONE, gate-2 achieved** (`pico-link-cz0.3`) — Bluetooth radio up:
  BTstack Classic GAP inquiry over pico-sdk's cyw43 HCI transport, discovered
  devices rendered on the panel with address and RSSI, webcam-verified. This
  is the milestone three earlier Rust-first sessions never reached.
- **M3 IN FLIGHT** (`pico-link-cz0.4`) — TinyUSB composite sound card.
  Remaining C2–C5-equivalent scope (PSRAM framebuffer bandwidth at full SPI
  speed, joystick/button input exercised on real hardware) tracked as beads,
  not restated here.

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

### F — Web companion (post-MVP)
A WebUSB page on a public HTTPS static site, linked from the device's WebUSB
landing-page descriptor (Chromium browsers). It talks to the device over the proven
iface-6 config channel (CLASS requests). **Invariant: every function the device
offers stays reachable from the buttons; the web only adds.** See
[ADR 2026-09-26](decisions/2026-09-26-web-companion-is-an-optional-power-tool.md).

- **F1** Home page that visually mirrors the device home (same layout, live
  moving level bars over a telemetry stream), plus EQ editor + preset library
  (subsumes `pico-link-ryw.12.7`).
- **F2** AutoEQ search.
- **F3** Live diagnostics — link graphs + log; retires the CDC tty for
  development; bandwidth budgeted against audio.
- **F4** Firmware update over PICOBOOT — gated on a feasibility spike.

## Sequencing

```
A ──────────────────────────────► (squash, new identity)
   ↘ B1 → B2, B3 → B4                    (host, no hardware)
C1 → C2 → C3 → C4, C5                    (hardware)
              B4 + C5 → D1 → D2 → D3 → D4  ◄── MVP
                                        → E
```

## Live risks

- **PSRAM framebuffer bandwidth / panel colour accuracy** (`pico-link-14l`) —
  the assumption everything visual rests on is still only partly proven: the
  SPI clock is held at a deliberately conservative 1MHz, making a full 240x240
  blit take ~1.008s, and absolute panel colour has not been verified.
- **RM2 board config** (C2) — mitigated by keeping a stock Pico 2 W as reference.
- ~~**Rust/pico-sdk linking** (C3)~~ — RETIRED as a linking question, then
  RESURFACED as a running question. The C-spike linked BTstack and TinyUSB into
  a Cargo-owned binary with zero undefined symbols, but three sessions of real
  bring-up (a HardFault, a hang, inert `.preinit_array` runtime-init hooks)
  showed the binary couldn't actually run without pico-sdk's own
  `runtime_init`. Resolved by the C-first ADR: pico-sdk now owns `main()`.
- ~~**`no_std` port scope** (B1)~~ — RETIRED. Done; `core` cross-compiles for
  `thumbv8m.main-none-eabihf`, and stays true regardless of who owns `main()`.
- ~~**Upstream relationship reopened.**~~ — RETIRED. Briefly reopened by the
  C-first pivot, now decided (same-day update to the C-first ADR): Route B,
  we write our own C against pico-sdk; USBPods stays a read-only reference,
  never forked or vendored.
- **Rewriting working audio** — the real-time USB/resample/encode/A2DP path is
  genuine engineering. Mitigated by keeping TinyUSB's UAC2 in C rather than
  writing a Rust one, so the clock-feedback loop is proven code. Do not rewrite
  that loop until the dongle works.
- ~~**Gate-2 radio bring-up not yet achieved.**~~ — RETIRED, ACHIEVED
  2026-08-27 into 2026-08-28 (`pico-link-cz0.3`, merged `4b14cc9`). BTstack
  Classic GAP inquiry runs on real hardware over pico-sdk's cyw43 HCI
  transport, against the C-first architecture, webcam-verified.
- **No automated input path on the real target** (`pico-link-d7k`) — the
  d-pad-select -> `PL_CMD_CONNECT` path is implemented but has never been
  exercised on real hardware, because there is no way for an agent to drive
  buttons on the physical device. A standing gap in the three-run-modes
  testability story.
- **TinyUSB composite sound card** (`pico-link-cz0.4`, M3, in flight) — the
  next milestone gating the MVP.

## Settled decisions

- **C owns `main()`, `runtime_init` and scheduling; Rust is a staticlib behind
  a narrow FFI.** Flipped 2026-08-27 from "Rust owns the binary" after three
  sessions established that linking BTstack into a Rust-owned binary was never
  the risk — running it without pico-sdk's runtime was. BTstack's run loop is
  the scheduler. See
  [ADR 2026-08-27](decisions/2026-08-27-c-first-pico-sdk-owns-main.md).
- **BTstack and libldac stay C behind FFI** — there is no Rust Bluetooth
  Classic host stack and writing one is not a project. This did not change.
- **240x240 is fixed.** The larger 2" panel was considered and dropped, so the
  resolution is a constant, not a parameter.
- **No rotary encoder.** The joystick and four buttons are the input model.
- **TinyUSB owns the USB device controller.** The `embassy-usb` CDC console
  that bootstrapped observability during bring-up is retired now that it has
  served its purpose. See
  [ADR 2026-08-27](decisions/2026-08-27-usb-device-stack-returns-to-tinyusb.md).
- **`cyw43-driver` licensing is fine while RP-only.** Its `LICENSE.RP` applies
  (not the default non-commercial licence) because RP2350 is Raspberry Pi Ltd
  silicon. Recorded on bead `pico-link-kq9`.
- **We write our own C against pico-sdk; USBPods stays a read-only
  reference — never forked or vendored (Route B).** Decided same-day as a
  follow-up to the C-first ADR. GPL-3 is viral across the link boundary, so
  there is no partial fork: vendoring even one USBPods `.c` file would make
  the entire linked binary GPL-3. The hard parts already have licence-clean
  first-party references vendored in the SDK itself (TinyUSB's `uac2_headset`
  example, MIT; BTstack's `a2dp_source_demo.c`; Sony's Apache-2.0 `libldac`),
  which is what makes Route B affordable — a few extra days on the
  explicit-feedback clock loop, not weeks. Our own code keeps a free choice
  of licence. Safeguards: never clone USBPods into the repo tree (read it in
  a scratch directory outside the checkout); every non-trivial C file we
  write carries a one-line provenance header; a provenance gate runs before
  M4 merges. See the "Update (same day, 2026-08-27)" section of
  [ADR 2026-08-27](decisions/2026-08-27-c-first-pico-sdk-owns-main.md).
