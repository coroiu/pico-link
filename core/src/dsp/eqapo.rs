//! A parser for the Equalizer APO / AutoEQ parametric-EQ text format, plus
//! (bead `pico-link-ryw.11` only) the non-persisted debug override it
//! builds for the `PL_DEBUG_REMOTE` CDC console.
//!
//! **The parser itself ([`EqApoSession`]/[`ParsedDocument`]/[`ParsedFilter`])
//! is deliberately conversion-free** -- it turns text into plain `f32`
//! values (a preamp in dB, and a list of `(kind, freq_hz, gain_db, q)`
//! tuples) and nothing else. Bead `pico-link-ryw.12` (the real import
//! feature, which redesigns the `Band`/blob model to carry more bands and
//! finer precision) reuses this parser UNCHANGED and adds its own
//! conversion into that redesigned model; [`EqApoOverride`] below
//! (RBJ biquads, explicit preamp, no persistence) is ryw.11's own
//! conversion for the debug console, not a conversion ryw.12 should
//! build on.
//!
//! [`EqApoSession`] is line-oriented, not whole-document: the CDC console
//! (`firmware/src/debug_remote.c`'s `EQ BEGIN`/`EQ <line>`/`EQ END`/`EQ
//! OFF` commands) is itself line-based, so a session accumulates state
//! across `feed_line` calls the same way the console delivers them, one
//! line per `pl_ui_debug_eq_command` FFI call.
//!
//! Malformed input is always rejected, never guessed at (the bead's
//! explicit requirement), and every parse error carries the 1-based line
//! number within the session that produced it -- see [`EqApoLineError`].

use alloc::string::String;
use alloc::vec::Vec;

use super::coeffs::{rbj_high_shelf, rbj_low_shelf, rbj_peaking, Biquad, Program};
use super::preset::{BandKind, MAX_BANDS};

/// What can go wrong parsing one line, or finishing a session. Every
/// variant is a hard rejection -- nothing here falls back to a guessed
/// value (contrast [`super::preset::BandKind::from_wire`]'s per-field
/// fallback discipline, deliberately NOT used here: a corrupt wire byte
/// degrading gracefully is fine, a mistyped debug command silently doing
/// the wrong thing is not).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EqApoError {
    /// A line didn't match either the `Preamp:` or `Filter N:` grammar at
    /// all, or a `Filter N:` line had extra/missing tokens.
    MalformedLine,
    /// A numeric field (Fc, Gain, BW, Q, the preamp) didn't parse as a
    /// finite `f32`.
    InvalidNumber,
    /// A `Filter N:` line's shape token wasn't `LS`, `PK` or `HS`.
    UnknownKind,
    /// The session already holds [`super::preset::MAX_BANDS`] filters --
    /// also `PL_DSP_MAX_BIQUADS`, the FFI program's fixed array capacity
    /// (`ui-ffi`'s `PlDspProgram::biquad`), so this is a hard cap, not a
    /// policy choice.
    TooManyBands,
    /// [`EqApoSession::finish`] was called with no `Preamp:` line seen.
    MissingPreamp,
    /// [`EqApoSession::finish`] was called with zero (enabled) filters --
    /// an all-`OFF` or empty document is rejected rather than silently
    /// producing a flat/no-op result.
    NoBands,
    /// A second `Preamp:` line arrived in the same session.
    DuplicatePreamp,
}

/// One [`EqApoError`] plus the 1-based line number (within the session,
/// counting every `feed_line` call including blank ones) that produced
/// it -- so a host-side importer (or the `EQ` debug console) can report
/// exactly which pasted line was rejected. [`EqApoSession::finish`]'s own
/// errors ([`EqApoError::MissingPreamp`]/[`EqApoError::NoBands`]) are
/// session-wide, not one line's fault, so `finish` returns a bare
/// [`EqApoError`], not this wrapped form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EqApoLineError {
    pub line: usize,
    pub error: EqApoError,
}

/// One parsed `Filter N:` line's parameters -- plain `f32` throughout,
/// unlike [`super::preset::Band`]'s quantised `gain_half_db i8`/`q_idx
/// u8` on-wire encoding, because the parser's job is to carry the pasted
/// preset EXACTLY; quantising (or not) into some destination model is
/// entirely up to the caller (ryw.11's [`EqApoOverride`] here, or
/// ryw.12's own redesigned model elsewhere).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParsedFilter {
    pub kind: BandKind,
    pub freq_hz: f32,
    pub gain_db: f32,
    pub q: f32,
}

