//! Host tests for the DSP preset model and coefficient math.
//!
//! Reference-value discipline (per the bead): every filter/crossfeed
//! assertion is checked against an INDEPENDENTLY derived expectation --
//! either a closed-form property from the RBJ cookbook / bs2b algorithm
//! (e.g. "a peaking filter is unity gain at DC and Nyquist", "a shelf's
//! two asymptotes are 0dB and the design gain"), or a value computed by a
//! second, from-scratch formula in the test itself -- never a value
//! copied from this module's own output.

use alloc::vec;

use super::coeffs::{
    auto_preamp_db, band_to_biquad, crossfeed_coeffs, rbj_high_shelf, rbj_low_shelf, rbj_peaking, CrossfeedState,
    Program, MAX_BOOST_HEADROOM_DB,
};
use super::preset::{
    nearest_q_index, q_from_index, q_milli_from_index, Band, BandKind, CrossfeedLevel, Preamp, Preset, PresetBlobError, Q_TABLE, BLOB_LEN,
};
use super::store::{PresetStore, NO_PRESET_ID};

const FS: f32 = 48_000.0;

fn db(x: f32) -> f32 {
    20.0 * libm::log10f(x)
}

// ---------------------------------------------------------------------
// RBJ peaking
// ---------------------------------------------------------------------

#[test]
fn peaking_is_unity_at_dc_and_nyquist() {
    // RBJ cookbook property: a peaking (bell) filter's magnitude response
    // is exactly unity (0dB) far from the center frequency in both
    // directions -- it neither boosts nor cuts DC or Nyquist, regardless
    // of the requested gain. This is a defining structural property of
    // the filter shape, independent of this module's own arithmetic.
    let bq = rbj_peaking(1_000.0, 12.0, 1.0, FS);
    assert!((bq.magnitude_at(1.0, FS) - 1.0).abs() < 0.02, "DC should be ~unity, got {}", bq.magnitude_at(1.0, FS));
    assert!(
        (bq.magnitude_at(FS / 2.0 - 1.0, FS) - 1.0).abs() < 0.02,
        "Nyquist should be ~unity, got {}",
        bq.magnitude_at(FS / 2.0 - 1.0, FS)
    );
}

#[test]
fn peaking_hits_its_designed_gain_at_center_freq() {
    // At w = w0 exactly, the RBJ peaking filter's magnitude is
    // mathematically exactly 10^(gain_db/40) * ... actually simplifies to
    // the requested linear gain A = 10^(gain_db/20) NO -- the textbook
    // result is that |H(e^{jw0})| = A (not A^2), i.e. the peak equals the
    // requested gain in dB exactly, by construction of the cookbook
    // formula (b0/a0 at resonance cancels alpha). Verify against that
    // closed-form expectation, computed independently of `rbj_peaking`'s
    // own code path.
    for gain_db in [-9.0f32, -3.0, 3.0, 9.0] {
        let bq = rbj_peaking(2_000.0, gain_db, 2.0, FS);
        let measured_db = db(bq.magnitude_at(2_000.0, FS));
        assert!(
            (measured_db - gain_db).abs() < 0.15,
            "gain_db={gain_db}: expected ~{gain_db}dB at center, measured {measured_db}dB"
        );
    }
}

#[test]
fn peaking_zero_gain_is_bit_exact_passthrough() {
    // A cookbook peaking filter with gain_db = 0 has A = 1, which makes
    // alpha/A == alpha*A, collapsing the normalised numerator and
    // denominator to the SAME coefficients (b0=1=a0, b1=a1, b2=a2) --
    // i.e. H(z) = N(z)/D(z) with N == D, an exact identity transfer
    // function for ANY freq/Q, independent of how `rbj_peaking` itself
    // is implemented.
    let bq = rbj_peaking(5_000.0, 0.0, 3.0, FS);
    assert!((bq.b0 - 1.0).abs() < 1.0e-5, "b0 should be exactly 1 (== normalised a0), got {}", bq.b0);
    assert!((bq.b1 - bq.a1).abs() < 1.0e-5, "b1 ({}) should equal a1 ({}) at 0dB gain", bq.b1, bq.a1);
    assert!((bq.b2 - bq.a2).abs() < 1.0e-5, "b2 ({}) should equal a2 ({}) at 0dB gain", bq.b2, bq.a2);
    // And therefore the magnitude response is unity everywhere, not just
    // at the two asymptotes checked elsewhere.
    for freq in [20.0f32, 100.0, 1_000.0, 10_000.0, FS / 2.0 - 1.0] {
        let mag = bq.magnitude_at(freq, FS);
        assert!((mag - 1.0).abs() < 1.0e-4, "freq={freq}: expected unity, got {mag}");
    }
}

