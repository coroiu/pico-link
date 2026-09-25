//! Converts an [`eqapo::ParsedDocument`] into a locked, imported v2
//! [`Preset`] and hands it to a [`PresetStore`] -- bead `pico-link-ryw.12.2`,
//! Ada's DESIGN comment on `pico-link-ryw.12` ("import conversion" and
//! "limits" sections) plus Andreas's 2026-09-25 rulings and Uma's UX
//! reconciliation (`pico-link-ryw.12.3`'s durable comment). This module
//! stays FFI-free like the rest of [`super`] -- wiring an FFI entry point
//! that calls [`import`] and turns its `PresetId` into a
//! `Command::SavePreset` is `pico-link-ryw.12.4`'s job, not this one.
//!
//! # Limits (never clamped, always rejected)
//!
//! More than [`super::preset::MAX_BANDS`] enabled filters is already a hard
//! parse-time rejection ([`EqApoError::TooManyBands`], raised by
//! [`EqApoSession::feed_line`] itself) -- nothing here needs to re-check
//! band count. What this module adds are the value-range checks the parser
//! doesn't do, because the parser's job is to carry the pasted numbers
//! exactly (`eqapo`'s module doc): `|gain_db| <= 30`, `q` in `[0.1, 65.0]`,
//! `freq_hz` in `[10.0, 0.45 * 44100.0]` (`19845.0`), and the preamp in
//! `[-30.0, 6.0]`. Any violation rejects the whole import with the
//! offending band's 1-based `Filter N:` index (or the preamp, which has no
//! band index) -- never a silent clamp.
//!
//! # Naming and duplicates (Andreas's ruling, `pico-link-ryw.12` comment
//! thread, 2026-09-25 22:01/22:05)
//!
//! The name comes from the computer, never the device: an optional
//! `Name:` line in the pasted text, else `host_name` (already resolved on
//! the host side, e.g. its own `--name` flag or the file's stem), always
//! truncated to [`MAX_NAME_BYTES`]. A same-name collision against an
//! already-IMPORTED effect (`eq_locked` is this module's only "was this
//! imported" signal -- the on-device editor never sets it) REPLACES it in
//! place, preserving its id (so device assignments keep resolving) and its
//! crossfeed (so a re-sent tweak doesn't silently reset a setting the user
//! chose on the device). A collision against a HAND-MADE effect never
//! overwrites it: the import gets a `" 2"` (then `" 3"`, ...) suffix,
//! lowest free, base truncated at a UTF-8 boundary so the whole name still
//! fits [`MAX_NAME_BYTES`]. A brand-new name against a full ([`MAX_PRESETS`])
//! store is rejected with [`ImportError::StoreFull`] -- a same-name REPLACE
//! never needs a free slot, so it is never blocked by a full store.
//!
//! New imports always start with [`CrossfeedLevel::Off`] (Uma's design,
//! sec 3: "so they sound exactly like the published curve"), get
//! [`Preamp::Explicit`] from the parsed preamp line, and are `eq_locked`.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::eqapo::{EqApoError, EqApoLineError, EqApoSession, ParsedDocument};
use super::preset::{Band, CrossfeedLevel, Preamp, Preset, MAX_NAME_BYTES};
use super::store::PresetStore;

/// The flash store's slot budget (`PL:P:0`..`PL:P:7`, `firmware/src/persist.h`)
/// -- the same value as `crate::app::screens::effects::MAX_EFFECTS`, kept as
/// its own constant here because [`super`] is deliberately app- and
/// FFI-free (see this module's doc comment) and must not depend on
/// `crate::app`.
pub const MAX_PRESETS: usize = 8;

/// The accepted gain range, in dB, symmetric (design: `|gain| > 30`
/// rejects).
const GAIN_DB_MAX: f32 = 30.0;
/// The accepted Q range (design: `Q outside 0.1-65`).
const Q_MIN: f32 = 0.1;
const Q_MAX: f32 = 65.0;
/// The accepted center-frequency range, in Hz (design: `Fc outside 10 Hz -
/// 0.45*44100`).
const FREQ_HZ_MIN: f32 = 10.0;
const FREQ_HZ_MAX: f32 = 0.45 * 44_100.0; // 19_845.0
/// The accepted preamp range, in dB (design: `preamp outside -30..+6`).
const PREAMP_DB_MIN: f32 = -30.0;
const PREAMP_DB_MAX: f32 = 6.0;

