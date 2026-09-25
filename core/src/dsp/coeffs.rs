//! Params -> coefficients: RBJ cookbook biquads, a bs2b-style crossfeed,
//! and the automatic preamp (design sec 3.1: "`core` ... owns ...
//! params -> coefficients: RBJ cookbook biquads, bs2b crossfeed, auto
//! preamp").
//!
//! Nothing here is FFI shape -- see [`super`]'s module doc. [`Program`] is
//! the Rust-side bundle a future `pico-link-ryw.5` copies field-for-field
//! into the C `PlDspProgram` struct (design sec 3.2).
//!
//! # References
//! - RBJ biquads: Robert Bristow-Johnson's "Audio EQ Cookbook" (the
//!   `Q`-based peaking/shelving formulas -- the cookbook offers both an
//!   `S`(shelf slope)-based and a `Q`-based derivation for the two shelf
//!   filters; this module uses the `Q`-based one throughout so a single
//!   [`preset::Q_TABLE`](super::preset::Q_TABLE) lookup serves all three
//!   band kinds).
//! - Crossfeed: the bs2b ("Bauer stereophonic-to-binaural") algorithm --
//!   a first-order lowpass mixed cross-channel plus a first-order high
//!   shelf kept on the direct channel, normalised so the pair is
//!   loudness-neutral. Coefficient derivation independently confirmed
//!   against a from-scratch reimplementation of the reference `bs2b.c`
//!   library (not derived from this module).

use core::f32::consts::PI;

use alloc::vec::Vec;

use super::preset::{Band, BandKind, Preset};

/// Converts a sample rate to [`f32`] for use in the trig/log formulas below.
/// `clippy::cast_precision_loss` is silenced deliberately: every `fs_hz`
/// this crate ever sees is a real audio sample rate (`48_000` today, at
/// most a few hundred kHz), many orders of magnitude below where `u32`
/// -> `f32` could actually lose an integer bit (`f32`'s mantissa is exact
/// up to `2^24`, about 16.7M) -- not a resource-constrained-target shortcut.
#[allow(clippy::cast_precision_loss)]
fn fs_hz_to_f32(fs_hz: u32) -> f32 {
    fs_hz as f32
}

/// `PL_DSP_MAX_BIQUADS` (design sec 3.2) -- also [`super::preset::MAX_BANDS`],
/// re-exported here under the FFI-facing name for readers coming from the
/// design doc.
pub const MAX_BIQUADS: usize = super::preset::MAX_BANDS;

/// One a0-normalised biquad in Transposed Direct Form II, matching the
/// design's `PlBiquad` field set exactly (sec 3.2: "`PlBiquad {f32
/// b0,b1,b2,a1,a2}`, a0-normalised") so a future FFI layer can copy this
/// struct's fields straight across.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Biquad {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
}

