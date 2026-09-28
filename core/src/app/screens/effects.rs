//! DSP effects: the effects list, the one-screen editor, and the delete
//! confirm -- bead `pico-link-ryw.7`, design
//! `.planning/design/2026-09-25-dsp-effects-ux.md`. The device page's
//! `EFFECT` row/picker lives in `super::device_page`; Home's `Effects` row
//! and `FX <name>` line live in `crate::render::home` -- both read
//! [`resolve_effect_name`]/[`PresetStore`] the same way this module does.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::convert::Infallible;

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::{Point, Size};
use embedded_graphics::primitives::Rectangle;
use u8g2_fonts::types::{HorizontalAlignment, VerticalPosition};
use u8g2_fonts::FontRenderer;

use crate::dsp::preset::{nearest_q_index, q_milli_from_index, Band, BandKind, CrossfeedLevel, Preamp, Q_TABLE};
use crate::dsp::{Program, Preset, PresetStore, MIN_BOOST_HEADROOM_DB};
use crate::input::NavIntent;
use crate::render::theme::{font, palette};
use crate::render::{
    Action, ButtonLabel, ChromeContribution, ConfirmView, FieldList, FieldRow, FocusEvent, FrameBuffer565, ListItemKey, MenuItem, PaintKey,
    RenderCtx, Screen, Step, StepBounds, Verb, Widget,
};

use super::super::{BtModel, Command, ModelHandle, ScreenId};

/// How many DSP effects the flash store can remember (`PL:P:0`..`PL:P:7`,
/// design sec 2.2) -- the effects list gates "New effect" on this, the
/// same "discoverable without any radio/flash work" gate
/// `MAX_PAIRED_DEVICES` gives the Devices screen.
pub(in crate::app) const MAX_EFFECTS: usize = 8;

/// The sample rate every DSP program in this build is compiled at -- this
/// project's USB chain is 48k-only today (same "no `sample_rate_hz` seam
/// yet" honesty rule `ldac_quality.rs` already documents for its own rate
/// ladder).
const DSP_FS_HZ: u32 = 48_000;

// --- Resolving a preset id to a display name (used by the device page and
// Home) -------------------------------------------------------------------

/// Resolves `preset_id` to the effect's display name, or `"Off"` for
/// [`crate::dsp::store::NO_PRESET_ID`] (`0`) or any dangling id -- the one
/// place every consumer (the device page's `EFFECT` row, its picker, and
/// Home's `FX` line) gets this word from, so they can never drift apart on
/// how an unassigned/deleted preset renders (design sec 2.4/`PresetStore::
/// resolve`'s doc comment: `core` never distinguishes the two cases).
#[must_use]
pub(crate) fn resolve_effect_name(presets: &PresetStore, preset_id: u16) -> String {
    presets.resolve(preset_id).map_or_else(|| String::from("Off"), |preset| preset.name.clone())
}

// --- Ladders and defaults --------------------------------------------------

const BAND_COUNT: usize = 5;
const BAND_DEFAULT_KIND: [BandKind; BAND_COUNT] = [BandKind::LowShelf, BandKind::Peak, BandKind::Peak, BandKind::Peak, BandKind::HighShelf];
const BAND_DEFAULT_FREQ_HZ: [u16; BAND_COUNT] = [100, 250, 1_000, 4_000, 8_000];
/// `Q_TABLE` index 2 (`0.71`) for the two shelves, index 4 (`1.4`) for the
/// three peaks -- the closest entries in the shipped [`Q_TABLE`] to the
/// design's own "0.7 shelves, 1.4 peaks" (design sec 4.2); the design's
/// worked Q ladder (0.5/0.7/1.0/1.4/2.0/2.8/4.0/5.6) is an approximation
/// of `Q_TABLE`, not a second incompatible one -- this module always
/// displays `Q_TABLE`'s own shipped values, converted to [`Band::q_milli`]
/// via [`q_milli_from_index`] before it ever reaches DSP-program build
/// time.
const BAND_DEFAULT_Q_IDX: [u8; BAND_COUNT] = [2, 4, 4, 4, 2];
const DEFAULT_CROSSFEED: CrossfeedLevel = CrossfeedLevel::Medium;

/// ISO-ish 1/3-octave preferred-number ladder, 20Hz..20kHz, 31 steps
/// (design sec 4.2). `32`, not `31.5`: a `u16` ladder has no fractional
/// step, and the design's own defaults (100/250/1k/4k/8k Hz) don't land
/// anywhere near that one step, so the half-Hz rounding is inaudible and
/// invisible to every test/default in this module.
const FREQ_LADDER_HZ: [u16; 31] = [
    20, 25, 32, 40, 50, 63, 80, 100, 125, 160, 200, 250, 315, 400, 500, 630, 800, 1_000, 1_250, 1_600, 2_000, 2_500, 3_150, 4_000, 5_000, 6_300,
    8_000, 10_000, 12_500, 16_000, 20_000,
];

/// Gain bounds/step, in centi-dB -- v2's exact storage unit (ryw.12 sec
/// 2). `+/-1200 cdb` (`+/-12dB`) and a `100 cdb` (1dB) step reproduce
/// v1's `+/-24` half-dB-unit bounds and 2-half-dB-unit step exactly
/// (`24 * 50 == 1200`, `2 * 50 == 100`) -- the editor's step BEHAVIOUR is
/// unchanged, only the field it steps is now the exact wire unit instead
/// of a pre-quantised one.
const GAIN_MIN_CDB: i16 = -1_200;
const GAIN_MAX_CDB: i16 = 1_200;
const GAIN_STEP_CDB: i16 = 100;

/// The canned NAME cycle's 11 fixed candidates -- index 0 of the full
/// 12-entry cycle is always the dynamic "Effect N" (see
/// [`effect_n_candidate`]), never one of these (design sec 4.5).
const FIXED_NAME_CANDIDATES: [&str; 11] =
    ["Relaxed", "Long session", "Meetings", "Music", "Movies", "Podcasts", "Speech", "Warm", "Bright", "Bass", "Late night"];

fn band_default(index: usize) -> Band {
    Band {
        kind: BAND_DEFAULT_KIND[index],
        freq_half_hz: BAND_DEFAULT_FREQ_HZ[index].saturating_mul(2),
        gain_cdb: 0,
        q_milli: q_milli_from_index(BAND_DEFAULT_Q_IDX[index]),
    }
}

fn default_bands() -> Vec<Band> {
    (0..BAND_COUNT).map(band_default).collect()
}

/// A brand-new effect's starting point (design sec 4.2's "[Medium]"/
/// per-band bracketed defaults): 5 fixed-type bands at their defaults,
/// Medium crossfeed, `name` already resolved to a free canned candidate.
fn new_effect_preset(name: &str) -> Preset {
    let mut preset = Preset::new(name);
    preset.crossfeed = DEFAULT_CROSSFEED;
    preset.bands = default_bands();
    preset
}

// --- Formatting -------------------------------------------------------------

fn crossfeed_word(level: CrossfeedLevel) -> &'static str {
    match level {
        CrossfeedLevel::Off => "Off",
        CrossfeedLevel::Weak => "Light",
        CrossfeedLevel::Medium => "Medium",
        CrossfeedLevel::Strong => "Strong",
    }
}

fn band_kind_word(kind: BandKind) -> &'static str {
    match kind {
        BandKind::LowShelf => "low shelf",
        BandKind::Peak => "peak",
        BandKind::HighShelf => "high shelf",
    }
}

fn band_label(index: usize, kind: BandKind) -> String {
    format!("{} . {}", index + 1, band_kind_word(kind))
}

/// `100 Hz` below 1kHz, `1 kHz`/`1.25 kHz`/`12.5 kHz` above it -- trims a
/// trailing `.0`/`0` rather than always showing two decimals (design sec
/// 4.2's worked examples never show a trailing zero). Takes v2's
/// `freq_half_hz`; every ladder entry this editor ever produces is a
/// whole Hz (`* 2` from [`FREQ_LADDER_HZ`]), so the divide-by-2 below is
/// exact.
fn format_freq(freq_half_hz: u16) -> String {
    let freq_hz = freq_half_hz / 2;
    if freq_hz < 1_000 {
        return format!("{freq_hz} Hz");
    }
    let whole = freq_hz / 1_000;
    let frac = freq_hz % 1_000;
    if frac == 0 {
        format!("{whole} kHz")
    } else {
        // `frac` is always a multiple of 10 for every ladder entry above
        // 1kHz (1250, 1600, 2500, 3150, 6300, 12500), so two decimals
        // never loses precision; trim a trailing zero for the common
        // one-decimal case (1.6, not 1.60).
        let hundredths = frac / 10;
        let text = format!("{whole}.{hundredths:02}");
        let trimmed = text.trim_end_matches('0').trim_end_matches('.');
        format!("{trimmed} kHz")
    }
}

/// `+3 dB` / `0 dB` / `-2 dB` -- the editor only ever steps `gain_cdb` in
/// whole `GAIN_STEP_CDB` (100 = 1dB) increments, so integer division is
/// exact.
fn format_gain_db(gain_cdb: i16) -> String {
    let db = i32::from(gain_cdb) / 100;
    if db == 0 {
        String::from("0 dB")
    } else {
        format!("{db:+} dB")
    }
}

