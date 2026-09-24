//! Idle-screensaver + deep-sleep power policy.
//!
//! Two tiers, both measured from the same "time since last polled input"
//! clock `crate::run::IdlePolicy` already tracks:
//!
//! - **Tier 1 (screensaver)**: after the configured idle timeout, `run`
//!   dims or blanks the display via `crate::platform::DisplaySurface::
//!   set_power`, depending on [`ScreensaverMode`].
//! - **Tier 2 (deep sleep)**: after [`DEFAULT_DEEP_SLEEP_TIMEOUT`] (`Tb`,
//!   `Tb` > `Ta` so the screen is already dimmed/blanked by the time this
//!   tier can fire) idle, `run` additionally calls
//!   `crate::platform::PowerControl::enter_deep_sleep` — but only if
//!   [`DEEP_SLEEP_ARMED`] (see its doc comment for why it is currently
//!   always `false`).
//!
//! [`DisplaySettings`] is the single persisted setting covering both tiers:
//! `timeout == Never` disables the screensaver AND deep sleep together (by
//! construction — see its `idle_timeout`/`deep_sleep_timeout` methods), so
//! "disabling the screensaver disables deep sleep" holds by construction
//! rather than needing separate enforcement. `run` itself stays platform-
//! and setting-free: it only ever sees the `Option<Duration>` values these
//! methods compute, plus the [`ScreensaverMode`] itself.

use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use crate::platform::Storage;

/// How long the display stays fully on with no input before `run`'s
/// idle-screensaver tier acts (`Ta`), for the default timeout option. 60
/// seconds -- Andreas's 2026-09-01 ruling on bead pico-link-4vb.3 ("For now 1
/// minute hardcoded"), superseding the earlier design's "~2min" target. Only
/// takes effect while `App::is_at_home_root()` (see
/// `crate::run::Runner::step`); armed anywhere else (Devices, Settings, the
/// pairing wizard) would read as a crash.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the device stays idle (from the same last-input clock as the
/// screensaver timeout, not from when the screen dimmed/blanked) before
/// `run`'s deep-sleep tier fires (`Tb`), once armed. 600 seconds (10
/// minutes): an arbitrary, conservative `PoC` value — comfortably longer
/// than the longest screensaver timeout option (5 min) so the screen is
/// always already dimmed/blanked first, but not tuned against any real
/// battery-life target. Easy to retune later; nothing about the mechanism
/// depends on the exact value.
pub const DEFAULT_DEEP_SLEEP_TIMEOUT: Duration = Duration::from_secs(600);

/// Whether the deep-sleep tier is armed at all, independent of the
/// screensaver timeout.
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
/// read in exactly one place ([`DisplaySettings::deep_sleep_timeout`]), and
/// a constant is enough to guarantee today's builds never arm it while
/// still being a one-line flip later.
pub const DEEP_SLEEP_ARMED: bool = false;

/// The backlight level (out of 1000, i.e. permille) used for
/// [`crate::platform::DisplayPower::Dim`]. Single source of truth: C never
/// hardcodes a level, it asks `pl_ui_backlight_permille`.
pub const DIM_BACKLIGHT_PERMILLE: u16 = 100;

const _: () = assert!(
    ScreensaverTimeout::Min5.as_secs() < DEFAULT_DEEP_SLEEP_TIMEOUT.as_secs() as u32,
    "the longest screensaver timeout option must stay comfortably under deep sleep's"
);

/// What the screensaver does once idle: dim the backlight, or blank it
/// entirely. Replaces the old `IdlePowerSetting.enabled` bool -- there is no
/// separate on/off toggle; "off" is simply `ScreensaverTimeout::Never`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreensaverMode {
    /// Blank the display (today's behavior).
    Off,
    /// Dim the backlight to [`DIM_BACKLIGHT_PERMILLE`], keeping content
    /// visible.
    Dim,
}

impl ScreensaverMode {
    #[must_use]
    pub const fn to_wire(self) -> u8 {
        match self {
            Self::Off => 1,
            Self::Dim => 2,
        }
    }

