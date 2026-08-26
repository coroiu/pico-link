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

pub mod app;
pub mod input;
pub mod platform;
pub mod power;
pub mod render;
pub mod run;

pub use app::App;
pub use input::NavIntent;
pub use power::{IdlePowerSetting, DEFAULT_DEEP_SLEEP_TIMEOUT, DEFAULT_IDLE_TIMEOUT};
pub use run::run;
