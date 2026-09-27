//! Shared preset value-range validation, factored out of
//! [`super::import::to_preset`] (design section 4, ADA DESIGN comment on
//! `pico-link-jyhk.17`: "Value ranges use the import limits (gain, Q, Fc,
//! preamp) factored out of import.rs:172 `to_preset` into one
//! `validate(&Preset)` shared by import, SAVE and PREVIEW") -- one set of
//! limits, one place values are rejected, so a web-originated SAVE/PREVIEW
//! and an imported document can never disagree about what's in range.
//!
//! Reject, never clamp -- the same discipline [`super::import`]'s own module
//! doc documents.

use super::preset::{Preamp, Preset, MAX_BANDS};

/// The accepted gain range, in dB, symmetric.
pub const GAIN_DB_MAX: f32 = 30.0;
/// The accepted Q range.
pub const Q_MIN: f32 = 0.1;
pub const Q_MAX: f32 = 65.0;
/// The accepted center-frequency range, in Hz.
pub const FREQ_HZ_MIN: f32 = 10.0;
pub const FREQ_HZ_MAX: f32 = 0.45 * 44_100.0; // 19_845.0
/// The accepted preamp range, in dB.
pub const PREAMP_DB_MIN: f32 = -30.0;
pub const PREAMP_DB_MAX: f32 = 6.0;

/// Everything [`validate_preset`] (or a caller checking one value at a
/// time, like [`super::import::to_preset`]) can reject -- band indices are
/// 1-based, matching the source `Filter N:` line / the band's position in
/// [`Preset::bands`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ValidateError {
    /// More bands than a `PlDspProgram` (`PL_DSP_MAX_BIQUADS`) can hold.
    TooManyBands { band_count: usize },
    GainOutOfRange { band_index: usize, gain_db: f32 },
    FreqOutOfRange { band_index: usize, freq_hz: f32 },
    QOutOfRange { band_index: usize, q: f32 },
    PreampOutOfRange { preamp_db: f32 },
}

/// Checks one band's already-converted values against the accepted ranges.
/// Pure and unit-agnostic to the caller's own representation (f32 dB/Hz/Q,
/// whether decoded from a wire blob or parsed from text) -- both
/// [`validate_preset`] and [`super::import::to_preset`] call this with
/// their own conversion of the same underlying value.
///
/// # Errors
///
/// The first out-of-range field, in gain/freq/Q order.
pub fn validate_band_values(band_index: usize, gain_db: f32, freq_hz: f32, q: f32) -> Result<(), ValidateError> {
    if !(-GAIN_DB_MAX..=GAIN_DB_MAX).contains(&gain_db) {
        return Err(ValidateError::GainOutOfRange { band_index, gain_db });
    }
    if !(FREQ_HZ_MIN..=FREQ_HZ_MAX).contains(&freq_hz) {
        return Err(ValidateError::FreqOutOfRange { band_index, freq_hz });
    }
    if !(Q_MIN..=Q_MAX).contains(&q) {
        return Err(ValidateError::QOutOfRange { band_index, q });
    }
    Ok(())
}

/// Checks a preamp value in dB against the accepted range.
///
/// # Errors
///
/// [`ValidateError::PreampOutOfRange`] if out of range.
pub fn validate_preamp_db(preamp_db: f32) -> Result<(), ValidateError> {
    if !(PREAMP_DB_MIN..=PREAMP_DB_MAX).contains(&preamp_db) {
        return Err(ValidateError::PreampOutOfRange { preamp_db });
    }
    Ok(())
}

