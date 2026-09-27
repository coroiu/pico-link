//! The preset model and its on-wire blob format.
//!
//! A [`Preset`] is a name, an optional crossfeed strength and up to
//! [`MAX_BANDS`] parametric EQ bands. It is entirely data -- turning it
//! into filter coefficients is [`super::coeffs`]'s job, and giving it an
//! id and a durable home is [`super::store::PresetStore`]'s.
//!
//! # Blob v2 (bead `pico-link-ryw.12.1`, Ada's DESIGN comment on
//! `pico-link-ryw.12`)
//!
//! v1 stored every field pre-quantised to the editor's own coarse steps
//! (half-dB gain, a `Q_TABLE` index, whole-Hz frequency) -- fine for a
//! hand-built preset, but lossy for an imported APO/AutoEQ curve that
//! needs its exact published numbers preserved. v2 stores the *exact*
//! value (centi-dB gain, milli-Q, half-Hz frequency) and moves the
//! coarse-step vocabulary ([`Q_TABLE`]) to be purely the on-device
//! editor's picker, not the wire format. It still fits the existing
//! 80-byte blob (design sec 2's whole point: no C, FFI or `persist.c`
//! change), and reads v1 exactly, rewriting as v2 on the next save (see
//! [`Preset::from_wire`]).

use alloc::string::String;
use alloc::vec::Vec;

/// `PL_DSP_MAX_BIQUADS` in the design (sec 3.2) -- the FFI program's fixed
/// biquad-array capacity, and therefore the most bands one preset can hold.
pub const MAX_BANDS: usize = 10;

/// The blob's name field width (design sec 2.2/ryw.12 sec 2: `name[16]`).
pub const MAX_NAME_BYTES: usize = 16;

/// The blob's total wire length, unchanged across v1 and v2 (ryw.12 sec 2:
/// "still 80 bytes: no C, FFI or persist change"). v1 only used about 70 of
/// these 80 bytes; v2 uses all of them. C's on-flash record reserves
/// `blob[80]` regardless of which version core wrote.
pub const BLOB_LEN: usize = 80;

/// v1's band record length: `{type u8, freq_hz u16 LE, gain_half_db i8,
/// q_idx u8}` (design sec 2.2) -- kept only for [`Preset::from_wire_v1`],
/// the legacy reader.
const V1_BAND_RECORD_LEN: usize = 5;

/// v2's band record length: `{kind_gain u16 LE, freq_half_hz u16 LE,
/// q_milli u16 LE}` (ryw.12 sec 2).
const V2_BAND_RECORD_LEN: usize = 6;

/// v1's wire layout offsets (design sec 2.2): `{version, name_len,
/// name[16], crossfeed, band_count, band[10]}`.
mod v1_layout {
    pub const NAME_LEN_OFF: usize = 1;
    pub const NAME_OFF: usize = 2;
    pub const CROSSFEED_OFF: usize = NAME_OFF + super::MAX_NAME_BYTES; // 18
    pub const BAND_COUNT_OFF: usize = CROSSFEED_OFF + 1; // 19
    pub const BANDS_OFF: usize = BAND_COUNT_OFF + 1; // 20
}

/// v2's wire layout offsets (ryw.12 sec 2): `{version, name[16], flags,
/// preamp_cdb[2], band[10]}`.
mod v2_layout {
    pub const NAME_OFF: usize = 1;
    pub const FLAGS_OFF: usize = NAME_OFF + super::MAX_NAME_BYTES; // 17
    pub const PREAMP_OFF: usize = FLAGS_OFF + 1; // 18
    pub const BANDS_OFF: usize = PREAMP_OFF + 2; // 20
}

/// The wire format version this build WRITES (ryw.12 sec 2's "Writes
/// always emit v2"). [`Preset::from_wire`] still reads v1 (see its doc
/// comment) and any other byte via [`Preset::from_wire_unknown`].
const BLOB_VERSION_V1: u8 = 1;
const BLOB_VERSION_V2: u8 = 2;