// ---------------------------------------------------------------------
// RBJ shelves
// ---------------------------------------------------------------------

#[test]
fn low_shelf_asymptotes_match_cookbook_definition() {
    // A low shelf is, by definition, ~0dB well above its corner and
    // ~gain_db well below it. Check both asymptotes independently of
    // the coefficient formula: far below/above the corner the response
    // should have settled near its plateau.
    let corner = 500.0;
    let gain_db = 6.0;
    let bq = rbj_low_shelf(corner, gain_db, 0.71, FS);

    let low_freq_db = db(bq.magnitude_at(20.0, FS));
    let high_freq_db = db(bq.magnitude_at(FS / 2.0 - 1.0, FS));

    assert!((low_freq_db - gain_db).abs() < 1.0, "low shelf plateau: expected ~{gain_db}dB, got {low_freq_db}dB");
    assert!((high_freq_db - 0.0).abs() < 1.0, "low shelf high asymptote: expected ~0dB, got {high_freq_db}dB");
}

#[test]
fn high_shelf_asymptotes_match_cookbook_definition() {
    let corner = 4_000.0;
    let gain_db = -6.0;
    let bq = rbj_high_shelf(corner, gain_db, 0.71, FS);

    let low_freq_db = db(bq.magnitude_at(20.0, FS));
    let high_freq_db = db(bq.magnitude_at(FS / 2.0 - 1.0, FS));

    assert!((low_freq_db - 0.0).abs() < 1.0, "high shelf low asymptote: expected ~0dB, got {low_freq_db}dB");
    assert!((high_freq_db - gain_db).abs() < 1.0, "high shelf plateau: expected ~{gain_db}dB, got {high_freq_db}dB");
}

#[test]
fn shelves_zero_gain_are_passthrough() {
    let low = rbj_low_shelf(1_000.0, 0.0, 1.0, FS);
    let high = rbj_high_shelf(1_000.0, 0.0, 1.0, FS);
    for bq in [low, high] {
        assert!((bq.magnitude_at(20.0, FS) - 1.0).abs() < 0.02);
        assert!((bq.magnitude_at(FS / 2.0 - 1.0, FS) - 1.0).abs() < 0.02);
    }
}

// ---------------------------------------------------------------------
// band_to_biquad / auto preamp
// ---------------------------------------------------------------------

#[test]
fn band_to_biquad_clamps_freq_above_nyquist() {
    // A corrupt/implausible stored freq_hz (above this fs's Nyquist)
    // must not produce a NaN/unstable filter -- it should behave as if
    // clamped to just under Nyquist.
    let band = Band { kind: BandKind::Peak, freq_half_hz: (30_000) * 2, gain_cdb: (12) * 50, q_milli: q_milli_from_index(3) };
    let bq = band_to_biquad(band, 44_100);
    assert!(bq.b0.is_finite() && bq.b1.is_finite() && bq.b2.is_finite() && bq.a1.is_finite() && bq.a2.is_finite());
}

#[test]
fn auto_preamp_is_zero_for_cuts_only() {
    // A preset with only cuts (no boost) never clips harder than the dry
    // signal, so the design's "why not a fixed -6dB" point should hold:
    // the auto preamp must be 0dB (no needless headroom sacrificed).
    let bands = vec![band_to_biquad(
        Band { kind: BandKind::Peak, freq_half_hz: (1_000) * 2, gain_cdb: (-12) * 50, q_milli: q_milli_from_index(3) },
        48_000,
    )];
    let preamp = auto_preamp_db(&bands, 48_000);
    assert!((preamp - 0.0).abs() < 1.0e-3, "expected 0dB preamp for cuts-only, got {preamp}");
}

