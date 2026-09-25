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
//! else if Active && app.is_at_home_root() && now - last_input >= idle_timeout:
//!     set_power(Off); Asleep
//! if app.dirty() && Active: render + flush // blanked while Asleep
//! ```
//!
//! The `app.is_at_home_root()` guard (bead pico-link-4vb.3) is what keeps
//! the screensaver from ever firing away from Home -- most importantly,
//! never during the pairing wizard, where a blanked screen reads as a
//! crash. It only gates *arming* (transitioning `Active` -> `Asleep`); it
//! does not reset `last_input`, so navigating away from Home and back
//! doesn't get a free extension -- the same idle clock keeps running the
//! whole time, and any `NavIntent` (including the presses that navigate)
//! already resets it via the branch above.
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

/// Wake-on-fault constants (design `.planning/design/2026-09-07-audio-
/// fault-model.md` §7.5, `.planning/design/2026-09-07-home-fault-strip.md`
/// §7). Kept in one place beside [`IdlePolicy`], which is the type that
/// enforces them — never in C (audio-fault-model §7.5's explicit ruling:
/// "they live in Rust beside `IdlePolicy`, not in C").
///
/// How long a granted fault wake holds the display on before it's allowed
/// to blank again (home-fault-strip §7.3). A fault wake never touches
/// [`IdlePolicy`]'s `last_input`, so this is the *entire* grant — once it
/// expires, the screen blanks immediately if the ordinary 60s screensaver
/// clock has already elapsed (which it usually has, since a fault wake by
/// definition arrives with no real input).
pub const FAULT_WAKE_HOLD: Duration = Duration::from_secs(20);
/// The minimum gap between two granted fault wakes, regardless of key
/// (home-fault-strip §7.4 limiter 2) — the first of two independent storm
/// limiters (the second is [`FAULT_WAKE_SESSION_CAP`]).
pub const FAULT_WAKE_COOLDOWN: Duration = Duration::from_secs(5 * 60);
/// How many fault wakes one continuous stream session may spend before
/// wake-on-fault stops entirely for that session (home-fault-strip §7.4
/// limiter 3). Resets on a stream stop→start transition
/// ([`IdlePolicy::reset_fault_wake_session`]) or any real press (handled
/// automatically by [`IdlePolicy::tick`]'s `had_input` branch).
pub const FAULT_WAKE_SESSION_CAP: u8 = 6;
/// How long a raised fault key counts as "Live" for wake-gating purposes
/// (audio-fault-model §7.2: "a repeat of a key already showing Live never
/// wakes") — see [`crate::app::FaultLog::is_live`]. Also the freshness
/// tier boundary the eventual render layer uses (home-fault-strip §5);
/// kept here, in the one place these five constants live, even though
/// `IdlePolicy` itself never reads it.
pub const FAULT_LIVE_WINDOW: Duration = Duration::from_secs(20);
/// How long a fault entry survives with no refresh before the (future)
/// render layer retires it from the strip (home-fault-strip §5). Not
/// consumed by anything in this bead — S3 (`pico-link-9eq2.3.3`) is where
/// render-time retirement is computed — kept here so every fault-wake-
/// related constant lives in one place per the design's instruction.
pub const FAULT_RETIRE: Duration = Duration::from_secs(120);

/// What one [`IdlePolicy::tick`] call decided, for the caller to act on.
/// Both fields default to "do nothing" (`None`/`false`) on a tick that
/// changed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IdleDecision {
    /// `Some(_)` only on the tick the power level actually *changed* --
    /// e.g. crossing the idle timeout, or the wake half of the state
    /// machine (see [`IdlePolicy::on_input`]) requesting `On`. A caller
    /// that re-applies [`IdlePolicy::display_power`] every frame (as the
    /// firmware's pull-based `pl_ui_display_power` does -- see
    /// `.planning/design/2026-09-01-idle-policy-across-the-ffi-seam.md`)
    /// can ignore this field entirely; it exists for a caller like
    /// [`Runner::step`] that only wants to call
    /// `DisplaySurface::set_power` on an actual transition.
    pub power_transition: Option<DisplayPower>,
    /// `true` on the one tick deep sleep should fire (see
    /// [`IdlePolicy`]'s "Deep sleep" section) -- at most once per
    /// `IdlePolicy` instance, mirroring the old `deep_sleep_triggered`
    /// latch.
    pub enter_deep_sleep: bool,
}

/// The idle/wake + deep-sleep decision, extracted from [`Runner::step`] so
/// both the emulator's `run`/`Runner` and firmware's `ui-ffi` can share
/// **one** implementation of the tiers instead of the firmware silently
/// having none at all -- see pico-link-i3e and
/// `.planning/design/2026-09-01-idle-policy-across-the-ffi-seam.md`.
/// Deliberately `Platform`-free: this type touches no trait from
/// `crate::platform` except the plain data types [`DisplayPower`] and
/// [`crate::platform::Instant`], so it is usable from `ui-ffi` (which has
/// no `Platform` impl and never will under the C-first ADR) exactly as
/// easily as from [`Runner`].
///
/// Two halves, deliberately split because a caller like `ui-ffi`'s
/// `pl_ui_input` has no clock reading available (C's clock only arrives
/// later, via `pl_ui_tick`'s `now_us`):
///
/// - [`IdlePolicy::on_input`] -- call synchronously when a non-empty input
///   batch arrives, before forwarding it anywhere. Purely a state
///   transition (`Asleep` -> `Active`); needs no clock.
/// - [`IdlePolicy::tick`] -- call once per frame with that frame's clock
///   reading and whether input arrived. Updates the idle clock and
///   evaluates both timeout tiers.
///
/// `last_input` is lazily initialized on the first [`IdlePolicy::tick`]
/// call rather than seeded at construction -- `IdlePolicy::new` takes no
/// clock reading precisely because `ui-ffi`'s `pl_ui_create` has none
/// available; seeding a `0` baseline there would mean a UI created, say,
/// 61 seconds after device boot blanks on its very first rendered frame.
pub struct IdlePolicy {
    power_state: PowerState,
    last_input: Option<crate::platform::Instant>,
    /// Set once deep sleep has fired, so a host test's no-op recording
    /// stub (which, unlike real hardware, actually returns) doesn't
    /// re-fire it every subsequent tick once eligible -- see the "Deep
    /// sleep" section below.
    deep_sleep_triggered: bool,
    idle_timeout: Option<Duration>,
    deep_sleep_timeout: Option<Duration>,
    /// What the screensaver does once `Asleep` -- `Off` blanks, `Dim`
    /// keeps content visible at a reduced backlight (design
    /// `.planning/design/2026-09-24-screensaver-dim-and-timeout.md`,
    /// Andreas's decisions on pico-link-qivj.2). Set via
    /// [`IdlePolicy::configure`]; `Off` at construction.
    mode: crate::power::ScreensaverMode,
    /// The literal mute/zero-volume floor: `true` whenever the most recent
    /// `tick` observed `mute_or_zero`. While set, `Asleep` renders as
    /// `Dim` regardless of `mode` (Andreas's Q1 ruling: muted/zero volume
    /// + idle always lands on Dim, never Off).
    dim_floor: bool,
    /// The wake-on-fault floor's expiry, if a fault wake is currently
    /// holding the display on (design §7.5's third, *expiring* floor,
    /// alongside the mute/zero floor above). `Some(until)` while active;
    /// cleared either when `until` is reached ([`IdlePolicy::tick`], no
    /// one-frame flash: the screensaver tier evaluates in the same call
    /// that clears it) or by any real press (design §7.3: "any real button
    /// press during the hold cancels it"). **Never derived from
    /// `last_input`** — a fault wake must not extend the idle timer (design
    /// §7.5's correction: no `volume_wake_since_last_tick`-style OR into
    /// `had_input`).
    fault_hold_until: Option<crate::platform::Instant>,
    /// When the most recent *granted* fault wake occurred — the storm
    /// limiter's cooldown clock ([`FAULT_WAKE_COOLDOWN`]). `None` until the
    /// first grant.
    fault_wake_last: Option<crate::platform::Instant>,
    /// How many fault wakes this policy has granted since the last reset
    /// (a real press, or [`IdlePolicy::reset_fault_wake_session`]) — the
    /// session cap ([`FAULT_WAKE_SESSION_CAP`]).
    fault_wake_session_count: u8,
}

