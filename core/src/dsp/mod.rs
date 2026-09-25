//! DSP effects: global EQ/crossfeed presets and coefficient computation
//! (bead `pico-link-ryw.2`, design `.planning/design/2026-09-25-dsp-effects-stage.md`
//! sec 3.1).
//!
//! `core` owns the WHOLE preset model and ALL coefficient math: the preset
//! type, its on-wire blob format (`preset::Preset::to_wire`/`from_wire`),
//! the never-reused-id preset store (`store::PresetStore`), and the
//! params -> coefficients conversion (RBJ cookbook biquads, a bs2b-style
//! crossfeed, and the automatic preamp). C never parses a preset blob and
//! never computes a coefficient -- it stores opaque bytes
//! (`persist.c`, `PL:P:<slot>`) and runs a fixed-point kernel over
//! whatever [`coeffs::Program`] this module hands it (`dsp.c`, bead
//! `pico-link-ryw.1`, built in parallel).
//!
//! This module is deliberately FFI-free: turning a [`coeffs::Program`] into
//! the wire-compatible `PlDspProgram` C struct, the `PlUi` pull API, and
//! the three new `PlCommand`/`PlEvent` variants is bead `pico-link-ryw.5`
//! ("FFI SEAM"), not this one. Persisting a [`preset::Preset`] to
//! `PL:P:<slot>` and resolving a device's `preset_id` against a loaded
//! store is `pico-link-ryw.6`. This bead only has to produce numbers a
//! future FFI layer can copy field-for-field.

pub mod coeffs;
pub mod eqapo;
pub mod import;
pub mod preset;
pub mod store;

#[cfg(test)]
mod tests;

pub use coeffs::{
    auto_preamp_db, crossfeed_coeffs, rbj_high_shelf, rbj_low_shelf, rbj_peaking, Biquad, CrossfeedCoeffs, Program,
    MAX_BOOST_HEADROOM_DB, MIN_BOOST_HEADROOM_DB,
};
pub use eqapo::{bw_oct_to_q, EqApoError, EqApoLineError, EqApoOverride, EqApoSession, ParsedDocument, ParsedFilter};
pub use import::{import as import_preset, ImportError, ImportOutcome, MAX_PRESETS};
pub use preset::{Band, BandKind, CrossfeedLevel, Preset, BLOB_LEN, MAX_BANDS, MAX_NAME_BYTES};
pub use store::PresetStore;