/// A fixed table of musically useful Q values -- as of blob v2, this is
/// PURELY the on-device editor's picker vocabulary (ryw.12 sec 2: "`Q_TABLE`
/// stays, but only as the EDITOR's picker steps"), not the wire format:
/// [`Band::q_milli`] stores an exact milli-Q, and the editor snaps to the
/// nearest entry here on first press rather than being constrained to
/// live only on these 8 values. Spans a wide, log-ish spread from broad
/// (`0.4`) to sharp (`8.0`); index 3 (`1.0`) is the RBJ cookbook's "no
/// resonance" reference Q and is the picker default.
pub const Q_TABLE: [f32; 8] = [0.4, 0.6, 0.71, 1.0, 1.4, 2.0, 3.2, 8.0];

/// [`Q_TABLE`] widened to milli-Q (`* 1000`) as literal constants, not a
/// runtime `libm` multiply -- every entry is exact at this precision (e.g.
/// `0.71 * 1000 == 710` with no rounding uncertainty), so a literal table
/// keeps the v1->v2 conversion bit-reproducible without depending on
/// float-to-int rounding behaviour. Index-for-index parallel to
/// [`Q_TABLE`] -- see `q_table_milli_matches_q_table` in `dsp::tests`.
const Q_TABLE_MILLI: [u16; 8] = [400, 600, 710, 1_000, 1_400, 2_000, 3_200, 8_000];

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

/// Milli-Q for [`Q_TABLE`] index `q_idx`, clamped like [`q_from_index`] --
/// the editor's exact-value counterpart, used when converting a picker
/// selection into a [`Band::q_milli`] to store.
#[must_use]
pub fn q_milli_from_index(q_idx: u8) -> u16 {
    let idx = (q_idx as usize).min(Q_TABLE_MILLI.len() - 1);
    Q_TABLE_MILLI[idx]
}

/// The [`Q_TABLE`] index whose milli-Q is closest to `q_milli` -- the
/// editor's "snap to the nearest table entry" rule (ryw.12 sec 2). Ties
/// break toward the lower index (first minimum found).
#[must_use]
pub fn nearest_q_index(q_milli: u16) -> u8 {
    let mut best_idx = 0usize;
    let mut best_diff = u32::MAX;
    for (idx, &table_milli) in Q_TABLE_MILLI.iter().enumerate() {
        let diff = i32::from(table_milli).abs_diff(i32::from(q_milli));
        if diff < best_diff {
            best_diff = diff;
            best_idx = idx;
        }
    }
    // MAX_BANDS/Q_TABLE_MILLI.len() are both tiny fixed sizes (8), so this
    // never truncates.
    #[allow(clippy::cast_possible_truncation)]
    {
        best_idx as u8
    }
}

/// Which RBJ cookbook filter shape a [`Band`] is. Wire: `0` = `Peak`, `1` =
/// `LowShelf`, `2` = `HighShelf`. Any other wire value falls back to
/// `Peak` (see [`BandKind::from_wire`]). v2's `kind_gain` field reserves
/// wire values 3-7 for a future LP/HP/notch/AP -- see [`super`]'s module
/// doc.
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

/// One parametric EQ band, stored at v2's exact precision (ryw.12 sec 2):
/// `freq_half_hz` in 0.5Hz steps, `gain_cdb` in 0.01dB steps (centi-dB),
/// `q_milli` in 0.001 steps (milli-Q). None of these are quantised to the
/// on-device editor's coarser picker vocabulary ([`Q_TABLE`], the
/// 1/3-octave frequency ladder) -- that quantisation is the EDITOR's job
/// (`core::app::screens::effects`), not the storage model's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    pub kind: BandKind,
    pub freq_half_hz: u16,
    pub gain_cdb: i16,
    pub q_milli: u16,
}

