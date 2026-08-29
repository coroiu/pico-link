//! The unified, `Platform`-generic main loop: the one piece of code that
//! drives an [`App`] regardless of which run mode (headless, windowed, a
//! future real-board target) it's given a [`Platform`] for. The three
//! modes must differ *only* in which concrete
//! `DisplaySurface`/`InputSource`/`Clock`/`Storage`/`PowerControl` they
//! hand to [`run`] — this function itself never branches on which mode
//! it's in.
//!
//! ```text
//! loop {
//!     let intents = input.poll();
//!     app.handle_input(intents);
//!     if app.dirty() {
//!         let fb = app.render();
//!         display.flush(&fb);
//!     }
//!     sleep(frame_budget - elapsed);
//! }
//! ```
//!
//! Note the `if app.dirty()` gate: render+flush only happen when
//! something actually changed, not unconditionally every iteration — an
//! idle loop (no input) still spins at `frame_budget` cadence but skips
//! the expensive part entirely.
//!
//! # Idle/wake
//!
//! `run` owns a second, small state machine — `Active`/`Asleep` — layered
//! on top of the above. It is entirely local to this loop: `App` and
//! `Navigator`/`NavIntent` know nothing about it (`DisplaySurface::
//! set_power` is the seam).
//!
//! ```text
//! if idle_timeout is None: never sleeps (the screensaver-disabled seam).
//! intents = input.poll()                 // every iteration, asleep or not
//! if intents non-empty:
//!     last_input = now
//!     if Asleep: set_power(On); Active; app.mark_dirty(); DROP intents
//!     else:      app.handle_input(intents)
//! else if Active && now - last_input >= idle_timeout:
//!     set_power(Off); Asleep
//! if app.dirty() && Active: render + flush // blanked while Asleep
//! ```
//!
//! Two decisions worth calling out because they are easy to get backwards:
//!
//! - The wake-triggering input is **never** forwarded to `app.handle_input`
//!   — the first input after `Asleep` only wakes the display; navigation
//!   resumes on the *next* poll. This is why waking still calls
//!   `app.mark_dirty()` directly: the framebuffer's content never changed
//!   while blanked, but the display needs a fresh flush once it's back on.
//!
//! # Deep sleep
//!
//! A second, deeper tier layered below the display-blank tier above,
//! gated by its own `deep_sleep_timeout: Option<Duration>` parameter —
//! mirroring `idle_timeout`'s own "`None` disables it entirely" contract.
//! When `Some(Tb)`, every iteration with no polled input additionally
//! checks, using the exact same `last_input` clock the screensaver tier
//! already tracks:
//!
//! ```text
//! if now - last_input >= Tb and not power().on_external_power():
//!     power().enter_deep_sleep()   // fires at most once per `run` call
//! ```
//!
//! `Tb` is expected to be well past `Ta` (see `crate::power`'s doc
//! comment), so in practice the display is already blanked by the time
//! this can fire — but the check is independent of `PowerState`, not
//! gated on `Asleep`, so a test (or an unusual `Tb <= Ta` configuration)
//! that jumps straight past `Tb` in one step still behaves correctly.
//!
//! The external-power veto exists so deep sleep can never pull the rug
//! out from under a USB-connected dev/flash/charging session — see
//! `crate::platform::PowerControl`'s doc comment. Once armed, firing is a
//! one-way trip on real hardware (`PowerControl::enter_deep_sleep`'s doc
//! comment); the `deep_sleep_triggered` guard below only matters for host
//! testing (a no-op recording stub would otherwise keep "firing" every
//! subsequent iteration once eligible).
//!
//! # Why `should_continue` instead of an unconditional `loop`
//!
//! A bare infinite loop is exactly right for a real-target firmware
//! binary (it never exits) — but host callers need *some* way to stop the
//! loop (closing the window, an HTTP shutdown signal, or — for headless
//! automated verification — "stop after N frames so a screenshot can be
//! taken and the process can exit"). Rather than hardcode any of those
//! conditions here (which would smuggle a run-mode-specific concept into
//! supposedly mode-generic code), the loop takes a `should_continue`
//! predicate and lets each binary decide what "keep going" means for it.
//! A real-target binary just passes `|| true`.
//!
//! # Why sleeping is a `Clock` method
//!
//! This used to read the other way: `Clock` exposed only `now()`, and the
//! loop called `std::thread::sleep` directly, on the reasoning that sleeping
//! is scheduling, not clock-reading, and `std::thread::sleep` was available
//! on every target this project built for -- so there was no portability
//! reason to route it through an injected trait.
//!
//! That premise held only while every run mode had `std`. This crate is now
//! `no_std` + `alloc` (see `lib.rs`), and the RP2350 firmware target this
//! is heading for has no `std::thread` to sleep on -- "blocking" there means
//! something target-specific (a busy-wait against a hardware timer, a WFI/
//! low-power instruction, an RTOS-less delay loop), which is exactly the
//! kind of platform-specific detail this crate's trait seams exist to keep
//! out of `run`. So the loop no longer calls a sleep primitive directly at
//! all: it asks the injected [`crate::platform::Clock`] to sleep, and each
//! concrete `Clock` impl (the emulator's, backed by `std::thread::sleep`
//! today; a future RP2350 impl backed by whatever the hardware needs)
//! decides what "block for this long" actually means on its target. The
//! trait's *shape* argument from before still holds -- this doesn't turn
//! `Clock` into a general scheduler, it just adds the one sleep primitive
//! `run`'s frame-budget wait actually needs.