/// The pure parse result: an explicit preamp in dB and the enabled
/// filters, in document order. No conversion, no FFI shape, no `Program`
/// -- see this module's doc comment for why that's deliberate.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedDocument {
    /// An optional `Name: <text>` line's body, trimmed -- `pico-link-ryw.11`
    /// (this bead)'s debug override ignores it entirely (there's no status
    /// line slot for it); `pico-link-ryw.12`'s real import feature names
    /// the imported preset from it, falling back to some default if `None`.
    pub name: Option<String>,
    pub preamp_db: f32,
    pub bands: Vec<ParsedFilter>,
}

/// Converts an Equalizer APO `BW Oct N` bandwidth to the RBJ cookbook's
/// `Q`: `Q = sqrt(2^N) / (2^N - 1)` (bead description; this is the
/// standard octave-bandwidth-to-Q identity, independent of gain).
#[must_use]
pub fn bw_oct_to_q(bw_oct: f32) -> f32 {
    let p = libm::powf(2.0, bw_oct);
    libm::sqrtf(p) / (p - 1.0)
}

fn parse_f32(tok: &str) -> Result<f32, EqApoError> {
    let v: f32 = tok.parse().map_err(|_| EqApoError::InvalidNumber)?;
    if v.is_finite() {
        Ok(v)
    } else {
        Err(EqApoError::InvalidNumber)
    }
}

fn next_tok<'a>(toks: &[&'a str], i: &mut usize) -> Result<&'a str, EqApoError> {
    let t = *toks.get(*i).ok_or(EqApoError::MalformedLine)?;
    *i += 1;
    Ok(t)
}

fn expect_tok(toks: &[&str], i: &mut usize, want: &str) -> Result<(), EqApoError> {
    if next_tok(toks, i)? == want {
        Ok(())
    } else {
        Err(EqApoError::MalformedLine)
    }
}

/// Parses a `Preamp: <n> dB` line's body (the text after `Preamp:`, e.g.
/// `" -4.41 dB"`).
fn parse_preamp(rest: &str) -> Result<f32, EqApoError> {
    let toks: Vec<&str> = rest.split_whitespace().collect();
    if toks.len() != 2 || toks[1] != "dB" {
        return Err(EqApoError::MalformedLine);
    }
    parse_f32(toks[0])
}

/// Parses a `Filter N: ON|OFF LS|PK|HS Fc <n> Hz Gain <n> dB (BW Oct
/// <n>|Q <n>)` line's body (the text after `Filter`, e.g. `" 1:  ON  LS
/// Fc 40 Hz  Gain -1.76 dB  BW Oct 1.917"`). Returns `Ok(None)` for a
/// well-formed `OFF` filter (skipped, per Equalizer APO convention --
/// not an error, just not emitted as a band) and `Ok(Some(_))` for a
/// well-formed `ON` one.
fn parse_filter(rest: &str) -> Result<Option<ParsedFilter>, EqApoError> {
    let toks: Vec<&str> = rest.split_whitespace().collect();
    let mut i = 0;

    let index_tok = next_tok(&toks, &mut i)?;
    if !index_tok.ends_with(':') {
        return Err(EqApoError::MalformedLine);
    }

    match next_tok(&toks, &mut i)? {
        "OFF" => return Ok(None),
        "ON" => {}
        _ => return Err(EqApoError::MalformedLine),
    }

    let kind = match next_tok(&toks, &mut i)? {
        "LS" => BandKind::LowShelf,
        "PK" => BandKind::Peak,
        "HS" => BandKind::HighShelf,
        _ => return Err(EqApoError::UnknownKind),
    };

    expect_tok(&toks, &mut i, "Fc")?;
    let freq_hz = parse_f32(next_tok(&toks, &mut i)?)?;
    expect_tok(&toks, &mut i, "Hz")?;
    expect_tok(&toks, &mut i, "Gain")?;
    let gain_db = parse_f32(next_tok(&toks, &mut i)?)?;
    expect_tok(&toks, &mut i, "dB")?;

    let q = match next_tok(&toks, &mut i)? {
        "BW" => {
            expect_tok(&toks, &mut i, "Oct")?;
            bw_oct_to_q(parse_f32(next_tok(&toks, &mut i)?)?)
        }
        "Q" => parse_f32(next_tok(&toks, &mut i)?)?,
        _ => return Err(EqApoError::MalformedLine),
    };

    // Trailing garbage past the last recognized token is rejected, not
    // ignored -- "reject rather than guess" (bead description).
    if i != toks.len() {
        return Err(EqApoError::MalformedLine);
    }
    if freq_hz <= 0.0 || q <= 0.0 {
        return Err(EqApoError::InvalidNumber);
    }

    Ok(Some(ParsedFilter { kind, freq_hz, gain_db, q }))
}