/// Validates a fully-built [`Preset`]'s band count and every band/preamp
/// value, in the preset's own exact wire units ([`super::preset::Band`]'s
/// `gain_db`/`freq_hz`/`q` accessors, [`Preamp::Explicit`]'s centi-dB) --
/// the shape a `HOST_OP` `SAVE`/`PREVIEW` (already strictly decoded off the
/// wire, design section 4) validates before it ever reaches the store or
/// the live DSP preview.
///
/// # Errors
///
/// The first out-of-range field found (band count, then each band in
/// order, then the preamp).
pub fn validate_preset(preset: &Preset) -> Result<(), ValidateError> {
    if preset.bands.len() > MAX_BANDS {
        return Err(ValidateError::TooManyBands { band_count: preset.bands.len() });
    }
    for (i, band) in preset.bands.iter().enumerate() {
        validate_band_values(i + 1, band.gain_db(), band.freq_hz(), band.q())?;
    }
    if let Preamp::Explicit(cdb) = preset.preamp {
        validate_preamp_db(f32::from(cdb) * 0.01)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::preset::{Band, BandKind, CrossfeedLevel};
    use super::*;
    use alloc::vec;

    // Every caller in this test module passes values already within (or
    // just past, for the rejection tests) the accepted ranges, so these
    // casts never truncate/wrap in practice -- same discipline
    // `import.rs`'s own `#[allow]`s document for the identical conversions.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn band(gain_db: f32, freq_hz: f32, q: f32) -> Band {
        Band { kind: BandKind::Peak, freq_half_hz: libm::roundf(freq_hz * 2.0) as u16, gain_cdb: libm::roundf(gain_db * 100.0) as i16, q_milli: libm::roundf(q * 1000.0) as u16 }
    }

    #[test]
    fn a_preset_within_every_limit_validates() {
        let preset = Preset { name: "OK".into(), crossfeed: CrossfeedLevel::Off, bands: vec![band(3.0, 1000.0, 1.0)], preamp: Preamp::Auto, eq_locked: false };
        assert!(validate_preset(&preset).is_ok());
    }

    #[test]
    fn too_many_bands_is_rejected() {
        let bands = core::iter::repeat(band(0.0, 1000.0, 1.0)).take(MAX_BANDS + 1).collect();
        let preset = Preset { name: "Big".into(), crossfeed: CrossfeedLevel::Off, bands, preamp: Preamp::Auto, eq_locked: false };
        assert_eq!(validate_preset(&preset), Err(ValidateError::TooManyBands { band_count: MAX_BANDS + 1 }));
    }

    #[test]
    fn gain_over_30db_is_rejected_with_its_1_based_band_index() {
        let preset = Preset { name: "Loud".into(), crossfeed: CrossfeedLevel::Off, bands: vec![band(3.0, 1000.0, 1.0), band(35.0, 500.0, 1.0)], preamp: Preamp::Auto, eq_locked: false };
        let err = validate_preset(&preset).unwrap_err();
        assert!(matches!(err, ValidateError::GainOutOfRange { band_index: 2, .. }), "{err:?}");
    }

    #[test]
    fn freq_out_of_range_is_rejected() {
        let preset = Preset { name: "Hi".into(), crossfeed: CrossfeedLevel::Off, bands: vec![band(1.0, 20_000.0, 1.0)], preamp: Preamp::Auto, eq_locked: false };
        assert!(matches!(validate_preset(&preset).unwrap_err(), ValidateError::FreqOutOfRange { band_index: 1, .. }));
    }

    #[test]
    fn q_out_of_range_is_rejected() {
        let preset = Preset { name: "Sharp".into(), crossfeed: CrossfeedLevel::Off, bands: vec![band(1.0, 1000.0, 0.01)], preamp: Preamp::Auto, eq_locked: false };
        assert!(matches!(validate_preset(&preset).unwrap_err(), ValidateError::QOutOfRange { band_index: 1, .. }));
    }

    #[test]
    fn explicit_preamp_out_of_range_is_rejected() {
        let preset = Preset { name: "Loud".into(), crossfeed: CrossfeedLevel::Off, bands: vec![], preamp: Preamp::Explicit(1000), eq_locked: false };
        assert!(matches!(validate_preset(&preset).unwrap_err(), ValidateError::PreampOutOfRange { .. }));
    }

    #[test]
    fn auto_preamp_never_needs_range_checking() {
        let preset = Preset { name: "Auto".into(), crossfeed: CrossfeedLevel::Off, bands: vec![], preamp: Preamp::Auto, eq_locked: false };
        assert!(validate_preset(&preset).is_ok());
    }
}