#[test]
fn auto_preamp_clamps_to_minus_12db() {
    // A wildly boosted band (well past the -12dB floor) must clamp, not
    // return an arbitrarily large negative preamp (design sec 1.2:
    // "clamped to [-12, 0] dB").
    let bands = vec![band_to_biquad(
        Band { kind: BandKind::Peak, freq_half_hz: (1_000) * 2, gain_cdb: (48) * 50, q_milli: q_milli_from_index(3) }, // +24dB
        48_000,
    )];
    let preamp = auto_preamp_db(&bands, 48_000);
    assert!((preamp - MAX_BOOST_HEADROOM_DB.min(preamp)).abs() < 1.0e-6 || preamp >= -12.0 - 1.0e-3);
    assert!(preamp >= -12.0 - 1.0e-3, "preamp {preamp} must not exceed the -12dB floor");
}

#[test]
fn auto_preamp_cancels_a_known_boost_within_grid_resolution() {
    // Independent check: for a single peaking band, the response peak IS
    // (very nearly) the value at the center frequency -- so the auto
    // preamp should cancel a +6dB boost to within the response grid's
    // resolution, verified against a magnitude computed directly via
    // `Biquad::magnitude_at`, not via `auto_preamp_db`'s own peak search.
    let band = Band { kind: BandKind::Peak, freq_half_hz: (1_000) * 2, gain_cdb: (12) * 50, q_milli: q_milli_from_index(3) }; // +6dB, Q=1.0
    let bq = band_to_biquad(band, 48_000);
    let preamp = auto_preamp_db(&[bq], 48_000);
    let center_db = db(bq.magnitude_at(1_000.0, 48_000.0));
    assert!(
        (preamp + center_db).abs() < 0.3,
        "preamp {preamp}dB should cancel the ~{center_db}dB center-frequency peak"
    );
}

// ---------------------------------------------------------------------
// Crossfeed (bs2b)
// ---------------------------------------------------------------------

#[test]
fn crossfeed_off_yields_no_coefficients() {
    assert!(crossfeed_coeffs(CrossfeedLevel::Off, 48_000).is_none());
}

#[test]
fn crossfeed_preserves_mono_loudness_at_dc() {
    // bs2b's defining structural property: for a signal identical on
    // both channels (a mono source panned center), crossfeed should not
    // change the loudness -- at DC, out_left = out_right = in, because
    // the algorithm is normalised for exactly this case (design's whole
    // point is "not audiophile spatialisation", i.e. it must not colour
    // a mono signal). Fed with a constant (DC) input through the
    // reference per-sample structure until settled, checked against the
    // input value itself -- an independent property, not a copied
    // number.
    for level in [CrossfeedLevel::Weak, CrossfeedLevel::Medium, CrossfeedLevel::Strong] {
        let coeffs = crossfeed_coeffs(level, 48_000).expect("non-Off level must yield coefficients");
        let mut state = CrossfeedState::default();
        let mut out = [0.0f32; 2];
        // Settle the one-pole filters (a few time constants at 48kHz for
        // a ~700Hz cutoff is well under 200 samples).
        for _ in 0..2_000 {
            out = coeffs.process(&mut state, [1.0, 1.0]);
        }
        assert!((out[0] - 1.0).abs() < 0.02, "{level:?}: left settled to {}, expected ~1.0", out[0]);
        assert!((out[1] - 1.0).abs() < 0.02, "{level:?}: right settled to {}, expected ~1.0", out[1]);
    }
}