    /// Any wire value other than the two valid ones falls back to
    /// [`Self::default`] (`Off`).
    #[must_use]
    pub const fn from_wire(value: u8) -> Self {
        match value {
            2 => Self::Dim,
            _ => Self::Off,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Dim => "Dim",
        }
    }
}

impl Default for ScreensaverMode {
    fn default() -> Self {
        Self::Off
    }
}

/// How long the device waits for input before the screensaver acts.
/// `Never` disables the screensaver (and, by construction, deep sleep) --
/// this replaces the old `IdlePowerSetting.enabled == false` case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScreensaverTimeout {
    Sec30,
    #[default]
    Min1,
    Min2,
    Min5,
    Never,
}

impl ScreensaverTimeout {
    /// All options, in display order.
    pub const ALL: [Self; 5] = [Self::Sec30, Self::Min1, Self::Min2, Self::Min5, Self::Never];

    #[must_use]
    pub const fn as_secs(self) -> u32 {
        match self {
            Self::Sec30 => 30,
            Self::Min1 => 60,
            Self::Min2 => 120,
            Self::Min5 => 300,
            Self::Never => 0,
        }
    }

    /// Any wire value other than `{0, 30, 60, 120, 300}` falls back to
    /// [`Self::default`] (`Min1`, 60s).
    #[must_use]
    pub const fn from_secs(secs: u16) -> Self {
        match secs {
            0 => Self::Never,
            30 => Self::Sec30,
            60 => Self::Min1,
            120 => Self::Min2,
            300 => Self::Min5,
            _ => Self::Min1,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Sec30 => "30 sec",
            Self::Min1 => "1 min",
            Self::Min2 => "2 min",
            Self::Min5 => "5 min",
            Self::Never => "Never",
        }
    }

    /// The `idle_timeout` to hand `crate::run`: `None` for `Never`,
    /// otherwise `Some(as_secs())`.
    #[must_use]
    pub fn idle_timeout(self) -> Option<Duration> {
        if matches!(self, Self::Never) {
            None
        } else {
            Some(Duration::from_secs(u64::from(self.as_secs())))
        }
    }
}

/// The persisted display-power setting: [`ScreensaverMode`] (what happens)
/// crossed with [`ScreensaverTimeout`] (when). Replaces `IdlePowerSetting`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DisplaySettings {
    pub mode: ScreensaverMode,
    pub timeout: ScreensaverTimeout,
}

impl DisplaySettings {
    /// `Storage` key this setting persists under. Well under NVS's 15-byte
    /// key-length limit.
    pub const STORAGE_KEY: &'static str = "disp_set";

    /// The key the old, now-superseded `IdlePowerSetting` persisted under.
    /// Never written by this type; read only as a one-time migration
    /// fallback when [`Self::STORAGE_KEY`] is absent, so a device that only
    /// ever saved the old setting keeps its old effective behavior.
    pub const LEGACY_STORAGE_KEY: &'static str = "idle_pwr";

    #[must_use]
    pub const fn to_wire(self) -> (u8, u16) {
        (self.mode.to_wire(), self.timeout.as_secs() as u16)
    }

    /// Builds a `DisplaySettings` from wire values, each field falling back
    /// to its own default independently of the other.
    #[must_use]
    pub const fn from_wire(mode: u8, timeout_s: u16) -> Self {
        Self { mode: ScreensaverMode::from_wire(mode), timeout: ScreensaverTimeout::from_secs(timeout_s) }
    }

    #[must_use]
    pub fn idle_timeout(&self) -> Option<Duration> {
        self.timeout.idle_timeout()
    }

    /// The `deep_sleep_timeout` to hand `crate::run::run`: `Some(Tb)` only
    /// if the screensaver timeout isn't `Never` **and** the deep-sleep tier
    /// is armed (see [`DEEP_SLEEP_ARMED`]) -- `None` otherwise, including
    /// "timeout set but not yet armed," which is exactly today's shipped
    /// default (see that constant's doc comment for why).
    #[must_use]
    pub fn deep_sleep_timeout(&self) -> Option<Duration> {
        deep_sleep_timeout_for(!matches!(self.timeout, ScreensaverTimeout::Never), DEEP_SLEEP_ARMED)
    }

