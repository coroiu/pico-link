# Pico Link

**A USB-to-Bluetooth audio dongle with a screen.**

Plug it into any computer, console or handheld. The host sees a driverless USB
sound card. Pico Link streams that audio to your Bluetooth headphones over A2DP
with a hi-res codec — LDAC first. No app, no driver, no pairing dance on the host.

The difference is the display. Comparable dongles are blind — a button, an LED,
and a serial console if you're lucky. Pico Link has a 240x240 screen and a d-pad,
so pairing, switching devices, choosing a codec and watching the live link happen
on the device itself.

> **Status: early.** The repo currently holds the UI framework and its desktop
> emulator. Firmware integration is in progress. See `.planning/roadmap.md`.

## Hardware

| Part | |
|------|--|
| [Pimoroni Pico Plus 2 W](https://shop.pimoroni.com/products/pico-plus-2-w) | RP2350B, 8MB PSRAM, 16MB flash, RM2 radio (Bluetooth 5.2 Classic + LE), USB-C |
| [Waveshare Pico-LCD-1.3](https://www.waveshare.com/wiki/Pico-LCD-1.3) | 240x240 IPS, ST7789, SPI, 5-way joystick + 4 buttons |

The LCD plugs directly onto the board — no soldering.

## Why RP2350

LDAC rides on A2DP, which requires Bluetooth Classic (BR/EDR). That rules out
most hobbyist wireless MCUs: the ESP32-S3 is BLE-only, and the original ESP32 has
BR/EDR but no USB peripheral, so it can't present itself as a sound card. RP2350
paired with the CYW43439 has both — native USB device *and* Bluetooth Classic.

## Building

Host-side (UI framework and emulator):

```sh
cargo build
cargo test
cargo run --bin desktop   # windowed emulator
```

The `core` crate is platform-free by construction and must never depend on a
platform crate. `emulator` provides windowed, headless and PNG-capture run modes,
so the interface can be developed and tested with no hardware attached.

## Credits and licence

The audio and radio half draws on **[USBPods](https://github.com/wasdwasd0105/USBPods-Pico2W)**
by wasdwasd0105 — UAC1/UAC2 in, SBC/AAC/AAC-ELD/LDAC/LHDC out on a Pico 2 W — as
a reference for how pico-sdk, BTstack, TinyUSB and libldac fit together on this
hardware. Pico Link's own firmware links the same C libraries directly; the
display and UI are Rust called into from C over a narrow FFI. See
`.planning/decisions/` for the architecture ADRs.

LDAC encoding uses Sony's Apache-2.0 `libldac`; Bluetooth is BlueKitchen's
BTstack as distributed with the Pico SDK.