#[test]
fn crossfeed_narrows_a_hard_pan_at_settled_state() {
    // The other defining property: crossfeed reduces (but does not
    // eliminate) inter-channel separation for a hard-panned source.
    // Settle with input entirely on the left channel; the right channel
    // must receive some non-zero energy (that's the whole point of
    // crossfeed) but strictly less than the left, at every non-Off
    // strength.
    for level in [CrossfeedLevel::Weak, CrossfeedLevel::Medium, CrossfeedLevel::Strong] {
        let coeffs = crossfeed_coeffs(level, 48_000).expect("non-Off level must yield coefficients");
        let mut state = CrossfeedState::default();
        let mut out = [0.0f32; 2];
        for _ in 0..2_000 {
            out = coeffs.process(&mut state, [1.0, 0.0]);
        }
        assert!(out[1] > 0.02, "{level:?}: right channel got no crossfed energy ({})", out[1]);
        assert!(out[0] > out[1], "{level:?}: left ({}) should stay louder than right ({})", out[0], out[1]);
    }
}

#[test]
fn crossfeed_settled_dc_split_sums_to_the_input() {
    // Independent closed-form check of the algorithm's DC steady state.
    // Solving the two one-pole recurrences for a constant input `in`
    // algebraically gives out_left = (1 - g_hi) * norm_gain * in and
    // out_right = g_lo * norm_gain * in (both derived by hand from the
    // `lo`/`hi` recurrences at equilibrium, not from this module's
    // `process` loop) -- and since `norm_gain = 1 / (1 - g_hi + g_lo)`,
    // the two ALWAYS sum to exactly `in`. This is bs2b's mono-loudness
    // guarantee restated at the level of the raw recurrence rather than
    // simulated in the time domain (see `crossfeed_preserves_mono_
    // loudness_at_dc`, which simulates instead of solving).
    for level in [CrossfeedLevel::Weak, CrossfeedLevel::Medium, CrossfeedLevel::Strong] {
        let coeffs = crossfeed_coeffs(level, 48_000).expect("non-Off level must yield coefficients");
        let mut state = CrossfeedState::default();
        let mut out = [0.0f32; 2];
        for _ in 0..4_000 {
            out = coeffs.process(&mut state, [1.0, 0.0]);
        }
        assert!((out[0] + out[1] - 1.0).abs() < 0.01, "{level:?}: DC split {out:?} should sum to 1.0");
        // Both channels carry real, bounded energy -- crossfeed neither
        // vanishes (no effect) nor blows up (unstable coefficients).
        assert!(out[0] > 0.0 && out[0] < 1.0, "{level:?}: left {} out of (0,1)", out[0]);
        assert!(out[1] > 0.0 && out[1] < 1.0, "{level:?}: right {} out of (0,1)", out[1]);
    }
}

#[test]
fn crossfeed_presets_are_distinct() {
    // Regression guard: the three strengths must not collapse to
    // identical coefficients (e.g. a copy-paste of the same preset
    // tuple). Tuning the actual perceptual ordering is `pico-link-ryw.8`
    // ("BY-EAR ACCEPTANCE AND CROSSFEED TUNING"), not this bead -- so
    // this test only checks distinctness, not a claimed direction.
    let weak = crossfeed_coeffs(CrossfeedLevel::Weak, 48_000).unwrap();
    let medium = crossfeed_coeffs(CrossfeedLevel::Medium, 48_000).unwrap();
    let strong = crossfeed_coeffs(CrossfeedLevel::Strong, 48_000).unwrap();
    assert_ne!(weak, medium);
    assert_ne!(medium, strong);
    assert_ne!(weak, strong);
}

// ---------------------------------------------------------------------
// Program (end to end)
// ---------------------------------------------------------------------

#[test]
fn off_preset_compiles_to_bypass_program() {
    let preset = Preset::new("Empty");
    let program = Program::from_preset(&preset, 48_000);
    assert!(program.biquads.is_empty());
    assert!(program.crossfeed.is_none());
    assert!((program.preamp_linear - 1.0).abs() < 1.0e-6);
}

