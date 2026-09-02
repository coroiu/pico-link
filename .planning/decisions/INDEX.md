# Architecture Decision Records

One ADR per file, named `YYYY-MM-DD-short-title.md`.
Format: Context / Decision / Rationale / Alternatives / Consequences.

Superseded entries are marked Deprecated, never deleted.

| Date | Decision | Status |
|------|----------|--------|
| 2026-08-11 | [UI framework: fixed chrome regions + linear stacks, not a general layout engine](2026-08-11-ui-framework-reuse-vs-rewrite.md) | Accepted (written retroactively 2026-09-01, bead pico-link-3i8 — see its provenance note) |
| 2026-08-26 | [Rust owns the firmware binary; C libraries are linked in, not forked](2026-08-26-rust-owns-the-binary-no-usbpods-fork.md) | **Superseded** by 2026-08-27 (C-first) |
| 2026-08-27 | [TinyUSB owns the USB device controller; the embassy-usb CDC console is unwound](2026-08-27-usb-device-stack-returns-to-tinyusb.md) | Accepted |
| 2026-08-27 | [C-first: pico-sdk owns `main()`; the Rust core becomes a staticlib called over FFI](2026-08-27-c-first-pico-sdk-owns-main.md) | Accepted |
| 2026-08-31 | [The widget-facing clock seam: a frame-scoped `RenderCtx`, not a `Clock` trait object](2026-08-31-render-ctx-frame-scoped-clock.md) | Accepted (designed, not yet implemented) |
| 2026-09-02 | [Core 1 allocation, and how to raise the repaint ceiling](2026-09-02-core1-allocation-and-the-repaint-ceiling.md) | Accepted (rejects `pico-link-yz6`'s original premise: core1 is reserved for LDAC, not the display) |