/// Accumulates one `EQ BEGIN` .. `EQ END` console session's lines, or
/// (for `pico-link-ryw.12`) one whole pasted/uploaded document fed line
/// by line. Pure parsing only -- see this module's doc comment.
#[derive(Debug, Default)]
pub struct EqApoSession {
    line_no: usize,
    name: Option<String>,
    preamp_db: Option<f32>,
    bands: Vec<ParsedFilter>,
}

impl EqApoSession {
    #[must_use]
    pub fn new() -> Self {
        Self { line_no: 0, name: None, preamp_db: None, bands: Vec::new() }
    }

    /// Feeds one already newline-stripped line (leading/trailing
    /// whitespace tolerated, blank lines ignored -- but still counted,
    /// so [`EqApoLineError::line`] always matches the caller's own
    /// line-number count of everything it fed in, blanks included).
    /// Recognizes a `Preamp:` line (at most once per session) and a
    /// `Filter N:` line (at most [`super::preset::MAX_BANDS`] enabled
    /// ones); anything else is [`EqApoError::MalformedLine`].
    pub fn feed_line(&mut self, line: &str) -> Result<(), EqApoLineError> {
        self.line_no += 1;
        self.feed_line_inner(line).map_err(|error| EqApoLineError { line: self.line_no, error })
    }

    fn feed_line_inner(&mut self, line: &str) -> Result<(), EqApoError> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(());
        }
        if let Some(rest) = line.strip_prefix("Name:") {
            // Last one wins on a duplicate -- unlike a duplicate `Preamp:`
            // line, a duplicate `Name:` isn't a sign of a corrupt paste
            // worth hard-rejecting, and it has no downstream effect for
            // this bead's own debug override (which ignores it either way).
            self.name = Some(String::from(rest.trim()));
            return Ok(());
        }
        if let Some(rest) = line.strip_prefix("Preamp:") {
            if self.preamp_db.is_some() {
                return Err(EqApoError::DuplicatePreamp);
            }
            self.preamp_db = Some(parse_preamp(rest)?);
            return Ok(());
        }
        if let Some(rest) = line.strip_prefix("Filter") {
            if let Some(band) = parse_filter(rest)? {
                if self.bands.len() >= MAX_BANDS {
                    return Err(EqApoError::TooManyBands);
                }
                self.bands.push(band);
            }
            return Ok(());
        }
        Err(EqApoError::MalformedLine)
    }

    /// Consumes the session into a [`ParsedDocument`]. Requires a
    /// `Preamp:` line to have been seen ([`EqApoError::MissingPreamp`])
    /// and at least one enabled `Filter` line ([`EqApoError::NoBands`]).
    /// Session-wide errors, not tied to one line -- returns a bare
    /// [`EqApoError`], not [`EqApoLineError`].
    pub fn finish(self) -> Result<ParsedDocument, EqApoError> {
        let preamp_db = self.preamp_db.ok_or(EqApoError::MissingPreamp)?;
        if self.bands.is_empty() {
            return Err(EqApoError::NoBands);
        }
        Ok(ParsedDocument { name: self.name, preamp_db, bands: self.bands })
    }
}

fn parsed_filter_to_biquad(band: ParsedFilter, fs_hz: u32) -> Biquad {
    #[allow(clippy::cast_precision_loss)] // fs_hz is always a real audio rate; see coeffs::fs_hz_to_f32's identical allow.
    let fs = fs_hz as f32;
    let nyquist = fs / 2.0;
    let freq = band.freq_hz.clamp(10.0, nyquist - 1.0);
    match band.kind {
        BandKind::Peak => rbj_peaking(freq, band.gain_db, band.q, fs),
        BandKind::LowShelf => rbj_low_shelf(freq, band.gain_db, band.q, fs),
        BandKind::HighShelf => rbj_high_shelf(freq, band.gain_db, band.q, fs),
    }
}

/// **Bead `pico-link-ryw.11` only** -- NOT part of the reusable parser
/// above. Compiles a [`ParsedDocument`] to exact RBJ biquads with the
/// EXPLICIT preamp (not [`super::coeffs::auto_preamp_db`]'s computed
/// one), for the non-persisted debug DSP override
/// (`crate::app::App::dsp_program` returns this ahead of everything
/// else while active). `pico-link-ryw.12`'s real import feature converts
/// a [`ParsedDocument`] its own way, through the redesigned
/// `Band`/`Preset` model, not through this type.
#[derive(Debug, Clone, PartialEq)]
pub struct EqApoOverride {
    doc: ParsedDocument,
}