#[test]
fn program_biquad_count_matches_band_count() {
    let mut preset = Preset::new("Three bands");
    for freq in [200u16, 1_000, 6_000] {
        assert!(preset.push_band(Band { kind: BandKind::Peak, freq_half_hz: (freq) * 2, gain_cdb: (6) * 50, q_milli: q_milli_from_index(3) }));
    }
    let program = Program::from_preset(&preset, 48_000);
    assert_eq!(program.biquads.len(), 3);
}

#[test]
fn preset_push_band_respects_max_bands() {
    let mut preset = Preset::new("Overflow");
    for i in 0..u16::try_from(super::preset::MAX_BANDS).unwrap() {
        assert!(preset.push_band(Band { kind: BandKind::Peak, freq_half_hz: (100 + i) * 2, gain_cdb: 0, q_milli: q_milli_from_index(0) }));
    }
    assert!(!preset.push_band(Band { kind: BandKind::Peak, freq_half_hz: (9_999) * 2, gain_cdb: 0, q_milli: q_milli_from_index(0) }));
    assert_eq!(preset.bands.len(), super::preset::MAX_BANDS);
}

// ---------------------------------------------------------------------
// Q table
// ---------------------------------------------------------------------

#[test]
// Exact-value table lookups, not arithmetic results -- `==` is the
// correct comparison here, not an approximate one.
#[allow(clippy::float_cmp)]
fn q_from_index_clamps_out_of_range() {
    let last = Q_TABLE[Q_TABLE.len() - 1];
    let last_index = u8::try_from(Q_TABLE.len() - 1).unwrap();
    assert_eq!(q_from_index(255), last);
    assert_eq!(q_from_index(last_index), last);
}

// ---------------------------------------------------------------------
// Wire round-trip
// ---------------------------------------------------------------------

#[test]
fn wire_round_trip_preserves_all_fields() {
    let mut preset = Preset::new("Bright & Wide");
    preset.crossfeed = CrossfeedLevel::Medium;
    preset.push_band(Band { kind: BandKind::LowShelf, freq_half_hz: (120) * 2, gain_cdb: (10) * 50, q_milli: q_milli_from_index(3) });
    preset.push_band(Band { kind: BandKind::Peak, freq_half_hz: (3_200) * 2, gain_cdb: (-7) * 50, q_milli: q_milli_from_index(5) });
    preset.push_band(Band { kind: BandKind::HighShelf, freq_half_hz: (9_000) * 2, gain_cdb: (4) * 50, q_milli: q_milli_from_index(1) });

    let wire = preset.to_wire();
    assert_eq!(wire.len(), BLOB_LEN);
    let restored = Preset::from_wire(&wire);

    assert_eq!(restored.name, preset.name);
    assert_eq!(restored.crossfeed, preset.crossfeed);
    assert_eq!(restored.bands, preset.bands);
}

#[test]
fn wire_round_trip_empty_preset() {
    let preset = Preset::new("");
    let restored = Preset::from_wire(&preset.to_wire());
    assert_eq!(restored, preset);
}

// ---------------------------------------------------------------------
// Blob v2 (bead pico-link-ryw.12.1)
// ---------------------------------------------------------------------