/// Everything that can reject an import -- a parse failure (with a line
/// number, when the parser can attribute one), an out-of-range value (with
/// the offending band's 1-based `Filter N:` index, when there is one), or
/// a full store blocking a genuinely new name. Never a silent clamp (see
/// this module's doc comment).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ImportError {
    /// One line failed to parse -- carries its 1-based line number.
    Line(EqApoLineError),
    /// [`EqApoSession::finish`]'s session-wide failure
    /// ([`EqApoError::MissingPreamp`] or [`EqApoError::NoBands`]) -- not
    /// attributable to a single line.
    Session(EqApoError),
    /// `Filter band_index:`'s gain was outside `[-30, +30]` dB.
    GainOutOfRange { band_index: usize, gain_db: f32 },
    /// `Filter band_index:`'s center frequency was outside `[10, 19845]` Hz.
    FreqOutOfRange { band_index: usize, freq_hz: f32 },
    /// `Filter band_index:`'s Q was outside `[0.1, 65]`.
    QOutOfRange { band_index: usize, q: f32 },
    /// The `Preamp:` line was outside `[-30, +6]` dB.
    PreampOutOfRange { preamp_db: f32 },
    /// The name is new (not a same-name replace of an existing imported
    /// effect) and the store already holds [`MAX_PRESETS`] presets.
    StoreFull,
}

impl From<EqApoLineError> for ImportError {
    fn from(e: EqApoLineError) -> Self {
        Self::Line(e)
    }
}

/// What [`import`] did with the store, alongside the resulting preset's id.
/// A caller building the `Command::SavePreset` a real save needs
/// (`pico-link-ryw.12.4`) uses the returned id either way -- `Replaced`
/// and `Renamed` both still need one save, just like `Created`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportOutcome {
    /// No existing preset's name matched: a fresh id was allocated.
    Created,
    /// The name matched an existing IMPORTED effect: replaced in place,
    /// same id, its crossfeed and (by construction, since the id didn't
    /// change) its device assignments preserved.
    Replaced,
    /// The name matched an existing HAND-MADE effect: imported under a
    /// `" N"`-suffixed name instead, with a fresh id.
    Renamed,
}

/// Parses `text` (an Equalizer APO / `AutoEQ` document, one line per
/// `str::lines`-separated line) and imports it into `store`: converts to a
/// locked v2 [`Preset`] (limits above), resolves the name (this module's
/// doc comment), and applies the replace/suffix/reject duplicate-name
/// policy. `host_name` is the name the host side already resolved (its own
/// `--name` flag or the uploaded file's stem) -- used only when the parsed
/// document has no `Name:` line.
///
/// Returns the resulting preset's id and what happened to the store.
///
/// # Errors
///
/// Returns the first [`ImportError`] encountered -- a parse failure, an
/// out-of-range value, or a full store blocking a genuinely new name (see
/// this module's doc comment). Never partially mutates `store` on an error
/// path -- every check that can fail runs before any
/// `store.create`/`store.update` call.
pub fn import(store: &mut PresetStore, text: &str, host_name: &str) -> Result<(u16, ImportOutcome), ImportError> {
    let doc = parse(text)?;
    let preset = to_preset(&doc, host_name)?;
    place(store, preset)
}

/// Runs `text` through [`EqApoSession`] line by line, same as the `EQ
/// BEGIN`/`EQ <line>`/`EQ END` debug console does one call at a time
/// (`eqapo`'s module doc) -- this is just the whole-document-at-once
/// shape an uploaded file needs (one `str::lines` call splitting on
/// `\n`/`\r\n`).
fn parse(text: &str) -> Result<ParsedDocument, ImportError> {
    let mut session = EqApoSession::new();
    for line in text.lines() {
        session.feed_line(line)?;
    }
    session.finish().map_err(ImportError::Session)
}

