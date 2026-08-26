//! Platform capability bundle: the traits the app core is injected with
//! (`DisplaySurface`, `InputSource`, `Clock`, `Storage`, `PowerControl`),
//! plus the [`Platform`] trait that groups them. This is the seam that
//! lets a single, platform-free app core (`crate::app`/`crate::run`) run
//! unmodified across every concrete run mode a caller wires up — headless,
//! windowed, or a future real-board target — each supplying its own
//! implementations of these traits.
//!
//! The render core itself — including the real [`FrameBuffer565`]
//! definition `DisplaySurface::flush` refers to — lives in `crate::render`;
//! it's re-exported here only so this module's signatures stay meaningful
//! without a second definition.

use crate::input::NavIntent;
use std::time::Instant;

pub use crate::render::FrameBuffer565;

/// Requested display power state, for the idle-screensaver seam. `Off`
/// means "blank the display to save power / avoid burn-in while idle";
/// `On` means "restore normal output". `crate::run::run` is the caller
/// that drives these transitions off its idle-input clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayPower {
    On,
    Off,
}

/// Transfers the shared framebuffer to a physical or virtual display.
/// Implementations: headless (PNG capture), windowed (minifb), real-target
/// (ST7789 over SPI).
pub trait DisplaySurface {
    type Error;

    /// # Errors
    ///
    /// Returns `Self::Error` if the framebuffer could not be transferred to
    /// the underlying display (e.g. an SPI write failure on real hardware).
    fn flush(&mut self, framebuffer: &FrameBuffer565) -> Result<(), Self::Error>;

    /// Requests a display power transition (blank on idle / restore on
    /// wake). Same error-absorption contract as [`DisplaySurface::flush`]:
    /// a device-specific failure (e.g. an SPI write error toggling a
    /// backlight-control pin) surfaces as `Self::Error` here; `run` is
    /// responsible for absorbing it rather than propagating it into the
    /// platform-free core, mirroring how it already handles `flush`
    /// failures.
    ///
    /// # Errors
    ///
    /// Returns `Self::Error` if the power transition could not be applied
    /// to the underlying display.
    fn set_power(&mut self, power: DisplayPower) -> Result<(), Self::Error>;
}

/// Polls for input, already resolved to the semantic `NavIntent` level
/// (see `crate::input`). Per the ADR, raw platform events (encoder ticks,
/// keycodes) stay driver-local and are mapped to `NavIntent` before
/// reaching this trait.
pub trait InputSource {
    fn poll(&mut self) -> Vec<NavIntent>;
}

/// Wall-clock access, injected so the app core never calls platform time
/// APIs directly. `std::time::Instant` is available on the host today, and
/// the RP2350 side will supply an equivalent via its own `Clock` impl, so
/// no custom time type is needed yet.
pub trait Clock {
    fn now(&self) -> Instant;
}

/// Persistent key/value storage. Implementations: native filesystem
/// (emulator), NVS (a future board target).
pub trait Storage {
    type Error;

    fn get(&self, key: &str) -> Option<Vec<u8>>;

    /// # Errors
    ///
    /// Returns `Self::Error` if the value could not be persisted (e.g. an
    /// NVS write failure on real hardware, or a filesystem error on host).
    fn set(&mut self, key: &str, value: Vec<u8>) -> Result<(), Self::Error>;

    /// Removes `key` if present; a no-op (still `Ok`) if it was already
    /// absent.
    ///
    /// Default implementation overwrites `key` with an empty blob rather
    /// than truly deleting it, so every existing `Storage` implementor
    /// keeps compiling unchanged -- callers that already treat a missing
    /// key and an empty blob identically can't tell the difference either
    /// way. Implementations backed by a store with a real delete
    /// operation (NVS's `embedded_svc::storage::StorageBase::remove`, the
    /// emulator's `HashMap::remove`) should override this to actually free
    /// the entry, so the key doesn't linger forever (relevant on a
    /// size-constrained NVS partition) -- see
    /// `emulator::platform::storage::FileStorage`.
    ///
    /// # Errors
    ///
    /// Returns `Self::Error` under the same conditions as [`Storage::set`].
    fn remove(&mut self, key: &str) -> Result<(), Self::Error> {
        self.set(key, Vec::new())
    }
}

/// One redacted-lifetime request to emit output (e.g. type text, or in the
/// future press a control key) through whatever output sink a call site
/// wires up — the payload [`crate::render::Action::Emit`] carries. Nothing
/// in this crate currently produces or consumes one (see
/// [`Platform`]'s doc comment: no capability trait exposes an output sink
/// today), but the type stays defined here so the render core's `Action`
/// enum and `Navigator::take_output` seam keep compiling against a real
/// shape — a future output capability (e.g. an audio-control or
/// Bluetooth-pairing action) can reuse this exact plumbing.
///
/// # Security: no content-printing `Debug`
///
/// Deliberately does **not** derive `Debug` — that would put whatever
/// payload it carries into logs/panic messages unredacted. [`Debug`] is
/// hand-written below (delegating to [`OutputRequestBody`]'s own redacting
/// impl) so callers can still log/assert on an `OutputRequest` without a
/// plaintext leak.
#[derive(Clone, PartialEq, Eq)]
pub struct OutputRequest {
    pub body: OutputRequestBody,
}

impl std::fmt::Debug for OutputRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputRequest").field("body", &self.body).finish()
    }
}

/// A single non-secret control keystroke an [`OutputStep::Key`] can
/// request. Deliberately a closed, tiny set (not a general keycode enum) —
/// `Tab`/`Enter` are placeholders from this seam's original keyboard-typing
/// use case; a future output capability can extend or replace this set
/// entirely without reshaping [`OutputStep`]/[`OutputRequestBody`].
///
/// Carries no secret, so [`OutputStep`]'s hand-written `Debug` renders it
/// literally rather than redacting it (contrast `OutputStep::Type`'s
/// payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HidKey {
    Tab,
    Enter,
}