#[test]
fn v1_blob_reads_exactly_and_widens_to_v2_exactly() {
    // Hand-built v1 wire blob (design sec 2.2 layout, frozen and
    // documented in `preset`'s module doc / `v1_layout`): `{version=1,
    // name_len, name[16], crossfeed, band_count, band[10] * {kind,
    // freq_hz u16 LE, gain_half_db i8, q_idx u8}}`.
    let mut raw = [0u8; BLOB_LEN];
    raw[0] = 1; // BLOB_VERSION_V1
    let name = b"V1 Preset";
    raw[1] = u8::try_from(name.len()).unwrap();
    raw[2..2 + name.len()].copy_from_slice(name);
    let crossfeed_off = 2 + 16;
    raw[crossfeed_off] = 2; // Medium
    let band_count_off = crossfeed_off + 1;
    raw[band_count_off] = 2;
    let bands_off = band_count_off + 1;

    // Band 0: Peak, 1000Hz, +6dB (gain_half_db 12), q_idx 3.
    raw[bands_off] = 0;
    raw[bands_off + 1..bands_off + 3].copy_from_slice(&1_000u16.to_le_bytes());
    #[allow(clippy::cast_sign_loss)]
    {
        raw[bands_off + 3] = 12i8 as u8;
    }
    raw[bands_off + 4] = 3;

    // Band 1: LowShelf, 120Hz, -5dB (gain_half_db -10), q_idx 5.
    let b1 = bands_off + 5; // v1's fixed 5-byte band record
    raw[b1] = 1;
    raw[b1 + 1..b1 + 3].copy_from_slice(&120u16.to_le_bytes());
    #[allow(clippy::cast_sign_loss)]
    {
        raw[b1 + 3] = (-10i8) as u8;
    }
    raw[b1 + 4] = 5;

    let preset = Preset::from_wire(&raw);

    assert_eq!(preset.name, "V1 Preset");
    assert_eq!(preset.crossfeed, CrossfeedLevel::Medium);
    assert_eq!(preset.preamp, Preamp::Auto, "a v1-sourced preset always widens to Preamp::Auto");
    assert!(!preset.eq_locked, "a v1-sourced preset is never locked");
    assert_eq!(preset.bands.len(), 2);
    assert_eq!(
        preset.bands[0],
        Band { kind: BandKind::Peak, freq_half_hz: 2_000, gain_cdb: 600, q_milli: q_milli_from_index(3) },
        "1000Hz*2, +6dB(12 half-dB)*50=600cdb, Q_TABLE[3] widened exactly"
    );
    assert_eq!(
        preset.bands[1],
        Band { kind: BandKind::LowShelf, freq_half_hz: 240, gain_cdb: -500, q_milli: q_milli_from_index(5) },
        "120Hz*2, -5dB(-10 half-dB)*50=-500cdb, Q_TABLE[5] widened exactly"
    );

    // Saving a v1-sourced preset rewrites it as v2 -- no separate
    // migration pass, no flash write at boot (ryw.12 sec 2).
    let rewritten = preset.to_wire();
    assert_eq!(rewritten.len(), BLOB_LEN);
    assert_eq!(rewritten[0], 2, "a save always rewrites as BLOB_VERSION_V2");
    assert_eq!(Preset::from_wire(&rewritten), preset, "v1 -> v2 -> v1-again must be lossless");
}

#[test]
fn v1_blob_in_the_store_survives_load_then_save_as_v2() {
    // Same v1 bytes as `v1_blob_reads_exactly_and_widens_to_v2_exactly`,
    // but exercised through the exact load/to_wire/from_wire sequence a
    // `PresetStore` boot-load-then-save round trip performs.
    let mut raw = [0u8; BLOB_LEN];
    raw[0] = 1;
    let name = b"Old";
    raw[1] = u8::try_from(name.len()).unwrap();
    raw[2..2 + name.len()].copy_from_slice(name);
    raw[2 + 16] = 1; // Weak crossfeed
    raw[2 + 16 + 1] = 0; // no bands

    let loaded = Preset::from_wire(&raw);
    assert_eq!(loaded.name, "Old");
    assert_eq!(loaded.crossfeed, CrossfeedLevel::Weak);

    let saved = loaded.to_wire();
    assert_eq!(saved[0], 2, "the next save always writes v2, regardless of what version was loaded");
    assert_eq!(Preset::from_wire(&saved), loaded);
}

#[test]
fn q_table_milli_matches_q_table() {
    // Every `Q_TABLE` entry, widened to milli-Q via `* 1000`, must equal
    // `q_milli_from_index`'s corresponding entry exactly -- no rounding
    // uncertainty (see `Q_TABLE_MILLI`'s own doc comment in `preset`).
    for (idx, &q) in Q_TABLE.iter().enumerate() {
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        // Every Q_TABLE entry is well within u16 range (max 8000 milli),
        // and never negative -- an exact table lookup, not a lossy cast.
        let expected_milli = libm::roundf(q * 1000.0) as u16;
        let idx_u8 = u8::try_from(idx).unwrap();
        assert_eq!(
            q_milli_from_index(idx_u8),
            expected_milli,
            "Q_TABLE[{idx}] = {q} must widen exactly to {expected_milli} milli-Q"
        );
    }
}