/// v2's `kind_gain` packing: gain is a signed 13-bit centi-dB value
/// (`+/-4095`, i.e. `+/-40.95dB`, comfortably past the accepted-import
/// range of `+/-30dB`) in bits 0-12, `kind` in bits 13-15 (ryw.12 sec 2).
const KIND_GAIN_GAIN_BITS: u32 = 13;
const KIND_GAIN_GAIN_MASK: u16 = (1 << KIND_GAIN_GAIN_BITS) - 1; // 0x1FFF
const KIND_GAIN_GAIN_SIGN_BIT: i16 = 1 << (KIND_GAIN_GAIN_BITS - 1); // 0x1000
const KIND_GAIN_GAIN_MAX: i16 = KIND_GAIN_GAIN_SIGN_BIT - 1; // 4095
const KIND_GAIN_GAIN_MIN: i16 = -KIND_GAIN_GAIN_MAX; // -4095, kept symmetric

impl Band {
    #[must_use]
    pub fn freq_hz(self) -> f32 {
        f32::from(self.freq_half_hz) * 0.5
    }

    #[must_use]
    pub fn gain_db(self) -> f32 {
        f32::from(self.gain_cdb) * 0.01
    }

    #[must_use]
    pub fn q(self) -> f32 {
        f32::from(self.q_milli) * 0.001
    }

    /// Packs `kind` and `gain_cdb` into v2's 16-bit `kind_gain` field.
    /// `gain_cdb` is clamped to `[KIND_GAIN_GAIN_MIN, KIND_GAIN_GAIN_MAX]`
    /// first -- the field only has 13 signed bits, and this bead's own
    /// values (a widened v1 gain, at most `+/-64 * 50 == +/-3200`) always
    /// fit; the clamp only guards a future caller that doesn't.
    #[allow(clippy::cast_sign_loss)] // two's-complement truncation, documented below
    fn pack_kind_gain(kind: BandKind, gain_cdb: i16) -> u16 {
        let clamped = gain_cdb.clamp(KIND_GAIN_GAIN_MIN, KIND_GAIN_GAIN_MAX);
        // Truncating a value that fits in N signed bits to its low N bits
        // of two's-complement representation IS that value's N-bit
        // two's-complement encoding -- not a lossy reinterpretation, see
        // `kind_gain_pack_unpack_round_trips_the_full_clamped_range`.
        let gain_bits = (clamped as u16) & KIND_GAIN_GAIN_MASK;
        let kind_bits = u16::from(kind.to_wire()) << KIND_GAIN_GAIN_BITS;
        kind_bits | gain_bits
    }

    #[allow(clippy::cast_possible_wrap)]
    fn unpack_kind_gain(raw: u16) -> (BandKind, i16) {
        let kind = BandKind::from_wire((raw >> KIND_GAIN_GAIN_BITS) as u8 & 0x7);
        let gain_bits = raw & KIND_GAIN_GAIN_MASK;
        let gain_cdb = if gain_bits & (KIND_GAIN_GAIN_SIGN_BIT as u16) != 0 {
            (gain_bits as i16) - (1 << KIND_GAIN_GAIN_BITS)
        } else {
            gain_bits as i16
        };
        (kind, gain_cdb)
    }

    fn to_wire_v2(self, out: &mut [u8; V2_BAND_RECORD_LEN]) {
        let kind_gain = Self::pack_kind_gain(self.kind, self.gain_cdb);
        out[0..2].copy_from_slice(&kind_gain.to_le_bytes());
        out[2..4].copy_from_slice(&self.freq_half_hz.to_le_bytes());
        out[4..6].copy_from_slice(&self.q_milli.to_le_bytes());
    }

    fn from_wire_v2(raw: [u8; V2_BAND_RECORD_LEN]) -> Self {
        let kind_gain = u16::from_le_bytes([raw[0], raw[1]]);
        let (kind, gain_cdb) = Self::unpack_kind_gain(kind_gain);
        let freq_half_hz = u16::from_le_bytes([raw[2], raw[3]]);
        let q_milli = u16::from_le_bytes([raw[4], raw[5]]);
        Self { kind, freq_half_hz, gain_cdb, q_milli }
    }

