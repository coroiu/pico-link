//! Idle-screensaver + deep-sleep power policy.
//!
//! Two tiers, both measured from the same "time since last polled input"
//! clock `crate::run::run` already tracks:
//!
//! - **Tier 1 (screensaver)**: after [`DEFAULT_IDLE_TIMEOUT`] (`Ta`) idle,
//!   `run` blanks the display via `crate::platform::DisplaySurface::
//!   set_power`.
//! - **Tier 2 (deep sleep)**: after [`DEFAULT_DEEP_SLEEP_TIMEOUT`] (`Tb`,
//!   `Tb` > `Ta` so the screen is already blanked by the time this tier can
//!   fire) idle, `run` additionally calls
//!   `crate::platform::PowerControl::enter_deep_sleep` — but only if
//!   [`IdlePowerSetting`] is armed (see its doc comment for why "armed" is
//!   currently always `false`, regardless of the persisted setting).
//!
//! [`IdlePowerSetting`] is the single persisted toggle covering both tiers:
//! disabling it disables the screensaver AND deep sleep together (by
//! construction — see its `idle_timeout`/`deep_sleep_timeout` methods), so
//! "disabling the screensaver disables deep sleep" holds by construction
//! rather than needing separate enforcement. `run` itself stays platform-
//! and setting-free: it only ever sees the two `Option<Duration>` values
//! these methods compute, mirroring the existing `idle_timeout:
//! Option<Duration>` seam.

use alloc::vec;
use core::time::Duration;

use crate::platform::Storage;

/// How long the display stays on with no input before `run`'s
/// idle-screensaver tier blanks it (`Ta`). 60 seconds -- Andreas's
/// 2026-09-01 ruling on bead pico-link-4vb.3 ("For now 1 minute hardcoded"),
/// superseding the earlier design's "~2min" target. Only takes effect while
/// `App::is_at_home_root()` (see `crate::run::Runner::step`); armed
/// anywhere else (Devices, Settings, the pairing wizard) would read as a
/// crash.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the device stays idle (from the same last-input clock as
/// [`DEFAULT_IDLE_TIMEOUT`], not from when the screen blanked) before
/// `run`'s deep-sleep tier fires (`Tb`), once armed. 600 seconds (10
/// minutes): an arbitrary, conservative `PoC` value — comfortably longer
/// than `Ta` so the screen is always already blanked first, but not tuned
/// against any real battery-life target. Easy to retune later; nothing
/// about the mechanism depends on the exact value.
pub const DEFAULT_DEEP_SLEEP_TIMEOUT: Duration = Duration::from_secs(600);

/// Whether the deep-sleep tier is armed at all, independent of
/// [`IdlePowerSetting::enabled`].
///
/// **Deliberately `false` today.** Deep sleep on this device is a cold
/// boot: any RAM-only application state is wiped on wake, forcing whatever
/// re-sync or re-init the concrete product needs before the device is
/// useful again. Arming this tier by default is a product-level sequencing
/// decision (land persistence first, or ship a documented "state is lost
/// across deep wake" limitation) that is still pending. Flipping this to
/// `true` (or wiring it to a build feature) is the one-line change that
/// arms it once that decision lands; nothing else in this module, `run`,
/// or either `Platform` impl needs to change.
///
/// Kept as a plain module constant (not a cargo feature) for now: it's
/// read in exactly one place ([`IdlePowerSetting::deep_sleep_timeout`]),
/// and a constant is enough to guarantee today's builds never arm it
/// while still being a one-line flip later.
pub const DEEP_SLEEP_ARMED: bool = false;

/// The persisted idle-power toggle: one bool controlling both power tiers
/// together, so "disabling the screensaver disables deep sleep" holds by
/// construction. `enabled` maps to `idle_timeout = Some(Ta)` (screensaver
/// on); `!enabled` maps to `idle_timeout = None` (screensaver off,
/// matching `run`'s pre-existing "`None` disables it entirely" contract)
/// — deep sleep can never be armed without the screensaver also being on,
/// but the screensaver can be on without deep sleep being armed (see
/// [`DEEP_SLEEP_ARMED`]).
///
/// This is the settings *model* only — the Settings-screen UI toggle that
/// reads/writes it is a separate concern. Nothing here renders anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdlePowerSetting {
    pub enabled: bool,
}