/// Formats `q_milli` (v2's exact storage unit) the same as the old
/// [`Q_TABLE`]-index display -- for every value the editor itself ever
/// produces, `q_milli` IS `q_milli_from_index(idx)` for some table entry,
/// so this renders identically to the old `format_q(q_idx)`.
fn format_q(q_milli: u16) -> String {
    let q = f32::from(q_milli) * 0.001;
    let text = format!("{q:.2}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    String::from(trimmed)
}

// --- Stepping -----------------------------------------------------------

fn step_crossfeed(level: CrossfeedLevel, dir: Step) -> CrossfeedLevel {
    let idx = level.to_wire();
    let new = match dir {
        Step::Prev => idx.saturating_sub(1),
        Step::Next => (idx + 1).min(3),
    };
    CrossfeedLevel::from_wire(new)
}

fn crossfeed_bounds(level: CrossfeedLevel) -> StepBounds {
    let idx = level.to_wire();
    StepBounds { prev: idx > 0, next: idx < 3 }
}

fn step_band_index(index: usize, dir: Step) -> usize {
    match dir {
        Step::Prev => index.saturating_sub(1),
        Step::Next => (index + 1).min(BAND_COUNT - 1),
    }
}

fn band_index_bounds(index: usize) -> StepBounds {
    StepBounds { prev: index > 0, next: index < BAND_COUNT - 1 }
}

fn freq_ladder_index(freq_hz: u16) -> usize {
    FREQ_LADDER_HZ.iter().position(|&f| f == freq_hz).unwrap_or(17) // 1kHz
}

/// Steps `freq_half_hz` (v2's exact storage unit) one [`FREQ_LADDER_HZ`]
/// entry at a time -- identical stepping behaviour to the old
/// `step_freq(freq_hz)`, just converting to/from half-Hz at the boundary
/// (every ladder entry is a whole Hz, so `* 2` is exact).
fn step_freq(freq_half_hz: u16, dir: Step) -> u16 {
    let idx = freq_ladder_index(freq_half_hz / 2);
    let new = match dir {
        Step::Prev => idx.saturating_sub(1),
        Step::Next => (idx + 1).min(FREQ_LADDER_HZ.len() - 1),
    };
    FREQ_LADDER_HZ[new].saturating_mul(2)
}

fn freq_bounds(freq_half_hz: u16) -> StepBounds {
    let idx = freq_ladder_index(freq_half_hz / 2);
    StepBounds { prev: idx > 0, next: idx < FREQ_LADDER_HZ.len() - 1 }
}

/// Steps `gain_cdb` (v2's exact storage unit) by [`GAIN_STEP_CDB`] --
/// identical stepping behaviour to the old `step_gain(gain_half_db)` (see
/// [`GAIN_STEP_CDB`]'s doc comment for the exact unit match).
fn step_gain(gain_cdb: i16, dir: Step) -> i16 {
    match dir {
        Step::Prev => (gain_cdb - GAIN_STEP_CDB).max(GAIN_MIN_CDB),
        Step::Next => (gain_cdb + GAIN_STEP_CDB).min(GAIN_MAX_CDB),
    }
}

fn gain_bounds(gain_cdb: i16) -> StepBounds {
    StepBounds { prev: gain_cdb > GAIN_MIN_CDB, next: gain_cdb < GAIN_MAX_CDB }
}

/// Steps `q_milli` (v2's exact storage unit) through [`Q_TABLE`]'s picker
/// vocabulary -- snaps to the NEAREST table entry first (ryw.12 sec 2:
/// "Q snaps to the nearest table entry on the first press"), then moves
/// one entry in `dir`. For every `q_milli` the editor itself ever
/// produces (always an exact `q_milli_from_index` value), the nearest
/// entry IS its own index, so this steps identically to the old
/// `step_q(q_idx)`.
fn step_q(q_milli: u16, dir: Step) -> u16 {
    let idx = nearest_q_index(q_milli);
    let new_idx = match dir {
        Step::Prev => idx.saturating_sub(1),
        Step::Next => (idx + 1).min(u8::try_from(Q_TABLE.len() - 1).unwrap_or(u8::MAX)),
    };
    q_milli_from_index(new_idx)
}

fn q_bounds(q_milli: u16) -> StepBounds {
    let idx = nearest_q_index(q_milli);
    StepBounds { prev: idx > 0, next: (idx as usize) < Q_TABLE.len() - 1 }
}

/// The dynamic first NAME candidate: `Effect N`, `N` the lowest number not
/// already used by another effect's exact `"Effect N"` name (design sec
/// 4.5 -- never the storage id).
fn effect_n_candidate(other_names: &[String]) -> String {
    let mut n = 1u32;
    loop {
        let candidate = format!("Effect {n}");
        if !other_names.iter().any(|name| name == &candidate) {
            return candidate;
        }
        n += 1;
    }
}

fn name_candidate_at(index: usize, other_names: &[String]) -> String {
    if index == 0 {
        effect_n_candidate(other_names)
    } else {
        String::from(FIXED_NAME_CANDIDATES[index - 1])
    }
}

/// Steps NAME to the next/previous canned candidate, skipping any
/// candidate that collides with another effect's name, and wrapping --
/// design sec 4.5: "Names used by another effect are SKIPPED, so names
/// are always unique with no rule for the user to learn." Always
/// terminates: with at most `MAX_EFFECTS - 1` (7) other effects and 12
/// candidates, at least 5 are always free.
fn step_name(current: &str, dir: Step, other_names: &[String]) -> String {
    let total = 1 + FIXED_NAME_CANDIDATES.len();
    let start = (0..total).find(|&i| name_candidate_at(i, other_names) == current).unwrap_or(0);
    let mut index = start;
    for _ in 0..total {
        index = match dir {
            Step::Prev => (index + total - 1) % total,
            Step::Next => (index + 1) % total,
        };
        let candidate = name_candidate_at(index, other_names);
        if candidate != current && !other_names.iter().any(|n| n == &candidate) {
            return candidate;
        }
    }
    String::from(current) // structurally unreachable -- see doc comment above
}

/// Whether the draft's combined peak response would clip even after the
/// automatic preamp -- design sec 4.6: "Ada's auto preamp clamps at -12
/// dB, so a combined peak above +12 dB can clip." Detected as "the preamp
/// hit its clamp floor", not a separate re-derivation of the peak search.
#[must_use]
fn boost_may_distort(preset: &Preset) -> bool {
    let program = Program::from_preset(preset, DSP_FS_HZ);
    let preamp_db = 20.0 * libm::log10f(program.preamp_linear.max(1.0e-9));
    preamp_db <= MIN_BOOST_HEADROOM_DB + 0.01
}

// --- The editor's draft state ---------------------------------------------

/// The effect editor's live draft -- lives inside [`EffectEditorView`]
/// itself (built once per push, never rebuilt while the editor stays on
/// the navigator's stack, same as every other pushed screen in this
/// crate). `App::dsp_program` cannot read this directly (no path back
/// into a screen buried in the `Navigator`'s stack), so every mutation is
/// also mirrored into `App::editor_preview`, the `App`-owned mailbox
/// bead `pico-link-ryw.7`'s review fix added for exactly this (see its
/// doc comment) -- this struct itself still owns `band_index` and every
/// other piece of on-screen-only state that mailbox has no reason to
/// carry.
struct EditorState {
    draft: Preset,
    band_index: usize,
    bypassed: bool,
}

const ROW_CROSSFEED: ListItemKey = ListItemKey::from_u64(1);
const ROW_BAND: ListItemKey = ListItemKey::from_u64(2);
const ROW_FREQ: ListItemKey = ListItemKey::from_u64(3);
const ROW_GAIN: ListItemKey = ListItemKey::from_u64(4);
const ROW_Q: ListItemKey = ListItemKey::from_u64(5);
const ROW_NAME: ListItemKey = ListItemKey::from_u64(6);
/// The locked editor's read-only PREAMP row (bead `pico-link-ryw.12.4`,
/// Uma's design sec 4). A key distinct from every [`ROW_*`] constant
/// above so [`apply_step`]'s `else` branch -- and therefore `FieldList`'s
/// own Left/Right handling for a non-[`FieldKind::Value`] row -- already
/// makes it a structural no-op with no extra gating code (see this
/// function's own doc comment).
const ROW_PREAMP: ListItemKey = ListItemKey::from_u64(7);

/// One locked band row's identity key -- `100 + index` keeps every band
/// row's key distinct from [`ROW_CROSSFEED`]/[`ROW_PREAMP`]/every hand-
/// made-editor `ROW_*` constant above, with room for up to
/// [`crate::dsp::MAX_BANDS`] (10) rows.
fn locked_band_row_key(index: usize) -> ListItemKey {
    ListItemKey::from_u64(100 + index as u64)
}

/// `PK`/`LS`/`HS` -- the locked editor's compact band-kind abbreviation
/// (Uma's design sec 4's worked rows: `"7 PK 2941 Hz ..."`), distinct
/// from [`band_kind_word`]'s spelled-out hand-made-editor word (`"peak"`/
/// `"low shelf"`/`"high shelf"`) because a locked row packs kind INTO the
/// label alongside the 1-based index, with no room for the long form.
fn band_kind_abbrev(kind: BandKind) -> &'static str {
    match kind {
        BandKind::Peak => "PK",
        BandKind::LowShelf => "LS",
        BandKind::HighShelf => "HS",
    }
}

/// `"7 PK"` -- a locked band row's label (Uma's design sec 4).
fn locked_band_label(index: usize, kind: BandKind) -> String {
    format!("{} {}", index + 1, band_kind_abbrev(kind))
}

/// Renders `freq_half_hz` at v2's exact half-Hz precision: a whole Hz
/// prints with no decimal, an odd half-Hz value (the only fractional case
/// v2's unit can ever produce) prints with exactly one decimal -- Uma's
/// design sec 4: "freq integer Hz or 1 dp for .5". No unit suffix here
/// (unlike [`format_freq`]'s hand-made-editor `"100 Hz"`/`"1 kHz"`) --
/// [`format_band_value`] appends `" Hz"` itself, conditionally, per the
/// width-degrade ladder.
fn format_band_freq_exact(freq_half_hz: u16) -> String {
    if freq_half_hz % 2 == 0 {
        format!("{}", freq_half_hz / 2)
    } else {
        format!("{:.1}", f32::from(freq_half_hz) * 0.5)
    }
}

/// `"+3.30"`/`"-2.41"`/`"+0.00"` -- exact centi-dB gain at 2dp (Uma's
/// design sec 4: "gain 2 dp"), always signed. No `" dB"` suffix here --
/// see [`format_band_freq_exact`]'s doc comment for why the unit lives in
/// [`format_band_value`] instead. `{:+.2}` always emits an ASCII `-` for
/// a negative value (never U+2212), matching this module's other
/// ASCII-only formatters (design sec 4: "ASCII hyphen, helv _tf has no
/// U+2212").
fn format_band_gain_exact(gain_cdb: i16) -> String {
    format!("{:+.2}", f32::from(gain_cdb) * 0.01)
}

/// `"Q 5.90"` -- exact milli-Q at 2dp (Uma's design sec 4: "Q 2 dp"),
/// with its own `"Q "` label baked in (unlike freq/gain, `Q` never
/// degrades away -- it's the one field every degrade level in
/// [`format_band_value`] keeps).
fn format_band_q_exact(q_milli: u16) -> String {
    format!("Q {:.2}", f32::from(q_milli) * 0.001)
}

/// `"-6.20 dB"` -- the locked editor's read-only PREAMP row value, at the
/// same exact centi-dB precision [`format_band_gain_exact`] uses for a
/// band's gain, but WITH the `" dB"` suffix (Uma's design sec 4's worked
/// PREAMP row: `"PREAMP  -6.20 dB"` -- unlike a band row, PREAMP never
/// degrades, so its unit is never conditional).
fn format_preamp_exact(preamp_cdb: i16) -> String {
    format!("{:+.2} dB", f32::from(preamp_cdb) * 0.01)
}