    /// Widens a v1 band record to v2's exact representation -- every
    /// field is an EXACT scale, never a rounded approximation (ryw.12 sec
    /// 2: "exact widening (`gain_half_db*50`, `freq*2`, `Q_TABLE` value*1000,
    /// and every table entry is exact in milli-Q)"), so a v1 preset reads
    /// back and re-serialises to v2 bit-identically to what a from-scratch
    /// v2 conversion of the same logical values would produce.
    #[allow(clippy::cast_possible_wrap)] // v1's `raw[3] as i8` bit-pattern reinterpretation, same as always
    fn from_wire_v1(raw: [u8; V1_BAND_RECORD_LEN]) -> Self {
        let kind = BandKind::from_wire(raw[0]);
        let freq_hz = u16::from_le_bytes([raw[1], raw[2]]);
        let gain_half_db = raw[3] as i8;
        let q_idx = raw[4];

        Self {
            kind,
            freq_half_hz: freq_hz.saturating_mul(2),
            gain_cdb: i16::from(gain_half_db) * 50,
            q_milli: q_milli_from_index(q_idx),
        }
    }
}

/// The Bauer-style crossfeed strength a preset carries alongside its EQ
/// bands. Wire: `0` = `Off`, `1..3` = increasing strength (design sec
/// 2.2: "crossfeed u8 (0 = off, 1..3 = strengths)"). Any other wire value
/// falls back to `Off`. v2 packs this into 2 bits of the flags byte
/// (ryw.12 sec 2) -- the same 4-value range, just relocated.
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

/// A preset's preamp: hand-made (on-device editor) presets are always
/// [`Self::Auto`] -- the editor never shows or edits a preamp, so that UX
/// is unchanged by v2 -- while an IMPORTED preset carries the published
/// value verbatim as [`Self::Explicit`] (centi-dB), because reproducing
/// the source exactly is the whole point of an import (ryw.12 sec 2:
/// "Imported presets are Explicit"). [`super::coeffs::Program::from_preset`]
/// uses [`Self::Explicit`]'s value directly or falls back to
/// [`super::coeffs::auto_preamp_db`] for [`Self::Auto`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preamp {
    #[default]
    Auto,
    /// Centi-dB (0.01dB units), matching [`Band::gain_cdb`]'s precision.
    Explicit(i16),
}

/// v2's flags byte layout (ryw.12 sec 2): bits 0-1 crossfeed, bit 2
/// `preamp_explicit`, bit 3 `eq_locked`, bits 4-7 `band_count`.
const FLAGS_CROSSFEED_MASK: u8 = 0b0000_0011;
const FLAGS_PREAMP_EXPLICIT_BIT: u8 = 0b0000_0100;
const FLAGS_EQ_LOCKED_BIT: u8 = 0b0000_1000;
const FLAGS_BAND_COUNT_SHIFT: u32 = 4;

/// A global, named EQ + crossfeed preset. `id` is NOT part of the wire
/// blob -- per the design (sec 2.2), the id lives in C's record header,
/// separate from the blob and from the slot index, so a delete-then-create
/// in the same flash slot allocates a fresh id rather than aliasing an old
/// reference. [`Preset::to_wire`]/[`Preset::from_wire`] therefore only
/// round-trip `name`/`crossfeed`/`bands`/`preamp`/`eq_locked` -- callers
/// that need the id (the store, or a caller building a full record) carry
/// it alongside.
#[derive(Debug, Clone, PartialEq)]
pub struct Preset {
    pub name: String,
    pub crossfeed: CrossfeedLevel,
    pub bands: Vec<Band>,
    /// [`Preamp::Auto`] for every on-device-editor preset (ryw.12 sec 2) --
    /// only an import ever sets [`Preamp::Explicit`].
    pub preamp: Preamp,
    /// Set on an imported preset (ryw.12 sec 3: the editor's band rows
    /// become read-only) or on a [`Self::from_wire`] fallback for an
    /// unrecognised blob version (so an older firmware reading a newer
    /// blob can never overwrite it through the editor). Never set by
    /// [`Self::new`] or the on-device editor itself.
    pub eq_locked: bool,
}

impl Preset {
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self { name: truncate_name(name), crossfeed: CrossfeedLevel::Off, bands: Vec::new(), preamp: Preamp::Auto, eq_locked: false }
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