/// Converts a [`ParsedDocument`] to a locked, `Explicit`-preamp,
/// `Off`-crossfeed [`Preset`] -- the pure numeric conversion, with no
/// store access, so it can be unit-tested (and its error paths exercised)
/// without a [`PresetStore`] in the loop.
fn to_preset(doc: &ParsedDocument, host_name: &str) -> Result<Preset, ImportError> {
    if !(PREAMP_DB_MIN..=PREAMP_DB_MAX).contains(&doc.preamp_db) {
        return Err(ImportError::PreampOutOfRange { preamp_db: doc.preamp_db });
    }
    #[allow(clippy::cast_possible_truncation)] // clamped to the accepted range just above; *100 of [-3000, 600] fits i16 easily
    let preamp_cdb = libm::roundf(doc.preamp_db * 100.0) as i16;

    let mut bands = Vec::with_capacity(doc.bands.len());
    for (i, band) in doc.bands.iter().enumerate() {
        let band_index = i + 1; // 1-based, matching the source `Filter N:` line
        if !(-GAIN_DB_MAX..=GAIN_DB_MAX).contains(&band.gain_db) {
            return Err(ImportError::GainOutOfRange { band_index, gain_db: band.gain_db });
        }
        if !(FREQ_HZ_MIN..=FREQ_HZ_MAX).contains(&band.freq_hz) {
            return Err(ImportError::FreqOutOfRange { band_index, freq_hz: band.freq_hz });
        }
        if !(Q_MIN..=Q_MAX).contains(&band.q) {
            return Err(ImportError::QOutOfRange { band_index, q: band.q });
        }

        #[allow(clippy::cast_possible_truncation)] // gain_db in [-30, 30] => gain_cdb in [-3000, 3000], well within kind_gain's +/-4095
        let gain_cdb = libm::roundf(band.gain_db * 100.0) as i16;
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)] // freq_hz in [10, 19845] => freq_half_hz in [20, 39690], well within u16
        let freq_half_hz = libm::roundf(band.freq_hz * 2.0) as u16;
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)] // q in [0.1, 65] => q_milli in [100, 65000], well within u16
        let q_milli = libm::roundf(band.q * 1000.0) as u16;

        bands.push(Band { kind: band.kind, freq_half_hz, gain_cdb, q_milli });
    }

    let name = resolve_name(doc, host_name);
    Ok(Preset { name, crossfeed: CrossfeedLevel::Off, bands, preamp: Preamp::Explicit(preamp_cdb), eq_locked: true })
}

/// `Name:` line, else `host_name` -- Andreas's ruling -- truncated to
/// [`MAX_NAME_BYTES`] either way. [`Preset::new`]'s own truncation isn't
/// reused here because that constructor also resets every other field to
/// its hand-made defaults, which this caller immediately overwrites.
fn resolve_name(doc: &ParsedDocument, host_name: &str) -> String {
    let raw = doc.name.as_deref().unwrap_or(host_name);
    truncate_to_name_bytes(raw)
}

fn truncate_to_name_bytes(name: &str) -> String {
    if name.len() <= MAX_NAME_BYTES {
        return String::from(name);
    }
    let mut end = MAX_NAME_BYTES;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    String::from(&name[..end])
}

/// Applies the replace/suffix/reject duplicate-name policy and commits
/// `candidate` into `store` -- see this module's doc comment.
fn place(store: &mut PresetStore, candidate: Preset) -> Result<(u16, ImportOutcome), ImportError> {
    if let Some(existing_id) = find_by_name(store, &candidate.name) {
        // Safe to unwrap: `find_by_name` only returns ids it just found in the store.
        let existing_locked = store.get(existing_id).is_some_and(|p| p.eq_locked);
        if existing_locked {
            // Replace in place: keep the id (device assignments resolve
            // against it unchanged) and the existing crossfeed (Uma's
            // design sec 3: "keeping ... its crossfeed setting").
            let existing_crossfeed = store.get(existing_id).map(|p| p.crossfeed).unwrap_or_default();
            let replacement = Preset { crossfeed: existing_crossfeed, ..candidate };
            store.update(existing_id, replacement);
            return Ok((existing_id, ImportOutcome::Replaced));
        }

        // Hand-made collision: suffix, lowest free, never overwrite.
        if store.len() >= MAX_PRESETS {
            return Err(ImportError::StoreFull);
        }
        let suffixed_name = lowest_free_suffixed_name(store, &candidate.name);
        let renamed = Preset { name: suffixed_name, ..candidate };
        let id = store.create(renamed);
        return Ok((id, ImportOutcome::Renamed));
    }

    // A genuinely new name.
    if store.len() >= MAX_PRESETS {
        return Err(ImportError::StoreFull);
    }
    let id = store.create(candidate);
    Ok((id, ImportOutcome::Created))
}