impl Biquad {
    /// The identity filter (bit-exact passthrough) -- never used as an
    /// active band today (an empty [`Preset::bands`] just yields zero
    /// biquads), kept for tests and as the crossfade fixture's "old
    /// program" endpoint.
    pub const IDENTITY: Self = Self { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

    /// The filter's magnitude response `|H(e^{jw})|` at `freq_hz`,
    /// linear (not dB), evaluated directly from the coefficients -- the
    /// textbook definition, used by both [`auto_preamp_db`] and the unit
    /// tests below to check a computed filter's shape independently of
    /// how [`rbj_peaking`]/[`rbj_low_shelf`]/[`rbj_high_shelf`] arrived at
    /// the coefficients.
    #[must_use]
    // `sin_w`/`cos_w` vs `sin_2w`/`cos_2w`: deliberately named after the
    // H(e^{jw}) terms they represent (w and 2w), not a naming accident.
    #[allow(clippy::similar_names)]
    pub fn magnitude_at(self, freq_hz: f32, fs_hz: f32) -> f32 {
        let w = 2.0 * PI * freq_hz / fs_hz;
        let (sin_w, cos_w) = (libm::sinf(w), libm::cosf(w));
        let (sin_2w, cos_2w) = (libm::sinf(2.0 * w), libm::cosf(2.0 * w));

        // H(e^{jw}) = (b0 + b1*e^{-jw} + b2*e^{-2jw}) / (1 + a1*e^{-jw} + a2*e^{-2jw})
        let num_re = self.b0 + self.b1 * cos_w + self.b2 * cos_2w;
        let num_im = -(self.b1 * sin_w + self.b2 * sin_2w);
        let den_re = 1.0 + self.a1 * cos_w + self.a2 * cos_2w;
        let den_im = -(self.a1 * sin_w + self.a2 * sin_2w);

        let num_mag = libm::sqrtf(num_re * num_re + num_im * num_im);
        let den_mag = libm::sqrtf(den_re * den_re + den_im * den_im);
        num_mag / den_mag
    }
}

/// RBJ cookbook peaking (bell) EQ. `gain_db` boosts (positive) or cuts
/// (negative) a band centred on `freq_hz` with quality `q`; magnitude
/// response is unity far from `freq_hz` in both directions.
#[must_use]
pub fn rbj_peaking(freq_hz: f32, gain_db: f32, q: f32, fs_hz: f32) -> Biquad {
    let a = libm::powf(10.0, gain_db / 40.0);
    let w0 = 2.0 * PI * freq_hz / fs_hz;
    let (sin_w0, cos_w0) = (libm::sinf(w0), libm::cosf(w0));
    let alpha = sin_w0 / (2.0 * q);

    let a0 = 1.0 + alpha / a;
    let b0 = (1.0 + alpha * a) / a0;
    let b1 = (-2.0 * cos_w0) / a0;
    let b2 = (1.0 - alpha * a) / a0;
    let a1 = (-2.0 * cos_w0) / a0;
    let a2 = (1.0 - alpha / a) / a0;

    Biquad { b0, b1, b2, a1, a2 }
}

/// RBJ cookbook low shelf (`Q`-based form): unity below `freq_hz`, settles
/// to `gain_db` above it.
#[must_use]
pub fn rbj_low_shelf(freq_hz: f32, gain_db: f32, q: f32, fs_hz: f32) -> Biquad {
    let a = libm::powf(10.0, gain_db / 40.0);
    let w0 = 2.0 * PI * freq_hz / fs_hz;
    let (sin_w0, cos_w0) = (libm::sinf(w0), libm::cosf(w0));
    let alpha = sin_w0 / (2.0 * q);
    let sqrt_a = libm::sqrtf(a);
    let two_sqrt_a_alpha = 2.0 * sqrt_a * alpha;

    let a0 = (a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha;
    let b0 = (a * ((a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha)) / a0;
    let b1 = (2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0)) / a0;
    let b2 = (a * ((a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha)) / a0;
    let a1 = (-2.0 * ((a - 1.0) + (a + 1.0) * cos_w0)) / a0;
    let a2 = ((a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha) / a0;

    Biquad { b0, b1, b2, a1, a2 }
}

/// RBJ cookbook high shelf (`Q`-based form): unity below `freq_hz`,
/// settles to `gain_db` above it.
#[must_use]
pub fn rbj_high_shelf(freq_hz: f32, gain_db: f32, q: f32, fs_hz: f32) -> Biquad {
    let a = libm::powf(10.0, gain_db / 40.0);
    let w0 = 2.0 * PI * freq_hz / fs_hz;
    let (sin_w0, cos_w0) = (libm::sinf(w0), libm::cosf(w0));
    let alpha = sin_w0 / (2.0 * q);
    let sqrt_a = libm::sqrtf(a);
    let two_sqrt_a_alpha = 2.0 * sqrt_a * alpha;

    let a0 = (a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha;
    let b0 = (a * ((a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha)) / a0;
    let b1 = (-2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0)) / a0;
    let b2 = (a * ((a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha)) / a0;
    let a1 = (2.0 * ((a - 1.0) - (a + 1.0) * cos_w0)) / a0;
    let a2 = ((a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha) / a0;

    Biquad { b0, b1, b2, a1, a2 }
}

/// A [`Band`] compiled to a [`Biquad`] at a concrete sample rate. `freq_hz`
/// is clamped to `[10, fs_hz/2 - 1]` first -- a stored band's frequency is
/// fs-independent (it's just a `u16`), so an implausible or
/// above-Nyquist value (a corrupt blob, or a future higher `fs_hz`
/// shrinking the valid range) is pulled back into range rather than
/// producing a `NaN`/unstable filter.
#[must_use]
pub fn band_to_biquad(band: Band, fs_hz: u32) -> Biquad {
    let nyquist = fs_hz_to_f32(fs_hz) / 2.0;
    let freq = f32::from(band.freq_hz).clamp(10.0, nyquist - 1.0);
    let gain_db = band.gain_db();
    let q = band.q();
    let fs = fs_hz_to_f32(fs_hz);

    match band.kind {
        BandKind::Peak => rbj_peaking(freq, gain_db, q, fs),
        BandKind::LowShelf => rbj_low_shelf(freq, gain_db, q, fs),
        BandKind::HighShelf => rbj_high_shelf(freq, gain_db, q, fs),
    }
}

/// A bs2b-style crossfeed's compiled coefficients: a single-pole lowpass
/// mixed onto the OTHER channel, a single-pole high shelf kept on the
/// SAME channel, and an overall gain normalising the pair so a mono
/// signal (equal L/R) is not louder or quieter than with crossfeed off.
///
/// Field names follow the reference algorithm's own variable names
/// (`lo`/`hi`/`gain`), not the design doc's placeholder FFI names (design
/// sec 3.2's "lowpass b0/a1, cross gain, high-shelf b0/b1/a1, norm") --
/// mapping this struct onto the eventual `PlDspProgram` fields is
/// `pico-link-ryw.5`'s job (this bead is explicitly FFI-free, see
/// [`super`]'s module doc), and getting that mapping right needs the C
/// kernel's actual per-sample loop (`pico-link-ryw.1`, built in parallel)
/// in view -- which this bead doesn't have.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CrossfeedCoeffs {
    /// Lowpass feedforward coefficient (`a0_lo` in the reference).
    pub lo_b0: f32,
    /// Lowpass one-pole feedback coefficient (`b1_lo`).
    pub lo_a1: f32,
    /// High shelf feedforward coefficient on the current input (`a0_hi`).
    pub hi_b0: f32,
    /// High shelf feedforward coefficient on the PREVIOUS raw input
    /// sample (`a1_hi`) -- the per-sample kernel needs a one-sample
    /// "as-is" delay line per channel to apply this term.
    pub hi_b1: f32,
    /// High shelf one-pole feedback coefficient (`b1_hi`).
    pub hi_a1: f32,
    /// Overall linear gain applied after mixing, so the pair stays
    /// loudness-neutral.
    pub norm_gain: f32,
}

/// Per-sample reference application of [`CrossfeedCoeffs`], used only by
/// this module's own tests to check the coefficients' behaviour (DC gain,
/// high-frequency separation) against the textbook bs2b structure -- NOT
/// part of the production kernel, which is C (`pico-link-ryw.1`).
#[derive(Debug, Clone, Copy, Default)]
pub struct CrossfeedState {
    lo: [f32; 2],
    hi: [f32; 2],
    asis: [f32; 2],
}

impl CrossfeedCoeffs {
    /// Runs one stereo sample through the reference structure: `lo[ch] =
    /// lo_b0*in[ch] + lo_a1*state.lo[ch]`, `hi[ch] = hi_b0*in[ch] +
    /// hi_b1*state.asis[ch] + hi_a1*state.hi[ch]`, `out_left = (hi[0] +
    /// lo[1]) * norm_gain`, `out_right = (hi[1] + lo[0]) * norm_gain`.
    #[must_use]
    pub fn process(self, state: &mut CrossfeedState, input: [f32; 2]) -> [f32; 2] {
        let mut lo = [0.0f32; 2];
        let mut hi = [0.0f32; 2];
        for ch in 0..2 {
            lo[ch] = self.lo_b0 * input[ch] + self.lo_a1 * state.lo[ch];
            hi[ch] = self.hi_b0 * input[ch] + self.hi_b1 * state.asis[ch] + self.hi_a1 * state.hi[ch];
        }
        state.lo = lo;
        state.hi = hi;
        state.asis = input;

        [(hi[0] + lo[1]) * self.norm_gain, (hi[1] + lo[0]) * self.norm_gain]
    }
}

/// Fixed cutoff/feed parameters for [`CrossfeedLevel`](super::preset::CrossfeedLevel)'s
/// three non-`Off` strengths, taken from bs2b's own named presets (fcut
/// Hz, feed in tenths of a dB) as a reasonable starting point -- the
/// actual taste pass is `pico-link-ryw.8` ("BY-EAR ACCEPTANCE AND
/// CROSSFEED TUNING", design sec 4), not this bead.
const CROSSFEED_PRESETS: [(f32, f32); 3] = [
    (700.0, 45.0), // Weak   -- bs2b "Default"
    (700.0, 60.0), // Medium -- bs2b "Chu Moy circuit"
    (650.0, 95.0), // Strong -- bs2b "Jan Meier circuit"
];

/// Computes [`CrossfeedCoeffs`] for a crossfeed strength at `fs_hz`, or
/// `None` for [`CrossfeedLevel::Off`](super::preset::CrossfeedLevel::Off)
/// (nothing to compute -- the engine bypasses crossfeed entirely on
/// `xfeed_on = false`, design sec 3.2).
///
/// # Derivation (bs2b algorithm)
/// Given a cutoff `fcut` and a feed level `level_db`:
/// ```text
/// gb_lo = level_db * (-5/6) - 3
/// gb_hi = level_db / 6 - 3
/// g_lo  = 10^(gb_lo / 20)
/// g_hi  = 1 - 10^(gb_hi / 20)
/// fc_hi = fcut * 2^((gb_lo - 20*log10(g_hi)) / 12)
/// x_lo  = exp(-2*pi*fcut / fs)
/// x_hi  = exp(-2*pi*fc_hi / fs)
/// lo_b0 = g_lo * (1 - x_lo),  lo_a1 = x_lo
/// hi_b0 = 1 - g_hi*(1 - x_hi), hi_b1 = -x_hi, hi_a1 = x_hi
/// norm_gain = 1 / (1 - g_hi + g_lo)
/// ```
#[must_use]
// `gb_lo`/`g_lo` and `gb_hi`/`g_hi` are the reference algorithm's own
// variable names (a "gain-in-dB" and its linear form), kept close to the
// derivation on purpose so this function is checkable line-by-line
// against the formula in the doc comment above.
#[allow(clippy::similar_names)]
pub fn crossfeed_coeffs(level: super::preset::CrossfeedLevel, fs_hz: u32) -> Option<CrossfeedCoeffs> {
    use super::preset::CrossfeedLevel;

    let idx = match level {
        CrossfeedLevel::Off => return None,
        CrossfeedLevel::Weak => 0,
        CrossfeedLevel::Medium => 1,
        CrossfeedLevel::Strong => 2,
    };
    let (fcut, feed_tenths_db) = CROSSFEED_PRESETS[idx];
    let fs = fs_hz_to_f32(fs_hz);
    let level_db = feed_tenths_db / 10.0;

    let gb_lo = level_db * (-5.0 / 6.0) - 3.0;
    let gb_hi = level_db / 6.0 - 3.0;
    let g_lo = libm::powf(10.0, gb_lo / 20.0);
    let g_hi = 1.0 - libm::powf(10.0, gb_hi / 20.0);
    let fc_hi = fcut * libm::powf(2.0, (gb_lo - 20.0 * libm::log10f(g_hi)) / 12.0);

    let x_lo = libm::expf(-2.0 * PI * fcut / fs);
    let x_hi = libm::expf(-2.0 * PI * fc_hi / fs);

    Some(CrossfeedCoeffs {
        lo_b0: g_lo * (1.0 - x_lo),
        lo_a1: x_lo,
        hi_b0: 1.0 - g_hi * (1.0 - x_hi),
        hi_b1: -x_hi,
        hi_a1: x_hi,
        norm_gain: 1.0 / (1.0 - g_hi + g_lo),
    })
}

/// The automatic preamp's clamp range (design sec 1.2: "clamped to [-12,
/// 0] dB").
pub const MIN_BOOST_HEADROOM_DB: f32 = -12.0;
pub const MAX_BOOST_HEADROOM_DB: f32 = 0.0;

/// How many log-spaced points the peak search samples across 20Hz..Nyquist.
/// Coarse but sufficient: peaking/shelf filters are smooth, wide-Q
/// features, not narrow spikes that a sparse grid could straddle and miss.
const RESPONSE_GRID_POINTS: usize = 128;

/// The automatic preamp (design sec 1.2): "minus the peak of the combined
/// magnitude response on a log grid, clamped to `[-12, 0]` dB". A
/// preset with only cuts (or no bands) never needs headroom, so its
/// preamp is `0dB`, not a wasted fixed `-6dB` (the "why not a fixed
/// headroom" point the design makes explicitly).
///
/// Returns the preamp in dB (negative or zero); [`Program::from_preset`]
/// converts it to the linear gain the FFI program field wants.
#[must_use]
// The grid index cast below (`i as f32`, `i < RESPONSE_GRID_POINTS ==
// 128`) is always exact -- nowhere near `f32`'s 2^24 exact-integer bound.
#[allow(clippy::cast_precision_loss)]
pub fn auto_preamp_db(biquads: &[Biquad], fs_hz: u32) -> f32 {
    if biquads.is_empty() {
        return 0.0;
    }
    let fs = fs_hz_to_f32(fs_hz);
    let nyquist = fs / 2.0;
    let log_min = libm::log10f(20.0);
    let log_max = libm::log10f(nyquist - 1.0);

    let mut peak_db = 0.0f32;
    for i in 0..RESPONSE_GRID_POINTS {
        let t = i as f32 / (RESPONSE_GRID_POINTS - 1) as f32;
        let freq = libm::powf(10.0, log_min + t * (log_max - log_min));

        let mut mag = 1.0f32;
        for bq in biquads {
            mag *= bq.magnitude_at(freq, fs);
        }
        let db = 20.0 * libm::log10f(mag.max(1.0e-9));
        if db > peak_db {
            peak_db = db;
        }
    }

    (-peak_db).clamp(MIN_BOOST_HEADROOM_DB, MAX_BOOST_HEADROOM_DB)
}

/// A preset compiled at a concrete sample rate -- everything a future FFI
/// layer needs to fill in a `PlDspProgram` (design sec 3.2), minus the
/// FFI shape itself (see [`CrossfeedCoeffs`]'s doc comment for why).
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub fs_hz: u32,
    /// Linear gain (NOT dB) -- `10^(auto_preamp_db/20)`, ready to multiply
    /// samples by directly.
    pub preamp_linear: f32,
    pub biquads: Vec<Biquad>,
    pub crossfeed: Option<CrossfeedCoeffs>,
}

impl Program {
    /// The bypass program: no biquads, no crossfeed, unity preamp. The
    /// design's "structural bypass" (sec 1.2: "an Off program ... returns
    /// before touching a sample") lives in the C kernel, not here, but
    /// this is the value that produces it -- what an unassigned or
    /// dangling-reference device resolves to (design sec 2.3).
    #[must_use]
    pub fn off(fs_hz: u32) -> Self {
        Self { fs_hz, preamp_linear: 1.0, biquads: Vec::new(), crossfeed: None }
    }

    /// Compiles `preset` at `fs_hz`: every [`Band`] to a [`Biquad`]
    /// (RBJ), the crossfeed strength to [`CrossfeedCoeffs`] (bs2b), and
    /// the automatic preamp from the resulting biquad cascade's combined
    /// magnitude response.
    #[must_use]
    pub fn from_preset(preset: &Preset, fs_hz: u32) -> Self {
        let biquads: Vec<Biquad> = preset.bands.iter().map(|band| band_to_biquad(*band, fs_hz)).collect();
        let preamp_db = auto_preamp_db(&biquads, fs_hz);
        let crossfeed = crossfeed_coeffs(preset.crossfeed, fs_hz);

        Self { fs_hz, preamp_linear: libm::powf(10.0, preamp_db / 20.0), biquads, crossfeed }
    }
}