    /// Serialises to the exact [`BLOB_LEN`]-byte v2 wire format (ryw.12
    /// sec 2): `{version=2, name[16], flags, preamp_cdb[2], band[10]}`.
    /// Always produces a fixed-length buffer regardless of how many bands
    /// are set -- unused band slots are zeroed and ignored by
    /// [`Self::from_wire_v2`] via the flags byte's `band_count`. v1
    /// records loaded via [`Self::from_wire`] are therefore rewritten as
    /// v2 the next time they're saved (no separate migration pass, no
    /// flash write at boot -- ryw.12 sec 2).
    #[must_use]
    // `band_count` is `.min()`-clamped to `MAX_BANDS` (10) just above its
    // cast/shift, so it never overflows the flags byte's 4 high bits --
    // same reasoning as `crate::app::model`'s scoped
    // `cast_possible_truncation` allows.
    #[allow(clippy::cast_possible_truncation)]
    pub fn to_wire(&self) -> [u8; BLOB_LEN] {
        let mut out = [0u8; BLOB_LEN];
        out[0] = BLOB_VERSION_V2;

        let name_bytes = self.name.as_bytes();
        let name_len = name_bytes.len().min(MAX_NAME_BYTES);
        out[v2_layout::NAME_OFF..v2_layout::NAME_OFF + name_len].copy_from_slice(&name_bytes[..name_len]);

        let band_count = self.bands.len().min(MAX_BANDS) as u8;
        let preamp_explicit = matches!(self.preamp, Preamp::Explicit(_));
        let mut flags = self.crossfeed.to_wire() & FLAGS_CROSSFEED_MASK;
        if preamp_explicit {
            flags |= FLAGS_PREAMP_EXPLICIT_BIT;
        }
        if self.eq_locked {
            flags |= FLAGS_EQ_LOCKED_BIT;
        }
        flags |= band_count << FLAGS_BAND_COUNT_SHIFT;
        out[v2_layout::FLAGS_OFF] = flags;

        let preamp_cdb: i16 = match self.preamp {
            Preamp::Auto => 0,
            Preamp::Explicit(v) => v,
        };
        out[v2_layout::PREAMP_OFF..v2_layout::PREAMP_OFF + 2].copy_from_slice(&preamp_cdb.to_le_bytes());

        let mut record = [0u8; V2_BAND_RECORD_LEN];
        for (i, band) in self.bands.iter().take(MAX_BANDS).enumerate() {
            band.to_wire_v2(&mut record);
            let start = v2_layout::BANDS_OFF + i * V2_BAND_RECORD_LEN;
            out[start..start + V2_BAND_RECORD_LEN].copy_from_slice(&record);
        }

        out
    }

    /// Deserialises a blob, dispatching on the version byte (ryw.12 sec
    /// 2):
    /// - `1`: [`Self::from_wire_v1`], widened exactly to v2's in-memory
    ///   representation, `preamp: Auto`, `eq_locked: false`.
    /// - `2`: [`Self::from_wire_v2`].
    /// - anything else: [`Self::from_wire_unknown`] -- an empty, LOCKED
    ///   preset that keeps its name bytes, so an older firmware reading a
    ///   newer blob format can never overwrite it through the editor.
    ///
    /// A buffer shorter than [`BLOB_LEN`] is never trusted past its own
    /// length -- every field read goes through a bounds-checked `get`
    /// helper that treats a missing byte as `0`, so this never panics or
    /// indexes out of bounds (same per-field-fallback discipline
    /// [`crate::audio::CushionPolicy::from_wire`] documents).
    #[must_use]
    pub fn from_wire(raw: &[u8]) -> Self {
        match raw.first().copied().unwrap_or(0) {
            BLOB_VERSION_V1 => Self::from_wire_v1(raw),
            BLOB_VERSION_V2 => Self::from_wire_v2(raw),
            _ => Self::from_wire_unknown(raw),
        }
    }