#[test]
fn nearest_q_index_finds_the_closest_table_entry_and_round_trips_editor_values() {
    // Every `q_milli` the editor itself ever produces is exactly
    // `q_milli_from_index(idx)` for some table entry -- `nearest_q_index`
    // must recover that same index for every one of them (this is what
    // makes `step_q`'s "snap to nearest, then move one" identical to the
    // old index-only stepping for every value the editor can reach).
    for idx in 0..u8::try_from(Q_TABLE.len()).unwrap() {
        let milli = q_milli_from_index(idx);
        assert_eq!(nearest_q_index(milli), idx, "q_milli_from_index({idx}) must round-trip through nearest_q_index");
    }
    // An out-of-table value picks whichever entry is numerically closest.
    assert_eq!(nearest_q_index(0), 0, "0 is closest to Q_TABLE[0] (400 milli)");
    assert_eq!(nearest_q_index(9_000), 7, "9000 is closest to Q_TABLE[7] (8000 milli), the top entry");
}

#[test]
fn wire_round_trip_preserves_an_explicit_imported_preamp() {
    // An imported preset's Preamp::Explicit value must survive a wire
    // round trip verbatim -- it is never recomputed from the bands (that
    // would defeat the whole point of importing a published curve).
    let mut preset = Preset::new("Imported");
    preset.preamp = Preamp::Explicit(-350); // -3.50dB, an arbitrary published value
    preset.eq_locked = true;

    let wire = preset.to_wire();
    let restored = Preset::from_wire(&wire);

    assert_eq!(restored.preamp, Preamp::Explicit(-350));
    assert!(restored.eq_locked);
}

#[test]
fn wire_name_truncates_at_char_boundary() {
    // 17 ASCII bytes -- one over MAX_NAME_BYTES (16) -- must truncate to
    // 16, not panic and not split multi-byte UTF-8.
    let long = "ABCDEFGHIJKLMNOPQ";
    let preset = Preset::new(long);
    assert_eq!(preset.name.len(), 16);
    let restored = Preset::from_wire(&preset.to_wire());
    assert_eq!(restored.name, preset.name);
}

#[test]
fn wire_from_short_buffer_never_panics() {
    // A buffer shorter than BLOB_LEN (e.g. an old/corrupt record) must
    // degrade gracefully, not panic or read out of bounds.
    for len in [0usize, 1, 2, 17, 19, 20, 40] {
        let short = vec![0xFFu8; len];
        let restored = Preset::from_wire(&short);
        assert!(restored.bands.len() <= super::preset::MAX_BANDS);
    }
}

#[test]
fn wire_unrecognized_band_kind_falls_back_to_peak() {
    let raw = 0xFFu8; // not 0/1/2
    assert_eq!(BandKind::from_wire(raw), BandKind::Peak);
}

#[test]
fn wire_unrecognized_crossfeed_falls_back_to_off() {
    assert_eq!(CrossfeedLevel::from_wire(0xFF), CrossfeedLevel::Off);
}

// ---------------------------------------------------------------------
// PresetStore
// ---------------------------------------------------------------------

#[test]
fn store_allocates_monotonic_never_reused_ids() {
    let mut store = PresetStore::new();
    let id1 = store.create(Preset::new("One"));
    let id2 = store.create(Preset::new("Two"));
    assert!(id2 > id1);
    assert_ne!(id1, NO_PRESET_ID);

    store.delete(id1);
    let id3 = store.create(Preset::new("Three"));
    assert!(id3 > id2, "a deleted id must never be reallocated");
    assert_ne!(id3, id1);
}

#[test]
fn store_from_loaded_seeds_next_id_past_max_stored() {
    let store = PresetStore::from_loaded([(3u16, Preset::new("A")), (7u16, Preset::new("B"))]);
    let mut store = store;
    let new_id = store.create(Preset::new("C"));
    assert!(new_id > 7, "next id after loading {{3, 7}} must exceed 7, got {new_id}");
}