impl IdlePolicy {
    /// Builds a policy starting `Active`, with `last_input` unset (see the
    /// type doc's note on lazy initialization). `idle_timeout`/
    /// `deep_sleep_timeout` are the same `None`-disables-the-tier seams
    /// [`run`] always took.
    #[must_use]
    pub const fn new(idle_timeout: Option<Duration>, deep_sleep_timeout: Option<Duration>) -> Self {
        Self {
            power_state: PowerState::Active,
            last_input: None,
            deep_sleep_triggered: false,
            idle_timeout,
            deep_sleep_timeout,
            mode: crate::power::ScreensaverMode::Off,
            dim_floor: false,
            fault_hold_until: None,
            fault_wake_last: None,
            fault_wake_session_count: 0,
        }
    }

    /// Overwrites `idle_timeout`, `deep_sleep_timeout` and `mode` in place
    /// -- everything else (`power_state`, `last_input`, the fault-wake
    /// bookkeeping) is left untouched, so applying a settings change
    /// mid-run never resets the idle clock or drops an in-progress fault
    /// hold. The next arm decision (screensaver or deep-sleep) picks up
    /// the new values immediately.
    pub fn configure(&mut self, idle_timeout: Option<Duration>, deep_sleep_timeout: Option<Duration>, mode: crate::power::ScreensaverMode) {
        self.idle_timeout = idle_timeout;
        self.deep_sleep_timeout = deep_sleep_timeout;
        self.mode = mode;
    }

    /// The wake half. Call synchronously when a non-empty input batch has
    /// arrived, **before** forwarding it to `App`. Returns `true` if the
    /// display was `Asleep` and this call just woke it -- the caller must
    /// swallow this batch (never forward it to `app.handle_input`) and
    /// mark the app dirty instead, so the just-woken display gets a fresh
    /// flush even though nothing on screen actually changed (see the
    /// module doc's "Idle/wake" section, which this reproduces exactly).
    /// Returns `false` (forward normally) if already `Active`.
    ///
    /// Does not touch `last_input` -- that update happens in the following
    /// [`IdlePolicy::tick`] call, which every caller is expected to make
    /// once per frame regardless of whether input arrived.
    pub fn on_input(&mut self) -> bool {
        if self.power_state == PowerState::Asleep {
            self.power_state = PowerState::Active;
            true
        } else {
            false
        }
    }

    /// The arm half. Call exactly once per frame with `now` (this frame's
    /// clock reading), `had_input` (whether a non-empty batch arrived this
    /// frame -- and, if so, whether [`IdlePolicy::on_input`] was already
    /// called for it), `at_home_root` (`App::is_at_home_root()`),
    /// `on_external_power` (`PowerControl::on_external_power()`), and
    /// `mute_or_zero` (`App::volume_requires_dim_floor()` -- design
    /// `.planning/design/2026-09-07-volume-on-display.md` section 5.4's
    /// third tier).
    ///
    /// Lazily initializes `last_input` to `now` on the very first call
    /// (see the type doc). Any frame with `had_input == true` resets
    /// `last_input` to `now`, mirroring the module doc's "Idle/wake"
    /// pseudocode. Every frame first records the `mute_or_zero` floor (see
    /// below), independent of `had_input` -- a volume event is never
    /// itself routed through the `had_input`/[`IdlePolicy::on_input`] path
    /// (see [`crate::app::VolumeState::wakes_idle`]'s doc comment for why
    /// that's a *separate*, event-driven wake), so a mute/zero reading
    /// that arrived between ticks must still surface here rather than
    /// waiting for the next real input. A frame with no input then
    /// evaluates, in order:
    ///
    /// - the screensaver tier: goes `Asleep` once `Active && at_home_root
    ///   && !fault_hold && now - last_input >= idle_timeout`, rendering as
    ///   [`DisplayPower::Dim`] or [`DisplayPower::Off`] per
    ///   [`IdlePolicy::display_power`] (mode and the mute/zero floor,
    ///   below). Unlike the fault-hold floor, the mute/zero floor no
    ///   longer gates *arming* -- entering mute/zero never itself blanks
    ///   or un-blanks the screen; it only changes what an already-`Asleep`
    ///   screen renders as.
    /// - the deep-sleep tier (independent of the screensaver's own
    ///   `PowerState`, per the module doc's "Deep sleep" section): fires
    ///   at most once, when `!deep_sleep_triggered && now - last_input >=
    ///   deep_sleep_timeout && !on_external_power`.
    ///
    /// Either tier is permanently disabled by passing `None` for its
    /// timeout.
    ///
    /// # The mute/zero floor -- a literal `Dim`
    ///
    /// As of pico-link-qivj.2 the backlight is real PWM (GP13, see
    /// `firmware/src/st7789.c`'s `st7789_set_backlight_permille`), so
    /// "dim" is a literal [`DisplayPower::Dim`] level, not a policy floor
    /// that merely stayed `On`. `dim_floor` (set every tick from
    /// `mute_or_zero`) forces `display_power()` to return `Dim` instead of
    /// `Off` whenever the screen is `Asleep`, regardless of
    /// [`ScreensaverMode`](crate::power::ScreensaverMode) -- Andreas's Q1
    /// ruling on pico-link-qivj.2: muted/zero volume + idle always lands
    /// on `Dim`, never `Off`, in either screensaver mode. It never
    /// promotes `Asleep` back to `Active` (that would extend the idle
    /// timer) and never arms the screensaver early either -- it only
    /// changes what an already-idle screen renders as.
    // Four independent boolean inputs, not a bitflags/enum bundle: each
    // one is read from a different source at the call site (input poll,
    // `App::is_at_home_root`, `PowerControl::on_external_power`,
    // `App::volume_requires_dim_floor`) and named at every call site
    // already (`policy.tick(now, had_input, at_home_root, ...)`), which is
    // what this pedantic lint is really guarding against -- an opaque
    // string of positional bools with no names in sight. Bundling them
    // into a struct would cost every caller (and every existing test) a
    // construction step for no readability gain here.
    #[allow(clippy::fn_params_excessive_bools)]
    pub fn tick(
        &mut self,
        now: crate::platform::Instant,
        had_input: bool,
        at_home_root: bool,
        on_external_power: bool,
        mute_or_zero: bool,
    ) -> IdleDecision {
        let last_input = *self.last_input.get_or_insert(now);
        let mut decision = IdleDecision::default();

        // The literal mute/zero floor (Andreas's Q1): recorded every tick,
        // independent of `had_input` -- a volume event never itself routes
        // through `had_input`/`on_input` (see `VolumeState::wakes_idle`'s
        // doc comment), so a mute/zero reading that arrived between ticks
        // must still surface here rather than waiting for the next real
        // input.
        self.dim_floor = mute_or_zero;

        // The fault-wake floor: expire it first (clearing it if `now` has
        // reached it) so the screensaver-arm check below sees an
        // up-to-date value in the SAME tick the hold expires -- this is
        // what makes expiry blank immediately rather than one frame late
        // (design `.planning/design/2026-09-07-home-fault-strip.md` §7.3).
        let fault_hold = self.fault_hold_active(now);

        // The fault-hold floor: an already-Asleep screen must never stay
        // that way while a fault wake holds it on (audio-fault-model
        // §7.5) -- checked unconditionally, ahead of the `had_input` early
        // return, since a fault-hold promotion is not "input" and must not
        // reset `last_input`. The mute/zero floor no longer needs a
        // separate self-heal branch here: it is now a literal `Dim`
        // level applied by `display_power()` below, not a `PowerState`
        // promotion (Andreas's Q1 ruling on pico-link-qivj.2 -- muted/zero
        // + idle stays `Asleep`, just rendered as `Dim`).
        if self.power_state == PowerState::Asleep && fault_hold {
            self.power_state = PowerState::Active;
            decision.power_transition = Some(self.display_power());
        }

        if had_input {
            self.last_input = Some(now);
            // Any real press cancels the fault hold and hands control back
            // to the ordinary screensaver (home-fault-strip §7.3), and
            // resets the storm-limiter session cap (§7.4 limiter 3's
            // "resets on ... any real press").
            self.fault_hold_until = None;
            self.fault_wake_session_count = 0;
            return decision;
        }

        if let Some(idle_timeout) = self.idle_timeout {
            if self.power_state == PowerState::Active
                && at_home_root
                && !fault_hold
                && now.saturating_duration_since(last_input) >= idle_timeout
            {
                self.power_state = PowerState::Asleep;
                decision.power_transition = Some(self.display_power());
            }
        }

        if let Some(deep_sleep_timeout) = self.deep_sleep_timeout {
            let idle_elapsed = now.saturating_duration_since(last_input);
            if !self.deep_sleep_triggered && idle_elapsed >= deep_sleep_timeout && !on_external_power {
                self.deep_sleep_triggered = true;
                decision.enter_deep_sleep = true;
            }
        }

        decision
    }