impl HidKey {
    /// The ASCII control character this key conventionally maps to (`'\t'`
    /// for [`HidKey::Tab`], `'\n'` for [`HidKey::Enter`]) — lets an output
    /// sink reuse a char-to-code lookup table for a lone control key
    /// instead of hand-rolling a second mapping.
    #[must_use]
    pub fn as_char(self) -> char {
        match self {
            HidKey::Tab => '\t',
            HidKey::Enter => '\n',
        }
    }
}

/// One step of an [`OutputRequestBody::Sequence`]: either type a string of
/// characters, or press a single control key (see [`HidKey`]). Together
/// these let a caller express a multi-step action ("type X, press Tab,
/// type Y") as a single atomic [`OutputRequest`] instead of several
/// separate ones an output sink could see interleaved with unrelated
/// activity.
#[derive(Clone, PartialEq, Eq)]
pub enum OutputStep {
    Type(String),
    Key(HidKey),
}

impl std::fmt::Debug for OutputStep {
    /// Redacts `Type`'s payload exactly like [`OutputRequestBody::TypeText`]'s
    /// existing redaction (character count only); `Key` carries no secret
    /// (see [`HidKey`]'s doc comment) so it's rendered literally.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Type(text) => write!(f, "Type(<redacted, {} chars>)", text.chars().count()),
            Self::Key(key) => write!(f, "Key({key:?})"),
        }
    }
}

/// The payload of an [`OutputRequest`]. `TypeText` is a single field's
/// plaintext with no control keys, kept as its own variant rather than a
/// one-element `Sequence` for the simple single-string case. `Sequence` is
/// the general shape: an ordered list of [`OutputStep`]s, for actions that
/// need more than one piece of text and/or a control keystroke in between.
#[derive(Clone, PartialEq, Eq)]
pub enum OutputRequestBody {
    TypeText(String),
    Sequence(Vec<OutputStep>),
}

impl std::fmt::Debug for OutputRequestBody {
    /// Redacts `TypeText`'s carried text exactly as before; `Sequence`
    /// delegates to `Vec<OutputStep>`'s own (derived) `Debug`, which in
    /// turn calls each [`OutputStep`]'s hand-written, redacting `Debug` —
    /// see [`OutputRequest`]'s doc comment on why `Debug` is hand-written
    /// here instead of derived.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TypeText(text) => write!(f, "TypeText(<redacted, {} chars>)", text.chars().count()),
            Self::Sequence(steps) => f.debug_tuple("Sequence").field(steps).finish(),
        }
    }
}

/// Connection state of a wireless link (e.g. Bluetooth pairing/streaming),
/// surfaced so the UI can show connection status and gate link-dependent
/// actions on `Connected` rather than let a user try to act on a
/// disconnected/nonexistent link. Carried over from this project's
/// previous incarnation's BLE HID keyboard-output link; kept as a
/// general-purpose connection-state indicator for Pico Link's own
/// Bluetooth audio link — no capability trait exposes it yet (that's
/// future wiring work), but the shape is ready to reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HidLinkState {
    /// The platform has no wireless-link capability at all (e.g. a build
    /// without Bluetooth support).
    Unavailable,
    /// The capability exists but no host is currently paired/connected.
    Disconnected,
    /// Pairing is in progress.
    Pairing,
    /// Paired and connected.
    Connected,
}

/// Controls the device's deeper (below display-blank) power state — the
/// deep-sleep tier of the idle-power policy. Implementations: a real
/// board's deep-sleep actuator (wake on some GPIO input), the emulator's
/// recording/no-op stub. `crate::run::run` is the only caller — see
/// `crate::power` for the setting that decides whether (and when) it ever
/// actually does.
pub trait PowerControl {
    /// Whether the device is currently running on external (e.g. USB)
    /// power rather than battery. `crate::run::run`'s deep-sleep policy
    /// treats this as an unconditional veto — regardless of idle time or
    /// link state — so plugging in for development/flashing (or, on
    /// hardware with a dumb USB charger, just charging) never drops the
    /// device into deep sleep out from under whatever is using that
    /// connection.
    fn on_external_power(&self) -> bool;

    /// Enters deep sleep. On real hardware this is a one-way trip for the
    /// running process: the RP2350's equivalent low-power halt never
    /// returns — the device is fully off until a wake source (the
    /// joystick/button GPIOs) fires an interrupt and the chip cold
    /// boots, re-running `main` from scratch. Host implementations (no
    /// real hardware to power down) instead just record that the call
    /// happened, so `crate::run::run`'s policy is testable without a
    /// device.
    fn enter_deep_sleep(&mut self);
}

/// Capability bundle: groups the five injected platform traits behind a
/// single generic parameter, so app-wiring code (the unified main loop)
/// can be generic over "a platform" instead of threading five separate
/// type parameters through every function signature.
///
/// `Power`/`power()` is the deep-sleep capability: `crate::run::run` is
/// its only caller — see `crate::power` for the setting that decides
/// whether (and when) it ever actually fires.
pub trait Platform {
    type Display: DisplaySurface;
    type Input: InputSource;
    type Clock: Clock;
    type Storage: Storage;
    type Power: PowerControl;

    fn display(&mut self) -> &mut Self::Display;
    fn input(&mut self) -> &mut Self::Input;
    fn clock(&self) -> &Self::Clock;
    fn storage(&mut self) -> &mut Self::Storage;
    fn power(&mut self) -> &mut Self::Power;
}
