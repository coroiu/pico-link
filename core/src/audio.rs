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

/// The global LDAC Adaptive floor (bead `pico-link-d42g`, design
/// `.planning/design/2026-09-25-adaptive-floor.md` sec 2/4): caps how far
/// ABR may step the LDAC encoder down when a device's `QUALITY` setting is
/// Adaptive. Not a pinned pick -- pins are unaffected (see the design's
/// "Mid-stream behaviour" table). Global, not per-device, same reasoning as
/// [`CushionPolicy`] above (Andreas's 2026-09-25 ruling: "the air varies,
/// not the headset").
///
/// Wire: 1 byte, `0` = unset -> [`Self::default`] (`Kbps330`), `1` =
/// `Kbps330`, `2` = `Kbps246`, `3` = `Kbps198`. Any other value falls back
/// to [`Self::default`], the same per-field-fallback discipline
/// [`CushionPolicy::from_wire`] uses. `firmware/src/codec_ldac.c`'s
/// `pl_codec_ldac_set_floor` maps this exact wire byte to a ladder rung
/// (330=rung4, 246=rung6, 198=rung8) -- `core` never sees a rung, only the
/// wire enum, so the stored/transmitted format is independent of the
/// firmware ladder's shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AbrFloor {
    /// 330 kbps (libldac MQ, rung 4). The default.
    #[default]
    Kbps330,
    /// 246 kbps (libldac Q3, rung 6).
    Kbps246,
    /// 198 kbps (libldac Q5, rung 8, the rail).
    Kbps198,
}

impl AbrFloor {
    #[must_use]
    pub const fn to_wire(self) -> u8 {
        match self {
            Self::Kbps330 => 1,
            Self::Kbps246 => 2,
            Self::Kbps198 => 3,
        }
    }

    /// Any wire value other than `{1, 2, 3}` (including `0` = unset) falls
    /// back to [`Self::default`] (`Kbps330`).
    #[must_use]
    pub const fn from_wire(value: u8) -> Self {
        match value {
            2 => Self::Kbps246,
            3 => Self::Kbps198,
            _ => Self::Kbps330,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Kbps330 => "330 kbps",
            Self::Kbps246 => "246 kbps",
            Self::Kbps198 => "198 kbps",
        }
    }
}