    /// Loads the setting from `storage`. Tries the current key first; if
    /// absent, falls back to the legacy `IdlePowerSetting` key (`[0]` ->
    /// `Off`/`Never`, `[1]` or anything else -> [`Self::default`]); if
    /// neither key is present, uses [`Self::default`]. The legacy key is
    /// never written or deleted by this path.
    #[must_use]
    pub fn load<S: Storage>(storage: &S) -> Self {
        if let Some(bytes) = storage.get(Self::STORAGE_KEY) {
            if let [mode, secs_lo, secs_hi] = bytes[..] {
                return Self::from_wire(mode, u16::from(secs_lo) | (u16::from(secs_hi) << 8));
            }
        }
        match storage.get(Self::LEGACY_STORAGE_KEY).as_deref() {
            Some([0]) => Self { mode: ScreensaverMode::Off, timeout: ScreensaverTimeout::Never },
            Some(_) => Self::default(),
            None => Self::default(),
        }
    }

    /// Persists the setting as 3 bytes (`[mode, secs_lo, secs_hi]`) under
    /// [`Self::STORAGE_KEY`]. Never touches the legacy key.
    ///
    /// # Errors
    ///
    /// Returns `S::Error` if the underlying `Storage::set` write fails.
    pub fn save<S: Storage>(&self, storage: &mut S) -> Result<(), S::Error> {
        let (mode, secs) = self.to_wire();
        let bytes: Vec<u8> = vec![mode, (secs & 0xFF) as u8, (secs >> 8) as u8];
        storage.set(Self::STORAGE_KEY, bytes)
    }
}

