//! The preset model and its on-wire blob format.
//!
//! A [`Preset`] is a name, an optional crossfeed strength and up to
//! [`MAX_BANDS`] parametric EQ bands. It is entirely data -- turning it
//! into filter coefficients is [`super::coeffs`]'s job, and giving it an
//! id and a durable home is [`super::store::PresetStore`]'s.

use alloc::string::String;
use alloc::vec::Vec;

/// `PL_DSP_MAX_BIQUADS` in the design (sec 3.2) -- the FFI program's fixed
/// biquad-array capacity, and therefore the most bands one preset can hold.
pub const MAX_BANDS: usize = 10;

/// The blob's `name[16]` field width (design sec 2.2).
pub const MAX_NAME_BYTES: usize = 16;

/// Blob v1's total wire length: `1 (version) + 1 (name_len) + 16 (name) +
/// 1 (crossfeed) + 1 (band_count) + 10 * 5 (band records)` = 70 bytes,
/// matching the design's "Blob v1 is about 70B" (sec 2.2). C's on-flash
/// record reserves `blob[80]` (design sec 2.2) for headroom past this.
pub const BLOB_LEN: usize = 1 + 1 + MAX_NAME_BYTES + 1 + 1 + MAX_BANDS * BAND_RECORD_LEN;

/// One band record on the wire: `{type u8, freq_hz u16 LE, gain_half_db i8,
/// q_idx u8}` (design sec 2.2).
const BAND_RECORD_LEN: usize = 5;

/// The only blob format version this build understands. A future format
/// change bumps this and adds a new match arm in [`Preset::from_wire`] --
/// see the doc comment there for the fallback discipline on an unknown
/// version.
const BLOB_VERSION: u8 = 1;

/// A fixed table of musically useful Q values a [`Band`] indexes into
/// (`q_idx`) rather than storing a raw float -- keeps the wire format a
/// single byte per band and keeps every on-device picker enumerable.
/// Spans a wide, log-ish spread from broad (`0.4`) to sharp (`8.0`); index
/// 3 (`1.0`) is the RBJ cookbook's "no resonance" reference Q and is the
/// picker default.
pub const Q_TABLE: [f32; 8] = [0.4, 0.6, 0.71, 1.0, 1.4, 2.0, 3.2, 8.0];

/// Looks up `q_idx` in [`Q_TABLE`], clamping an out-of-range index to the
/// last entry rather than panicking -- the same per-field-fallback
/// discipline [`CrossfeedLevel::from_wire`] and
/// [`crate::audio::CushionPolicy::from_wire`] use: a corrupt or
/// forward-versioned byte degrades gracefully instead of crashing the
/// preset store's boot-time load.
#[must_use]
pub fn q_from_index(q_idx: u8) -> f32 {
    let idx = (q_idx as usize).min(Q_TABLE.len() - 1);
    Q_TABLE[idx]
}

/// Which RBJ cookbook filter shape a [`Band`] is. Wire: `0` = `Peak`, `1` =
/// `LowShelf`, `2` = `HighShelf`. Any other wire value falls back to
/// `Peak` (see [`BandKind::from_wire`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BandKind {
    #[default]
    Peak,
    LowShelf,
    HighShelf,
}

impl BandKind {
    #[must_use]
    pub const fn to_wire(self) -> u8 {
        match self {
            Self::Peak => 0,
            Self::LowShelf => 1,
            Self::HighShelf => 2,
        }
    }

    /// Any wire value other than `{0, 1, 2}` falls back to
    /// [`Self::default`] (`Peak`) -- same per-field-fallback discipline as
    /// [`crate::audio::AbrFloor::from_wire`].
    #[must_use]
    pub const fn from_wire(value: u8) -> Self {
        match value {
            1 => Self::LowShelf,
            2 => Self::HighShelf,
            _ => Self::Peak,
        }
    }
}

/// One parametric EQ band. `gain_half_db` is in 0.5dB steps (`as f32 *
/// 0.5` gives the actual dB) so a single signed byte covers +/-64dB, far
/// past any sane boost/cut. `q_idx` indexes [`Q_TABLE`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    pub kind: BandKind,
    pub freq_hz: u16,
    pub gain_half_db: i8,
    pub q_idx: u8,
}