impl IdlePowerSetting {
    /// `Storage` key this setting persists under. Well under NVS's 15-byte
    /// key-length limit (8 ASCII bytes).
    pub const STORAGE_KEY: &'static str = "idle_pwr";

    /// The `idle_timeout` to hand `crate::run::run`: `Some(Ta)` if enabled
    /// (today's always-on screensaver behavior), `None` if disabled.
    #[must_use]
    pub const fn idle_timeout(&self) -> Option<Duration> {
        if self.enabled {
            Some(DEFAULT_IDLE_TIMEOUT)
        } else {
            None
        }
    }

    /// The `deep_sleep_timeout` to hand `crate::run::run`: `Some(Tb)` only
    /// if this setting is enabled **and** the deep-sleep tier is armed
    /// (see [`DEEP_SLEEP_ARMED`]) — `None` in every other case, including
    /// "enabled but not yet armed," which is exactly today's shipped
    /// default (see that constant's doc comment for why).
    #[must_use]
    pub fn deep_sleep_timeout(&self) -> Option<Duration> {
        deep_sleep_timeout_for(self.enabled, DEEP_SLEEP_ARMED)
    }

    /// Loads the setting from `storage`, defaulting to
    /// [`IdlePowerSetting::default`] (`enabled: true`, matching the
    /// pre-existing always-on screensaver behavior) if the key is absent
    /// or its value isn't the single-byte shape this type writes —
    /// corrupt/legacy/foreign data under this key is treated the same as
    /// "never saved," never as a hard error (there is nothing sensible to
    /// propagate a `Storage`-read error into here; `get` itself is
    /// infallible per the `Storage` trait).
    #[must_use]
    pub fn load<S: Storage>(storage: &S) -> Self {
        match storage.get(Self::STORAGE_KEY).as_deref() {
            Some([0]) => Self { enabled: false },
            Some([1]) => Self { enabled: true },
            _ => Self::default(),
        }
    }

    /// Persists the setting as a single byte (`0`/`1`) under
    /// [`Self::STORAGE_KEY`].
    ///
    /// # Errors
    ///
    /// Returns `S::Error` if the underlying `Storage::set` write fails
    /// (e.g. an NVS write failure on real hardware).
    pub fn save<S: Storage>(&self, storage: &mut S) -> Result<(), S::Error> {
        storage.set(Self::STORAGE_KEY, vec![u8::from(self.enabled)])
    }
}