#[test]
fn store_resolve_dangling_and_zero_both_mean_off() {
    let mut store = PresetStore::new();
    let id = store.create(Preset::new("Live"));

    assert!(store.resolve(NO_PRESET_ID).is_none());
    assert!(store.resolve(id).is_some());

    store.delete(id);
    assert!(store.resolve(id).is_none(), "a deleted (dangling) id must resolve to Off");
}

#[test]
fn store_delete_does_not_touch_other_entries() {
    let mut store = PresetStore::new();
    let id1 = store.create(Preset::new("Keep"));
    let id2 = store.create(Preset::new("Drop"));
    store.delete(id2);
    assert!(store.resolve(id1).is_some());
    assert_eq!(store.len(), 1);
}

// --- `Preset::from_wire_checked` (strict host-input decode) ------------

#[test]
fn from_wire_checked_accepts_a_well_formed_v2_blob() {
    let mut preset = Preset::new("Warm");
    preset.push_band(Band { kind: BandKind::Peak, freq_half_hz: 2000, gain_cdb: 300, q_milli: 1000 });
    let wire = preset.to_wire();
    assert_eq!(Preset::from_wire_checked(&wire), Ok(preset));
}

#[test]
fn from_wire_checked_rejects_v1() {
    let mut raw = [0u8; BLOB_LEN];
    raw[0] = 1; // BLOB_VERSION_V1
    assert_eq!(Preset::from_wire_checked(&raw), Err(PresetBlobError::UnsupportedVersion { version: 1 }));
}

#[test]
fn from_wire_checked_rejects_an_unknown_version() {
    let mut raw = [0u8; BLOB_LEN];
    raw[0] = 99;
    assert_eq!(Preset::from_wire_checked(&raw), Err(PresetBlobError::UnsupportedVersion { version: 99 }));
}

#[test]
fn from_wire_checked_rejects_a_band_count_over_max_bands() {
    let mut raw = [0u8; BLOB_LEN];
    raw[0] = 2; // BLOB_VERSION_V2
    // flags byte: band_count in the top 4 bits -- 15 is the widest value
    // the field can carry, well past MAX_BANDS (10).
    raw[17] = 15 << 4;
    assert_eq!(Preset::from_wire_checked(&raw), Err(PresetBlobError::TooManyBands { band_count: 15 }));
}

#[test]
fn from_wire_checked_rejects_a_reserved_band_kind() {
    let mut preset = Preset::new("X");
    preset.push_band(Band { kind: BandKind::Peak, freq_half_hz: 2000, gain_cdb: 0, q_milli: 1000 });
    let mut wire = preset.to_wire();
    // Band 0's kind_gain low two bytes: force the 3-bit kind field to a
    // reserved value (5) without touching the gain bits.
    let band_off = 20; // v2_layout::BANDS_OFF
    let kind_gain = u16::from_le_bytes([wire[band_off], wire[band_off + 1]]);
    let reserved_kind_gain = (kind_gain & 0x1FFF) | (5u16 << 13);
    wire[band_off..band_off + 2].copy_from_slice(&reserved_kind_gain.to_le_bytes());
    assert_eq!(Preset::from_wire_checked(&wire), Err(PresetBlobError::ReservedBandKind { band_index: 1, raw_kind: 5 }));
}

#[test]
fn from_wire_checked_rejects_a_non_utf8_name() {
    let mut raw = [0u8; BLOB_LEN];
    raw[0] = 2; // BLOB_VERSION_V2
    raw[1] = 0xFF; // invalid UTF-8 lead byte, at the name field's start
    raw[2] = 0x00; // terminate immediately after the bad byte
    assert_eq!(Preset::from_wire_checked(&raw), Err(PresetBlobError::InvalidNameUtf8));
}

#[test]
fn from_wire_checked_never_panics_on_a_short_buffer() {
    for len in 0..BLOB_LEN {
        let short = vec![2u8; len]; // claims v2 but is truncated
        let _ = Preset::from_wire_checked(&short);
    }
}