impl EqApoOverride {
    #[must_use]
    pub fn from_parsed(doc: ParsedDocument) -> Self {
        Self { doc }
    }

    /// Compiles this override to a [`Program`] at `fs_hz` -- recomputed
    /// on every call (not cached), mirroring
    /// `crate::app::App::dsp_program`'s existing editor-preview override,
    /// which recompiles its draft [`super::preset::Preset`] on every call
    /// too.
    #[must_use]
    pub fn to_program(&self, fs_hz: u32) -> Program {
        let biquads = self.doc.bands.iter().map(|b| parsed_filter_to_biquad(*b, fs_hz)).collect();
        Program { fs_hz, preamp_linear: libm::powf(10.0, self.doc.preamp_db / 20.0), biquads, crossfeed: None }
    }

    /// How many bands this override carries -- for a debug status line
    /// only (`pl_ui_debug_eq_status`), not used by [`Self::to_program`]
    /// itself (`self.doc.bands.len()` already drives that).
    #[must_use]
    pub fn band_count(&self) -> u8 {
        // `self.doc.bands.len() <= MAX_BANDS == 10` (enforced by
        // `EqApoSession::feed_line`'s `TooManyBands` check), so this
        // never truncates.
        #[allow(clippy::cast_possible_truncation)]
        let n = self.doc.bands.len() as u8;
        n
    }

