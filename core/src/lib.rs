//! Platform-free application core.
//!
//! This crate is the compiler-enforced portability boundary: it must never
//! depend on pico-sdk bindings, `minifb`, `tiny_http`, or any other
//! platform-specific crate (see `Cargo.toml`). Every run mode's binary
//! (host/emulator today, a future real-board target) depends on this
//! crate and implements its trait seams.
//!
//! This crate is a generic UI-framework template — a reusable render
//! core, navigation model, and app-loop skeleton with no domain model of
//! its own. A concrete product builds on top of it: real content
//! [`Screen`]s, an app-specific data model, and whatever `Platform`
//! capabilities its run modes need.
//!
//! Current contents:
//! - [`input`]: the frozen `NavIntent` semantic input vocabulary.
//! - [`platform`]: the `DisplaySurface`/`InputSource`/`Clock`/`Storage`/
//!   `PowerControl` trait seams, plus the [`platform::Platform`] bundle
//!   trait that groups them.
//! - [`render`]: the render core — `FrameBuffer565`, `Widget`/`Action`/
//!   `FocusEvent`, `Screen`/`Navigator`, chrome layout, and the
//!   `VerticalList`/`MenuList`/`ConfirmView`/`MessageView` content
//!   widgets.
//! - [`power`]: [`power::DEFAULT_IDLE_TIMEOUT`] (the idle-screensaver
//!   timeout, `Ta`), [`power::DEFAULT_DEEP_SLEEP_TIMEOUT`] (`Tb`), and
//!   [`power::IdlePowerSetting`] (the single persisted toggle covering
//!   both power tiers).
//! - [`app::App`]: the platform-free application state — a `Navigator`
//!   built once over a placeholder root screen.
//! - [`run::run`]: the unified, `Platform`-generic main loop that drives
//!   an `App` — the one loop shared by every run mode.
//!
//! # `no_std` + `alloc`
//!
//! This crate is `no_std` (heap-allocating via `alloc`, e.g. `Vec`/`Box`/
//! `String`, but no OS/libc dependency) everywhere except its own `#[cfg(test)]`
//! modules, which compile against `std` as usual so `cargo test` needs no
//! special target or allocator setup. The `not(test)` gate is what makes
//! that possible: `cargo test` sets `cfg(test)` for this crate's own unit
//! tests, so those keep using `std::{rc, cell, time}` etc. unmodified, while
//! every other consumer (the `emulator` crate today building this as a
//! plain dependency, and eventually the RP2350 firmware) gets the real
//! `no_std` build -- which is exactly the build that must work on a target
//! with no OS. A future firmware binary supplies the global allocator (a
//! `#[global_allocator]`, likely backed by the RP2350's PSRAM) and this
//! crate's own `Instant`/`Clock`/sleep seam (see `platform.rs`); nothing
//! else in this file changes for that.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod app;
pub mod input;
pub mod platform;
pub mod power;
pub mod render;
pub mod run;

pub use app::{App, Command, DeviceEntry, LinkState};
pub use input::NavIntent;
pub use power::{IdlePowerSetting, DEFAULT_DEEP_SLEEP_TIMEOUT, DEFAULT_IDLE_TIMEOUT};
pub use run::run;
