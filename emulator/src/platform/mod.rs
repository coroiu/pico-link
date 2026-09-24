//! Host implementations of `pico_link_core::platform`'s capability-bundle traits
//! (`DisplaySurface`, `InputSource`, `Clock`, `Storage`, `PowerControl`),
//! plus a small `Platform` bundle wiring them together.
//!
//! [`power::RecordingPowerControl`] is the `PowerControl` capability's
//! real (if inert) implementation — see that module's doc comment.
//!
//! This gives the emulator real adapters for the trait seams
//! `pico_link_core::platform` freezes, built on top of the render core
//! (`FrameBuffer565`, `Navigator`).
//!
//! Two `DisplaySurface`s exist side by side, per the presentation-surface
//! design: [`headless_surface::HeadlessSurface`] (PNG-on-demand, no window)
//! and [`minifb_surface::MinifbSurface`] (a real `minifb` window). Both
//! consume the exact same `FrameBuffer565`, so they are provably
//! pixel-identical — see `emulator/tests/surface_parity.rs`.
//!
//! The headless HTTP `NavIntent` injection + screenshot protocol is
//! implemented here: [`headless_surface::SharedHeadlessSurface`] wraps a
//! [`HeadlessSurface`] in an `Arc<Mutex<_>>` so both the render loop
//! (`DisplaySurface::flush`) and `GET /api/screenshot`, served from a
//! different thread — see `emulator::desktop::http_server::HttpServer` —
//! can see the same captured frame; [`input::HttpInput`] is the
//! `InputSource` counterpart, draining a `NavIntent` queue `POST
//! /api/input` feeds.

pub mod clock;
pub mod headless_surface;
pub mod host_platform;
pub mod input;
pub mod minifb_surface;
// Recording/no-op `PowerControl` -- see the module's own doc comment.
pub mod power;
pub mod storage;

pub use clock::HostClock;
pub use headless_surface::{HeadlessSurface, SharedHeadlessSurface};
pub use host_platform::HostPlatform;
pub use input::{HttpInput, NoopInput, WindowedInput};
pub use minifb_surface::MinifbSurface;
pub use power::RecordingPowerControl;
pub use storage::{FileStorage, FileStorageError};

/// The perceptual dim factor both [`headless_surface`]'s PNG preview and
/// [`minifb_surface`]'s live window rasterization multiply each 8-bit RGB
/// channel by while [`pico_link_core::platform::DisplayPower::Dim`] is
/// active -- one shared formula so the two surfaces stay provably
/// pixel-identical (the same guarantee `emulator/tests/surface_parity.rs`
/// already holds for `On`/`Off`). `(DIM_BACKLIGHT_PERMILLE / 1000) ^
/// (1/2.2)`: a gamma-corrected approximation of what a physical backlight
/// dimmed to that duty cycle actually looks like to the eye, not a flat
/// linear scale-down (which would look far darker than the real panel at
/// the same PWM level).
#[must_use]
pub fn dim_factor() -> f32 {
    (f32::from(pico_link_core::DIM_BACKLIGHT_PERMILLE) / 1000.0).powf(1.0 / 2.2)
}