    /// The explicit preamp this override was built with, in dB -- for a
    /// debug status line only, same as [`Self::band_count`].
    #[must_use]
    pub fn preamp_db(&self) -> f32 {
        self.doc.preamp_db
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn feed_all(lines: &[&str]) -> Result<ParsedDocument, EqApoError> {
        let mut session = EqApoSession::new();
        for line in lines {
            session.feed_line(line).map_err(|e| e.error)?;
        }
        session.finish()
    }

    #[test]
    fn parses_the_xm3_preset_exactly() {
        let doc = feed_all(XM3_PRESET).expect("preset parses");
        assert_eq!(doc.bands.len(), 10);
        assert!((doc.preamp_db - (-4.41)).abs() < 1e-6);
        assert_eq!(doc.bands[0].kind, BandKind::LowShelf);
        assert!((doc.bands[0].freq_hz - 40.0).abs() < 1e-6);
        assert!((doc.bands[0].gain_db - (-1.76)).abs() < 1e-6);
        assert_eq!(doc.bands[9].kind, BandKind::HighShelf);
        assert!((doc.bands[9].freq_hz - 10_000.0).abs() < 1e-6);
    }

    #[test]
    fn produces_a_program_with_ten_biquads_and_the_explicit_preamp() {
        let doc = feed_all(XM3_PRESET).expect("preset parses");
        let over = EqApoOverride::from_parsed(doc);
        let program = over.to_program(48_000);
        assert_eq!(program.biquads.len(), 10);
        assert_eq!(program.fs_hz, 48_000);
        // preamp_linear = 10^(-4.41/20), NOT auto_preamp_db's computed value.
        let expected_linear = libm::powf(10.0, -4.41 / 20.0);
        assert!((program.preamp_linear - expected_linear).abs() < 1e-6);
    }

    #[test]
    fn bw_to_q_matches_an_independently_computed_value() {
        // Filter 1: BW Oct 1.917 -> independently computed via
        // Q = sqrt(2^N)/(2^N - 1) with N = 1.917:
        // 2^1.917 = 3.7794..., sqrt = 1.94407..., Q = 1.94407/2.7794 = 0.69946...
        let q = bw_oct_to_q(1.917);
        assert!((q - 0.699_46).abs() < 1e-3, "q={q}");

        // A textbook sanity point: N = 1 octave -> Q = sqrt(2)/1 = sqrt(2).
        let q_one_octave = bw_oct_to_q(1.0);
        assert!((q_one_octave - core::f32::consts::SQRT_2).abs() < 1e-5);
    }

    #[test]
    fn a_second_preamp_line_is_rejected_with_its_own_line_number() {
        let mut session = EqApoSession::new();
        session.feed_line("Preamp: -1 dB").unwrap();
        let err = session.feed_line("Preamp: -2 dB").unwrap_err();
        assert_eq!(err, EqApoLineError { line: 2, error: EqApoError::DuplicatePreamp });
    }

    #[test]
    fn finish_without_a_preamp_line_is_rejected() {
        let mut session = EqApoSession::new();
        session.feed_line("Filter 1: ON PK Fc 100 Hz Gain 1 dB Q 1").unwrap();
        assert_eq!(session.finish(), Err(EqApoError::MissingPreamp));
    }

    #[test]
    fn finish_with_zero_bands_is_rejected() {
        let mut session = EqApoSession::new();
        session.feed_line("Preamp: 0 dB").unwrap();
        assert_eq!(session.finish(), Err(EqApoError::NoBands));
    }

    #[test]
    fn an_off_filter_is_skipped_not_an_error_and_not_a_band() {
        let mut session = EqApoSession::new();
        session.feed_line("Preamp: 0 dB").unwrap();
        session.feed_line("Filter 1: OFF PK Fc 100 Hz Gain 1 dB Q 1").unwrap();
        session.feed_line("Filter 2: ON PK Fc 200 Hz Gain 2 dB Q 1").unwrap();
        let doc = session.finish().unwrap();
        assert_eq!(doc.bands.len(), 1);
    }

    #[test]
    fn the_q_form_is_accepted_alongside_bw_oct() {
        let mut session = EqApoSession::new();
        session.feed_line("Preamp: 0 dB").unwrap();
        session.feed_line("Filter 1: ON PK Fc 1000 Hz Gain 3 dB Q 2.5").unwrap();
        let doc = session.finish().unwrap();
        assert!((doc.bands[0].q - 2.5).abs() < 1e-6);
    }

    #[test]
    fn rejects_malformed_lines_rather_than_guessing_and_reports_the_line_number() {
        let mut session = EqApoSession::new();
        // Line 1: missing "dB" unit on the preamp.
        let err = session.feed_line("Preamp: -4.41").unwrap_err();
        assert_eq!(err, EqApoLineError { line: 1, error: EqApoError::MalformedLine });

        session.feed_line("Preamp: -4.41 dB").unwrap(); // line 2, recover so later lines can be tested

        // Line 3: bad kind token.
        let err = session.feed_line("Filter 1: ON XX Fc 100 Hz Gain 1 dB Q 1").unwrap_err();
        assert_eq!(err, EqApoLineError { line: 3, error: EqApoError::UnknownKind });

        // Line 4: missing "Hz" unit.
        let err = session.feed_line("Filter 1: ON PK Fc 100 Gain 1 dB Q 1").unwrap_err();
        assert_eq!(err, EqApoLineError { line: 4, error: EqApoError::MalformedLine });

        // Line 5: non-numeric gain.
        let err = session.feed_line("Filter 1: ON PK Fc 100 Hz Gain oops dB Q 1").unwrap_err();
        assert_eq!(err, EqApoLineError { line: 5, error: EqApoError::InvalidNumber });

        // Line 6: trailing garbage.
        let err = session.feed_line("Filter 1: ON PK Fc 100 Hz Gain 1 dB Q 1 EXTRA").unwrap_err();
        assert_eq!(err, EqApoLineError { line: 6, error: EqApoError::MalformedLine });

        // Line 7: a totally unrelated line.
        let err = session.feed_line("Hello there").unwrap_err();
        assert_eq!(err, EqApoLineError { line: 7, error: EqApoError::MalformedLine });
    }

    #[test]
    fn an_optional_name_line_is_parsed_and_the_last_one_wins() {
        let mut session = EqApoSession::new();
        session.feed_line("Name: Sony WH-1000XM3").unwrap();
        session.feed_line("Preamp: 0 dB").unwrap();
        session.feed_line("Filter 1: ON PK Fc 100 Hz Gain 1 dB Q 1").unwrap();
        session.feed_line("Name: Overridden Name").unwrap();
        let doc = session.finish().unwrap();
        assert_eq!(doc.name.as_deref(), Some("Overridden Name"));
    }

    #[test]
    fn a_document_with_no_name_line_parses_with_name_none() {
        let doc = feed_all(XM3_PRESET).expect("preset parses");
        assert_eq!(doc.name, None);
    }

    #[test]
    fn too_many_bands_is_rejected() {
        let mut session = EqApoSession::new();
        session.feed_line("Preamp: 0 dB").unwrap();
        for i in 0..MAX_BANDS {
            let line = alloc::format!("Filter {}: ON PK Fc 100 Hz Gain 1 dB Q 1", i + 1);
            session.feed_line(&line).unwrap();
        }
        let line = alloc::format!("Filter {}: ON PK Fc 100 Hz Gain 1 dB Q 1", MAX_BANDS + 1);
        assert_eq!(session.feed_line(&line).unwrap_err().error, EqApoError::TooManyBands);
    }
}