    fn from_wire_v1(raw: &[u8]) -> Self {
        let get = |i: usize| -> u8 { raw.get(i).copied().unwrap_or(0) };

        let name_len = (get(v1_layout::NAME_LEN_OFF) as usize).min(MAX_NAME_BYTES);
        let mut name_buf = [0u8; MAX_NAME_BYTES];
        for (i, b) in name_buf.iter_mut().enumerate().take(name_len) {
            *b = get(v1_layout::NAME_OFF + i);
        }
        let name = String::from_utf8_lossy(&name_buf[..name_len]).into_owned();

        let crossfeed = CrossfeedLevel::from_wire(get(v1_layout::CROSSFEED_OFF));
        let band_count = (get(v1_layout::BAND_COUNT_OFF) as usize).min(MAX_BANDS);

        let mut bands = Vec::with_capacity(band_count);
        for i in 0..band_count {
            let start = v1_layout::BANDS_OFF + i * V1_BAND_RECORD_LEN;
            let record = [get(start), get(start + 1), get(start + 2), get(start + 3), get(start + 4)];
            bands.push(Band::from_wire_v1(record));
        }

        Self { name, crossfeed, bands, preamp: Preamp::Auto, eq_locked: false }
    }

    fn from_wire_v2(raw: &[u8]) -> Self {
        let get = |i: usize| -> u8 { raw.get(i).copied().unwrap_or(0) };
        let get_u16 = |i: usize| -> u16 { u16::from_le_bytes([get(i), get(i + 1)]) };

        let mut name_buf = [0u8; MAX_NAME_BYTES];
        for (i, b) in name_buf.iter_mut().enumerate() {
            *b = get(v2_layout::NAME_OFF + i);
        }
        let name_len = name_buf.iter().position(|&b| b == 0).unwrap_or(MAX_NAME_BYTES);
        let name = String::from_utf8_lossy(&name_buf[..name_len]).into_owned();

        let flags = get(v2_layout::FLAGS_OFF);
        let crossfeed = CrossfeedLevel::from_wire(flags & FLAGS_CROSSFEED_MASK);
        let preamp_explicit = flags & FLAGS_PREAMP_EXPLICIT_BIT != 0;
        let eq_locked = flags & FLAGS_EQ_LOCKED_BIT != 0;
        let band_count = ((flags >> FLAGS_BAND_COUNT_SHIFT) as usize).min(MAX_BANDS);

        #[allow(clippy::cast_possible_wrap)]
        let preamp_cdb = get_u16(v2_layout::PREAMP_OFF) as i16;
        let preamp = if preamp_explicit { Preamp::Explicit(preamp_cdb) } else { Preamp::Auto };

        let mut bands = Vec::with_capacity(band_count);
        for i in 0..band_count {
            let start = v2_layout::BANDS_OFF + i * V2_BAND_RECORD_LEN;
            let record = [get(start), get(start + 1), get(start + 2), get(start + 3), get(start + 4), get(start + 5)];
            bands.push(Band::from_wire_v2(record));
        }

        Self { name, crossfeed, bands, preamp, eq_locked }
    }

    /// An unrecognised blob version: empty, locked, `Off` crossfeed, `Auto`
    /// preamp, but the name bytes are still read (at v2's name offset --
    /// the only layout a real future version is expected to share) so a
    /// forward-versioned preset at least keeps showing its name rather
    /// than going blank (ryw.12 sec 2).
    fn from_wire_unknown(raw: &[u8]) -> Self {
        let get = |i: usize| -> u8 { raw.get(i).copied().unwrap_or(0) };
        let mut name_buf = [0u8; MAX_NAME_BYTES];
        for (i, b) in name_buf.iter_mut().enumerate() {
            *b = get(v2_layout::NAME_OFF + i);
        }
        let name_len = name_buf.iter().position(|&b| b == 0).unwrap_or(MAX_NAME_BYTES);
        let name = String::from_utf8_lossy(&name_buf[..name_len]).into_owned();

        Self { name, crossfeed: CrossfeedLevel::Off, bands: Vec::new(), preamp: Preamp::Auto, eq_locked: true }
    }
}