    /// `true` if the fault-wake hold is still within its
    /// [`FAULT_WAKE_HOLD`] window at `now`. Clears `fault_hold_until` the
    /// moment it has expired (mutating even though this looks like a plain
    /// query) so the caller -- [`IdlePolicy::tick`] -- observes the
    /// post-expiry state in the very same call, which is what makes expiry
    /// blank immediately rather than one tick late (home-fault-strip §7.3:
    /// "the screen blanks immediately at hold expiry").
    fn fault_hold_active(&mut self, now: crate::platform::Instant) -> bool {
        match self.fault_hold_until {
            Some(until) if now < until => true,
            Some(_) => {
                self.fault_hold_until = None;
                false
            }
            None => false,
        }
    }

    /// Attempts to grant a fault wake at `now` -- the storm-limited half of
    /// design §7.5's wake-on-fault mechanism. Returns `true` and applies
    /// the wake (flips to `Active`, arms a fresh [`FAULT_WAKE_HOLD`]
    /// window, records the grant for [`FAULT_WAKE_COOLDOWN`] and counts
    /// against [`FAULT_WAKE_SESSION_CAP`]) if neither limiter refuses it;
    /// otherwise a no-op returning `false`.
    ///
    /// Callers are expected to have already checked the *eligibility*
    /// half -- severity `Audible`, the key's static `wakes_display`, and
    /// "not already Live" (design §7.2) -- before calling this; those
    /// checks need [`crate::app::FaultLog`], which this `Platform`-free
    /// type deliberately knows nothing about (mirrors [`IdlePolicy::tick`]'s
    /// own `mute_or_zero` being computed by the caller, not read from
    /// `App` directly).
    ///
    /// **Never touches `last_input`** -- a fault is not evidence of a human
    /// at the device (design §7.5's explicit correction against reusing
    /// `volume_wake_since_last_tick`'s `had_input`-OR'ing shape).
    pub fn on_fault_wake(&mut self, now: crate::platform::Instant) -> bool {
        if let Some(last) = self.fault_wake_last {
            if now.saturating_duration_since(last) < FAULT_WAKE_COOLDOWN {
                return false;
            }
        }
        if self.fault_wake_session_count >= FAULT_WAKE_SESSION_CAP {
            return false;
        }
        self.fault_wake_last = Some(now);
        self.fault_wake_session_count += 1;
        self.fault_hold_until = Some(now + FAULT_WAKE_HOLD);
        self.power_state = PowerState::Active;
        true
    }

    /// Resets the fault-wake session cap ([`FAULT_WAKE_SESSION_CAP`]) back
    /// to zero -- home-fault-strip §7.4 limiter 3's other reset trigger, "a
    /// stream stop -> start transition" (the "any real press" trigger is
    /// handled automatically inside [`IdlePolicy::tick`]). Call sites are
    /// not wired in this bead (`pico-link-9eq2.3.1`) -- `ui-ffi` has no
    /// stream-transition event to observe yet; that's `pico-link-9eq2.3.2`'s
    /// firmware half. Exposed now so that wiring is a one-line call, not a
    /// new method, once it lands.
    pub fn reset_fault_wake_session(&mut self) {
        self.fault_wake_session_count = 0;
    }

    /// The current requested display power level -- `On` unless the
    /// screensaver tier has blanked it. A **level**, not an edge: a caller
    /// may read this every frame and re-apply it idempotently (the
    /// firmware's `pl_ui_display_power` does exactly this) rather than
    /// relying on catching every [`IdleDecision::power_transition`].
    #[must_use]
    pub fn display_power(&self) -> DisplayPower {
        match self.power_state {
            PowerState::Active => DisplayPower::On,
            PowerState::Asleep => {
                if self.mode == crate::power::ScreensaverMode::Dim || self.dim_floor {
                    DisplayPower::Dim
                } else {
                    DisplayPower::Off
                }
            }
        }
    }
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
    /// The idle/deep-sleep decision, extracted into a `Platform`-free unit
    /// shared with `ui-ffi` -- see [`IdlePolicy`]'s doc comment.
    idle: IdlePolicy,
    /// The display power level last applied to `platform.display()`. Used
    /// to diff against `self.idle.display_power()` every step so a level
    /// change (`On` -> `Dim`, `Dim` -> `Off`, etc.) is applied exactly
    /// once, regardless of which of the three levels it started or ended
    /// on -- replaces the old edge-triggered `IdleDecision::power_transition`
    /// consumption, which only knew about two levels.
    applied_power: DisplayPower,
}