use core::time::Duration;

use crate::app::App;
use crate::platform::{Clock, DisplayPower, DisplaySurface, InputSource, Platform, PowerControl};

/// Rolling-average frame-timing accumulator, active only behind the
/// off-by-default `frame-timing` feature. Diagnostic-only: measures where
/// a *rendered* (dirty) frame's time actually goes -- render-into-
/// framebuffer vs. flush-to-display -- using the same injected [`Clock`]
/// the loop itself uses for its frame-budget sleep, so these numbers are
/// directly comparable to `frame_budget`.
///
/// Only dirty frames are counted (a frame that skips render+flush
/// entirely has nothing meaningful to report for either duration), so
/// "30 frames" here means 30 *rendered* frames, however many total loop
/// iterations that spans.
#[cfg(feature = "frame-timing")]
struct FrameTiming {
    render_total: Duration,
    flush_total: Duration,
    count: u32,
}

/// Rate-limits `DisplaySurface` failure logging (both `flush` and
/// `set_power`) so a persistently-failing display doesn't spam the log at
/// frame rate.
///
/// A DMA misconfiguration (or any other persistent hardware fault) can
/// make `flush` fail on *every single frame* -- if the loop discarded the
/// `Result` outright, the screen would just silently freeze on the last
/// good frame with no signal anywhere, looking like a hang rather than a
/// failure. The loop must still stay infallible (device-specific errors
/// are absorbed at the surface adapter, not propagated into the
/// platform-free core) -- this only adds *visibility*, logging a warning
/// on the first failure (so a transition from healthy to broken is never
/// silent) and then every [`FlushErrorTracker::REPEAT_INTERVAL`]th
/// consecutive failure after that (so a *persistent* failure keeps
/// showing up over serial without drowning normal operation in per-frame
/// noise), resetting on the next success so a later failure logs fresh
/// again.
struct FlushErrorTracker {
    consecutive_errors: u32,
    /// Identifies which `DisplaySurface` method this instance is tracking,
    /// for the log line only (e.g. `"DisplaySurface::flush"` vs.
    /// `"DisplaySurface::set_power"`).
    label: &'static str,
}

impl FlushErrorTracker {
    /// Arbitrary, not tuned against a real failure's time-to-notice
    /// requirement: at the 33ms/frame budget this is roughly every 5
    /// seconds, which is frequent enough that a human watching serial
    /// output won't wait long to see it repeat, without being frequent
    /// enough to look like per-frame spam.
    const REPEAT_INTERVAL: u32 = 150;

    const fn new(label: &'static str) -> Self {
        Self { consecutive_errors: 0, label }
    }

    /// Records a failed call and logs a warning if this is the first
    /// failure since the last success, or every `REPEAT_INTERVAL`th one
    /// after that.
    fn on_err(&mut self, error: &impl core::fmt::Debug) {
        self.consecutive_errors += 1;
        if self.consecutive_errors == 1 || self.consecutive_errors % Self::REPEAT_INTERVAL == 0 {
            log::warn!("{} failed ({} consecutive): {error:?}", self.label, self.consecutive_errors);
        }
    }

    /// Records a successful call, resetting the consecutive-error count
    /// so a later failure is treated as a fresh "just started failing"
    /// event (and logged immediately) rather than a continuation of an
    /// old, already-resolved one.
    fn on_ok(&mut self) {
        self.consecutive_errors = 0;
    }
}

/// The idle/wake state machine's two states (see the module doc). Local to
/// `run` — never exposed to `App`/`Navigator`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PowerState {
    Active,
    Asleep,
}

#[cfg(feature = "frame-timing")]
impl FrameTiming {
    const WINDOW: u32 = 30;

    const fn new() -> Self {
        Self { render_total: Duration::ZERO, flush_total: Duration::ZERO, count: 0 }
    }

    /// Records one dirty frame's render/flush durations; logs and resets
    /// the accumulator once `WINDOW` frames have been recorded.
    fn record(&mut self, render: Duration, flush: Duration) {
        self.render_total += render;
        self.flush_total += flush;
        self.count += 1;

        if self.count >= Self::WINDOW {
            let n = f64::from(self.count);
            let avg_render_ms = self.render_total.as_secs_f64() * 1000.0 / n;
            let avg_flush_ms = self.flush_total.as_secs_f64() * 1000.0 / n;
            let avg_total_ms = avg_render_ms + avg_flush_ms;
            let fps = if avg_total_ms > 0.0 { 1000.0 / avg_total_ms } else { 0.0 };
            log::info!(
                "frame-timing: render={avg_render_ms:.2}ms flush={avg_flush_ms:.2}ms total={avg_total_ms:.2}ms -> {fps:.1}fps (avg over {} rendered frames)",
                self.count
            );
            *self = Self::new();
        }
    }
}

/// What one [`Runner::step`] call actually did, for a caller that wants to
/// know without re-deriving it from `App`/`DisplaySurface` state. Currently
/// informational only (no caller in this crate branches on it -- `run`'s
/// `while` loop below ignores the return value entirely), but a future
/// caller with no frame-budget sleep of its own to drive off (e.g. the M1b
/// `ui-ffi` staticlib's `pl_ui_tick`, called from C on C's own clock) can use
/// it to decide whether a render actually happened this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// No input arrived and nothing was rendered this step (may still have
    /// changed `PowerState`, e.g. crossing the idle timeout).
    Idle,
    /// Input was polled and forwarded to `App`, but nothing was dirty (or
    /// the display was asleep), so no render/flush happened.
    InputHandled,
    /// `App` was dirty and `DisplaySurface::flush` was called (regardless
    /// of whether it succeeded -- see [`FlushErrorTracker`] for how a
    /// failure is surfaced instead of propagated).
    Rendered,
}