/// What [`Preset::from_wire_checked`] rejects outright, where
/// [`Preset::from_wire`]'s tolerant boot-load path instead falls back to an
/// empty locked preset (design section 4, ADA DESIGN comment on
/// `pico-link-jyhk.17`: "Host blobs are validated STRICTLY, not via
/// `Preset::from_wire`'s tolerant path: `from_wire` turns an unknown
/// version into an empty locked preset -- fine for flash, silently
/// destructive for input").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetBlobError {
    /// Anything other than the v2 blob version this build writes (`v1` is
    /// a legacy *reader*-only format -- host/web input is never expected to
    /// send it, and accepting it here would let a host silently downgrade a
    /// stored preset's precision).
    UnsupportedVersion { version: u8 },
    /// The flags byte's `band_count` (4 bits, so representable up to `15`)
    /// exceeded [`MAX_BANDS`] -- wider than a `PlDspProgram`
    /// (`PL_DSP_MAX_BIQUADS`) can actually hold.
    TooManyBands { band_count: usize },
    /// A band's `kind` bits (design's `kind_gain` packing) decoded to a
    /// value [`BandKind::from_wire`] has no name for (wire values `3`-`7`,
    /// reserved for a future filter shape). `from_wire`'s tolerant path
    /// silently rewrites this to `Peak`; strict host input must not
    /// silently change what the caller asked for.
    ReservedBandKind { band_index: usize, raw_kind: u8 },
    /// The name field's bytes up to its first `0` terminator were not valid
    /// UTF-8 -- `from_wire`'s tolerant path uses
    /// `String::from_utf8_lossy`, replacing bad bytes rather than
    /// rejecting; strict host input treats a non-UTF-8 name as invalid
    /// input (design section 4: "`NAME_INVALID` (empty / not UTF-8)").
    InvalidNameUtf8,
}

impl Preset {
    /// Strictly decodes a `HOST_OP` `SAVE`/`PREVIEW` blob -- design section
    /// 4's "Host blobs are validated STRICTLY" contract. Unlike
    /// [`Self::from_wire`], an unrecognised version, an over-wide band
    /// count, a reserved band `kind`, or a non-UTF-8 name is REJECTED
    /// outright rather than degraded into an empty locked preset or
    /// silently corrected -- see [`PresetBlobError`]'s doc comment for why
    /// each check exists. [`Self::from_wire`] itself is UNCHANGED: the
    /// boot-load/echo-decode path (`App::on_preset_loaded`) keeps its
    /// tolerant, never-panics, never-rejects behaviour, because a corrupt
    /// byte already on flash must degrade gracefully, not brick the whole
    /// load.
    ///
    /// # Errors
    ///
    /// The first [`PresetBlobError`] found, in the order: version, band
    /// count, then each band's `kind` in order, then the name's UTF-8
    /// validity.
    pub fn from_wire_checked(raw: &[u8]) -> Result<Self, PresetBlobError> {
        let version = raw.first().copied().unwrap_or(0);
        if version != BLOB_VERSION_V2 {
            return Err(PresetBlobError::UnsupportedVersion { version });
        }

        let get = |i: usize| -> u8 { raw.get(i).copied().unwrap_or(0) };

        let flags = get(v2_layout::FLAGS_OFF);
        let band_count = (flags >> FLAGS_BAND_COUNT_SHIFT) as usize;
        if band_count > MAX_BANDS {
            return Err(PresetBlobError::TooManyBands { band_count });
        }

        for i in 0..band_count {
            let start = v2_layout::BANDS_OFF + i * V2_BAND_RECORD_LEN;
            let kind_gain = u16::from_le_bytes([get(start), get(start + 1)]);
            let raw_kind = ((kind_gain >> KIND_GAIN_GAIN_BITS) as u8) & 0x7;
            if raw_kind > 2 {
                return Err(PresetBlobError::ReservedBandKind { band_index: i + 1, raw_kind });
            }
        }

        let mut name_buf = [0u8; MAX_NAME_BYTES];
        for (i, b) in name_buf.iter_mut().enumerate() {
            *b = get(v2_layout::NAME_OFF + i);
        }
        let name_len = name_buf.iter().position(|&b| b == 0).unwrap_or(MAX_NAME_BYTES);
        if core::str::from_utf8(&name_buf[..name_len]).is_err() {
            return Err(PresetBlobError::InvalidNameUtf8);
        }

        Ok(Self::from_wire_v2(raw))
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