impl Band {
    #[must_use]
    pub const fn gain_db(self) -> f32 {
        self.gain_half_db as f32 * 0.5
    }

    #[must_use]
    pub fn q(self) -> f32 {
        q_from_index(self.q_idx)
    }

    // `gain_half_db as u8`/`raw[3] as i8` below are a deliberate bit-
    // pattern reinterpretation (the wire format has no signed-byte type;
    // `i8`'s two's-complement bits ARE the `u8` on the wire), not a lossy
    // numeric cast -- `to_wire`/`from_wire` round-trip it exactly (see
    // `wire_round_trip_preserves_all_fields`).
    #[allow(clippy::cast_sign_loss)]
    fn to_wire(self, out: &mut [u8; BAND_RECORD_LEN]) {
        out[0] = self.kind.to_wire();
        let freq_bytes = self.freq_hz.to_le_bytes();
        out[1] = freq_bytes[0];
        out[2] = freq_bytes[1];
        out[3] = self.gain_half_db as u8;
        out[4] = self.q_idx;
    }

    #[allow(clippy::cast_possible_wrap)]
    fn from_wire(raw: [u8; BAND_RECORD_LEN]) -> Self {
        Self {
            kind: BandKind::from_wire(raw[0]),
            freq_hz: u16::from_le_bytes([raw[1], raw[2]]),
            gain_half_db: raw[3] as i8,
            q_idx: raw[4],
        }
    }
}

/// The Bauer-style crossfeed strength a preset carries alongside its EQ
/// bands. Wire: `0` = `Off`, `1..3` = increasing strength (design sec
/// 2.2: "crossfeed u8 (0 = off, 1..3 = strengths)"). Any other wire value
/// falls back to `Off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CrossfeedLevel {
    #[default]
    Off,
    Weak,
    Medium,
    Strong,
}

impl CrossfeedLevel {
    #[must_use]
    pub const fn to_wire(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Weak => 1,
            Self::Medium => 2,
            Self::Strong => 3,
        }
    }

    /// Any wire value other than `{0, 1, 2, 3}` falls back to
    /// [`Self::default`] (`Off`).
    #[must_use]
    pub const fn from_wire(value: u8) -> Self {
        match value {
            1 => Self::Weak,
            2 => Self::Medium,
            3 => Self::Strong,
            _ => Self::Off,
        }
    }
}

/// A global, named EQ + crossfeed preset. `id` is NOT part of the wire
/// blob -- per the design (sec 2.2), the id lives in C's record header,
/// separate from the blob and from the slot index, so a delete-then-create
/// in the same flash slot allocates a fresh id rather than aliasing an old
/// reference. [`Preset::to_wire`]/[`Preset::from_wire`] therefore only
/// round-trip `name`/`crossfeed`/`bands`; callers that need the id (the
/// store, or a caller building a full record) carry it alongside.
#[derive(Debug, Clone, PartialEq)]
pub struct Preset {
    pub name: String,
    pub crossfeed: CrossfeedLevel,
    pub bands: Vec<Band>,
}