impl Default for IdlePowerSetting {
    /// `enabled: true` — reproduces the exact behavior every build had
    /// before this setting existed (`run` always received
    /// `Some(DEFAULT_IDLE_TIMEOUT)` unconditionally), so a fresh device
    /// (or a `Storage` that has never had this key written) is
    /// indistinguishable from today's shipped firmware. Deep sleep stays
    /// off regardless (see [`DEEP_SLEEP_ARMED`]), so this default can
    /// never itself cause RAM-only application state to be lost to an
    /// unwanted deep sleep.
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// The pure decision [`IdlePowerSetting::deep_sleep_timeout`] delegates
/// to, factored out so it's testable independent of the current
/// [`DEEP_SLEEP_ARMED`] value (which is `false` today and, by design,
/// changes only via a source edit — see that constant's doc comment).
fn deep_sleep_timeout_for(enabled: bool, armed: bool) -> Option<Duration> {
    if enabled && armed {
        Some(DEFAULT_DEEP_SLEEP_TIMEOUT)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::convert::Infallible;

    #[derive(Default)]
    struct StubStorage {
        value: Option<Vec<u8>>,
    }
    impl Storage for StubStorage {
        type Error = Infallible;
        fn get(&self, key: &str) -> Option<Vec<u8>> {
            assert_eq!(key, IdlePowerSetting::STORAGE_KEY, "must read/write under the documented key");
            self.value.clone()
        }
        fn set(&mut self, key: &str, value: Vec<u8>) -> Result<(), Self::Error> {
            assert_eq!(key, IdlePowerSetting::STORAGE_KEY, "must read/write under the documented key");
            self.value = Some(value);
            Ok(())
        }
    }

    #[test]
    fn default_is_enabled_matching_pre_existing_always_on_behavior() {
        assert_eq!(IdlePowerSetting::default(), IdlePowerSetting { enabled: true });
    }

    #[test]
    fn load_with_no_stored_value_defaults_to_enabled() {
        let storage = StubStorage::default();
        assert_eq!(IdlePowerSetting::load(&storage), IdlePowerSetting { enabled: true });
    }

    #[test]
    fn load_with_a_corrupt_or_foreign_value_falls_back_to_the_default_rather_than_erroring() {
        let storage = StubStorage { value: Some(vec![9, 9, 9]) };
        assert_eq!(IdlePowerSetting::load(&storage), IdlePowerSetting::default());
    }

    #[test]
    fn save_then_load_round_trips_disabled() {
        let mut storage = StubStorage::default();
        IdlePowerSetting { enabled: false }.save(&mut storage).unwrap();
        assert_eq!(IdlePowerSetting::load(&storage), IdlePowerSetting { enabled: false });
    }

    #[test]
    fn save_then_load_round_trips_enabled() {
        let mut storage = StubStorage::default();
        IdlePowerSetting { enabled: true }.save(&mut storage).unwrap();
        assert_eq!(IdlePowerSetting::load(&storage), IdlePowerSetting { enabled: true });
    }

    #[test]
    fn enabled_maps_to_the_screensaver_idle_timeout() {
        assert_eq!(IdlePowerSetting { enabled: true }.idle_timeout(), Some(DEFAULT_IDLE_TIMEOUT));
    }

    #[test]
    fn disabled_maps_to_no_idle_timeout_at_all() {
        assert_eq!(IdlePowerSetting { enabled: false }.idle_timeout(), None);
    }

    #[test]
    #[allow(clippy::assertions_on_constants)] // deliberate: see the comment below
    fn deep_sleep_timeout_is_none_today_even_when_enabled_because_the_tier_is_not_yet_armed() {
        // The load-bearing safety assertion for this power policy:
        // as long as `DEEP_SLEEP_ARMED` is `false`, enabling the setting
        // must never itself arm deep sleep -- only the screensaver. This is
        // deliberately an assertion on a constant (clippy's default lint
        // against that is for the usual "dead code" case, not this one):
        // the whole point is to fail loudly and specifically here, not
        // silently pass by construction, if `DEEP_SLEEP_ARMED` is ever
        // flipped without deliberately updating this test.
        assert!(!DEEP_SLEEP_ARMED, "this test documents today's intended default; update it deliberately if this ever flips");
        assert_eq!(IdlePowerSetting { enabled: true }.deep_sleep_timeout(), None);
    }

    #[test]
    fn disabled_never_arms_deep_sleep_regardless_of_the_armed_flag() {
        assert_eq!(deep_sleep_timeout_for(false, true), None, "disabling the screensaver must disable deep sleep too");
    }

    #[test]
    fn enabled_and_armed_together_yield_the_deep_sleep_timeout() {
        // Proves the mapping itself is correct, independent of
        // `DEEP_SLEEP_ARMED`'s current value -- the one-line flip
        // documented on that constant will "just work" once flipped.
        assert_eq!(deep_sleep_timeout_for(true, true), Some(DEFAULT_DEEP_SLEEP_TIMEOUT));
    }

    #[test]
    fn enabled_but_not_armed_yields_no_deep_sleep_timeout() {
        assert_eq!(deep_sleep_timeout_for(true, false), None);
    }

    #[test]
    fn not_enabled_and_not_armed_yields_no_deep_sleep_timeout() {
        assert_eq!(deep_sleep_timeout_for(false, false), None);
    }
}
