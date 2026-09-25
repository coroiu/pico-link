//! The global congestion-cushion policy (bead `pico-link-8pp1`, design
//! `.planning/design/2026-09-24-congestion-cushion.md`).
//!
//! `core` neither implements the resync trim nor applies this policy --
//! that lives entirely in `firmware/src/a2dp.c`'s hold-timer mechanism
//! (`pl_a2dp_set_trim_policy`/`pl_a2dp_resync_decide`, S1). `core` only
//! holds the LIVE value so a Settings row/picker (S4) can display and
//! change it, the same "core owns the setting, C owns the effect" split
//! [`crate::power::DisplaySettings`] already uses for the screensaver.

/// Global (not per-device -- Andreas's 2026-09-24 ruling) trim-hold
/// policy. Wire: 1 byte, `0` = unset -> [`Self::default`] (`Low`), `1` =
/// `Low`, `2` = `Stable`. Wire value `3` is reserved for a possible future
/// "Super stable" mode (a 64KB ring -- Andreas 2026-09-24, no evidence yet
/// that it's needed): any value this enum doesn't yet understand,
/// including `3` today, falls back to [`Self::default`], the same
/// per-field-fallback discipline [`crate::power::ScreensaverMode::from_wire`]
/// already uses -- so adding that third variant later is purely additive
/// (a new match arm here, no record-version bump in `persist.c`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CushionPolicy {
    /// Hold 0ms, hard band 15ms -- bit-identical to the resync trim's
    /// pre-8pp1.1 behavior. The default (Andreas's 2026-09-24 ruling:
    /// default is Low until proven otherwise).
    #[default]
    Low,
    /// Hold 3000ms, hard band 70ms -- tolerates a stall long enough for
    /// LDAC ABR/catch-up headroom to drain the backlog before giving up
    /// and trimming (design sec 3).
    Stable,
}

impl CushionPolicy {
    #[must_use]
    pub const fn to_wire(self) -> u8 {
        match self {
            Self::Low => 1,
            Self::Stable => 2,
        }
    }

    /// Any wire value other than `{1, 2}` (including `0` = unset and `3`,
    /// reserved for a future third mode) falls back to [`Self::default`]
    /// (`Low`).
    #[must_use]
    pub const fn from_wire(value: u8) -> Self {
        match value {
            2 => Self::Stable,
            _ => Self::Low,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Low => "Low latency",
            Self::Stable => "Stable",
        }
    }
}
