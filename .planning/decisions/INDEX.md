# Architecture Decision Records

One ADR per file, named `YYYY-MM-DD-short-title.md`.
Format: Context / Decision / Rationale / Alternatives / Consequences.

Superseded entries are marked Deprecated, never deleted.

| Date | Decision | Status |
|------|----------|--------|
| 2026-08-26 | [Rust owns the firmware binary; C libraries are linked in, not forked](2026-08-26-rust-owns-the-binary-no-usbpods-fork.md) | **Superseded** by 2026-08-27 (C-first) |
| 2026-08-27 | [TinyUSB owns the USB device controller; the embassy-usb CDC console is unwound](2026-08-27-usb-device-stack-returns-to-tinyusb.md) | Accepted |
| 2026-08-27 | [C-first: pico-sdk owns `main()`; the Rust core becomes a staticlib called over FFI](2026-08-27-c-first-pico-sdk-owns-main.md) | Accepted |