/// One iteration's worth of the state the module doc's pseudocode
/// describes: input poll, idle/deep-sleep bookkeeping, and a dirty-gated
/// render+flush. Holds everything that must persist *between* iterations
/// (`PowerState`, `last_input`, the deep-sleep latch, the error-rate
/// limiters, and -- behind the `frame-timing` feature -- the rolling
/// accumulator) so a caller can drive it one step at a time instead of only
/// via the all-in-one [`run`] loop below.
///
/// This split exists so the *loop shape* (poll → dirty-gated render/flush →
/// sleep) is reusable by something other than [`run`]'s own
/// `while should_continue()` -- e.g. a future caller whose own clock/timing
/// isn't `crate::platform::Clock::sleep`-shaped -- without duplicating any
/// of the idle/deep-sleep/error-tracking logic above. `run` itself is now a
/// thin driver: read `now`, call [`Runner::step`], repeat.
pub struct Runner {
    #[cfg(feature = "frame-timing")]
    frame_timing: FrameTiming,
    flush_errors: FlushErrorTracker,
    power_errors: FlushErrorTracker,
    power_state: PowerState,
    last_input: crate::platform::Instant,
    /// Set once `PowerControl::enter_deep_sleep` has fired, so a host
    /// test's no-op recording stub (which, unlike real hardware, actually
    /// returns) doesn't re-fire it every subsequent iteration once
    /// eligible -- see the module doc's "Deep sleep" section.
    deep_sleep_triggered: bool,
    idle_timeout: Option<Duration>,
    deep_sleep_timeout: Option<Duration>,
}

impl Runner {
    /// Builds a fresh `Runner`, starting `Active` with `last_input` set to
    /// `now` (i.e. the idle clock starts counting from construction, not
    /// from some earlier unknown point) -- mirroring what the pre-refactor
    /// `run` did inline at the top of its own function body.
    #[must_use]
    pub fn new(now: crate::platform::Instant, idle_timeout: Option<Duration>, deep_sleep_timeout: Option<Duration>) -> Self {
        Self {
            #[cfg(feature = "frame-timing")]
            frame_timing: FrameTiming::new(),
            flush_errors: FlushErrorTracker::new("DisplaySurface::flush"),
            power_errors: FlushErrorTracker::new("DisplaySurface::set_power"),
            power_state: PowerState::Active,
            last_input: now,
            deep_sleep_triggered: false,
            idle_timeout,
            deep_sleep_timeout,
        }
    }

    /// Runs exactly one iteration of the module doc's loop body against
    /// `platform`/`app`, treating `now` as this step's `frame_start` (the
    /// caller reads the clock, not `step` itself, so a caller with its own
    /// timing source -- e.g. C owning the clock over FFI -- doesn't need a
    /// `crate::platform::Clock` at all). Does **not** sleep for the
    /// remainder of any frame budget -- that stays [`run`]'s concern, since
    /// a step-at-a-time caller may have a completely different idea of
    /// pacing (or none at all).
    pub fn step<P: Platform>(&mut self, platform: &mut P, app: &mut App, now: crate::platform::Instant) -> StepOutcome
    where
        <P::Display as DisplaySurface>::Error: core::fmt::Debug,
    {
        let frame_start = now;
        let intents = platform.input().poll();
        let mut outcome = StepOutcome::Idle;

        if intents.is_empty() {
            if let Some(idle_timeout) = self.idle_timeout {
                if self.power_state == PowerState::Active && frame_start.saturating_duration_since(self.last_input) >= idle_timeout {
                    match platform.display().set_power(DisplayPower::Off) {
                        Ok(()) => self.power_errors.on_ok(),
                        Err(error) => self.power_errors.on_err(&error),
                    }
                    self.power_state = PowerState::Asleep;
                }
            }

            // Deep sleep (see the module doc's "Deep sleep" section):
            // independent of `power_state` above (not gated on already
            // being `Asleep`) -- in practice `Tb` is well past `Ta` so the
            // screen is already blanked by the time this can fire, but
            // the check itself only cares about elapsed idle time and
            // external power.
            if let Some(deep_sleep_timeout) = self.deep_sleep_timeout {
                let idle_elapsed = frame_start.saturating_duration_since(self.last_input);
                if !self.deep_sleep_triggered && idle_elapsed >= deep_sleep_timeout && !platform.power().on_external_power() {
                    platform.power().enter_deep_sleep();
                    self.deep_sleep_triggered = true;
                }
            }
        } else {
            self.last_input = frame_start;
            match self.power_state {
                PowerState::Asleep => {
                    // The wake-triggering input only wakes the display —
                    // it is deliberately never forwarded to
                    // `app.handle_input` (see the module doc). `App`
                    // never learns it was asleep; `mark_dirty` forces the
                    // fresh flush the just-woken display needs even
                    // though nothing on screen actually changed.
                    match platform.display().set_power(DisplayPower::On) {
                        Ok(()) => self.power_errors.on_ok(),
                        Err(error) => self.power_errors.on_err(&error),
                    }
                    self.power_state = PowerState::Active;
                    app.mark_dirty();
                }
                PowerState::Active => {
                    app.handle_input(intents);
                    outcome = StepOutcome::InputHandled;
                }
            }
        }

        // Blanked while `Asleep`: skip render+flush entirely rather than
        // rendering into a framebuffer nothing will show. `app.dirty()`
        // deliberately stays untouched by this gate (not cleared, not
        // read via `render()`) — a dirty flag survives blanked frames so
        // the next real wake renders it immediately.
        if app.dirty() && self.power_state == PowerState::Active {
            #[cfg(feature = "frame-timing")]
            let render_start = platform.clock().now();

            let framebuffer = app.render();

            #[cfg(feature = "frame-timing")]
            let render_end = platform.clock().now();

            // A flush failure (e.g. a real SPI write error on hardware) is
            // not something this loop can meaningfully recover from frame
            // to frame; device-specific errors are absorbed at the
            // surface adapter, not propagated into the platform-free
            // core. Still not panicking here (the loop stays infallible)
            // -- but no longer silently discarded either:
            // `FlushErrorTracker` makes a persistent failure visible over
            // serial (rate-limited) instead of looking like an
            // inexplicable frozen screen.
            match platform.display().flush(framebuffer) {
                Ok(()) => self.flush_errors.on_ok(),
                Err(error) => self.flush_errors.on_err(&error),
            }
            outcome = StepOutcome::Rendered;

            #[cfg(feature = "frame-timing")]
            {
                let flush_end = platform.clock().now();
                self.frame_timing.record(
                    render_end.saturating_duration_since(render_start),
                    flush_end.saturating_duration_since(render_end),
                );
            }
        }

        outcome
    }
}