fn find_by_name(store: &PresetStore, name: &str) -> Option<u16> {
    store.iter().find(|(_, preset)| preset.name == name).map(|(id, _)| id)
}

/// `"{base} 2"`, `"{base} 3"`, ... -- the lowest-numbered suffix not
/// already used by some other preset's name, with `base` truncated at a
/// UTF-8 boundary so the whole suffixed name still fits [`MAX_NAME_BYTES`]
/// (Uma's design sec 3). Starts at 2 (there is no `" 1"` suffix -- the
/// unsuffixed name is the collision).
fn lowest_free_suffixed_name(store: &PresetStore, base: &str) -> String {
    for n in 2u32.. {
        let suffix = format!(" {n}");
        let candidate = with_suffix(base, &suffix);
        if find_by_name(store, &candidate).is_none() {
            return candidate;
        }
    }
    unreachable!("an unbounded loop always finds a free suffix before it could exhaust u32")
}

fn with_suffix(base: &str, suffix: &str) -> String {
    let budget = MAX_NAME_BYTES.saturating_sub(suffix.len());
    let mut end = base.len().min(budget);
    while end > 0 && !base.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &base[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::coeffs::Program;
    use crate::dsp::eqapo::EqApoOverride;

    const XM3_PRESET: &[&str] = &[
        "Preamp: -4.41 dB",
        "Filter 1:  ON  LS  Fc 40 Hz  Gain -1.76 dB  BW Oct 1.917",
        "Filter 2:  ON  PK  Fc 80 Hz  Gain -2 dB  BW Oct 1.485",
        "Filter 3:  ON  PK  Fc 540 Hz  Gain -1.4 dB  BW Oct 0.482",
        "Filter 4:  ON  PK  Fc 1220 Hz  Gain 3.3 dB  BW Oct 0.687",
        "Filter 5:  ON  PK  Fc 2941 Hz  Gain -2.4 dB  BW Oct 0.242",
        "Filter 6:  ON  PK  Fc 3438 Hz  Gain 1.7 dB  BW Oct 0.311",
        "Filter 7:  ON  PK  Fc 4544 Hz  Gain 6.5 dB  BW Oct 0.818",
        "Filter 8:  ON  PK  Fc 9250 Hz  Gain -4.7 dB  BW Oct 0.413",
        "Filter 9:  ON  PK  Fc 9822 Hz  Gain -0.1 dB  BW Oct 0.349",
        "Filter 10:  ON  HS  Fc 10000 Hz  Gain 5.9 dB  BW Oct 1.917",
    ];

    fn xm3_text() -> String {
        XM3_PRESET.join("\n")
    }

    fn approx_eq(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() < eps
    }

    #[test]
    fn xm3_preset_round_trips_exactly_through_v2_to_the_same_biquads_as_ryw11s_override() {
        let text = xm3_text();

        // ryw.11's own conversion path (EqApoOverride), the reference.
        let mut session = EqApoSession::new();
        for line in text.lines() {
            session.feed_line(line).unwrap();
        }
        let doc = session.finish().unwrap();
        let reference = EqApoOverride::from_parsed(doc).to_program(48_000);

        // ryw.12's import conversion path, through a real PresetStore.
        let mut store = PresetStore::new();
        let (id, outcome) = import(&mut store, &text, "fallback").expect("XM3 preset imports");
        assert_eq!(outcome, ImportOutcome::Created);
        let preset = store.get(id).expect("the imported preset is in the store");
        assert_eq!(preset.bands.len(), 10);
        assert!(preset.eq_locked);
        assert_eq!(preset.crossfeed, CrossfeedLevel::Off);
        assert_eq!(preset.preamp, Preamp::Explicit(-441));

        let under_test = Program::from_preset(preset, 48_000);

        assert_eq!(under_test.biquads.len(), reference.biquads.len());
        assert!(approx_eq(under_test.preamp_linear, reference.preamp_linear, 1e-5));
        for (a, b) in under_test.biquads.iter().zip(reference.biquads.iter()) {
            assert!(approx_eq(a.b0, b.b0, 1e-4), "b0: {} vs {}", a.b0, b.b0);
            assert!(approx_eq(a.b1, b.b1, 1e-4), "b1: {} vs {}", a.b1, b.b1);
            assert!(approx_eq(a.b2, b.b2, 1e-4), "b2: {} vs {}", a.b2, b.b2);
            assert!(approx_eq(a.a1, b.a1, 1e-4), "a1: {} vs {}", a.a1, b.a1);
            assert!(approx_eq(a.a2, b.a2, 1e-4), "a2: {} vs {}", a.a2, b.a2);
        }
    }

    #[test]
    fn a_name_line_wins_over_the_host_name() {
        let text = format!("Name: From File\n{}", xm3_text());
        let mut store = PresetStore::new();
        let (id, _) = import(&mut store, &text, "From Host").unwrap();
        assert_eq!(store.get(id).unwrap().name, "From File");
    }

    #[test]
    fn the_host_name_is_used_when_there_is_no_name_line() {
        let mut store = PresetStore::new();
        let (id, _) = import(&mut store, &xm3_text(), "From Host").unwrap();
        assert_eq!(store.get(id).unwrap().name, "From Host");
    }

    #[test]
    fn a_long_name_is_truncated_to_max_name_bytes() {
        let text = format!("Name: This Name Is Definitely Too Long For Sixteen Bytes\n{}", xm3_text());
        let mut store = PresetStore::new();
        let (id, _) = import(&mut store, &text, "fallback").unwrap();
        assert!(store.get(id).unwrap().name.len() <= MAX_NAME_BYTES);
    }

    #[test]
    fn replacing_an_imported_effect_keeps_its_id_assignments_and_crossfeed() {
        let text = format!("Name: XM3\n{}", xm3_text());
        let mut store = PresetStore::new();
        let (first_id, first_outcome) = import(&mut store, &text, "fallback").unwrap();
        assert_eq!(first_outcome, ImportOutcome::Created);

        // Simulate the user setting crossfeed on the device after import.
        store.update(first_id, Preset { crossfeed: CrossfeedLevel::Medium, ..store.get(first_id).unwrap().clone() });

        // Re-import the same name with a tweak (different gain on filter 1).
        let retext = text.replace("Gain -1.76 dB", "Gain -1.50 dB");
        let (second_id, second_outcome) = import(&mut store, &retext, "fallback").unwrap();

        assert_eq!(second_id, first_id, "replace must keep the same id so device assignments still resolve");
        assert_eq!(second_outcome, ImportOutcome::Replaced);
        let replaced = store.get(second_id).unwrap();
        assert_eq!(replaced.crossfeed, CrossfeedLevel::Medium, "replace must preserve the existing crossfeed");
        assert_eq!(replaced.bands[0].gain_cdb, -150, "replace must apply the new values");
        assert_eq!(store.len(), 1, "a replace must not grow the store");
    }

    #[test]
    fn a_clash_with_a_hand_made_effect_gets_the_2_suffix() {
        let mut store = PresetStore::new();
        let hand_made = Preset::new("XM3");
        store.create(hand_made);

        let text = format!("Name: XM3\n{}", xm3_text());
        let (id, outcome) = import(&mut store, &text, "fallback").unwrap();
        assert_eq!(outcome, ImportOutcome::Renamed);
        assert_eq!(store.get(id).unwrap().name, "XM3 2");
        assert_eq!(store.len(), 2, "the hand-made effect must survive untouched");
    }

    #[test]
    fn a_second_hand_made_clash_gets_the_lowest_free_suffix() {
        let mut store = PresetStore::new();
        store.create(Preset::new("XM3"));
        store.create(Preset::new("XM3 2"));

        let text = format!("Name: XM3\n{}", xm3_text());
        let (id, outcome) = import(&mut store, &text, "fallback").unwrap();
        assert_eq!(outcome, ImportOutcome::Renamed);
        assert_eq!(store.get(id).unwrap().name, "XM3 3");
    }

    #[test]
    fn a_full_store_rejects_a_new_name() {
        let mut store = PresetStore::new();
        for i in 0..MAX_PRESETS {
            store.create(Preset::new(&format!("Hand {i}")));
        }
        assert_eq!(store.len(), MAX_PRESETS);

        let text = format!("Name: New One\n{}", xm3_text());
        let err = import(&mut store, &text, "fallback").unwrap_err();
        assert_eq!(err, ImportError::StoreFull);
    }

    #[test]
    fn a_full_store_still_accepts_a_same_name_replace_of_an_imported_effect() {
        let mut store = PresetStore::new();
        let text = format!("Name: XM3\n{}", xm3_text());
        import(&mut store, &text, "fallback").unwrap();
        for i in 1..MAX_PRESETS {
            store.create(Preset::new(&format!("Hand {i}")));
        }
        assert_eq!(store.len(), MAX_PRESETS);

        let retext = text.replace("Gain -1.76 dB", "Gain -1.50 dB");
        let (_, outcome) = import(&mut store, &retext, "fallback").unwrap();
        assert_eq!(outcome, ImportOutcome::Replaced);
        assert_eq!(store.len(), MAX_PRESETS, "a replace must not consume a slot");
    }

    #[test]
    fn more_than_ten_filters_is_rejected_not_dropped() {
        let mut lines: Vec<String> = alloc::vec![String::from("Preamp: 0 dB")];
        for i in 0..11 {
            lines.push(format!("Filter {}: ON PK Fc 100 Hz Gain 1 dB Q 1", i + 1));
        }
        let text = lines.join("\n");
        let mut store = PresetStore::new();
        let err = import(&mut store, &text, "fallback").unwrap_err();
        match err {
            ImportError::Line(EqApoLineError { error: EqApoError::TooManyBands, .. }) => {}
            other => panic!("expected TooManyBands, got {other:?}"),
        }
        assert!(store.is_empty(), "a rejected import must not touch the store");
    }

    #[test]
    fn a_gain_over_30db_is_rejected() {
        let text = "Preamp: 0 dB\nFilter 1: ON PK Fc 100 Hz Gain 35 dB Q 1";
        let mut store = PresetStore::new();
        let err = import(&mut store, text, "fallback").unwrap_err();
        assert_eq!(err, ImportError::GainOutOfRange { band_index: 1, gain_db: 35.0 });
    }

    #[test]
    fn a_q_below_the_floor_is_rejected() {
        let text = "Preamp: 0 dB\nFilter 1: ON PK Fc 100 Hz Gain 1 dB Q 0.05";
        let mut store = PresetStore::new();
        let err = import(&mut store, text, "fallback").unwrap_err();
        assert_eq!(err, ImportError::QOutOfRange { band_index: 1, q: 0.05 });
    }

    #[test]
    fn a_freq_above_the_ceiling_is_rejected() {
        let text = "Preamp: 0 dB\nFilter 1: ON PK Fc 20000 Hz Gain 1 dB Q 1";
        let mut store = PresetStore::new();
        let err = import(&mut store, text, "fallback").unwrap_err();
        assert_eq!(err, ImportError::FreqOutOfRange { band_index: 1, freq_hz: 20_000.0 });
    }

    #[test]
    fn a_preamp_out_of_range_is_rejected() {
        let text = "Preamp: 10 dB\nFilter 1: ON PK Fc 100 Hz Gain 1 dB Q 1";
        let mut store = PresetStore::new();
        let err = import(&mut store, text, "fallback").unwrap_err();
        assert_eq!(err, ImportError::PreampOutOfRange { preamp_db: 10.0 });
    }

    #[test]
    fn a_malformed_line_is_rejected_with_its_line_number() {
        let text = "Preamp: -4.41\nFilter 1: ON PK Fc 100 Hz Gain 1 dB Q 1";
        let mut store = PresetStore::new();
        let err = import(&mut store, text, "fallback").unwrap_err();
        assert_eq!(err, ImportError::Line(EqApoLineError { line: 1, error: EqApoError::MalformedLine }));
    }
}