impl Runner {
    /// Builds a fresh `Runner`. `now` is accepted for API stability (a
    /// caller with a clock reading in hand at construction time can still
    /// pass it), but is no longer used to seed the idle clock eagerly --
    /// [`IdlePolicy`] lazily initializes `last_input` on its first
    /// [`IdlePolicy::tick`] call instead (see that type's doc comment).
    /// [`run`]'s own construction site calls this immediately before its
    /// first `step`, so the two clock readings are for all practical
    /// purposes the same instant either way.
    #[must_use]
    pub fn new(now: crate::platform::Instant, idle_timeout: Option<Duration>, deep_sleep_timeout: Option<Duration>) -> Self {
        let _ = now;
        Self {
            #[cfg(feature = "frame-timing")]
            frame_timing: FrameTiming::new(),
            flush_errors: FlushErrorTracker::new("DisplaySurface::flush"),
            power_errors: FlushErrorTracker::new("DisplaySurface::set_power"),
            idle: IdlePolicy::new(idle_timeout, deep_sleep_timeout),
            applied_power: DisplayPower::On,
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
        let had_input = !intents.is_empty();

        if had_input {
            if self.idle.on_input() {
                // The wake-triggering input only wakes the display — it is
                // deliberately never forwarded to `app.handle_input` (see
                // the module doc). `App` never learns it was asleep;
                // `mark_dirty` forces the fresh flush the just-woken
                // display needs even though nothing on screen actually
                // changed.
                match platform.display().set_power(DisplayPower::On) {
                    Ok(()) => self.power_errors.on_ok(),
                    Err(error) => self.power_errors.on_err(&error),
                }
                self.applied_power = DisplayPower::On;
                app.mark_dirty();
            } else {
                app.handle_input(intents);
                outcome = StepOutcome::InputHandled;
            }
        }

        // A settings change picked on the Settings screen this step: apply
        // it to the live `IdlePolicy` (so the very next arm decision uses
        // it) and, separately, persist it if the save latch is set. Two
        // independent latches on `App` (see `DisplaySettingsState`'s doc
        // comment) -- `configure` never itself triggers a save, and a save
        // never re-seeds `configure` a second time.
        if let Some(settings) = app.take_display_settings_to_apply() {
            self.idle.configure(settings.idle_timeout(), settings.deep_sleep_timeout(), settings.mode);
        }
        if let Some(settings) = app.take_display_settings_to_save() {
            let _ = settings.save(platform.storage());
        }

        // `IdlePolicy::tick` updates `last_input` (if `had_input`) and
        // evaluates both timeout tiers (if not) -- see its doc comment.
        // Called every step regardless of `had_input`, mirroring the
        // module doc's pseudocode. `mute_or_zero` reads
        // `App::volume_requires_dim_floor` fresh every step -- the
        // emulator has no volume-event source of its own (see
        // `.planning/design/2026-09-07-volume-on-display.md` section 5.5),
        // but this keeps the same shared `IdlePolicy` code path exercised
        // by both callers, per that section's requirement.
        let decision = self.idle.tick(
            frame_start,
            had_input,
            app.is_at_home_root(),
            platform.power().on_external_power(),
            app.volume_requires_dim_floor(),
        );
        // Level-diff, not edge: `IdleDecision::power_transition` still
        // exists for callers that want the "did anything change" signal,
        // but `Runner::step` now re-derives the target level from
        // `display_power()` every step and applies it exactly when it
        // differs from what was last applied -- the only shape that
        // handles three levels correctly (an On->Dim->Off sequence with
        // no edge in between the tick that decided Dim and the tick that
        // decided Off would otherwise be missed).
        let level = self.idle.display_power();
        if level != self.applied_power {
            match platform.display().set_power(level) {
                Ok(()) => self.power_errors.on_ok(),
                Err(error) => self.power_errors.on_err(&error),
            }
            if level != DisplayPower::Off {
                // Dim still renders content -- force a fresh flush on the
                // On<->Dim transition even if nothing else changed this
                // step (mirrors the wake-from-Off `mark_dirty` above).
                app.mark_dirty();
            }
            self.applied_power = level;
        }
        if decision.enter_deep_sleep {
            platform.power().enter_deep_sleep();
        }

        // Forwards this step's sampled clock time into the app core so
        // `App::now_us` (and, via it, `RenderCtx`) reflects real time under
        // `run` -- see `.planning/decisions/2026-08-31-render-ctx-frame-
        // scoped-clock.md`'s "standalone bug" note: before this, the only
        // `App::tick` call sites in the repo were `ui-ffi`'s `pl_ui_tick`
        // and one test, so `App::now_us` was permanently 0 in both the
        // headless and windowed run modes. `App::tick` itself does not mark
        // the app dirty (see its own doc comment), so this is safe to call
        // unconditionally, every step, ahead of the `dirty()` gate below.
        app.tick(frame_start.as_micros());

        // Blanked while `Asleep`: skip render+flush entirely rather than
        // rendering into a framebuffer nothing will show. `app.dirty()`
        // deliberately stays untouched by this gate (not cleared, not
        // read via `render()`) — a dirty flag survives blanked frames so
        // the next real wake renders it immediately.
        if app.dirty() && self.idle.display_power() != DisplayPower::Off {
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
            match platform.display().flush(&framebuffer) {
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
/// with no polled input **while `app.is_at_home_root()`**, and restores it
/// on the next input (see the module doc for the full state machine).
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

    /// Like [`StubStorage`] (`get` always `None`), but records every `set`
    /// call -- used by the display-settings save-latch tests below to
    /// prove `Runner::step` actually calls `DisplaySettings::save`.
    #[derive(Default, Clone)]
    struct RecordingStorage {
        sets: Rc<RefCell<Vec<(alloc::string::String, Vec<u8>)>>>,
    }
    impl crate::platform::Storage for RecordingStorage {
        type Error = Infallible;
        fn get(&self, _key: &str) -> Option<Vec<u8>> {
            None
        }
        fn set(&mut self, key: &str, value: Vec<u8>) -> Result<(), Self::Error> {
            self.sets.borrow_mut().push((key.into(), value));
            Ok(())
        }
    }

    struct RecordingPlatform {
        display: RecordingDisplay,
        input: QueuedInput,
        clock: ControllableClock,
        storage: RecordingStorage,
        power: RecordingPower,
    }
    impl Platform for RecordingPlatform {
        type Display = RecordingDisplay;
        type Input = QueuedInput;
        type Clock = ControllableClock;
        type Storage = RecordingStorage;
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
        storage_sets: Rc<RefCell<Vec<(alloc::string::String, Vec<u8>)>>>,
    }

    /// `inputs` is the exact per-iteration `NavIntent` script `QueuedInput`
    /// will hand back, one `Vec` per `poll()` call.
    fn recording_platform(inputs: Vec<Vec<NavIntent>>) -> RecordingSetup {
        let clock = ControllableClock::new();
        let power_calls = Rc::new(RefCell::new(Vec::new()));
        let flush_count = Rc::new(RefCell::new(0));
        let storage_sets = Rc::new(RefCell::new(Vec::new()));
        let platform = RecordingPlatform {
            display: RecordingDisplay { power_calls: Rc::clone(&power_calls), flush_count: Rc::clone(&flush_count) },
            input: QueuedInput(inputs),
            clock: clock.clone(),
            storage: RecordingStorage { sets: Rc::clone(&storage_sets) },
            power: RecordingPower::new(false),
        };
        RecordingSetup { platform, clock, power_calls, flush_count, storage_sets }
    }

    #[test]
    fn app_now_us_advances_under_run_instead_of_staying_permanently_zero() {
        // Regression test for pico-link-04d: before this fix, the only
        // `App::tick` call sites in the repo were `ui-ffi`'s `pl_ui_tick`
        // and one direct `App` test -- `run`'s `Runner::step` sampled the
        // clock (for idle/deep-sleep timing) but never forwarded it to the
        // app, so `App::now_us` was permanently 0 in both the headless and
        // windowed run modes. See `.planning/decisions/2026-08-31-render-
        // ctx-frame-scoped-clock.md`'s "standalone bug" note.
        let RecordingSetup { mut platform, clock, .. } = recording_platform(vec![vec![], vec![], vec![]]);
        let mut app = App::new(240, 240);
        assert_eq!(app.now_us(), 0, "sanity: a fresh App starts at now_us == 0");

        let mut iterations = 0;
        run(&mut platform, &mut app, Duration::from_millis(0), None, None, || {
            // Advance the clock by 10ms before every iteration (including
            // the first -- `should_continue` runs before the loop body
            // reads `platform.clock().now()` each time) so each `step`
            // sees a strictly later `frame_start` than the last.
            clock.advance(Duration::from_millis(10));
            iterations += 1;
            iterations <= 3
        });

        assert_eq!(
            app.now_us(),
            30_000,
            "App::now_us must reflect the run loop's own sampled clock time, not stay stuck at 0"
        );
    }

    #[test]
    fn no_input_past_the_idle_timeout_blanks_the_display_exactly_once_and_stops_flushing() {
        let idle_timeout = Duration::from_secs(120);
        // Never any input at all -- every poll returns empty.
        let RecordingSetup { mut platform, clock, power_calls, flush_count, .. } = recording_platform(vec![Vec::new(); 3]);
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
        let RecordingSetup { mut platform, clock, power_calls, flush_count: _, .. } = recording_platform(vec![Vec::new(), Vec::new(), vec![NavIntent::Down]]);
        let mut app = App::new(240, 240);
        // Home (the root screen since `pico-link-znb.8`/E7) has no
        // focusable list on its default status face -- `Down` is
        // deliberately unbound there (Tier 1 scope boundary: volume is
        // Tier 2/E16). This test's proof only needs *some* focusable
        // content whose selection visibly moves on `Down`, decoupled from
        // whatever Home's own content happens to be -- a plain
        // `VerticalList` screen, the same shape `navigator.rs`'s own
        // tests use, serves that purpose without coupling this run-loop
        // test to Home's domain-specific input contract. It replaces the
        // *root* (not pushed on top, see pico-link-4vb.3) so the navigator
        // stays at depth 1/Home root -- the screensaver's arming gate --
        // rather than looking like the pairing wizard or a pushed screen.
        app.replace_root_for_test(crate::render::Screen::new(
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
        let RecordingSetup { mut platform, clock: _, power_calls: power_calls2, flush_count: _, .. } = recording_platform(vec![vec![NavIntent::Down]]);
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
        let RecordingSetup { mut platform, clock, power_calls, flush_count: _, .. } = recording_platform(vec![Vec::new(); 10]);
        let mut app = App::new(240, 240);

        let mut iterations = 0;
        run(&mut platform, &mut app, Duration::from_millis(0), None, None, || {
            iterations += 1;
            clock.advance(Duration::from_secs(120));
            iterations <= 10
        });

        assert!(power_calls.borrow().is_empty(), "idle_timeout: None must disable the screensaver entirely");
    }

    #[test]
    fn no_input_past_the_idle_timeout_off_home_root_never_blanks_the_display() {
        // Regression test for bead pico-link-4vb.3: the screensaver must
        // never arm away from Home (most importantly, never during the
        // pairing wizard -- a blanked screen mid-pairing reads as a
        // crash). Pushing a second screen (mirroring the wizard/Devices/
        // Settings shape: depth > 1) is enough to prove the gate, without
        // coupling this test to the wizard's own domain-specific state --
        // see `waking_input_is_swallowed_but_the_next_input_reaches_the_app`
        // above for the same pattern.
        let idle_timeout = Duration::from_secs(60);
        let RecordingSetup { mut platform, clock, power_calls, flush_count: _, .. } = recording_platform(vec![Vec::new(); 3]);
        let mut app = App::new(240, 240);
        app.push_screen_for_test(crate::render::Screen::new("probe", alloc::vec![]));
        assert_eq!(app.navigator_depth(), 2, "sanity: not at Home root");

        let mut iterations = 0;
        run(&mut platform, &mut app, Duration::from_millis(0), Some(idle_timeout), None, || {
            iterations += 1;
            if iterations == 2 {
                clock.advance(idle_timeout + Duration::from_millis(1));
            }
            iterations <= 3
        });

        assert!(power_calls.borrow().is_empty(), "the screensaver must never arm while off the Home root, no matter how much idle time passes");
    }

    #[test]
    fn returning_to_home_root_arms_the_screensaver_again_after_leaving_it() {
        // Complements the test above: once the navigator is *back* at Home
        // root, the same idle clock (never reset just by moving around --
        // see the module doc's note on `app.is_at_home_root()`) can still
        // trigger the blank. `push_screen_for_test`/`Navigator::pop` stand
        // in for "the user opened Devices, then backed out to Home",
        // without depending on the real Devices/wizard screens.
        let idle_timeout = Duration::from_secs(60);
        let RecordingSetup { mut platform, clock, power_calls, flush_count: _, .. } = recording_platform(vec![Vec::new(); 3]);
        let mut app = App::new(240, 240);
        app.push_screen_for_test(crate::render::Screen::new("probe", alloc::vec![]));
        app.pop_screen_for_test();
        assert_eq!(app.navigator_depth(), 1, "sanity: back at Home root");

        let mut iterations = 0;
        run(&mut platform, &mut app, Duration::from_millis(0), Some(idle_timeout), None, || {
            iterations += 1;
            if iterations == 2 {
                clock.advance(idle_timeout + Duration::from_millis(1));
            }
            iterations <= 3
        });

        assert_eq!(*power_calls.borrow(), vec![crate::platform::DisplayPower::Off], "back at Home root, the screensaver must still arm once idle_timeout elapses");
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

    // --- pico-link-qivj.2: `Runner::step`'s `DisplaySettings` apply/save
    // wiring (S4) -- proves `App::take_display_settings_to_apply`/`_to_save`
    // are actually drained and acted on, using `ControllableClock` so a
    // 30-second `ScreensaverTimeout` option can be crossed with zero real
    // wall-clock time (the emulator's own e2e test, which only has a real
    // `HostClock` available, cannot do this -- see that test module's doc
    // comment).

    #[test]
    fn a_picked_display_setting_is_applied_to_the_live_idle_policy_the_very_next_step() {
        let idle_timeout = crate::power::ScreensaverTimeout::Sec30.as_secs();
        let RecordingSetup { mut platform, clock, power_calls, .. } = recording_platform(vec![Vec::new(); 3]);
        let mut app = App::new(240, 240);
        app.set_display_settings(crate::power::DisplaySettings {
            mode: crate::power::ScreensaverMode::Dim,
            timeout: crate::power::ScreensaverTimeout::Sec30,
        });

        // `run`'s own `idle_timeout` param is `None` here -- the point of
        // this test is that the *applied* setting (Dim, 30s), not this
        // param, is what actually governs the screensaver.
        let mut iterations = 0;
        run(&mut platform, &mut app, Duration::from_millis(0), None, None, || {
            iterations += 1;
            if iterations == 2 {
                clock.advance(Duration::from_secs(u64::from(idle_timeout)) + Duration::from_millis(1));
            }
            iterations <= 3
        });

        assert_eq!(
            *power_calls.borrow(),
            vec![crate::platform::DisplayPower::Dim],
            "the picked Dim mode + 30s timeout must be applied and drive the screensaver, proving `None` (run's own param) was overridden"
        );
    }

    #[test]
    fn picking_a_screensaver_mode_on_the_settings_screen_persists_it_exactly_once() {
        // Drives the real Settings picker through ordinary input, end to
        // end: Home status -> menu face, Down to Settings, Select opens
        // it (landing on row 0, "BUFFER" -- bead `pico-link-8pp1.2` S4),
        // Down twice to row 2 ("IDLE SCREEN" -- row 1 is now "LDAC MIN",
        // bead `pico-link-d42g.4` F4), Select opens the ScreensaverMode
        // picker, Select picks its first option -- proving `Runner::step`
        // actually persists a real user pick, not just a hand-built
        // `DisplaySettings` value.
        let RecordingSetup { mut platform, storage_sets, .. } = recording_platform(vec![
            vec![NavIntent::Select],
            vec![NavIntent::Down],
            vec![NavIntent::Down],
            vec![NavIntent::Select],
            vec![NavIntent::Down],
            vec![NavIntent::Down],
            vec![NavIntent::Select],
            vec![NavIntent::Select],
            Vec::new(),
        ]);
        let mut app = App::new(240, 240);

        let mut iterations = 0;
        run(&mut platform, &mut app, Duration::from_millis(0), None, None, || {
            iterations += 1;
            iterations <= 8
        });

        let sets = storage_sets.borrow();
        assert_eq!(sets.len(), 1, "exactly one save, for the one pick made");
        assert_eq!(sets[0].0, crate::power::DisplaySettings::STORAGE_KEY);
    }

    #[test]
    fn set_display_settings_seeds_apply_but_never_save() {
        // Same assertion as the test above, phrased directly against
        // `App`'s own latch semantics rather than through `Runner::step`,
        // so a future change to either side breaks the right test.
        let mut app = App::new(240, 240);
        app.set_display_settings(crate::power::DisplaySettings::default());
        assert!(app.take_display_settings_to_apply().is_some(), "seeding must mark apply_pending");
        assert!(app.take_display_settings_to_save().is_none(), "seeding must NOT mark save_pending");
    }
}

/// Direct [`IdlePolicy`] unit tests -- no `Platform` stub needed at all,
/// which is the entire point of pulling the decision out of `Runner::step`
/// (see [`IdlePolicy`]'s doc comment and pico-link-i3e): these exercise the
/// exact same tiers the `run::tests` module above already covers via the
/// full `Runner`/`Platform` machinery, but directly against the
/// `Platform`-free type `ui-ffi` also calls.
#[cfg(test)]
mod idle_policy_tests {
    use super::{DisplayPower, IdlePolicy, FAULT_WAKE_COOLDOWN, FAULT_WAKE_HOLD, FAULT_WAKE_SESSION_CAP};
    use crate::platform::Instant;
    use core::time::Duration;

    #[test]
    fn starts_active_and_on() {
        let policy = IdlePolicy::new(Some(Duration::from_secs(60)), None);
        assert_eq!(policy.display_power(), DisplayPower::On);
    }

    #[test]
    fn last_input_is_lazily_initialized_not_seeded_to_zero() {
        // A tick at t=1000s with no prior `tick` call must NOT read as
        // "already idle for 1000s" -- the first tick call establishes the
        // baseline instead. This is the exact scenario the design doc
        // calls out: `pl_ui_create` has no clock, so if `last_input` were
        // seeded to 0 a UI created 61s after boot would blank on its very
        // first frame.
        let mut policy = IdlePolicy::new(Some(Duration::from_secs(60)), None);
        let far_future = Instant::from_micros(1_000_000_000);
        let decision = policy.tick(far_future, false, true, true, false);
        assert_eq!(decision.power_transition, None, "the first tick must establish the baseline, not read as already-idle");
        assert_eq!(policy.display_power(), DisplayPower::On);
    }

    #[test]
    fn blanks_once_idle_past_the_timeout_at_home_root() {
        let idle_timeout = Duration::from_secs(60);
        let mut policy = IdlePolicy::new(Some(idle_timeout), None);
        let t0 = Instant::from_micros(0);
        policy.tick(t0, false, true, true, false); // establishes baseline

        let still_before = policy.tick(t0 + Duration::from_secs(59), false, true, true, false);
        assert_eq!(still_before.power_transition, None);
        assert_eq!(policy.display_power(), DisplayPower::On);

        let crosses = policy.tick(t0 + idle_timeout, false, true, true, false);
        assert_eq!(crosses.power_transition, Some(DisplayPower::Off));
        assert_eq!(policy.display_power(), DisplayPower::Off);

        // Must not repeat the transition on a later tick while still idle.
        let later = policy.tick(t0 + idle_timeout + Duration::from_secs(1), false, true, true, false);
        assert_eq!(later.power_transition, None, "already Asleep -- no repeat transition");
    }

    #[test]
    fn never_blanks_off_home_root_no_matter_how_long_idle() {
        let idle_timeout = Duration::from_secs(60);
        let mut policy = IdlePolicy::new(Some(idle_timeout), None);
        let t0 = Instant::from_micros(0);
        policy.tick(t0, false, false, true, false);
        let decision = policy.tick(t0 + Duration::from_secs(1000), false, false, true, false);
        assert_eq!(decision.power_transition, None, "must never arm off Home root");
        assert_eq!(policy.display_power(), DisplayPower::On);
    }

    #[test]
    fn idle_timeout_none_never_blanks() {
        let mut policy = IdlePolicy::new(None, None);
        let t0 = Instant::from_micros(0);
        policy.tick(t0, false, true, true, false);
        let decision = policy.tick(t0 + Duration::from_secs(10_000), false, true, true, false);
        assert_eq!(decision.power_transition, None);
        assert_eq!(policy.display_power(), DisplayPower::On);
    }

    #[test]
    fn on_input_wakes_from_asleep_and_returns_true_only_once() {
        let idle_timeout = Duration::from_secs(60);
        let mut policy = IdlePolicy::new(Some(idle_timeout), None);
        let t0 = Instant::from_micros(0);
        policy.tick(t0, false, true, true, false);
        policy.tick(t0 + idle_timeout, false, true, true, false);
        assert_eq!(policy.display_power(), DisplayPower::Off, "sanity: asleep");

        assert!(policy.on_input(), "waking from Asleep must report true (caller must swallow this input)");
        assert_eq!(policy.display_power(), DisplayPower::On, "on_input must flip the level immediately, before the next tick");

        assert!(!policy.on_input(), "already Active -- must not report a wake a second time");
    }

    // --- Design `.planning/design/2026-09-07-volume-on-display.md`
    // section 5.4's `mute_or_zero` floor, at the `IdlePolicy` level ---

    #[test]
    fn mute_or_zero_past_the_idle_timeout_lands_on_dim_not_on() {
        // Andreas's Q1 ruling (pico-link-qivj.2): muted/zero + idle goes to
        // Dim, never stays On and never goes Off.
        let idle_timeout = Duration::from_secs(60);
        let mut policy = IdlePolicy::new(Some(idle_timeout), None);
        let t0 = Instant::from_micros(0);
        policy.tick(t0, false, true, true, false);

        let decision = policy.tick(t0 + idle_timeout, false, true, true, true);
        assert_eq!(decision.power_transition, Some(DisplayPower::Dim));
        assert_eq!(policy.display_power(), DisplayPower::Dim);
    }

    #[test]
    fn mute_or_zero_while_asleep_in_off_mode_reads_as_dim_and_unmuting_returns_to_off() {
        let idle_timeout = Duration::from_secs(60);
        let mut policy = IdlePolicy::new(Some(idle_timeout), None);
        let t0 = Instant::from_micros(0);
        policy.tick(t0, false, true, true, false);
        policy.tick(t0 + idle_timeout, false, true, true, false);
        assert_eq!(policy.display_power(), DisplayPower::Off, "sanity: asleep in Off mode");

        // Mute/zero arrives while already Asleep: no `PowerState` promotion
        // (still Asleep), but the rendered level becomes Dim.
        let decision = policy.tick(t0 + idle_timeout + Duration::from_secs(1), false, true, true, true);
        assert_eq!(decision.power_transition, None, "mute/zero is a rendering floor, not a PowerState promotion");
        assert_eq!(policy.display_power(), DisplayPower::Dim);

        // Unmuting with no other activity returns to Off immediately.
        let decision = policy.tick(t0 + idle_timeout + Duration::from_secs(2), false, true, true, false);
        assert_eq!(decision.power_transition, None, "display_power() is polled by level, not edge-signalled here");
        assert_eq!(policy.display_power(), DisplayPower::Off);
    }

    #[test]
    fn mute_does_not_extend_the_idle_timer() {
        let idle_timeout = Duration::from_secs(60);
        let mut policy = IdlePolicy::new(Some(idle_timeout), None);
        let t0 = Instant::from_micros(0);
        policy.tick(t0, false, true, true, false);

        // Mute/zero holds continuously from before the timeout through
        // after it; the idle clock must still be anchored to `t0`, not
        // reset by the mute/zero reads themselves.
        policy.tick(t0 + Duration::from_secs(30), false, true, true, true);
        assert_eq!(policy.display_power(), DisplayPower::On, "not yet idle");

        policy.tick(t0 + idle_timeout, false, true, true, true);
        assert_eq!(policy.display_power(), DisplayPower::Dim, "idle timeout elapsed on schedule despite mute/zero reads");
    }

    #[test]
    fn dim_mode_past_the_idle_timeout_reads_as_dim() {
        let idle_timeout = Duration::from_secs(60);
        let mut policy = IdlePolicy::new(Some(idle_timeout), None);
        policy.configure(Some(idle_timeout), None, crate::power::ScreensaverMode::Dim);
        let t0 = Instant::from_micros(0);
        policy.tick(t0, false, true, true, false);

        let decision = policy.tick(t0 + idle_timeout, false, true, true, false);
        assert_eq!(decision.power_transition, Some(DisplayPower::Dim));
        assert_eq!(policy.display_power(), DisplayPower::Dim);
    }

    #[test]
    fn configure_mid_run_changes_the_next_arm() {
        let idle_timeout = Duration::from_secs(60);
        let mut policy = IdlePolicy::new(Some(idle_timeout), None);
        let t0 = Instant::from_micros(0);
        policy.tick(t0, false, true, true, false);

        // Switch to Dim mode before the timeout elapses.
        policy.configure(Some(idle_timeout), None, crate::power::ScreensaverMode::Dim);

        let decision = policy.tick(t0 + idle_timeout, false, true, true, false);
        assert_eq!(decision.power_transition, Some(DisplayPower::Dim), "the new mode applies to the very next arm");
    }

    #[test]
    fn fault_wake_from_dim_goes_on_then_hold_expiry_returns_to_dim() {
        let idle_timeout = Duration::from_secs(60);
        let mut policy = IdlePolicy::new(Some(idle_timeout), None);
        policy.configure(Some(idle_timeout), None, crate::power::ScreensaverMode::Dim);
        let t0 = Instant::from_micros(0);
        policy.tick(t0, false, true, true, false);
        policy.tick(t0 + idle_timeout, false, true, true, false);
        assert_eq!(policy.display_power(), DisplayPower::Dim, "sanity: asleep, dim mode");

        let fault_wake_at = t0 + idle_timeout + Duration::from_secs(1);
        assert!(policy.on_fault_wake(fault_wake_at));
        assert_eq!(policy.display_power(), DisplayPower::On);

        let decision = policy.tick(fault_wake_at + FAULT_WAKE_HOLD + Duration::from_secs(1), false, true, true, false);
        assert_eq!(decision.power_transition, Some(DisplayPower::Dim), "hold expiry returns to the configured Dim mode, not Off");
        assert_eq!(policy.display_power(), DisplayPower::Dim);
    }

    #[test]
    fn input_while_active_resets_the_idle_clock() {
        let idle_timeout = Duration::from_secs(60);
        let mut policy = IdlePolicy::new(Some(idle_timeout), None);
        let t0 = Instant::from_micros(0);
        policy.tick(t0, false, true, true, false);

        // Input arrives just before the timeout would have fired.
        let just_before = t0 + Duration::from_secs(59);
        assert!(!policy.on_input(), "already Active");
        policy.tick(just_before, true, true, true, false);

        // A full `idle_timeout` after the ORIGINAL baseline (t0) has now
        // passed, but only ~1s has passed since the reset -- must not
        // blank yet.
        let decision = policy.tick(t0 + idle_timeout + Duration::from_millis(500), false, true, true, false);
        assert_eq!(decision.power_transition, None, "the idle clock must have reset on the input at `just_before`, not stayed anchored to t0");
    }

    #[test]
    fn deep_sleep_fires_once_past_tb_when_off_external_power() {
        let deep_sleep_timeout = Duration::from_secs(600);
        let mut policy = IdlePolicy::new(None, Some(deep_sleep_timeout));
        let t0 = Instant::from_micros(0);
        policy.tick(t0, false, true, false, false);

        let decision = policy.tick(t0 + deep_sleep_timeout, false, true, false, false);
        assert!(decision.enter_deep_sleep);

        let again = policy.tick(t0 + deep_sleep_timeout + Duration::from_secs(1), false, true, false, false);
        assert!(!again.enter_deep_sleep, "must fire at most once");
    }

    #[test]
    fn deep_sleep_never_fires_on_external_power() {
        let deep_sleep_timeout = Duration::from_secs(600);
        let mut policy = IdlePolicy::new(None, Some(deep_sleep_timeout));
        let t0 = Instant::from_micros(0);
        policy.tick(t0, false, true, true, false);
        let decision = policy.tick(t0 + deep_sleep_timeout, false, true, true, false);
        assert!(!decision.enter_deep_sleep);
    }

    #[test]
    fn deep_sleep_timeout_none_never_fires() {
        let mut policy = IdlePolicy::new(None, None);
        let t0 = Instant::from_micros(0);
        policy.tick(t0, false, true, false, false);
        let decision = policy.tick(t0 + Duration::from_secs(100_000), false, true, false, false);
        assert!(!decision.enter_deep_sleep);
    }

    // --- Wake-on-fault (design `.planning/design/2026-09-07-audio-fault-
    // model.md` §7.5, `.planning/design/2026-09-07-home-fault-strip.md`
    // §7), bead pico-link-9eq2.3.1 ---

    #[test]
    fn a_fault_wake_does_not_extend_last_input() {
        let idle_timeout = Duration::from_secs(60);
        let mut policy = IdlePolicy::new(Some(idle_timeout), None);
        let t0 = Instant::from_micros(0);
        policy.tick(t0, false, true, true, false);
        policy.tick(t0 + idle_timeout, false, true, true, false);
        assert_eq!(policy.display_power(), DisplayPower::Off, "sanity: asleep");

        // The fault arrives long after `t0` -- if `on_fault_wake` had
        // (incorrectly) bumped `last_input` to this instant, the hold
        // expiring below would still read as within a fresh 60s idle
        // window and stay On.
        let wake_at = t0 + idle_timeout + Duration::from_secs(30);
        assert!(policy.on_fault_wake(wake_at), "first fault wake in a fresh session must be granted");
        assert_eq!(policy.display_power(), DisplayPower::On);

        // Once the hold expires, the screen must blank on the very next
        // tick -- proof `last_input` is still `t0`, not `wake_at`: had it
        // been extended, `now - last_input` at hold-expiry (`FAULT_WAKE_
        // HOLD` after `wake_at`) would only be `FAULT_WAKE_HOLD`, well
        // under `idle_timeout`, and the screen would stay On.
        let decision = policy.tick(wake_at + FAULT_WAKE_HOLD, false, true, true, false);
        assert_eq!(decision.power_transition, Some(DisplayPower::Off), "the fault wake must not have extended last_input");
    }

    #[test]
    fn the_fault_hold_expiring_blanks_immediately_with_no_one_frame_flash() {
        let idle_timeout = Duration::from_secs(60);
        let mut policy = IdlePolicy::new(Some(idle_timeout), None);
        let t0 = Instant::from_micros(0);
        policy.tick(t0, false, true, true, false);
        policy.tick(t0 + idle_timeout, false, true, true, false);
        assert_eq!(policy.display_power(), DisplayPower::Off, "sanity: asleep");

        let wake_at = t0 + idle_timeout + Duration::from_secs(10);
        assert!(policy.on_fault_wake(wake_at));
        assert_eq!(policy.display_power(), DisplayPower::On, "on_fault_wake must apply synchronously, with no intervening tick needed");

        // A tick that lands one microsecond before expiry must still be On
        // (no premature blank)...
        let almost_expired = Instant::from_micros((wake_at + FAULT_WAKE_HOLD).as_micros() - 1);
        let still_holding = policy.tick(almost_expired, false, true, true, false);
        assert_eq!(still_holding.power_transition, None);
        assert_eq!(policy.display_power(), DisplayPower::On);

        // ...and the tick that reaches expiry must blank in that SAME
        // call -- no extra tick required, i.e. no one-frame flash where a
        // stale hold is still honoured one call too many.
        let expiry_tick = policy.tick(wake_at + FAULT_WAKE_HOLD, false, true, true, false);
        assert_eq!(expiry_tick.power_transition, Some(DisplayPower::Off), "the hold's expiry tick itself must blank, immediately");
        assert_eq!(policy.display_power(), DisplayPower::Off);
    }

    #[test]
    fn a_real_press_during_the_fault_hold_hands_control_back_to_the_ordinary_screensaver() {
        let idle_timeout = Duration::from_secs(60);
        let mut policy = IdlePolicy::new(Some(idle_timeout), None);
        let t0 = Instant::from_micros(0);
        policy.tick(t0, false, true, true, false);
        policy.tick(t0 + idle_timeout, false, true, true, false);

        let wake_at = t0 + idle_timeout + Duration::from_secs(5);
        assert!(policy.on_fault_wake(wake_at));

        // A real press arrives mid-hold.
        let press_at = wake_at + Duration::from_secs(2);
        policy.tick(press_at, true, true, true, false);
        assert_eq!(policy.display_power(), DisplayPower::On);

        // The ordinary 60s screensaver now governs, timed from the press
        // -- not from the (now-cancelled) fault hold, and not from the
        // stale original `t0`.
        let just_before_instant = Instant::from_micros((press_at + idle_timeout).as_micros() - 1);
        let just_before = policy.tick(just_before_instant, false, true, true, false);
        assert_eq!(just_before.power_transition, None, "must still be on just before a fresh 60s timeout from the press");

        let at_timeout = policy.tick(press_at + idle_timeout, false, true, true, false);
        assert_eq!(at_timeout.power_transition, Some(DisplayPower::Off), "must blank exactly one ordinary idle timeout after the press");
    }

    #[test]
    fn on_fault_wake_is_refused_within_the_cooldown_and_granted_once_it_elapses() {
        let mut policy = IdlePolicy::new(Some(Duration::from_secs(60)), None);
        let t0 = Instant::from_micros(0);

        assert!(policy.on_fault_wake(t0), "the first-ever wake has no prior grant to cool down from");
        let just_before_cooldown = Instant::from_micros((t0 + FAULT_WAKE_COOLDOWN).as_micros() - 1);
        assert!(!policy.on_fault_wake(just_before_cooldown), "a second wake one microsecond before the cooldown elapses must be refused");
        assert!(policy.on_fault_wake(t0 + FAULT_WAKE_COOLDOWN), "a wake exactly at the cooldown boundary must be granted");
    }

    #[test]
    fn on_fault_wake_session_cap_binds_and_resets_on_a_real_press() {
        let mut policy = IdlePolicy::new(Some(Duration::from_secs(60)), None);
        // Space every attempt a full cooldown apart so only the session
        // cap is under test.
        let mut now = Instant::from_micros(0);
        for i in 0..FAULT_WAKE_SESSION_CAP {
            assert!(policy.on_fault_wake(now), "grant {i} of {FAULT_WAKE_SESSION_CAP} must be allowed");
            now += FAULT_WAKE_COOLDOWN;
        }
        assert!(!policy.on_fault_wake(now), "the session cap must refuse a {}th wake", FAULT_WAKE_SESSION_CAP + 1);

        // A real press resets the cap (home-fault-strip §7.4 limiter 3).
        policy.tick(now, true, true, true, false);
        assert!(policy.on_fault_wake(now + FAULT_WAKE_COOLDOWN), "a real press must reset the session cap");
    }

    #[test]
    fn reset_fault_wake_session_resets_the_cap_without_a_press() {
        let mut policy = IdlePolicy::new(Some(Duration::from_secs(60)), None);
        let mut now = Instant::from_micros(0);
        for _ in 0..FAULT_WAKE_SESSION_CAP {
            assert!(policy.on_fault_wake(now));
            now += FAULT_WAKE_COOLDOWN;
        }
        assert!(!policy.on_fault_wake(now), "sanity: the cap is reached");

        // The stream-stop->start reset path (design §7.4 limiter 3's other
        // trigger) -- no press involved.
        policy.reset_fault_wake_session();
        assert!(policy.on_fault_wake(now + FAULT_WAKE_COOLDOWN), "reset_fault_wake_session must clear the cap");
    }
}