/// The pure decision [`DisplaySettings::deep_sleep_timeout`] delegates to,
/// factored out so it's testable independent of the current
/// [`DEEP_SLEEP_ARMED`] value (which is `false` today and, by design,
/// changes only via a source edit -- see that constant's doc comment).
fn deep_sleep_timeout_for(screensaver_enabled: bool, armed: bool) -> Option<Duration> {
    if screensaver_enabled && armed {
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
        values: alloc::collections::BTreeMap<alloc::string::String, Vec<u8>>,
    }
    impl Storage for StubStorage {
        type Error = Infallible;
        fn get(&self, key: &str) -> Option<Vec<u8>> {
            assert!(
                key == DisplaySettings::STORAGE_KEY || key == DisplaySettings::LEGACY_STORAGE_KEY,
                "must read/write under a documented key"
            );
            self.values.get(key).cloned()
        }
        fn set(&mut self, key: &str, value: Vec<u8>) -> Result<(), Self::Error> {
            assert_eq!(key, DisplaySettings::STORAGE_KEY, "must only ever write the current key");
            self.values.insert(key.into(), value);
            Ok(())
        }
    }

    #[test]
    fn default_is_off_min1_matching_pre_existing_always_on_behavior() {
        assert_eq!(
            DisplaySettings::default(),
            DisplaySettings { mode: ScreensaverMode::Off, timeout: ScreensaverTimeout::Min1 }
        );
    }

    #[test]
    fn load_with_no_stored_value_defaults() {
        let storage = StubStorage::default();
        assert_eq!(DisplaySettings::load(&storage), DisplaySettings::default());
    }

    #[test]
    fn load_prefers_new_key_over_legacy() {
        let mut storage = StubStorage::default();
        storage.values.insert(DisplaySettings::LEGACY_STORAGE_KEY.into(), vec![0]);
        let settings = DisplaySettings { mode: ScreensaverMode::Dim, timeout: ScreensaverTimeout::Sec30 };
        settings.save(&mut storage).unwrap();
        assert_eq!(DisplaySettings::load(&storage), settings);
    }

    #[test]
    fn load_legacy_disabled_maps_to_off_never() {
        let mut storage = StubStorage::default();
        storage.values.insert(DisplaySettings::LEGACY_STORAGE_KEY.into(), vec![0]);
        assert_eq!(
            DisplaySettings::load(&storage),
            DisplaySettings { mode: ScreensaverMode::Off, timeout: ScreensaverTimeout::Never }
        );
    }

    #[test]
    fn load_legacy_enabled_maps_to_default() {
        let mut storage = StubStorage::default();
        storage.values.insert(DisplaySettings::LEGACY_STORAGE_KEY.into(), vec![1]);
        assert_eq!(DisplaySettings::load(&storage), DisplaySettings::default());
    }

    #[test]
    fn load_with_a_corrupt_or_foreign_value_falls_back_to_the_default_rather_than_erroring() {
        let mut storage = StubStorage::default();
        storage.values.insert(DisplaySettings::STORAGE_KEY.into(), vec![9, 9, 9, 9]);
        assert_eq!(DisplaySettings::load(&storage), DisplaySettings::default());
    }

    #[test]
    fn invalid_mode_byte_keeps_a_valid_timeout() {
        let settings = DisplaySettings::from_wire(200, 120);
        assert_eq!(settings.mode, ScreensaverMode::Off);
        assert_eq!(settings.timeout, ScreensaverTimeout::Min2);
    }

    #[test]
    fn invalid_timeout_falls_back_to_default_timeout() {
        let settings = DisplaySettings::from_wire(2, 999);
        assert_eq!(settings.mode, ScreensaverMode::Dim);
        assert_eq!(settings.timeout, ScreensaverTimeout::Min1);
    }

    #[test]
    fn wire_round_trip_of_every_timeout_option() {
        for t in ScreensaverTimeout::ALL {
            for m in [ScreensaverMode::Off, ScreensaverMode::Dim] {
                let settings = DisplaySettings { mode: m, timeout: t };
                let (mode, secs) = settings.to_wire();
                assert_eq!(DisplaySettings::from_wire(mode, secs), settings);
            }
        }
    }

    #[test]
    fn save_then_load_round_trips() {
        let mut storage = StubStorage::default();
        let settings = DisplaySettings { mode: ScreensaverMode::Dim, timeout: ScreensaverTimeout::Min5 };
        settings.save(&mut storage).unwrap();
        assert_eq!(DisplaySettings::load(&storage), settings);
    }

    #[test]
    fn never_maps_to_no_idle_timeout_at_all() {
        assert_eq!(
            DisplaySettings { mode: ScreensaverMode::Off, timeout: ScreensaverTimeout::Never }.idle_timeout(),
            None
        );
    }

    #[test]
    fn min1_maps_to_the_screensaver_idle_timeout() {
        assert_eq!(
            DisplaySettings { mode: ScreensaverMode::Off, timeout: ScreensaverTimeout::Min1 }.idle_timeout(),
            Some(DEFAULT_IDLE_TIMEOUT)
        );
    }

    #[test]
    #[allow(clippy::assertions_on_constants)] // deliberate: see the comment below
    fn deep_sleep_timeout_is_none_today_even_when_screensaver_enabled_because_the_tier_is_not_yet_armed() {
        // The load-bearing safety assertion for this power policy:
        // as long as `DEEP_SLEEP_ARMED` is `false`, a non-Never timeout
        // must never itself arm deep sleep -- only the screensaver. This is
        // deliberately an assertion on a constant (clippy's default lint
        // against that is for the usual "dead code" case, not this one):
        // the whole point is to fail loudly and specifically here, not
        // silently pass by construction, if `DEEP_SLEEP_ARMED` is ever
        // flipped without deliberately updating this test.
        assert!(!DEEP_SLEEP_ARMED, "this test documents today's intended default; update it deliberately if this ever flips");
        assert_eq!(
            DisplaySettings { mode: ScreensaverMode::Off, timeout: ScreensaverTimeout::Min1 }.deep_sleep_timeout(),
            None
        );
    }

    #[test]
    fn never_never_arms_deep_sleep_regardless_of_the_armed_flag() {
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