impl Preset {
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            name: truncate_name(name),
            crossfeed: CrossfeedLevel::Off,
            bands: Vec::new(),
        }
    }

    /// Appends `band`, silently dropping it if the preset is already at
    /// [`MAX_BANDS`] -- mirrors the wire format's hard cap rather than
    /// growing past what a `PlDspProgram` (`PL_DSP_MAX_BIQUADS`) or the
    /// blob's fixed band-record area can carry. Returns whether the band
    /// was added.
    pub fn push_band(&mut self, band: Band) -> bool {
        if self.bands.len() >= MAX_BANDS {
            return false;
        }
        self.bands.push(band);
        true
    }

    /// Serialises to the exact [`BLOB_LEN`]-byte v1 wire format (design
    /// sec 2.2): `{version, name_len, name[16], crossfeed, band_count,
    /// band[10]}`. Always produces a fixed-length buffer regardless of how
    /// many bands are set -- unused band slots are zeroed and ignored by
    /// [`Self::from_wire`] via `band_count`.
    #[must_use]
    // `name_len`/`band_count` are `.min()`-clamped to `MAX_NAME_BYTES`
    // (16) / `MAX_BANDS` (10) just above their cast, so `as u8` never
    // truncates -- same reasoning as `crate::app::model`'s scoped
    // `cast_possible_truncation` allows.
    #[allow(clippy::cast_possible_truncation)]
    pub fn to_wire(&self) -> [u8; BLOB_LEN] {
        let mut out = [0u8; BLOB_LEN];
        out[0] = BLOB_VERSION;

        let name_bytes = self.name.as_bytes();
        let name_len = name_bytes.len().min(MAX_NAME_BYTES);
        out[1] = name_len as u8;
        out[2..2 + name_len].copy_from_slice(&name_bytes[..name_len]);

        let crossfeed_off = 2 + MAX_NAME_BYTES;
        out[crossfeed_off] = self.crossfeed.to_wire();

        let band_count_off = crossfeed_off + 1;
        let band_count = self.bands.len().min(MAX_BANDS);
        out[band_count_off] = band_count as u8;

        let bands_off = band_count_off + 1;
        let mut record = [0u8; BAND_RECORD_LEN];
        for (i, band) in self.bands.iter().take(MAX_BANDS).enumerate() {
            band.to_wire(&mut record);
            let start = bands_off + i * BAND_RECORD_LEN;
            out[start..start + BAND_RECORD_LEN].copy_from_slice(&record);
        }

        out
    }

    /// Deserialises a [`BLOB_LEN`]-byte (or shorter -- see below) buffer
    /// back into a [`Preset`], per the same per-field-fallback discipline
    /// [`crate::audio::CushionPolicy::from_wire`] documents: a corrupt or
    /// forward-versioned byte degrades that ONE field rather than
    /// rejecting the whole record, because a boot-time load has nowhere
    /// to surface a parse error and a dropped preset is worse than a
    /// slightly-wrong one.
    ///
    /// - `version` byte other than [`BLOB_VERSION`] is not (yet) rejected:
    ///   there is only one format today, so any byte here is parsed as
    ///   v1. A real v2 bump adds a real branch.
    /// - `name_len`/`band_count` are clamped to their field widths, never
    ///   trusted past them.
    /// - `name` bytes that are not valid UTF-8 (e.g. a torn flash write)
    ///   are lossily repaired rather than panicking.
    /// - A buffer shorter than [`BLOB_LEN`] (e.g. an old, smaller blob) is
    ///   treated as if every byte past its end were `0` -- an empty name,
    ///   `Off` crossfeed, zero bands. It never panics or indexes out of
    ///   bounds.
    #[must_use]
    pub fn from_wire(raw: &[u8]) -> Self {
        let get = |i: usize| -> u8 { raw.get(i).copied().unwrap_or(0) };

        let name_len = (get(1) as usize).min(MAX_NAME_BYTES);
        let mut name_buf = [0u8; MAX_NAME_BYTES];
        for (i, b) in name_buf.iter_mut().enumerate().take(name_len) {
            *b = get(2 + i);
        }
        let name = String::from_utf8_lossy(&name_buf[..name_len]).into_owned();

        let crossfeed_off = 2 + MAX_NAME_BYTES;
        let crossfeed = CrossfeedLevel::from_wire(get(crossfeed_off));

        let band_count_off = crossfeed_off + 1;
        let band_count = (get(band_count_off) as usize).min(MAX_BANDS);

        let bands_off = band_count_off + 1;
        let mut bands = Vec::with_capacity(band_count);
        for i in 0..band_count {
            let start = bands_off + i * BAND_RECORD_LEN;
            let record = [get(start), get(start + 1), get(start + 2), get(start + 3), get(start + 4)];
            bands.push(Band::from_wire(record));
        }

        Self { name, crossfeed, bands }
    }
}

/// Truncates `name` to at most [`MAX_NAME_BYTES`], respecting a UTF-8
/// character boundary -- same reasoning and same pattern as
/// `crate::app::model::truncate_device_name`: "Rust owns text; C owns
/// bytes", so truncation must happen before a name is ever placed on the
/// wire, not after.
fn truncate_name(name: &str) -> String {
    if name.len() <= MAX_NAME_BYTES {
        return String::from(name);
    }
    let mut end = MAX_NAME_BYTES;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    String::from(&name[..end])
}