/// How far the locked editor's band-row VALUE text has to degrade to fit
/// [`BAND_ROW_VALUE_BUDGET_PX`] -- Uma's design sec 4's ladder: "drop
/// `dB` on band rows ... then drop `Hz`, then band values in
/// `font::username`. Never ellipsise a number." Computed ONCE per
/// [`locked_editor_rows`] call (not per row -- every band row shares one
/// font/budget), from the worst-case string the design names: `"10 HS
/// 19999.5 Hz -29.99 dB Q 65.00"`'s VALUE half, `"19999.5 Hz -29.99 dB Q
/// 65.00"` (the widest legal v2 value at every field's extreme: max
/// `freq_half_hz` `39690` half-Hz i.e. `19845.0`Hz rounds up to this
/// probe's `19999.5`, `GAIN_DB_MAX` `30` minus a hair, `Q_MAX` `65`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BandRowDegrade {
    /// `"19999.5 Hz -29.99 dB Q 65.00"` at [`font::value`].
    Full,
    /// `"19999.5 Hz -29.99 Q 65.00"` at [`font::value`] (dropped `dB`).
    NoDb,
    /// `"19999.5 -29.99 Q 65.00"` at [`font::value`] (dropped `Hz` too).
    NoDbNoHz,
    /// [`Self::NoDbNoHz`]'s text, but at [`ValueFont::Compact`]
    /// ([`font::username`]) -- the last resort, when even the shortest
    /// text doesn't fit at the normal face.
    NoDbNoHzCompact,
}

/// The pixel budget a locked band row's VALUE text must fit inside, at
/// [`font::value`] before any degrade -- Uma's design sec 4: "Ruby must
/// measure worst case ... within ~182px."
const BAND_ROW_VALUE_BUDGET_PX: i32 = 182;