/// Runs the app loop against `platform` until `should_continue` returns
/// `false`. `frame_budget` is the target time per iteration (input poll +
/// app step + render + flush); if an iteration finishes early, the
/// remainder of the budget is spent asleep so the loop doesn't spin.
///
/// Takes `platform`/`app` by `&mut` (rather than by value) so callers
/// retain ownership after `run` returns — e.g. a headless caller that
/// wants to encode a PNG from its concrete `HeadlessSurface` once the loop
/// stops.
///
/// `<P::Display as DisplaySurface>::Error: Debug` is required so a
/// persistently-failing `flush` can be logged (see [`FlushErrorTracker`]).
///
/// `idle_timeout` is the idle-screensaver seam: `None` disables it
/// entirely (the display is never told to power off); `Some` blanks the
/// display via `DisplaySurface::set_power` after that much wall-clock time
/// with no polled input, and restores it on the next input (see the
/// module doc for the full state machine).
///
/// `deep_sleep_timeout` is the deeper power tier's seam (see the module
/// doc's "Deep sleep" section): `None` disables it entirely, mirroring
/// `idle_timeout`'s own contract; `Some` calls
/// `PowerControl::enter_deep_sleep` after that much idle time, provided
/// the platform isn't on external power.
///
/// This is now a thin loop around [`Runner::step`] -- see that type's doc
/// comment for why the state it used to hold inline was pulled out into a
/// reusable struct.
pub fn run<P: Platform>(
    platform: &mut P,
    app: &mut App,
    frame_budget: Duration,
    idle_timeout: Option<Duration>,
    deep_sleep_timeout: Option<Duration>,
    mut should_continue: impl FnMut() -> bool,
) where
    <P::Display as DisplaySurface>::Error: core::fmt::Debug,
{
    let mut runner = Runner::new(platform.clock().now(), idle_timeout, deep_sleep_timeout);

    while should_continue() {
        let frame_start = platform.clock().now();

        runner.step(platform, app, frame_start);

        let elapsed = platform.clock().now().saturating_duration_since(frame_start);
        if let Some(remaining) = frame_budget.checked_sub(elapsed) {
            platform.clock().sleep(remaining);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::NavIntent;
    use crate::platform::FrameBuffer565;
    use std::cell::{Cell, RefCell};
    use core::convert::Infallible;
    use std::rc::Rc;
    use crate::platform::Instant;

    /// A `PowerControl` stub whose `on_external_power` is settable and
    /// whose `enter_deep_sleep` calls are counted, via shared `Rc` handles.
    #[derive(Clone)]
    struct RecordingPower {
        external_power: Rc<Cell<bool>>,
        deep_sleep_calls: Rc<Cell<u32>>,
    }
    impl RecordingPower {
        fn new(external_power: bool) -> Self {
            Self { external_power: Rc::new(Cell::new(external_power)), deep_sleep_calls: Rc::new(Cell::new(0)) }
        }
        fn deep_sleep_call_count(&self) -> u32 {
            self.deep_sleep_calls.get()
        }
    }
    impl PowerControl for RecordingPower {
        fn on_external_power(&self) -> bool {
            self.external_power.get()
        }
        fn enter_deep_sleep(&mut self) {
            self.deep_sleep_calls.set(self.deep_sleep_calls.get() + 1);
        }
    }

    struct StubDisplay {
        flush_count: Rc<RefCell<u32>>,
    }
    impl DisplaySurface for StubDisplay {
        type Error = Infallible;
        fn flush(&mut self, _framebuffer: &FrameBuffer565) -> Result<(), Self::Error> {
            *self.flush_count.borrow_mut() += 1;
            Ok(())
        }
        fn set_power(&mut self, _power: crate::platform::DisplayPower) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    /// Error type for [`FailingStubDisplay`]. `Debug`-only (no `Display` or
    /// error-trait impl) -- deliberately the bare minimum `run`'s trait
    /// bound (`<P::Display as DisplaySurface>::Error: core::fmt::Debug`)
    /// requires, so this test doesn't accidentally prove more than the
    /// bound actually demands. No error-trait impl exists to name here
    /// regardless: this crate is `no_std` (see `lib.rs`), and `core` has no
    /// `Error` trait of its own to implement -- there was never real code
    /// behind the old `std::error::Error` wording in this comment, only
    /// the description of what `StubFlushError` deliberately omits.
    #[derive(Debug)]
    struct StubFlushError;

    /// A `DisplaySurface` whose `flush` always fails -- for proving
    /// `run` tolerates a *persistently* failing display without panicking,
    /// as opposed to `StubDisplay`'s always-succeeds `Infallible` case
    /// above.
    struct FailingStubDisplay;
    impl DisplaySurface for FailingStubDisplay {
        type Error = StubFlushError;
        fn flush(&mut self, _framebuffer: &FrameBuffer565) -> Result<(), Self::Error> {
            Err(StubFlushError)
        }
        fn set_power(&mut self, _power: crate::platform::DisplayPower) -> Result<(), Self::Error> {
            Err(StubFlushError)
        }
    }

    struct QueuedInput(Vec<Vec<NavIntent>>);
    impl InputSource for QueuedInput {
        fn poll(&mut self) -> Vec<NavIntent> {
            if self.0.is_empty() {
                Vec::new()
            } else {
                self.0.remove(0)
            }
        }
    }

    #[derive(Default)]
    struct StubStorage;
    impl crate::platform::Storage for StubStorage {
        type Error = Infallible;
        fn get(&self, _key: &str) -> Option<Vec<u8>> {
            None
        }
        fn set(&mut self, _key: &str, _value: Vec<u8>) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[derive(Default, Clone, Copy)]
    struct StubClock;
    impl Clock for StubClock {
        fn now(&self) -> Instant {
            // A fixed reading is sufficient: none of the tests using
            // `StubClock` exercise idle/deep-sleep timing (that's what
            // `ControllableClock` below is for), so only monotonicity
            // (trivially true for a constant) matters here.
            Instant::from_micros(0)
        }
        fn sleep(&self, _duration: Duration) {
            // No-op: unit tests must not actually block real wall-clock
            // time. Every test here passes `frame_budget:
            // Duration::from_millis(0)`, so a real sleep would never be
            // more than a few microseconds anyway, but a no-op keeps the
            // test suite's runtime independent of that even so.
        }
    }

    struct StubPlatform {
        display: StubDisplay,
        input: QueuedInput,
        clock: StubClock,
        storage: StubStorage,
        power: RecordingPower,
    }
    impl Platform for StubPlatform {
        type Display = StubDisplay;
        type Input = QueuedInput;
        type Clock = StubClock;
        type Storage = StubStorage;
        type Power = RecordingPower;

        fn display(&mut self) -> &mut Self::Display {
            &mut self.display
        }
        fn input(&mut self) -> &mut Self::Input {
            &mut self.input
        }
        fn clock(&self) -> &Self::Clock {
            &self.clock
        }
        fn storage(&mut self) -> &mut Self::Storage {
            &mut self.storage
        }
        fn power(&mut self) -> &mut Self::Power {
            &mut self.power
        }
    }

    /// Mirrors `StubPlatform`, but with `FailingStubDisplay` in place of
    /// `StubDisplay` -- used only by the flush-error-tolerance test
    /// below, so `run`'s `<P::Display as DisplaySurface>::Error: Debug`
    /// bound is exercised against a real (non-`Infallible`) error type,
    /// not just satisfied vacuously.
    struct FailingStubPlatform {
        display: FailingStubDisplay,
        input: QueuedInput,
        clock: StubClock,
        storage: StubStorage,
        power: RecordingPower,
    }
    impl Platform for FailingStubPlatform {
        type Display = FailingStubDisplay;
        type Input = QueuedInput;
        type Clock = StubClock;
        type Storage = StubStorage;
        type Power = RecordingPower;

        fn display(&mut self) -> &mut Self::Display {
            &mut self.display
        }
        fn input(&mut self) -> &mut Self::Input {
            &mut self.input
        }
        fn clock(&self) -> &Self::Clock {
            &self.clock
        }
        fn storage(&mut self) -> &mut Self::Storage {
            &mut self.storage
        }
        fn power(&mut self) -> &mut Self::Power {
            &mut self.power
        }
    }

    #[test]
    fn a_persistently_failing_flush_does_not_panic_and_the_loop_keeps_running() {
        // A display that fails on EVERY frame, not just an occasional one
        // (that's what a persistent hardware fault looks like). Every
        // queued frame carries a `Next` intent so `app.dirty()` is true
        // and `flush` (which always errors) is genuinely attempted on
        // every single iteration, not skipped by the dirty-gate.
        const ITERATIONS: usize = 10;
        let mut platform = FailingStubPlatform {
            display: FailingStubDisplay,
            input: QueuedInput(vec![vec![NavIntent::Down]; ITERATIONS]),
            clock: StubClock,
            storage: StubStorage,
            power: RecordingPower::new(false),
        };
        let mut app = App::new(240, 240);

        let mut iterations = 0;
        // The absence of a panic across every one of these iterations
        // IS the assertion: `run` took the `Err` branch (not `Ok`)
        // `ITERATIONS` times in a row and kept going regardless.
        run(&mut platform, &mut app, Duration::from_millis(0), None, None, || {
            iterations += 1;
            iterations <= ITERATIONS
        });

        assert_eq!(iterations, ITERATIONS + 1, "should_continue is checked once more after the last real iteration");
    }

    #[test]
    fn run_stops_when_should_continue_returns_false() {
        let flush_count = Rc::new(RefCell::new(0));
        let mut platform = StubPlatform {
            display: StubDisplay { flush_count: Rc::clone(&flush_count) },
            input: QueuedInput(Vec::new()),
            clock: StubClock,
            storage: StubStorage,
            power: RecordingPower::new(false),
        };
        let mut app = App::new(10, 10);

        let mut iterations = 0;
        run(&mut platform, &mut app, Duration::from_millis(0), None, None, || {
            iterations += 1;
            iterations <= 3
        });

        assert_eq!(iterations, 4, "should_continue is checked once more after the last real iteration");
        // Only the first iteration is dirty (fresh `App`); the rest have
        // nothing new to render.
        assert_eq!(*flush_count.borrow(), 1);
    }

    #[test]
    fn polled_intents_are_forwarded_to_the_app_and_trigger_a_flush() {
        let flush_count = Rc::new(RefCell::new(0));
        let mut platform = StubPlatform {
            display: StubDisplay { flush_count: Rc::clone(&flush_count) },
            input: QueuedInput(vec![vec![], vec![NavIntent::Down], vec![]]),
            clock: StubClock,
            storage: StubStorage,
            power: RecordingPower::new(false),
        };
        let mut app = App::new(240, 240);

        let mut iterations = 0;
        run(&mut platform, &mut app, Duration::from_millis(0), None, None, || {
            iterations += 1;
            iterations <= 3
        });

        // Frame 1: fresh app, dirty -> flush. Frame 2: `Next` intent ->
        // dirty -> flush. Frame 3: no new input -> not dirty -> no flush.
        assert_eq!(*flush_count.borrow(), 2);
    }

    // --- Idle/wake ---
    //
    // `StubClock` above returns a fixed reading, which is useless for
    // deterministically crossing an idle timeout in a fast unit test --
    // hence `ControllableClock`, whose `now()` is scriptable from the test
    // body (typically from inside the `should_continue` closure, so the
    // clock advances exactly once per iteration, under the test's full
    // control). `RecordingDisplay` mirrors `StubDisplay` but records every
    // `set_power` call (and, like `StubDisplay`, every `flush`) so the
    // acceptance tests below can assert on the exact sequence of power
    // transitions `run` requested.

    /// A `Clock` whose `now()` is a plain `Rc<RefCell<Instant>>` the test
    /// advances explicitly with [`ControllableClock::advance`] -- unlike
    /// `StubClock`, time here only ever moves when the test says so.
    #[derive(Clone)]
    struct ControllableClock(Rc<RefCell<Instant>>);

    impl ControllableClock {
        fn new() -> Self {
            // The reference point is arbitrary (see `Instant`'s doc
            // comment) -- these tests only ever read relative elapsed time
            // via `saturating_duration_since`, never the raw value, so
            // starting at zero is exactly as valid as starting from a real
            // clock reading.
            Self(Rc::new(RefCell::new(Instant::from_micros(0))))
        }

        fn advance(&self, duration: Duration) {
            let mut now = self.0.borrow_mut();
            *now += duration;
        }
    }

    impl Clock for ControllableClock {
        fn now(&self) -> Instant {
            *self.0.borrow()
        }
        fn sleep(&self, _duration: Duration) {
            // No-op, same rationale as `StubClock::sleep` -- these tests
            // advance time explicitly via `advance`, never by actually
            // blocking.
        }
    }

    /// A `DisplaySurface` that records every `set_power` call (in order)
    /// and counts `flush` calls, via shared handles so a test can inspect
    /// both after `platform` has been moved into `run`.
    #[derive(Default)]
    struct RecordingDisplay {
        power_calls: Rc<RefCell<Vec<crate::platform::DisplayPower>>>,
        flush_count: Rc<RefCell<u32>>,
    }

    impl DisplaySurface for RecordingDisplay {
        type Error = Infallible;
        fn flush(&mut self, _framebuffer: &FrameBuffer565) -> Result<(), Self::Error> {
            *self.flush_count.borrow_mut() += 1;
            Ok(())
        }
        fn set_power(&mut self, power: crate::platform::DisplayPower) -> Result<(), Self::Error> {
            self.power_calls.borrow_mut().push(power);
            Ok(())
        }
    }

    struct RecordingPlatform {
        display: RecordingDisplay,
        input: QueuedInput,
        clock: ControllableClock,
        storage: StubStorage,
        power: RecordingPower,
    }
    impl Platform for RecordingPlatform {
        type Display = RecordingDisplay;
        type Input = QueuedInput;
        type Clock = ControllableClock;
        type Storage = StubStorage;
        type Power = RecordingPower;

        fn display(&mut self) -> &mut Self::Display {
            &mut self.display
        }
        fn input(&mut self) -> &mut Self::Input {
            &mut self.input
        }
        fn clock(&self) -> &Self::Clock {
            &self.clock
        }
        fn storage(&mut self) -> &mut Self::Storage {
            &mut self.storage
        }
        fn power(&mut self) -> &mut Self::Power {
            &mut self.power
        }
    }

    /// Everything [`recording_platform`] builds: the platform itself plus
    /// the shared handles a test needs to inspect it after it has been
    /// moved into `run` (`power_calls`, `flush_count`), and the
    /// `ControllableClock` handle used to advance time from the test's
    /// `should_continue` closure.
    struct RecordingSetup {
        platform: RecordingPlatform,
        clock: ControllableClock,
        power_calls: Rc<RefCell<Vec<crate::platform::DisplayPower>>>,
        flush_count: Rc<RefCell<u32>>,
    }

    /// `inputs` is the exact per-iteration `NavIntent` script `QueuedInput`
    /// will hand back, one `Vec` per `poll()` call.
    fn recording_platform(inputs: Vec<Vec<NavIntent>>) -> RecordingSetup {
        let clock = ControllableClock::new();
        let power_calls = Rc::new(RefCell::new(Vec::new()));
        let flush_count = Rc::new(RefCell::new(0));
        let platform = RecordingPlatform {
            display: RecordingDisplay { power_calls: Rc::clone(&power_calls), flush_count: Rc::clone(&flush_count) },
            input: QueuedInput(inputs),
            clock: clock.clone(),
            storage: StubStorage,
            power: RecordingPower::new(false),
        };
        RecordingSetup { platform, clock, power_calls, flush_count }
    }

    #[test]
    fn no_input_past_the_idle_timeout_blanks_the_display_exactly_once_and_stops_flushing() {
        let idle_timeout = Duration::from_secs(120);
        // Never any input at all -- every poll returns empty.
        let RecordingSetup { mut platform, clock, power_calls, flush_count } = recording_platform(vec![Vec::new(); 3]);
        let mut app = App::new(240, 240);

        let mut iterations = 0;
        run(&mut platform, &mut app, Duration::from_millis(0), Some(idle_timeout), None, || {
            iterations += 1;
            // Jump the clock past the timeout right before the *second*
            // iteration's frame_start is read, so iteration 1 sees
            // elapsed == 0 (no sleep yet) and iteration 2 sees elapsed >=
            // idle_timeout (sleep triggers).
            if iterations == 2 {
                clock.advance(idle_timeout + Duration::from_millis(1));
            }
            iterations <= 3
        });

        assert_eq!(*power_calls.borrow(), vec![crate::platform::DisplayPower::Off], "exactly one Off, never repeated once already asleep");
        // Only iteration 1's fresh-`App` render actually flushed;
        // iterations 2 and 3 are blanked (asleep) so their render+flush
        // gate never opens, whether or not anything would have been dirty.
        assert_eq!(*flush_count.borrow(), 1, "no flush happened once asleep");
    }

    #[test]
    fn waking_input_is_swallowed_but_the_next_input_reaches_the_app() {
        let idle_timeout = Duration::from_secs(120);

        // Iteration 1: no input (fresh render). Iteration 2: no input, but
        // the clock jumps past the timeout first -> Off. Iteration 3: a
        // `Next` intent arrives while Asleep -> should wake (On) and be
        // dropped, NOT reach `app.handle_input`.
        let RecordingSetup { mut platform, clock, power_calls, flush_count: _ } = recording_platform(vec![Vec::new(), Vec::new(), vec![NavIntent::Down]]);
        let mut app = App::new(240, 240);
        // Home (the root screen since `pico-link-znb.8`/E7) has no
        // focusable list on its default status face -- `Down` is
        // deliberately unbound there (Tier 1 scope boundary: volume is
        // Tier 2/E16). This test's proof only needs *some* focusable
        // content whose selection visibly moves on `Down`, decoupled from
        // whatever Home's own content happens to be -- a plain pushed
        // `VerticalList` screen, the same shape `navigator.rs`'s own
        // tests use, serves that purpose without coupling this run-loop
        // test to Home's domain-specific input contract.
        app.push_screen_for_test(crate::render::Screen::new(
            "probe",
            alloc::vec![alloc::boxed::Box::new(crate::render::VerticalList::new(alloc::vec![
                crate::render::ListItem::new("row 0"),
                crate::render::ListItem::new("row 1"),
            ]))],
        ));

        let mut iterations = 0;
        run(&mut platform, &mut app, Duration::from_millis(0), Some(idle_timeout), None, || {
            iterations += 1;
            if iterations == 2 {
                clock.advance(idle_timeout + Duration::from_millis(1));
            }
            iterations <= 3
        });

        assert_eq!(*power_calls.borrow(), vec![crate::platform::DisplayPower::Off, crate::platform::DisplayPower::On], "the waking input must trigger exactly one On, after the earlier Off");

        // Row 0's selection-highlight pixel: still selected proves the
        // `Next` that woke the display was NOT forwarded to the
        // navigator -- if it had been, row 0 would no longer be selected.
        let row0_still_selected = app.render().pixel(embedded_graphics::prelude::Point::new(200, 18));
        assert_eq!(
            row0_still_selected,
            crate::render::theme::palette::SURFACE_ELEVATED,
            "the wake-triggering intent must be dropped, not delivered to the app"
        );

        // A second `run` call (fresh Active/Asleep state, which is exactly
        // where the first call left off) with one more `Next` proves input
        // delivery still works normally once Active: this time the intent
        // reaches `app.handle_input` and moves the selection.
        let RecordingSetup { mut platform, clock: _, power_calls: power_calls2, flush_count: _ } = recording_platform(vec![vec![NavIntent::Down]]);
        let mut iterations2 = 0;
        run(&mut platform, &mut app, Duration::from_millis(0), Some(idle_timeout), None, || {
            iterations2 += 1;
            iterations2 <= 1
        });

        assert!(power_calls2.borrow().is_empty(), "already-Active state must not call set_power again just because a fresh `run` call started");
        let row0_after_second_next = app.render().pixel(embedded_graphics::prelude::Point::new(200, 18));
        assert_ne!(
            row0_after_second_next,
            crate::render::theme::palette::SURFACE_ELEVATED,
            "a Next while Active must reach the app and move the selection"
        );
    }

    #[test]
    fn idle_timeout_none_never_sleeps_no_matter_how_much_time_passes() {
        // Ten iterations, each jumping the clock forward by a full
        // idle-timeout-sized step (well past any timeout that would
        // matter) -- with `idle_timeout: None`, none of that may ever
        // result in a `set_power` call.
        let RecordingSetup { mut platform, clock, power_calls, flush_count: _ } = recording_platform(vec![Vec::new(); 10]);
        let mut app = App::new(240, 240);

        let mut iterations = 0;
        run(&mut platform, &mut app, Duration::from_millis(0), None, None, || {
            iterations += 1;
            clock.advance(Duration::from_secs(120));
            iterations <= 10
        });

        assert!(power_calls.borrow().is_empty(), "idle_timeout: None must disable the screensaver entirely");
    }

    // --- Deep sleep ---
    //
    // None of the existing test `Platform` structs above combine a
    // controllable clock (needed to deterministically cross `Tb`, which is
    // minutes-scale -- a real test can't just sleep that long) with an
    // inspectable `PowerControl` -- `PowerTestPlatform` below is
    // purpose-built for exactly that combination.

    struct PowerTestPlatform {
        display: StubDisplay,
        input: QueuedInput,
        clock: ControllableClock,
        storage: StubStorage,
        power: RecordingPower,
    }
    impl Platform for PowerTestPlatform {
        type Display = StubDisplay;
        type Input = QueuedInput;
        type Clock = ControllableClock;
        type Storage = StubStorage;
        type Power = RecordingPower;

        fn display(&mut self) -> &mut Self::Display {
            &mut self.display
        }
        fn input(&mut self) -> &mut Self::Input {
            &mut self.input
        }
        fn clock(&self) -> &Self::Clock {
            &self.clock
        }
        fn storage(&mut self) -> &mut Self::Storage {
            &mut self.storage
        }
        fn power(&mut self) -> &mut Self::Power {
            &mut self.power
        }
    }

    /// Builds a [`PowerTestPlatform`] that polls `iterations` empty
    /// `NavIntent` vecs (these tests only ever exercise the no-input idle
    /// path -- input arriving would reset `last_input` and defeat the
    /// point), with `external_power` as given. Returns the platform plus
    /// the `ControllableClock`/`RecordingPower` handles a test needs to
    /// jump time and inspect `enter_deep_sleep` calls after `platform` has
    /// been moved into `run`.
    fn power_test_platform(iterations: usize, external_power: bool) -> (PowerTestPlatform, ControllableClock, RecordingPower) {
        let clock = ControllableClock::new();
        let power = RecordingPower::new(external_power);
        let platform = PowerTestPlatform {
            display: StubDisplay { flush_count: Rc::new(RefCell::new(0)) },
            input: QueuedInput(vec![Vec::new(); iterations]),
            clock: clock.clone(),
            storage: StubStorage,
            power: power.clone(),
        };
        (platform, clock, power)
    }

    #[test]
    fn deep_sleep_fires_once_idle_past_tb_while_off_external_power() {
        let deep_sleep_timeout = Duration::from_secs(600);
        let (mut platform, clock, power) = power_test_platform(3, false);
        let mut app = App::new(240, 240);

        let mut iterations = 0;
        run(&mut platform, &mut app, Duration::from_millis(0), None, Some(deep_sleep_timeout), || {
            iterations += 1;
            if iterations == 2 {
                clock.advance(deep_sleep_timeout + Duration::from_millis(1));
            }
            iterations <= 3
        });

        assert_eq!(power.deep_sleep_call_count(), 1, "eligible on every condition -- must fire, and exactly once");
    }

    #[test]
    fn deep_sleep_never_fires_while_on_external_power() {
        let deep_sleep_timeout = Duration::from_secs(600);
        let (mut platform, clock, power) = power_test_platform(3, true);
        let mut app = App::new(240, 240);

        let mut iterations = 0;
        run(&mut platform, &mut app, Duration::from_millis(0), None, Some(deep_sleep_timeout), || {
            iterations += 1;
            if iterations == 2 {
                clock.advance(deep_sleep_timeout + Duration::from_millis(1));
            }
            iterations <= 3
        });

        assert_eq!(power.deep_sleep_call_count(), 0, "external power must veto deep sleep even though idle time alone is eligible");
    }

    #[test]
    fn deep_sleep_timeout_none_never_fires_no_matter_how_much_time_passes() {
        let (mut platform, clock, power) = power_test_platform(5, false);
        let mut app = App::new(240, 240);

        let mut iterations = 0;
        run(&mut platform, &mut app, Duration::from_millis(0), None, None, || {
            iterations += 1;
            clock.advance(Duration::from_secs(600));
            iterations <= 5
        });

        assert_eq!(power.deep_sleep_call_count(), 0, "deep_sleep_timeout: None must disable the tier entirely");
    }
}