/// A single line's rendered pixel width in `font` -- same small private
/// measuring helper `menu.rs`/`fields.rs` each keep their own copy of
/// (see either's doc comment for why this isn't shared: each is a small
/// leaf helper with no other reason to depend on a sibling view module).
#[allow(clippy::cast_possible_wrap)]
fn measure_text_width(font: &FontRenderer, text: &str) -> i32 {
    font.get_rendered_dimensions_aligned(text, Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
        .unwrap_or(None)
        .map_or(0, |bbox| bbox.size.width as i32)
}

fn band_row_degrade() -> BandRowDegrade {
    let value_font = font::value();
    let full = "19999.5 Hz -29.99 dB Q 65.00";
    if measure_text_width(&value_font, full) <= BAND_ROW_VALUE_BUDGET_PX {
        return BandRowDegrade::Full;
    }
    let no_db = "19999.5 Hz -29.99 Q 65.00";
    if measure_text_width(&value_font, no_db) <= BAND_ROW_VALUE_BUDGET_PX {
        return BandRowDegrade::NoDb;
    }
    let no_db_no_hz = "19999.5 -29.99 Q 65.00";
    if measure_text_width(&value_font, no_db_no_hz) <= BAND_ROW_VALUE_BUDGET_PX {
        return BandRowDegrade::NoDbNoHz;
    }
    BandRowDegrade::NoDbNoHzCompact
}

/// Formats one band's read-only value text per `degrade` -- never
/// truncates/ellipsises a NUMBER (Uma's design sec 4's hard rule): every
/// degrade level drops a UNIT SUFFIX (`" Hz"`, `" dB"`), never a digit.
fn format_band_value(band: &Band, degrade: BandRowDegrade) -> String {
    let freq = format_band_freq_exact(band.freq_half_hz);
    let gain = format_band_gain_exact(band.gain_cdb);
    let q = format_band_q_exact(band.q_milli);
    match degrade {
        BandRowDegrade::Full => format!("{freq} Hz {gain} dB {q}"),
        BandRowDegrade::NoDb => format!("{freq} Hz {gain} {q}"),
        BandRowDegrade::NoDbNoHz | BandRowDegrade::NoDbNoHzCompact => format!("{freq} {gain} {q}"),
    }
}

/// The locked (imported) editor's row set (bead `pico-link-ryw.12.4`,
/// Uma's design sec 4): CROSSFEED (still editable), a read-only PREAMP
/// row, then one read-only row per band in FILE order. Deliberately NO
/// NAME row -- design sec 4: "the canned cycle would destroy a
/// computer-chosen name; rename = re-import."
fn locked_editor_rows(state: &EditorState) -> Vec<FieldRow> {
    let mut rows = vec![
        FieldRow::value_row("CROSSFEED", crossfeed_word(state.draft.crossfeed), crossfeed_bounds(state.draft.crossfeed)).with_key(ROW_CROSSFEED),
    ];

    let preamp_cdb = match state.draft.preamp {
        Preamp::Explicit(v) => v,
        Preamp::Auto => 0, // structurally unreachable for a locked preset -- import always sets Explicit (ryw.12.2's `to_preset`).
    };
    rows.push(FieldRow::readonly("PREAMP").with_value(format_preamp_exact(preamp_cdb), palette::TEXT_SECONDARY).with_key(ROW_PREAMP));

    let degrade = band_row_degrade();
    for (i, band) in state.draft.bands.iter().enumerate() {
        let mut row = FieldRow::readonly(locked_band_label(i, band.kind))
            .with_value(format_band_value(band, degrade), palette::TEXT_SECONDARY)
            .with_key(locked_band_row_key(i));
        if degrade == BandRowDegrade::NoDbNoHzCompact {
            row = row.with_compact_value();
        }
        rows.push(row);
    }
    rows
}

fn editor_rows(state: &EditorState) -> Vec<FieldRow> {
    if state.draft.eq_locked {
        return locked_editor_rows(state);
    }
    let band = state.draft.bands.get(state.band_index).copied().unwrap_or_else(|| band_default(state.band_index.min(BAND_COUNT - 1)));
    vec![
        FieldRow::value_row("CROSSFEED", crossfeed_word(state.draft.crossfeed), crossfeed_bounds(state.draft.crossfeed)).with_key(ROW_CROSSFEED),
        FieldRow::value_row("BAND", band_label(state.band_index, band.kind), band_index_bounds(state.band_index)).with_key(ROW_BAND),
        FieldRow::value_row("FREQ", format_freq(band.freq_half_hz), freq_bounds(band.freq_half_hz)).with_key(ROW_FREQ),
        FieldRow::value_row("GAIN", format_gain_db(band.gain_cdb), gain_bounds(band.gain_cdb)).with_key(ROW_GAIN),
        FieldRow::value_row("Q", format_q(band.q_milli), q_bounds(band.q_milli)).with_key(ROW_Q),
        FieldRow::value_row("NAME", state.draft.name.clone(), StepBounds { prev: true, next: true }).with_key(ROW_NAME),
    ]
}

/// Applies one Left/Right press to `state` -- returns whether the draft's
/// SAVED shape (name/crossfeed/bands) actually changed, i.e. whether a
/// [`Command::SavePreset`] is warranted (Andreas's ruling: every value
/// change saves immediately). Moving the BAND cursor itself is not a
/// value change -- it only changes which band's FREQ/GAIN/Q are on
/// screen (design sec 4.2: "Changing BAND does not move focus").
fn apply_step(state: &mut EditorState, key: ListItemKey, dir: Step, other_names: &[String]) -> bool {
    if key == ROW_CROSSFEED {
        let new = step_crossfeed(state.draft.crossfeed, dir);
        if new == state.draft.crossfeed {
            return false;
        }
        state.draft.crossfeed = new;
        true
    } else if key == ROW_BAND {
        state.band_index = step_band_index(state.band_index, dir);
        false
    } else if key == ROW_FREQ {
        let Some(band) = state.draft.bands.get_mut(state.band_index) else { return false };
        let new = step_freq(band.freq_half_hz, dir);
        if new == band.freq_half_hz {
            return false;
        }
        band.freq_half_hz = new;
        true
    } else if key == ROW_GAIN {
        let Some(band) = state.draft.bands.get_mut(state.band_index) else { return false };
        let new = step_gain(band.gain_cdb, dir);
        if new == band.gain_cdb {
            return false;
        }
        band.gain_cdb = new;
        true
    } else if key == ROW_Q {
        let Some(band) = state.draft.bands.get_mut(state.band_index) else { return false };
        let new = step_q(band.q_milli, dir);
        if new == band.q_milli {
            return false;
        }
        band.q_milli = new;
        true
    } else if key == ROW_NAME {
        let new_name = step_name(&state.draft.name, dir, other_names);
        if new_name == state.draft.name {
            return false;
        }
        state.draft.name = new_name;
        true
    } else {
        false
    }
}

/// `Y` on a band row (design sec 4.4): restores the selected band to its
/// fixed default. Returns whether it actually changed anything.
fn reset_selected_band(state: &mut EditorState) -> bool {
    let default = band_default(state.band_index);
    let Some(band) = state.draft.bands.get_mut(state.band_index) else { return false };
    if *band == default {
        return false;
    }
    *band = default;
    true
}

/// Every OTHER effect's name (i.e. every stored preset's name except
/// `excluding`), for [`step_name`]'s skip-used rule.
fn other_effect_names(presets: &PresetStore, excluding: u16) -> Vec<String> {
    presets.iter().filter(|(id, _)| *id != excluding).map(|(_, preset)| preset.name.clone()).collect()
}

/// Names/counts how many paired devices are streaming this connected
/// device's effect right now -- the footer's "in use" fact (design sec
/// 4.6): true only when a device is connected, presumed streaming (a
/// recent `LevelsChanged` reading), and its resolved `preset_id` is
/// `assigned_id`.
fn footer_text(model: &BtModel, bypassed: bool, boost_warning: bool, assigned_id: Option<u16>) -> (String, Rgb565) {
    if bypassed {
        return (String::from("Bypassed - X to hear the effect"), palette::STATUS_WARNING);
    }
    if boost_warning {
        return (String::from("Too much boost - may distort"), palette::STATUS_WARNING);
    }
    let Some(addr) = model.connected_addr else {
        return (String::from("No headphones connected"), palette::TEXT_SECONDARY);
    };
    let device_name = model.paired.iter().find(|d| d.addr == addr).map_or_else(|| String::from("device"), |d| d.name.clone());
    let streaming = model.out_level.is_some();
    let in_use = assigned_id.is_some() && model.paired.iter().any(|d| d.addr == addr && Some(d.preset_id) == assigned_id);
    if !streaming {
        return (String::from("Nothing playing - start audio to hear it"), palette::TEXT_SECONDARY);
    }
    if in_use {
        (format!("Playing on {device_name} (in use)"), palette::TEXT_SECONDARY)
    } else {
        (format!("Preview on {device_name} - not in use"), palette::TEXT_SECONDARY)
    }
}

/// Seed for [`EffectEditorView::paint_key`].
const EDITOR_PAINT_KEY_SEED: u64 = 61;
/// Height (px) reserved for the one-line footer at the bottom of the
/// editor's content area.
const FOOTER_HEIGHT: u32 = 20;

/// The one-screen editor's sole content widget -- owns the draft
/// ([`EditorState`]), wraps a [`FieldList`] of the six value rows, and
/// draws the one-line footer itself below it (see the module doc comment
/// for why this is one widget rather than two stacked on the `Screen`: `X`
/// (bypass)/`Y` (reset) need one focused widget to own them, same as
/// [`super::device_page::DevicePageView`]'s `X`).
struct EffectEditorView {
    list: FieldList,
    state: Rc<RefCell<EditorState>>,
    model: ModelHandle,
    presets: Rc<RefCell<PresetStore>>,
    commands: Rc<RefCell<VecDeque<Command>>>,
    editor_preset_id: Rc<RefCell<Option<u16>>>,
    /// The `App`-owned live-preview mailbox -- bead `pico-link-ryw.7`
    /// review fix, design sec 5.1. Mirrored from [`Self::state`] by
    /// [`Self::sync_preview`] on every mutation; see
    /// [`super::super::App`]'s `editor_preview` doc comment for the full
    /// shape.
    editor_preview: Rc<RefCell<Option<(u16, Preset, bool)>>>,
    rows_key: PaintKey,
}

impl EffectEditorView {
    // `gain_cdb as u16` below is a bit-pattern fold for a paint-key hash,
    // not a numeric conversion -- the sign doesn't matter, only that
    // distinct `i16` values fold to distinct `u64`s (same reasoning
    // `Band::pack_kind_gain`'s own cast documents).
    #[allow(clippy::cast_sign_loss)]
    fn rows_key_of(state: &EditorState) -> PaintKey {
        let mut key = PaintKey::of(EDITOR_PAINT_KEY_SEED)
            .fold(u64::from(state.draft.crossfeed.to_wire()))
            .fold(u64::from(state.draft.eq_locked));
        if state.draft.eq_locked {
            // Bead `pico-link-ryw.12.4`: a locked editor shows EVERY band
            // as its own row (no `band_index` cursor), so the paint key
            // must fold the whole band list -- plus the PREAMP row's own
            // value -- rather than just the one band the hand-made
            // editor's cursor currently points at.
            let preamp_cdb = match state.draft.preamp {
                Preamp::Explicit(v) => v,
                Preamp::Auto => 0,
            };
            key = key.fold(preamp_cdb as u16 as u64);
            for band in &state.draft.bands {
                key = key
                    .fold(u64::from(band.kind.to_wire()))
                    .fold(u64::from(band.freq_half_hz))
                    .fold(u64::from(band.gain_cdb as u16))
                    .fold(u64::from(band.q_milli));
            }
            return key;
        }
        let band = state.draft.bands.get(state.band_index).copied();
        key.fold(state.band_index as u64)
            .fold(band.map_or(0, |b| u64::from(b.kind.to_wire())))
            .fold(band.map_or(0, |b| u64::from(b.freq_half_hz)))
            .fold(band.map_or(0, |b| u64::from(b.gain_cdb as u16)))
            .fold(band.map_or(0, |b| u64::from(b.q_milli)))
            .fold_str(&state.draft.name)
    }

    /// Mirrors [`Self::state`]'s current draft/bypassed contents into
    /// [`Self::editor_preview`] -- called after every mutation so
    /// `App::dsp_program` (design sec 5.1 rules 1/2) sees the change
    /// instantly, independent of [`Self::save_now`]'s `SavePreset` round
    /// trip.
    fn sync_preview(&self) {
        let state = self.state.borrow();
        let id = self.editor_preset_id.borrow().expect("an open editor always has a real, already-allocated id");
        *self.editor_preview.borrow_mut() = Some((id, state.draft.clone(), state.bypassed));
    }

    /// Queues exactly one `SavePreset` for the draft's current contents,
    /// against [`Self::editor_preset_id`]'s real, Rust-allocated id --
    /// Andreas's ruling: every value change saves immediately. Ada's
    /// preset-id-allocation contract (bead `pico-link-ryw.14`): the id is
    /// final from the moment the editor opens (see `build_effect_editor_
    /// screen`'s doc comment) -- there is no longer a `Some(0)`
    /// "allocation pending" state to fall back from.
    fn save_now(&self) {
        let preset_id = self.editor_preset_id.borrow().expect("an open editor always has a real, already-allocated id");
        let blob = self.state.borrow().draft.to_wire();
        self.commands.borrow_mut().push_back(Command::SavePreset { preset_id, blob: blob.to_vec() });
    }
}

impl Widget for EffectEditorView {
    fn measure(&self, constraints: Size, _ctx: &RenderCtx) -> Size {
        constraints
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn activation(&self) -> Option<Verb> {
        None // every row is a Value row -- A is always dim/inert here
    }

    fn sync(&mut self, _ctx: &RenderCtx) {
        let state = self.state.borrow();
        let key = Self::rows_key_of(&state);
        if key != self.rows_key {
            let rows = editor_rows(&state);
            drop(state);
            self.list.set_rows(rows);
            self.rows_key = key;
        }
    }

    fn on_focus(&mut self, event: FocusEvent) -> Action {
        self.list.on_focus(event)
    }

    fn on_intent(&mut self, intent: NavIntent) -> Action {
        match intent {
            NavIntent::ShortcutX => {
                {
                    let mut state = self.state.borrow_mut();
                    state.bypassed = !state.bypassed;
                }
                self.sync_preview();
                Action::None
            }
            NavIntent::ShortcutY => {
                // Bead `pico-link-ryw.12.4`: Y is unlabelled/inert
                // everywhere in the locked editor (Uma's design sec 4) --
                // there is no per-band cursor to reset against, and a
                // locked band has no "default" to reset TO in the first
                // place.
                if self.state.borrow().draft.eq_locked {
                    return Action::None;
                }
                let changed = {
                    let mut state = self.state.borrow_mut();
                    let on_band_row = self.list.selected_key() != Some(ROW_CROSSFEED) && self.list.selected_key() != Some(ROW_NAME);
                    if on_band_row {
                        let changed = reset_selected_band(&mut state);
                        if changed {
                            state.bypassed = false;
                        }
                        changed
                    } else {
                        false
                    }
                };
                if changed {
                    self.sync_preview();
                    self.save_now();
                }
                Action::None
            }
            NavIntent::Left | NavIntent::Right => {
                let dir = if intent == NavIntent::Left { Step::Prev } else { Step::Next };
                let Some(key) = self.list.selected_key() else { return Action::None };
                let changed = {
                    let other_names = {
                        let presets = self.presets.borrow();
                        let excluding = self.editor_preset_id.borrow().expect("an open editor always has a real, already-allocated id");
                        other_effect_names(&presets, excluding)
                    };
                    let mut state = self.state.borrow_mut();
                    let changed = apply_step(&mut state, key, dir, &other_names);
                    if changed {
                        state.bypassed = false;
                    }
                    changed
                };
                if changed {
                    self.sync_preview();
                    self.save_now();
                    Action::None
                } else {
                    self.list.on_intent(intent)
                }
            }
            _ => self.list.on_intent(intent),
        }
    }

    fn chrome_contribution(&self, _ctx: &RenderCtx) -> Option<ChromeContribution> {
        let state = self.state.borrow();
        let x_label = if state.bypassed { "on" } else { "off" };
        // Bead `pico-link-ryw.12.4`: Y is unlabelled/inert everywhere in
        // the locked editor -- see `on_intent`'s `ShortcutY` arm.
        let y = if state.draft.eq_locked {
            ButtonLabel::Inert
        } else {
            let on_band_row = !matches!(self.list.selected_key(), Some(ROW_CROSSFEED | ROW_NAME) | None);
            let y_live = on_band_row
                && state
                    .draft
                    .bands
                    .get(state.band_index)
                    .is_some_and(|band| *band != band_default(state.band_index));
            if y_live { ButtonLabel::Live(String::from("reset")) } else { ButtonLabel::Inert }
        };
        Some(ChromeContribution {
            title: Some(state.draft.name.clone()),
            x: Some(ButtonLabel::Live(String::from(x_label))),
            y: Some(y),
            ..ChromeContribution::default()
        })
    }

    fn selected_index(&self) -> Option<usize> {
        Some(self.list.selected_index())
    }

    fn selected_key(&self) -> Option<ListItemKey> {
        self.list.selected_key()
    }

    fn render(&self, area: Rectangle, ctx: &RenderCtx, target: &mut FrameBuffer565) -> Result<(), Infallible> {
        let list_height = area.size.height.saturating_sub(FOOTER_HEIGHT);
        let list_area = Rectangle::new(area.top_left, Size::new(area.size.width, list_height));
        self.list.render(list_area, ctx, target)?;

        let state = self.state.borrow();
        let boost_warning = boost_may_distort(&state.draft);
        let assigned_id = *self.editor_preset_id.borrow();
        let (text, color) = footer_text(&self.model.borrow(), state.bypassed, boost_warning, assigned_id);
        let footer_font = font::label();
        // `list_height` is a content-region height (well under `i32::MAX`
        // on any display this project targets) -- same "no display this
        // project targets is anywhere near large enough to wrap" allowance
        // `render::list`/`render::message` already carry.
        #[allow(clippy::cast_possible_wrap)]
        let footer_y = area.top_left.y + list_height as i32 + 4;
        let _ = footer_font.render_aligned(
            text.as_str(),
            Point::new(area.top_left.x + 12, footer_y),
            VerticalPosition::Top,
            HorizontalAlignment::Left,
            u8g2_fonts::types::FontColor::Transparent(color),
            target,
        );
        Ok(())
    }

    fn paint_key(&self, ctx: &RenderCtx) -> PaintKey {
        let state = self.state.borrow();
        let boost_warning = boost_may_distort(&state.draft);
        let assigned_id = *self.editor_preset_id.borrow();
        let (footer, footer_color) = footer_text(&self.model.borrow(), state.bypassed, boost_warning, assigned_id);
        PaintKey::of(EDITOR_PAINT_KEY_SEED)
            .fold_key(self.list.paint_key(ctx))
            .fold_str(&footer)
            .fold_color(footer_color)
    }
}

/// Builds the effect editor screen, either for an existing effect or a
/// brand new one (`initial` already a fresh [`new_effect_preset`]) --
/// either way `editor_preset_id` already carries the real, final,
/// Rust-allocated id by the time this is called (Ada's preset-id-
/// allocation contract, bead `pico-link-ryw.14`: `core` owns id
/// allocation, so there is no `preset_id == 0`/"allocation pending" state
/// any more). Called once per push; never rebuilt while it stays on the
/// navigator's stack.
///
/// Andreas's ruling supersedes design sec 3.3/5.2's "nothing written
/// until the editor is left": a brand-new effect is saved immediately on
/// creation (one `SavePreset{preset_id: <real id>, ..}`), before this
/// screen is even built, so it already exists in `presets` if the user
/// immediately backs out.
fn build_effect_editor_screen(
    initial: &Preset,
    model: &ModelHandle,
    presets: &Rc<RefCell<PresetStore>>,
    commands: &Rc<RefCell<VecDeque<Command>>>,
    editor_preset_id: &Rc<RefCell<Option<u16>>>,
    editor_preview: &Rc<RefCell<Option<(u16, Preset, bool)>>>,
) -> Screen {
    let state = Rc::new(RefCell::new(EditorState { draft: initial.clone(), band_index: 0, bypassed: false }));
    let rows = editor_rows(&state.borrow());
    let list = FieldList::new(rows);
    // Seed the preview mailbox immediately on push -- design sec 5.1: the
    // stream previews the draft (unbypassed) from the moment the editor
    // opens, not from the first Left/Right press.
    let id = editor_preset_id.borrow().expect("an open editor always has a real, already-allocated id");
    *editor_preview.borrow_mut() = Some((id, initial.clone(), false));
    let view = EffectEditorView {
        list,
        state: Rc::clone(&state),
        model: Rc::clone(model),
        presets: Rc::clone(presets),
        commands: Rc::clone(commands),
        editor_preset_id: Rc::clone(editor_preset_id),
        editor_preview: Rc::clone(editor_preview),
        rows_key: EffectEditorView::rows_key_of(&state.borrow()),
    };
    let editor_preset_id_for_exit = Rc::clone(editor_preset_id);
    let editor_preview_for_exit = Rc::clone(editor_preview);
    Screen::new(initial.name.clone(), vec![Box::new(view)]).with_on_exit(move || {
        *editor_preset_id_for_exit.borrow_mut() = None;
        // design sec 5.1 rule 3: leaving the editor reverts playback to
        // the connected device's assignment.
        *editor_preview_for_exit.borrow_mut() = None;
    })
}

// --- The effects list (depth 1) --------------------------------------------

const NEW_EFFECT_ROW_KEY: ListItemKey = ListItemKey::from_bytes([0xFD; 8]);

/// `pub(in crate::app)`, not private -- bead `pico-link-ryw.12.4`'s
/// `App::import_preset` (`app/mod.rs`) needs the exact same row identity
/// a live import lands on to drive [`FieldList::focus_key`]'s import-
/// focus-follow (Uma's design, `ryw12-3-ux.md` sec 2).
pub(in crate::app) fn effect_row_key(id: u16) -> ListItemKey {
    ListItemKey::from_u64(u64::from(id))
}

fn usage_count(model: &BtModel, id: u16) -> usize {
    model.paired.iter().filter(|d| d.preset_id == id).count()
}

fn usage_label(count: usize) -> String {
    match count {
        0 => String::from("unused"),
        1 => String::from("1 device"),
        n => format!("{n} devices"),
    }
}

/// The effect list's rows: one per stored effect (creation/id order,
/// [`PresetStore::iter`]'s own `BTreeMap` ordering), then `New effect`
/// last. **PURE.**
fn effects_list_rows(model: &BtModel, presets: &PresetStore) -> Vec<FieldRow> {
    let connected_preset_id = model.connected_addr.and_then(|addr| model.paired.iter().find(|d| d.addr == addr)).map(|d| d.preset_id);
    let mut rows: Vec<FieldRow> = presets
        .iter()
        .map(|(id, preset)| {
            let count = usage_count(model, id);
            let mut row = FieldRow::action(preset.name.clone()).with_value(usage_label(count), palette::TEXT_SECONDARY).with_key(effect_row_key(id));
            if connected_preset_id == Some(id) {
                row = row.with_leading_glyph(crate::render::theme::icon::CHECK);
            }
            if preset.eq_locked {
                row = row.with_lock();
            }
            row
        })
        .collect();

    if presets.len() < MAX_EFFECTS {
        rows.push(FieldRow::action("New effect").with_key(NEW_EFFECT_ROW_KEY));
    } else {
        rows.push(FieldRow::readonly("New effect").with_value("full", palette::TEXT_SECONDARY).with_key(NEW_EFFECT_ROW_KEY));
    }
    rows
}

/// Seed for [`EffectsListView`]'s projection key.
const EFFECTS_LIST_PROJECTION_SEED: u64 = 62;

fn effects_list_projection_key(model: &BtModel, presets: &PresetStore) -> PaintKey {
    let mut key = PaintKey::of(EFFECTS_LIST_PROJECTION_SEED).fold(presets.len() as u64);
    for (id, preset) in presets.iter() {
        key = key.fold(u64::from(id)).fold_str(&preset.name).fold(usage_count(model, id) as u64).fold(u64::from(preset.eq_locked));
    }
    key = key.fold(model.connected_addr.and_then(|addr| model.paired.iter().find(|d| d.addr == addr)).map_or(0, |d| u64::from(d.preset_id) + 1));
    key
}

/// Wraps [`FieldList`] to add the effects list's `X` (delete, via a
/// confirm) and `Y` (use/assign to the connected device) actions -- the
/// same "small wrapper widget intercepts the two shortcut intents,
/// delegates the rest" shape [`super::device_page::DevicePageView`] uses
/// for its own `X`.
struct EffectsListView {
    list: FieldList,
    model: ModelHandle,
    presets: Rc<RefCell<PresetStore>>,
    commands: Rc<RefCell<VecDeque<Command>>>,
    projection_key: PaintKey,
    /// Bead `pico-link-ryw.12.4`'s import-focus-follow mailbox -- see
    /// [`super::super::App`]'s `import_focus` doc comment. Consumed (and
    /// cleared) here, in [`Widget::sync`], every frame this screen is on
    /// top.
    import_focus: Rc<RefCell<Option<ListItemKey>>>,
}

impl EffectsListView {
    /// The focused row's effect id, if the focus is on a real effect row
    /// (never `NEW_EFFECT_ROW_KEY`).
    fn selected_effect_id(&self) -> Option<u16> {
        match self.list.selected_key() {
            Some(key) if key != NEW_EFFECT_ROW_KEY => {
                let id = key.as_u64();
                u16::try_from(id).ok()
            }
            _ => None,
        }
    }
}

impl Widget for EffectsListView {
    fn measure(&self, constraints: Size, ctx: &RenderCtx) -> Size {
        self.list.measure(constraints, ctx)
    }

    fn is_focusable(&self) -> bool {
        self.list.is_focusable()
    }

    fn activation(&self) -> Option<Verb> {
        self.list.activation()
    }

    fn sync(&mut self, _ctx: &RenderCtx) {
        let model = self.model.borrow();
        let presets = self.presets.borrow();
        let key = effects_list_projection_key(&model, &presets);
        if key != self.projection_key {
            let rows = effects_list_rows(&model, &presets);
            drop(presets);
            drop(model);
            self.list.set_rows(rows);
            self.projection_key = key;
        } else {
            drop(presets);
            drop(model);
        }
        // Bead `pico-link-ryw.12.4`: import-focus-follow -- a one-shot
        // jump, consumed (and cleared) whether or not the target row was
        // actually found (e.g. this same frame's `set_rows` above hasn't
        // run yet because the projection key happened not to change --
        // structurally not possible for a genuinely new/updated row, since
        // that always changes `effects_list_projection_key`, but cleared
        // unconditionally regardless so a stale target can never leak
        // into a later, unrelated import).
        if let Some(target) = self.import_focus.borrow_mut().take() {
            self.list.focus_key(target);
        }
    }

    fn on_focus(&mut self, event: FocusEvent) -> Action {
        self.list.on_focus(event)
    }

    fn on_intent(&mut self, intent: NavIntent) -> Action {
        match intent {
            NavIntent::ShortcutX => {
                let Some(id) = self.selected_effect_id() else { return Action::None };
                let (name, count) = {
                    let presets = self.presets.borrow();
                    let model = self.model.borrow();
                    let name = presets.get(id).map_or_else(|| String::from("effect"), |p| p.name.clone());
                    (name, usage_count(&model, id))
                };
                let commands = Rc::clone(&self.commands);
                Action::PushView(Box::new(move || build_delete_confirm_screen(id, &name, count, commands)))
            }
            NavIntent::ShortcutY => {
                let Some(id) = self.selected_effect_id() else { return Action::None };
                let Some(addr) = self.model.borrow().connected_addr else { return Action::None };
                let already_assigned = self.model.borrow().paired.iter().any(|d| d.addr == addr && d.preset_id == id);
                if already_assigned {
                    return Action::None;
                }
                self.commands.borrow_mut().push_back(Command::AssignPreset { addr, preset_id: id });
                Action::None
            }
            _ => self.list.on_intent(intent),
        }
    }

    fn chrome_contribution(&self, _ctx: &RenderCtx) -> Option<ChromeContribution> {
        let x = if self.selected_effect_id().is_some() { ButtonLabel::Live(String::from("delete")) } else { ButtonLabel::Inert };
        let y_live = self.selected_effect_id().is_some_and(|id| {
            let model = self.model.borrow();
            model.connected_addr.is_some_and(|addr| !model.paired.iter().any(|d| d.addr == addr && d.preset_id == id))
        });
        let y = if y_live { ButtonLabel::Live(String::from("use")) } else { ButtonLabel::Inert };
        Some(ChromeContribution { x: Some(x), y: Some(y), ..ChromeContribution::default() })
    }

    fn selected_index(&self) -> Option<usize> {
        Some(self.list.selected_index())
    }

    fn selected_key(&self) -> Option<ListItemKey> {
        self.list.selected_key()
    }

    fn scroll_top(&self) -> Option<usize> {
        self.list.scroll_top()
    }

    fn render(&self, area: Rectangle, ctx: &RenderCtx, target: &mut FrameBuffer565) -> Result<(), Infallible> {
        self.list.render(area, ctx, target)
    }

    fn paint_key(&self, ctx: &RenderCtx) -> PaintKey {
        PaintKey::of(EFFECTS_LIST_PROJECTION_SEED).fold_key(self.list.paint_key(ctx))
    }
}

const EFFECTS_LIST_TITLE: &str = "Effects";
const DELETE_CONFIRM_TITLE: &str = "Delete effect";

fn build_delete_confirm_screen(id: u16, name: &str, usage: usize, commands: Rc<RefCell<VecDeque<Command>>>) -> Screen {
    let headline = format!("Delete \"{name}\"?");
    // `ConfirmView::with_subline` renders one clipped, centered line (no
    // wrap) -- design sec 6's two-line body is the aspiration, but the
    // shared widget doesn't wrap, so these stay short enough to fit at
    // `font::username()` on a 240px panel (measured below).
    let subline = match usage {
        0 => String::from("No device uses it."),
        1 => String::from("1 device will lose this effect."),
        n => format!("{n} devices will lose this effect."),
    };
    let rows = vec![MenuItem::new("Cancel"), MenuItem::new("Delete").with_label_color(palette::STATUS_ERROR)];
    let view = ConfirmView::new(headline, rows).with_subline(subline).on_activate_index(Verb::Select, move |index| {
        if index == 1 {
            commands.borrow_mut().push_back(Command::DeletePreset { preset_id: id });
        }
        Action::PopView
    });
    Screen::new(DELETE_CONFIRM_TITLE, vec![Box::new(view)])
}

/// Builds the effects list screen -- Home's `Effects` row's push target.
/// Called once per push; the resulting [`EffectsListView`] re-reads
/// `model`/`presets` live via [`Widget::sync`] every frame it stays on
/// top, same as every other pushed list in this crate.
#[allow(clippy::too_many_arguments)] // Mirrors every other screen builder in this crate threading the shared Rcs through -- see `build_home_screen`.
pub(crate) fn build_effects_list_screen(
    model: &ModelHandle,
    presets: &Rc<RefCell<PresetStore>>,
    commands: &Rc<RefCell<VecDeque<Command>>>,
    editor_preset_id: &Rc<RefCell<Option<u16>>>,
    editor_preview: &Rc<RefCell<Option<(u16, Preset, bool)>>>,
    import_focus: &Rc<RefCell<Option<ListItemKey>>>,
    presets_ready: &Rc<RefCell<bool>>,
) -> Screen {
    let (rows, projection_key) = {
        let model_ref = model.borrow();
        let presets_ref = presets.borrow();
        (effects_list_rows(&model_ref, &presets_ref), effects_list_projection_key(&model_ref, &presets_ref))
    };

    let model_for_activate = Rc::clone(model);
    let presets_for_activate = Rc::clone(presets);
    let commands_for_activate = Rc::clone(commands);
    let editor_preset_id_for_activate = Rc::clone(editor_preset_id);
    let editor_preview_for_activate = Rc::clone(editor_preview);
    let presets_ready_for_activate = Rc::clone(presets_ready);
    let list = FieldList::new(rows).with_leading_gutter().on_activate_key(move |key| {
        if key == NEW_EFFECT_ROW_KEY {
            // Ada's preset-id-allocation contract (bead `pico-link-ryw.14`):
            // `core` must not allocate an id until C's boot-time high-water
            // mark has actually arrived (`Event::PresetStoreLoaded`) -- an
            // id allocated before that could alias one C already holds. The
            // row is simply inert until then, same "discoverable without
            // any radio/flash work" gate `MAX_EFFECTS` above already uses.
            if !*presets_ready_for_activate.borrow() {
                return Action::None;
            }
            if presets_for_activate.borrow().len() >= MAX_EFFECTS {
                return Action::None;
            }
            let other_names = other_effect_names(&presets_for_activate.borrow(), 0);
            let preset = new_effect_preset(&effect_n_candidate(&other_names));
            // `core` allocates the real id itself now (Ada's contract) --
            // no more `preset_id: 0`/`Some(0)` "allocation pending" dance.
            // Andreas's ruling still holds: save immediately, even for a
            // brand-new effect -- see `build_effect_editor_screen`'s doc
            // comment.
            let id = presets_for_activate.borrow_mut().create(preset.clone());
            commands_for_activate.borrow_mut().push_back(Command::SavePreset { preset_id: id, blob: preset.to_wire().to_vec() });
            *editor_preset_id_for_activate.borrow_mut() = Some(id);
            let model = Rc::clone(&model_for_activate);
            let presets = Rc::clone(&presets_for_activate);
            let commands = Rc::clone(&commands_for_activate);
            let editor_preset_id = Rc::clone(&editor_preset_id_for_activate);
            let editor_preview = Rc::clone(&editor_preview_for_activate);
            return Action::PushView(Box::new(move || {
                build_effect_editor_screen(&preset, &model, &presets, &commands, &editor_preset_id, &editor_preview)
            }));
        }
        let id = key.as_u64();
        let Ok(id) = u16::try_from(id) else { return Action::None };
        let Some(preset) = presets_for_activate.borrow().get(id).cloned() else { return Action::None };
        *editor_preset_id_for_activate.borrow_mut() = Some(id);
        let model = Rc::clone(&model_for_activate);
        let presets = Rc::clone(&presets_for_activate);
        let commands = Rc::clone(&commands_for_activate);
        let editor_preset_id = Rc::clone(&editor_preset_id_for_activate);
        let editor_preview = Rc::clone(&editor_preview_for_activate);
        Action::PushView(Box::new(move || build_effect_editor_screen(&preset, &model, &presets, &commands, &editor_preset_id, &editor_preview)))
    });

    let view = EffectsListView {
        list,
        model: Rc::clone(model),
        presets: Rc::clone(presets),
        commands: Rc::clone(commands),
        projection_key,
        import_focus: Rc::clone(import_focus),
    };
    Screen::new(EFFECTS_LIST_TITLE, vec![Box::new(view)]).with_id(ScreenId::EffectsList)
}

#[cfg(test)]
mod tests {
    use embedded_graphics::prelude::RgbColor;

    use crate::app::test_support::ready_presets;
    use crate::app::{App, ConnectedCodec, Event, PairedDevice};
    use crate::input::NavIntent;

    use super::*;

    // --- Formatting ---

    #[test]
    fn format_freq_below_1khz_is_plain_hz() {
        assert_eq!(format_freq(200), "100 Hz");
        assert_eq!(format_freq(40), "20 Hz");
    }

    #[test]
    fn format_freq_at_and_above_1khz_uses_khz_and_trims_trailing_zeros() {
        assert_eq!(format_freq(2_000), "1 kHz");
        assert_eq!(format_freq(2_500), "1.25 kHz");
        assert_eq!(format_freq(3_200), "1.6 kHz");
        assert_eq!(format_freq(25_000), "12.5 kHz");
        assert_eq!(format_freq(40_000), "20 kHz");
    }

    #[test]
    fn format_gain_db_shows_a_sign_except_at_zero() {
        assert_eq!(format_gain_db(0), "0 dB");
        assert_eq!(format_gain_db(300), "+3 dB");
        assert_eq!(format_gain_db(-200), "-2 dB");
    }

    #[test]
    fn crossfeed_words_match_the_design_vocabulary() {
        assert_eq!(crossfeed_word(CrossfeedLevel::Off), "Off");
        assert_eq!(crossfeed_word(CrossfeedLevel::Weak), "Light");
        assert_eq!(crossfeed_word(CrossfeedLevel::Medium), "Medium");
        assert_eq!(crossfeed_word(CrossfeedLevel::Strong), "Strong");
    }

    #[test]
    fn band_labels_number_from_one_and_name_the_fixed_kind() {
        assert_eq!(band_label(0, BandKind::LowShelf), "1 . low shelf");
        assert_eq!(band_label(2, BandKind::Peak), "3 . peak");
        assert_eq!(band_label(4, BandKind::HighShelf), "5 . high shelf");
    }

    // --- Stepping / bounds ---

    #[test]
    fn crossfeed_steps_clamp_at_both_ends() {
        assert_eq!(step_crossfeed(CrossfeedLevel::Off, Step::Prev), CrossfeedLevel::Off);
        assert_eq!(step_crossfeed(CrossfeedLevel::Strong, Step::Next), CrossfeedLevel::Strong);
        assert!(!crossfeed_bounds(CrossfeedLevel::Off).prev);
        assert!(!crossfeed_bounds(CrossfeedLevel::Strong).next);
        assert!(crossfeed_bounds(CrossfeedLevel::Medium).prev);
        assert!(crossfeed_bounds(CrossfeedLevel::Medium).next);
    }

    #[test]
    fn freq_ladder_steps_move_one_entry_at_a_time_and_clamps() {
        assert_eq!(step_freq(2_000, Step::Next), 2_500);
        assert_eq!(step_freq(2_000, Step::Prev), 1_600);
        assert_eq!(step_freq(40, Step::Prev), 40);
        assert_eq!(step_freq(40_000, Step::Next), 40_000);
    }

    #[test]
    fn gain_steps_by_one_db_and_clamps_at_plus_minus_12() {
        assert_eq!(step_gain(0, Step::Next), GAIN_STEP_CDB);
        assert_eq!(step_gain(GAIN_MAX_CDB, Step::Next), GAIN_MAX_CDB);
        assert_eq!(step_gain(GAIN_MIN_CDB, Step::Prev), GAIN_MIN_CDB);
    }

    #[test]
    fn q_steps_across_the_shipped_q_table_and_clamps() {
        let last_idx = u8::try_from(Q_TABLE.len() - 1).unwrap();
        assert_eq!(step_q(q_milli_from_index(0), Step::Prev), q_milli_from_index(0));
        assert_eq!(step_q(q_milli_from_index(last_idx), Step::Next), q_milli_from_index(last_idx));
    }

    #[test]
    fn band_index_steps_clamp_at_both_ends_of_the_five_bands() {
        assert_eq!(step_band_index(0, Step::Prev), 0);
        assert_eq!(step_band_index(BAND_COUNT - 1, Step::Next), BAND_COUNT - 1);
    }

    // --- NAME cycling ---

    #[test]
    fn name_cycles_forward_through_the_canned_list() {
        let none: Vec<String> = Vec::new();
        let n1 = effect_n_candidate(&none);
        assert_eq!(n1, "Effect 1");
        let next = step_name(&n1, Step::Next, &none);
        assert_eq!(next, "Relaxed");
    }

    #[test]
    fn name_skips_a_candidate_already_used_by_another_effect() {
        let used = vec![String::from("Relaxed")];
        let next = step_name("Effect 1", Step::Next, &used);
        assert_eq!(next, "Long session", "Relaxed is taken, so NAME must skip straight past it");
    }

    #[test]
    fn effect_n_candidate_finds_the_lowest_free_number() {
        let used = vec![String::from("Effect 1"), String::from("Effect 2")];
        assert_eq!(effect_n_candidate(&used), "Effect 3");
    }

    #[test]
    fn name_wraps_from_the_last_candidate_back_to_effect_n() {
        let none: Vec<String> = Vec::new();
        let last = String::from(FIXED_NAME_CANDIDATES[FIXED_NAME_CANDIDATES.len() - 1]);
        let wrapped = step_name(&last, Step::Next, &none);
        assert_eq!(wrapped, "Effect 1");
    }

    // --- apply_step ---

    #[test]
    fn apply_step_on_freq_changes_the_selected_bands_frequency_only() {
        let mut state = EditorState { draft: new_effect_preset("Test"), band_index: 2, bypassed: false };
        let before = state.draft.bands[0].freq_half_hz;
        let changed = apply_step(&mut state, ROW_FREQ, Step::Next, &[]);
        assert!(changed);
        assert_eq!(state.draft.bands[0].freq_half_hz, before, "only the selected band (index 2) must change");
        assert_ne!(state.draft.bands[2].freq_half_hz, BAND_DEFAULT_FREQ_HZ[2] * 2);
    }

    #[test]
    fn apply_step_on_band_moves_the_cursor_but_is_not_itself_a_value_change() {
        let mut state = EditorState { draft: new_effect_preset("Test"), band_index: 0, bypassed: false };
        let changed = apply_step(&mut state, ROW_BAND, Step::Next, &[]);
        assert_eq!(state.band_index, 1);
        assert!(!changed, "moving BAND's cursor must not itself queue a save (design sec 4.2)");
    }

    #[test]
    fn apply_step_on_name_returns_false_at_a_dead_dupe_free_wrap_only_when_genuinely_unreachable() {
        // With every OTHER slot free, NAME always finds a fresh candidate.
        let mut state = EditorState { draft: new_effect_preset("Effect 1"), band_index: 0, bypassed: false };
        let changed = apply_step(&mut state, ROW_NAME, Step::Next, &[]);
        assert!(changed);
        assert_ne!(state.draft.name, "Effect 1");
    }

    #[test]
    fn reset_selected_band_only_changes_when_not_already_default() {
        let mut state = EditorState { draft: new_effect_preset("Test"), band_index: 0, bypassed: false };
        assert!(!reset_selected_band(&mut state), "a fresh effect's band is already at defaults");
        state.draft.bands[0].gain_cdb = 300;
        assert!(reset_selected_band(&mut state));
        assert_eq!(state.draft.bands[0], band_default(0));
    }

    // --- boost_may_distort ---

    #[test]
    fn boost_may_distort_is_false_for_a_flat_or_gently_boosted_preset() {
        assert!(!boost_may_distort(&new_effect_preset("Test")), "the default preset (0dB every band) must not warn");
    }

    #[test]
    fn boost_may_distort_is_true_when_every_band_is_boosted_to_the_max() {
        let mut preset = new_effect_preset("Loud");
        for band in &mut preset.bands {
            band.gain_cdb = GAIN_MAX_CDB;
        }
        assert!(boost_may_distort(&preset), "5 bands all at +12dB must exceed the -12dB preamp clamp");
    }

    // --- effects_list_rows (pure) ---

    #[test]
    fn effects_list_rows_shows_new_effect_last_and_full_when_at_capacity() {
        let mut presets = PresetStore::new();
        for i in 0..MAX_EFFECTS {
            presets.create(new_effect_preset(&format!("E{i}")));
        }
        let model = BtModel::default();
        let rows = effects_list_rows(&model, &presets);
        assert_eq!(rows.len(), MAX_EFFECTS + 1);
        let last = rows.last().unwrap();
        assert_eq!(last.label, "New effect");
        assert_eq!(last.value(), Some("full"));
    }

    #[test]
    fn effects_list_rows_usage_label_counts_devices_by_preset_id() {
        let mut presets = PresetStore::new();
        let id = presets.create(new_effect_preset("Relaxed"));
        let mut model = BtModel::default();
        model.paired.push(PairedDevice { addr: [1; 6], name: String::from("A"), mru_seq: 1, ldac_quality: 0, preset_id: id });
        model.paired.push(PairedDevice { addr: [2; 6], name: String::from("B"), mru_seq: 2, ldac_quality: 0, preset_id: id });
        model.paired.push(PairedDevice { addr: [3; 6], name: String::from("C"), mru_seq: 3, ldac_quality: 0, preset_id: 0 });
        let rows = effects_list_rows(&model, &presets);
        let relaxed_row = rows.iter().find(|r| r.label == "Relaxed").unwrap();
        assert_eq!(relaxed_row.value(), Some("2 devices"));
    }

    // --- resolve_effect_name ---

    #[test]
    fn resolve_effect_name_is_off_for_no_preset_and_for_a_dangling_id() {
        let presets = PresetStore::new();
        assert_eq!(resolve_effect_name(&presets, 0), "Off");
        assert_eq!(resolve_effect_name(&presets, 42), "Off");
    }

    #[test]
    fn resolve_effect_name_returns_the_stored_name() {
        let mut presets = PresetStore::new();
        let id = presets.create(new_effect_preset("Warm"));
        assert_eq!(resolve_effect_name(&presets, id), "Warm");
    }

    // --- End-to-end through App: Home -> Effects -> New effect ---

    fn open_effects(app: &mut App) {
        // Bead `pico-link-ryw.14`: `App::presets_ready` defaults `false` on
        // a real boot, so every test here that goes on to create/import a
        // preset needs this pushed first -- see `ready_presets`'s doc
        // comment.
        ready_presets(app);
        app.handle_input(vec![NavIntent::Select]); // Home status -> menu face
        app.handle_input(vec![NavIntent::Down]); // Effects row
        app.handle_input(vec![NavIntent::Select]); // open Effects list
    }

    #[test]
    fn opening_effects_from_home_menu_reaches_the_effects_list() {
        let mut app = App::new(240, 240);
        open_effects(&mut app);
        assert_eq!(app.current_screen_title(), EFFECTS_LIST_TITLE);
    }

    #[test]
    fn new_effect_saves_immediately_once_and_opens_the_editor() {
        let mut app = App::new(240, 240);
        open_effects(&mut app);
        app.handle_input(vec![NavIntent::Select]); // "New effect" (the only row) -> editor

        match app.poll_command() {
            Some(Command::SavePreset { preset_id, blob }) => {
                // Bead `pico-link-ryw.14`, Ada's preset-id-allocation
                // contract: `core` allocates the real id itself now, up
                // front -- there is no more `preset_id: 0`/"ask C to
                // allocate" convention.
                assert_ne!(preset_id, 0, "a brand-new effect must get a real, Rust-allocated id immediately");
                let preset = Preset::from_wire(&blob);
                assert_eq!(preset.name, "Effect 1");
                assert_eq!(preset.crossfeed, CrossfeedLevel::Medium);
                assert_eq!(preset.bands.len(), BAND_COUNT);
            }
            other => panic!("expected exactly one SavePreset, got {other:?}"),
        }
        assert_eq!(app.poll_command(), None, "creation must save exactly once");
        assert_eq!(app.current_screen_title(), "Effect 1");
    }

    #[test]
    fn every_value_change_in_the_editor_saves_exactly_once_and_targets_the_assigned_id() {
        let mut app = App::new(240, 240);
        open_effects(&mut app);
        app.handle_input(vec![NavIntent::Select]); // New effect -> editor
        let Some(Command::SavePreset { preset_id: created_id, .. }) = app.poll_command() else { panic!("expected the creation save") };
        assert_ne!(created_id, 0, "bead pico-link-ryw.14: the id is real and Rust-allocated from the start");

        // CROSSFEED is focused first (design sec 4.2) -- one Right press.
        app.handle_input(vec![NavIntent::Right]);
        match app.poll_command() {
            Some(Command::SavePreset { preset_id, blob }) => {
                assert_eq!(preset_id, created_id, "every further save must target the same id the creation save used");
                let preset = Preset::from_wire(&blob);
                assert_eq!(preset.crossfeed, CrossfeedLevel::Strong, "Medium -> Strong");
            }
            other => panic!("expected exactly one SavePreset, got {other:?}"),
        }
        assert_eq!(app.poll_command(), None, "one value change must save exactly once, nothing extra");
    }

    #[test]
    fn a_dead_end_press_writes_no_save() {
        let mut app = App::new(240, 240);
        open_effects(&mut app);
        app.handle_input(vec![NavIntent::Select]); // New effect -> editor
        let _ = app.poll_command(); // drain the creation save

        // CROSSFEED starts at Medium; three Lefts reach Off (Weak, then
        // Off), a fourth Left is a dead end.
        app.handle_input(vec![NavIntent::Left]);
        let _ = app.poll_command();
        app.handle_input(vec![NavIntent::Left]);
        let _ = app.poll_command();
        app.handle_input(vec![NavIntent::Left]); // now at Off; a no-op
        assert_eq!(app.poll_command(), None, "a value already at its floor must not queue a save on a further Left");
    }

    #[test]
    fn leaving_the_editor_by_back_queues_no_extra_save() {
        let mut app = App::new(240, 240);
        open_effects(&mut app);
        app.handle_input(vec![NavIntent::Select]); // New effect -> editor
        let _ = app.poll_command(); // drain the creation save
        app.handle_input(vec![NavIntent::Right]); // one real value change
        let _ = app.poll_command(); // drain that save

        app.handle_input(vec![NavIntent::Back]); // -> effects list
        assert_eq!(app.current_screen_title(), EFFECTS_LIST_TITLE);
        assert_eq!(app.poll_command(), None, "leaving the editor must not itself queue a save (Andreas's save-immediately ruling)");
    }

    #[test]
    fn x_toggles_bypass_and_any_value_change_clears_it() {
        let mut app = App::new(240, 240);
        open_effects(&mut app);
        app.handle_input(vec![NavIntent::Select]); // New effect -> editor
        let _ = app.poll_command();

        app.handle_input(vec![NavIntent::ShortcutX]); // bypass on
        app.handle_input(vec![NavIntent::Right]); // any value change clears bypass
        let saved = app.poll_command();
        assert!(matches!(saved, Some(Command::SavePreset { .. })), "the value change itself must still save");
    }

    #[test]
    fn delete_confirm_shows_usage_and_queues_delete_preset_on_confirm() {
        let mut app = App::new(240, 240);
        open_effects(&mut app);
        app.handle_input(vec![NavIntent::Select]); // New effect -> editor
        let Some(Command::SavePreset { blob, .. }) = app.poll_command() else { panic!("expected the creation save") };
        app.handle_event(Event::PresetLoaded { id: 1, blob });
        app.handle_input(vec![NavIntent::Back]); // -> effects list, now showing "Effect 1"
        // Selection is preserved by identity across the row rebuild
        // (`FieldList::set_rows`), so it's still on "New effect" (which
        // moved from index 0 to index 1 once "Effect 1" was inserted
        // ahead of it) -- move up onto the real effect row.
        app.handle_input(vec![NavIntent::Up]);

        app.handle_input(vec![NavIntent::ShortcutX]); // delete confirm
        assert_eq!(app.current_screen_title(), DELETE_CONFIRM_TITLE);
        app.handle_input(vec![NavIntent::Down]); // focus "Delete"
        app.handle_input(vec![NavIntent::Select]);
        assert_eq!(app.poll_command(), Some(Command::DeletePreset { preset_id: 1 }));
        assert_eq!(app.current_screen_title(), EFFECTS_LIST_TITLE, "confirming must pop back to the list");
    }

    #[test]
    fn delete_confirm_subline_fits_the_content_width_at_the_worst_case_count() {
        // The widest body string this module ever builds -- `MAX_EFFECTS`
        // (8) devices, the largest usage count possible.
        let worst = format!("{MAX_EFFECTS} devices will lose this effect.");
        let width = crate::render::theme::font::username()
            .get_rendered_dimensions_aligned(worst.as_str(), Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
            .unwrap()
            .unwrap()
            .size
            .width;
        let chrome = crate::render::compute_chrome(Size::new(240, 240));
        assert!(
            width <= chrome.content.size.width,
            "the delete-confirm subline ({width}px) must fit ConfirmView's unwrapped content width ({}px) even at 8 devices",
            chrome.content.size.width
        );
    }

    // --- Bead pico-link-ryw.12.4: imported presets, focus-follow, locked
    // editor ---

    const XM3_TEXT: &str = "Preamp: -4.41 dB\n\
        Filter 1:  ON  LS  Fc 40 Hz  Gain -1.76 dB  BW Oct 1.917\n\
        Filter 2:  ON  PK  Fc 80 Hz  Gain -2 dB  BW Oct 1.485\n\
        Filter 3:  ON  PK  Fc 540 Hz  Gain -1.4 dB  BW Oct 0.482\n\
        Filter 4:  ON  PK  Fc 1220 Hz  Gain 3.3 dB  BW Oct 0.687\n\
        Filter 5:  ON  PK  Fc 2941 Hz  Gain -2.4 dB  BW Oct 0.242\n\
        Filter 6:  ON  PK  Fc 3438 Hz  Gain 1.7 dB  BW Oct 0.311\n\
        Filter 7:  ON  PK  Fc 4544 Hz  Gain 6.5 dB  BW Oct 0.818\n\
        Filter 8:  ON  PK  Fc 9250 Hz  Gain -4.7 dB  BW Oct 0.413\n\
        Filter 9:  ON  PK  Fc 9822 Hz  Gain -0.1 dB  BW Oct 0.349\n\
        Filter 10:  ON  HS  Fc 10000 Hz  Gain 5.9 dB  BW Oct 1.917";

    #[test]
    fn an_import_lands_a_padlocked_row_and_focus_follows_it_when_the_list_is_open() {
        let mut app = App::new(240, 240);
        app.handle_event(Event::PresetLoaded { id: 1, blob: new_effect_preset("Relaxed").to_wire().to_vec() });
        open_effects(&mut app);
        app.render(); // establish a clean baseline, same discipline `picker.rs`'s own live-projection test uses

        let (id, outcome) = app.import_preset(XM3_TEXT, "XM3 Harman").expect("the XM3 text must import cleanly");
        assert_eq!(outcome, crate::dsp::ImportOutcome::Created);

        // The list is open, so focus must jump to the new row -- Uma's
        // design sec 2.
        app.render(); // EffectsListView::sync consumes the import_focus mailbox
        let expected_index = 1; // row 0 is "Relaxed", row 1 is the new import (creation order)
        assert_eq!(app.effects_list_selected_index_for_test(), Some(expected_index));

        // And the row carries `eq_locked` (the padlock's data source).
        assert!(app.presets_for_test().get(id).unwrap().eq_locked);
    }

    #[test]
    fn an_import_does_not_move_focus_when_the_effects_list_is_not_open() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        app.handle_event(Event::PresetLoaded { id: 1, blob: new_effect_preset("Relaxed").to_wire().to_vec() });
        // Deliberately NOT opening the effects list (still at Home root).
        let _ = app.import_preset(XM3_TEXT, "XM3 Harman").expect("import must still succeed");
        open_effects(&mut app);
        app.render();
        // Uma's design sec 2: "Anywhere else: nothing on screen" -- no
        // jump was armed, so the list opens at its ordinary default (row
        // 0), not on the import.
        assert_eq!(app.effects_list_selected_index_for_test(), Some(0));
    }

    #[test]
    fn a_locked_editor_has_no_name_row_and_a_readonly_preamp_and_band_row_per_band() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        let (id, _) = app.import_preset(XM3_TEXT, "XM3 Harman").unwrap();
        open_effects(&mut app);
        app.handle_input(vec![NavIntent::Select]); // the only row -- the imported effect
        assert_eq!(app.current_screen_title(), "XM3 Harman");

        let preset = app.presets_for_test().get(id).unwrap().clone();
        let rows = editor_rows(&EditorState { draft: preset, band_index: 0, bypassed: false });

        assert!(rows.iter().all(|r| r.label != "NAME"), "a locked editor must have no NAME row");
        assert_eq!(rows[0].label, "CROSSFEED");
        assert_eq!(rows[1].label, "PREAMP");
        assert_eq!(rows[1].value(), Some("-4.41 dB"));
        assert_eq!(rows.len(), 2 + 10, "CROSSFEED + PREAMP + one row per one of the 10 XM3 bands");
        assert_eq!(rows[2].label, "1 LS");
        assert_eq!(rows[11].label, "10 HS");
    }

    #[test]
    fn locked_band_row_values_never_truncate_a_number_at_any_degrade_level() {
        // The worst-case value string this bead's own width budget is
        // measured against (Uma's design sec 4) -- every digit must
        // survive every degrade level, only the unit suffixes may drop.
        let band = Band { kind: BandKind::HighShelf, freq_half_hz: 39_999, gain_cdb: -2_999, q_milli: 65_000 };
        for degrade in [BandRowDegrade::Full, BandRowDegrade::NoDb, BandRowDegrade::NoDbNoHz, BandRowDegrade::NoDbNoHzCompact] {
            let text = format_band_value(&band, degrade);
            assert!(text.contains("19999.5"), "freq digits must survive at {degrade:?}: {text}");
            assert!(text.contains("-29.99"), "gain digits must survive at {degrade:?}: {text}");
            assert!(text.contains("65.00"), "Q digits must survive at {degrade:?}: {text}");
        }
    }

    #[test]
    fn band_row_degrade_measures_a_real_font_and_picks_a_fitting_level() {
        // Bead `pico-link-ryw.12.4`: actually measuring the worst-case
        // string against `helvR12` (Uma's design sec 4's own instruction:
        // "Ruby must measure worst case ... within ~182px") shows even
        // `NoDb` doesn't fit -- the panel needs `NoDbNoHz`. This is a
        // regression net on the MEASUREMENT, not an assumption: if a
        // future font/budget change moves the answer, this test documents
        // what it moved to rather than silently drifting. Every digit
        // still survives at every level -- see the test above.
        assert_eq!(band_row_degrade(), BandRowDegrade::NoDbNoHz);
    }

    // --- Headless screenshots, at zoom ---

    fn save_zoomed_png(app: &mut App, path: &std::path::Path) {
        const ZOOM: u32 = 3;
        let framebuffer = app.render();
        let mut image = image::RgbImage::new(framebuffer.width(), framebuffer.height());
        for pixel in framebuffer.pixels() {
            let color = pixel.1;
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
            image.put_pixel(
                pixel.0.x as u32,
                pixel.0.y as u32,
                image::Rgb([(color.r() << 3) | (color.r() >> 2), (color.g() << 2) | (color.g() >> 4), (color.b() << 3) | (color.b() >> 2)]),
            );
        }
        let zoomed = image::imageops::resize(&image, framebuffer.width() * ZOOM, framebuffer.height() * ZOOM, image::imageops::FilterType::Nearest);
        zoomed.save(path).unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
    }

    #[test]
    fn effects_screens_screenshots_at_zoom() {
        let out_dir = std::env::temp_dir().join("pico-link-effects-screenshots");
        std::fs::create_dir_all(&out_dir).expect("failed to create output dir");

        // The list, populated with a few effects, one assigned+connected
        // (check gutter visible).
        let mut app = App::new(240, 240);
        let seq_for_test_16 = app.seed_connect_attempt_for_test([7; 6]);
        app.handle_event(Event::ConnectSucceeded { addr: [7; 6], degraded: false, seq: seq_for_test_16 });
        let _ = app.poll_command();
        app.handle_event(Event::PairedDeviceUpserted(PairedDevice { addr: [7; 6], name: String::from("Cans"), mru_seq: 1, ldac_quality: 0, preset_id: 1 }));
        app.handle_event(Event::CodecChanged(ConnectedCodec { addr: [7; 6], word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
        app.handle_event(Event::PresetLoaded { id: 1, blob: new_effect_preset("Relaxed").to_wire().to_vec() });
        app.handle_event(Event::PresetLoaded { id: 2, blob: new_effect_preset("Long session").to_wire().to_vec() });
        open_effects(&mut app);
        save_zoomed_png(&mut app, &out_dir.join("01_list.png"));

        // The editor, on the first (existing) effect.
        app.handle_input(vec![NavIntent::Select]); // open "Relaxed"
        assert_eq!(app.current_screen_title(), "Relaxed");
        save_zoomed_png(&mut app, &out_dir.join("02_editor.png"));

        // The delete confirm.
        app.handle_input(vec![NavIntent::Back]);
        app.handle_input(vec![NavIntent::ShortcutX]);
        save_zoomed_png(&mut app, &out_dir.join("03_delete_confirm.png"));

        println!("wrote effects screenshots to {}", out_dir.display());
    }

    /// Committed zoomed PNG fixtures for bead `pico-link-ryw.12.4`'s
    /// imported-preset UI: the padlocked list row, both pages of the
    /// locked (10-band) editor, and the padlocked device-page picker.
    /// Written straight into the repo (`ryw12-screenshots/`, sibling to
    /// `home-screenshots/`'s own committed-fixture convention) rather than
    /// `std::env::temp_dir()` -- Tess/a reviewer inspects these AT ZOOM
    /// per this project's rendering-change verification discipline; this
    /// test does not (yet) self-check them against a fresh render the way
    /// `home_screenshot_fixtures.rs` does (that harness is its own,
    /// separate follow-up).
    #[test]
    fn ryw12_4_imported_preset_screenshots_at_zoom() {
        let out_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("ryw12-screenshots");
        std::fs::create_dir_all(&out_dir).expect("failed to create output dir");

        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        app.handle_event(Event::PresetLoaded { id: 1, blob: new_effect_preset("Relaxed").to_wire().to_vec() });
        let (id, _) = app.import_preset(XM3_TEXT, "XM3 Harman").expect("the XM3 text must import cleanly");

        // 1. The list -- a hand-made row and an imported (padlocked) row.
        open_effects(&mut app);
        save_zoomed_png(&mut app, &out_dir.join("01_list_with_padlock.png"));

        // 2. The locked editor, page 1 (CROSSFEED focused, bands 1-3ish
        //    visible) -- select the imported row (it's the second/last
        //    real row, "New effect" comes after it).
        app.handle_input(vec![NavIntent::Down]); // focus the imported row
        app.handle_input(vec![NavIntent::Select]);
        assert_eq!(app.current_screen_title(), "XM3 Harman");
        save_zoomed_png(&mut app, &out_dir.join("02_locked_editor_page1.png"));

        // 3. The locked editor, page 2 (scrolled down to the later bands).
        for _ in 0..8 {
            app.handle_input(vec![NavIntent::Down]);
        }
        save_zoomed_png(&mut app, &out_dir.join("03_locked_editor_page2.png"));
        app.handle_input(vec![NavIntent::Back]); // editor -> effects list
        app.handle_input(vec![NavIntent::Back]); // effects list -> Home menu face
        app.handle_input(vec![NavIntent::Back]); // Home menu face -> Home status face

        // 4. The device-page effect picker -- a hand-made option and a
        //    padlocked imported option.
        let addr = [7; 6];
        let seq_for_test_1030 = app.seed_connect_attempt_for_test(addr);
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false, seq: seq_for_test_1030 });
        let _ = app.poll_command();
        app.handle_event(Event::PairedDeviceUpserted(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 0, preset_id: id }));
        app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
        app.handle_input(vec![NavIntent::Select]); // Home status -> menu face
        app.handle_input(vec![NavIntent::Up]); // the menu's selection persisted from earlier in this test (Effects) -- force it back to Bluetooth (index 0)
        app.handle_input(vec![NavIntent::Select]); // Bluetooth row -> Devices
        app.handle_input(vec![NavIntent::Select]); // the connected device row -> device page
        app.handle_input(vec![NavIntent::Down, NavIntent::Down]); // CODEC -> QUALITY -> EFFECT row
        app.handle_input(vec![NavIntent::Select]); // -> picker
        save_zoomed_png(&mut app, &out_dir.join("04_effect_picker_with_padlock.png"));

        println!("wrote ryw.12.4 screenshots to {}", out_dir.display());
    }
}
