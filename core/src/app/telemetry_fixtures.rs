//! Fixture-emitting tests for the web companion (bead `pico-link-jyhk.9`,
//! "FERN DESIGN (web-native)" comment on `pico-link-jyhk.8`, section 5:
//! "JS/RUST SYNC: CORE EMITS, JS ASSERTS"). Nothing in the future `web/`
//! tree retypes a Rust constant or wire offset by hand -- it reads these
//! committed files instead, and this module is what keeps them honest:
//! every fixture below is generated straight from the same code the wire
//! format and ballistics maths actually run, so a change to either shows
//! up as a failing `cargo test` here, not as a silent drift a browser
//! discovers first.
//!
//! # Check mode vs. regenerate mode
//!
//! By default (plain `cargo test`) every test in this module reads back
//! its `fixtures/telemetry/*` file and asserts it byte-for-byte (or
//! text-for-text) matches what the current code produces -- a stale
//! fixture is a test failure, not a warning. To regenerate the committed
//! files after a deliberate change, run:
//!
//! ```text
//! UPDATE_FIXTURES=1 cargo test -p pico-link-core telemetry_fixtures
//! ```
//!
//! and review the resulting `git diff` before committing -- exactly the
//! same discipline this crate's PNG screenshot fixtures already use
//! elsewhere in the repo, just for binary/JSON files instead of images.
//!
//! # What lives here vs. what doesn't
//!
//! `fixtures/telemetry/home-*.bin`/`.json` pairs -- one representative
//! [`super::telemetry::encode_home_snapshot`] scenario per pair, plus its
//! decoded form -- and `fixtures/telemetry/meter-trace.json` -- a scripted
//! [`App::on_levels_changed`] timeline plus render-time
//! [`super::model::decay_peak`] queries -- and `fixtures/telemetry/
//! constants.json` -- the handful of named constants a host-side ballistics
//! port needs and must not hand-copy. `fixtures/telemetry/info-v1.bin`
//! (GET_INFO's golden reply) is deliberately NOT emitted here: that struct
//! (`pl_cfg_info_wire_t`, `firmware/src/usb_config_itf.h`) has no Rust
//! owner, so it's a hand-written fixture backed by a C
//! `_Static_assert` on the struct's size instead -- see that header's doc
//! comment. The vertical meter's segment count and colour zones
//! (`crate::render::theme::VERTICAL_METER_SEGMENT_COUNT` and friends) are
//! deliberately NOT fixture-exported: bead pico-link-5ful is about to
//! change both, so only the underlying ballistics
//! ([`super::model::decay_peak`], `RELEASE_RATIO_PER_MS_Q16`) are pinned
//! here.

#![cfg(test)]

use alloc::string::{String, ToString};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use super::model::{decay_peak, ConnectedCodec, OutLevelSample, PairedDevice, OUT_LEVEL_HOLD_DURATION, RELEASE_RATIO_PER_MS_Q16};
use super::telemetry::{decode_home_snapshot, encode_home_snapshot, DecodedFault, HomeSnapshot, HomeSnapshotExtras, HOME_SNAPSHOT_LEN, TELEMETRY_PAGE_HOME, TELEMETRY_PROTO};
use super::{App, BtModel, FaultKey, FaultValue, VolumeSource, VolumeState};
use crate::dsp::{Preset, PresetStore};
use crate::render::hero::{OUT_LEVEL_REFRESH_INTERVAL, OUT_LEVEL_STALE_AFTER};
use crate::render::Instant;
use crate::run::{FAULT_LIVE_WINDOW, FAULT_RETIRE};

// --- Fixture I/O: check by default, regenerate under UPDATE_FIXTURES=1 -

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("fixtures").join("telemetry")
}

/// Writes (under `UPDATE_FIXTURES=1`) or asserts against (otherwise) one
/// binary fixture file. `rel_name` is just the file name -- every fixture
/// this module owns lives flat in [`fixtures_dir`].
fn check_or_write_bytes(rel_name: &str, actual: &[u8]) {
    let path = fixtures_dir().join(rel_name);
    if env::var_os("UPDATE_FIXTURES").is_some() {
        fs::create_dir_all(path.parent().expect("fixtures dir has a parent")).expect("create fixtures/telemetry");
        fs::write(&path, actual).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
        return;
    }
    let expected = fs::read(&path).unwrap_or_else(|e| {
        panic!("missing fixture {} ({e}) -- run `UPDATE_FIXTURES=1 cargo test -p pico-link-core telemetry_fixtures` and commit the result", path.display());
    });
    assert_eq!(
        actual,
        expected.as_slice(),
        "fixture {} is stale -- rerun `UPDATE_FIXTURES=1 cargo test -p pico-link-core telemetry_fixtures`, review the diff, and commit it",
        path.display()
    );
}

/// Same contract as [`check_or_write_bytes`], for the hand-rolled JSON
/// text fixtures (no serde in `core` -- see this module's doc comment).
fn check_or_write_text(rel_name: &str, actual: &str) {
    let path = fixtures_dir().join(rel_name);
    if env::var_os("UPDATE_FIXTURES").is_some() {
        fs::create_dir_all(path.parent().expect("fixtures dir has a parent")).expect("create fixtures/telemetry");
        fs::write(&path, actual).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("missing fixture {} ({e}) -- run `UPDATE_FIXTURES=1 cargo test -p pico-link-core telemetry_fixtures` and commit the result", path.display());
    });
    assert_eq!(
        actual,
        expected,
        "fixture {} is stale -- rerun `UPDATE_FIXTURES=1 cargo test -p pico-link-core telemetry_fixtures`, review the diff, and commit it",
        path.display()
    );
}

// --- A ~40-line hand-rolled JSON writer (design: "no serde in core") ----

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&std::format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn json_string(s: &str) -> String {
    std::format!("\"{}\"", json_escape(s))
}

fn json_bool(b: bool) -> &'static str {
    if b {
        "true"
    } else {
        "false"
    }
}

fn json_str_array(values: &[&str]) -> String {
    let items: Vec<String> = values.iter().map(|s| json_string(s)).collect();
    std::format!("[{}]", items.join(", "))
}

// --- HomeSnapshot -> JSON (the "expected decode" half of each pair) ----

fn fault_value_kind_label(value: Option<FaultValue>) -> &'static str {
    match value {
        None => "none",
        Some(FaultValue::Ratio(_)) => "ratio",
        Some(FaultValue::Count(_)) => "count",
        Some(FaultValue::Millis(_)) => "millis",
    }
}

fn fault_value_number(value: Option<FaultValue>) -> u16 {
    match value {
        None => 0,
        Some(FaultValue::Ratio(v) | FaultValue::Count(v) | FaultValue::Millis(v)) => v,
    }
}

fn decoded_fault_json(fault: Option<DecodedFault>) -> String {
    match fault {
        None => "null".to_string(),
        Some(f) => std::format!(
            "{{\"count\": {}, \"first_seen_ms\": {}, \"last_seen_ms\": {}, \"value_kind\": {}, \"value\": {}}}",
            f.count,
            f.first_seen_ms,
            f.last_seen_ms,
            json_string(fault_value_kind_label(f.value)),
            fault_value_number(f.value)
        ),
    }
}

fn volume_source_label(source: VolumeSource) -> &'static str {
    match source {
        VolumeSource::Host => "host",
        VolumeSource::Sink => "sink",
        VolumeSource::Device => "device",
    }
}

/// Renders a decoded snapshot as pretty-ish, deterministic JSON -- the
/// "expected output" half of each `home-*.bin`/`.json` pair: a web-side
/// decoder must produce exactly this from the paired `.bin`'s bytes.
fn snapshot_json(bytes: &[u8], snap: &HomeSnapshot) -> String {
    let faults: Vec<String> = snap.faults.iter().map(|f| decoded_fault_json(*f)).collect();
    std::format!(
        "{{\n  \"wire_len\": {},\n  \"uptime_ms\": {},\n  \"snap_seq\": {},\n  \"link_connected\": {},\n  \"codec_word\": {},\n  \"kbps\": {},\n  \"kbps_adaptive\": {},\n  \"kbps_is_live\": {},\n  \"device_name\": {},\n  \"fx_preset_name\": {},\n  \"volume_present\": {},\n  \"volume_level\": {},\n  \"volume_muted\": {},\n  \"volume_source\": {},\n  \"level_present\": {},\n  \"peak_l\": {},\n  \"peak_r\": {},\n  \"rms_l\": {},\n  \"rms_r\": {},\n  \"received_ms\": {},\n  \"faults\": [\n    {}\n  ],\n  \"library_rev\": {},\n  \"host_preview_active\": {},\n  \"device_editor_open\": {},\n  \"device_editor_effect_id\": {},\n  \"presets_ready\": {},\n  \"codec_fallback_reason\": {}\n}}\n",
        bytes.len(),
        snap.uptime_ms,
        snap.snap_seq,
        json_bool(snap.link_connected),
        json_string(&snap.codec_word),
        snap.kbps,
        json_bool(snap.kbps_adaptive),
        json_bool(snap.kbps_is_live),
        json_string(&snap.device_name),
        json_string(&snap.fx_preset_name),
        json_bool(snap.volume_present),
        snap.volume_level,
        json_bool(snap.volume_muted),
        json_string(volume_source_label(snap.volume_source)),
        json_bool(snap.level_present),
        snap.peak_l,
        snap.peak_r,
        snap.rms_l,
        snap.rms_r,
        snap.received_ms,
        faults.join(",\n    "),
        snap.library_rev,
        json_bool(snap.host_preview_active),
        json_bool(snap.device_editor_open),
        snap.device_editor_effect_id,
        json_bool(snap.presets_ready),
        snap.codec_fallback_reason,
    )
}

/// A neutral [`HomeSnapshotExtras`] -- every `emit_home_*` fixture below
/// that isn't specifically about the `163..169` append (bead
/// `pico-link-jyhk.18`) uses this, so the pre-existing fixtures' bytes stay
/// stable past offset 163 (all-zero/`false` tail) and only their length
/// changes (163 -> [`HOME_SNAPSHOT_LEN`], 169).
fn no_extras() -> HomeSnapshotExtras {
    HomeSnapshotExtras { library_rev: 0, host_preview_active: false, device_editor_open: false, device_editor_effect_id: 0, presets_ready: false, codec_fallback_reason: 0 }
}

/// Encodes `model`/`presets` at `(now, snap_seq)` with [`no_extras`],
/// decodes the result back (asserting the round trip -- a bad fixture must
/// never ship even in `UPDATE_FIXTURES=1` mode), and checks/writes both
/// halves of one `home-<name>.bin`/`.json` pair.
fn emit_snapshot_fixture(name: &str, model: &BtModel, presets: &PresetStore, now: Instant, snap_seq: u32) {
    emit_snapshot_fixture_with_extras(name, model, presets, now, snap_seq, &no_extras());
}

/// Same as [`emit_snapshot_fixture`], but with caller-supplied `extras` --
/// for fixtures specifically about the `163..169` append.
fn emit_snapshot_fixture_with_extras(name: &str, model: &BtModel, presets: &PresetStore, now: Instant, snap_seq: u32, extras: &HomeSnapshotExtras) {
    let bytes = encode_home_snapshot(model, presets, now, snap_seq, extras);
    let snap = decode_home_snapshot(&bytes).expect("a well-formed encode must always decode");
    check_or_write_bytes(&std::format!("home-{name}.bin"), &bytes);
    check_or_write_text(&std::format!("home-{name}.json"), &snapshot_json(&bytes, &snap));
}

fn addr(last_byte: u8) -> [u8; 6] {
    [0x94, 0xDB, 0x56, 0x54, 0x7C, last_byte]
}

#[test]
fn emit_home_golden() {
    // The same scenario `telemetry::tests::golden_bytes_layout_is_stable`
    // locks byte-for-byte -- see that test's doc comment for why this
    // exact shape (connected LDAC, adaptive, live kbps, volume, out level,
    // one Millis fault) is "golden".
    let mut model = BtModel::default();
    let mut presets = PresetStore::new();
    let preset_id = presets.create(Preset::new("Warm"));

    model.paired.push(PairedDevice { addr: addr(0xF2), name: "Pixel Buds".to_string(), mru_seq: 3, ldac_quality: 4, preset_id });
    model.connected_addr = Some(addr(0xF2));
    model.connected_codec = Some(ConnectedCodec { addr: addr(0xF2), word: "LDAC".to_string(), nominal_bitrate_bps: 990_000 });
    model.ldac_live_kbps = Some(909);
    model.volume = Some(VolumeState { level: 100, muted: false, source: VolumeSource::Host });
    model.out_level = Some(sample_out_level(Instant::from_micros(12_345_000)));
    model.fault_log.record(FaultKey::BufStarved, Instant::from_micros(1_000_000), Some(FaultValue::Millis(0)), 3);

    emit_snapshot_fixture("golden", &model, &presets, Instant::from_micros(20_000_000), 0xDEAD_BEEF);
}

#[test]
fn emit_home_empty_no_link() {
    let model = BtModel::default();
    let presets = PresetStore::new();
    emit_snapshot_fixture("empty-no-link", &model, &presets, Instant::from_micros(1_500_000), 5);
}

#[test]
fn emit_home_not_ready() {
    // snap_seq 0 means "not ready" REGARDLESS of what the model otherwise
    // holds (design: "0 means not ready") -- populate the model like a
    // real connected session to prove the host must key off `snap_seq`,
    // not "does the payload look empty".
    let mut model = BtModel::default();
    let mut presets = PresetStore::new();
    let preset_id = presets.create(Preset::new("Bright"));
    model.paired.push(PairedDevice { addr: addr(0x10), name: "Not Ready Buds".to_string(), mru_seq: 1, ldac_quality: 0, preset_id });
    model.connected_addr = Some(addr(0x10));
    model.connected_codec = Some(ConnectedCodec { addr: addr(0x10), word: "AAC".to_string(), nominal_bitrate_bps: 256_000 });
    emit_snapshot_fixture("not-ready", &model, &presets, Instant::from_micros(500_000), 0);
}

#[test]
fn emit_home_muted_sink_volume() {
    let mut model = BtModel::default();
    let presets = PresetStore::new();
    model.volume = Some(VolumeState { level: 42, muted: true, source: VolumeSource::Sink });
    emit_snapshot_fixture("muted-sink-volume", &model, &presets, Instant::from_micros(2_000_000), 9);
}

#[test]
fn emit_home_non_ldac() {
    let mut model = BtModel::default();
    let mut presets = PresetStore::new();
    let preset_id = presets.create(Preset::new("Flat"));
    model.paired.push(PairedDevice { addr: addr(0x01), name: "SBC Buds".to_string(), mru_seq: 1, ldac_quality: 4, preset_id });
    model.connected_addr = Some(addr(0x01));
    model.connected_codec = Some(ConnectedCodec { addr: addr(0x01), word: "SBC".to_string(), nominal_bitrate_bps: 328_000 });
    // A leftover live LDAC figure must never leak onto a non-LDAC codec --
    // see `telemetry::tests::encode_connected_non_ldac_never_reports_
    // live_or_adaptive` for the same guard.
    model.ldac_live_kbps = Some(909);
    emit_snapshot_fixture("non-ldac", &model, &presets, Instant::from_micros(3_000_000), 11);
}

#[test]
fn emit_home_fault_value_kinds() {
    // One of each `value_kind` this proto defines (0 none, 1 ratio, 2
    // count, 3 millis -- see `telemetry`'s module doc comment on the
    // deviation that added `3`), plus two never-raised keys left absent.
    let mut model = BtModel::default();
    let presets = PresetStore::new();
    let t1 = Instant::from_micros(1_000_000);
    let t2 = Instant::from_micros(4_000_000);
    model.fault_log.record(FaultKey::BufStarved, t1, Some(FaultValue::Millis(120)), 3);
    model.fault_log.record(FaultKey::UsbSupplyLow, t2, Some(FaultValue::Ratio(200)), 1);
    model.fault_log.record(FaultKey::AirLinkLost, t2, None, 2);
    model.fault_log.record(FaultKey::EncResync, t2, Some(FaultValue::Count(9)), 9);
    // BufOverflow/AirCongested left never-raised.
    emit_snapshot_fixture("fault-value-kinds", &model, &presets, t2, 13);
}

#[test]
fn emit_home_max_length_multibyte_names() {
    // Multi-byte UTF-8 strings that overflow their wire cap on a
    // NON-boundary byte count, forcing `telemetry::truncate_utf8` to walk
    // back to the nearest char boundary rather than landing exactly on the
    // cap -- see that function's doc comment. "\u{20AC}" (EUR sign) is 3
    // bytes; 11 repeats is 33 bytes against the 32-byte device-name cap
    // (32 isn't a multiple of 3). "\u{6C38}" is also 3 bytes; 3 repeats is
    // 9 bytes against the 8-byte codec-word cap.
    let mut model = BtModel::default();
    let mut presets = PresetStore::new();
    let long_name: String = "\u{20AC}".repeat(11);
    let long_codec_word: String = "\u{6C38}".repeat(3);
    // The FX preset name is validated/truncated at `Preset::new` time
    // (`dsp::preset::truncate_name`), so this exercises the SAME
    // multi-byte-safe truncation logic one layer up rather than at the
    // wire -- pick a name already at/under the 16-byte cap so the wire
    // path here is a plain round trip, not a second truncation.
    let preset_id = presets.create(Preset::new("\u{20AC}\u{20AC}\u{20AC}\u{20AC}\u{20AC}"));
    model.paired.push(PairedDevice { addr: addr(0x77), name: long_name, mru_seq: 1, ldac_quality: 0, preset_id });
    model.connected_addr = Some(addr(0x77));
    model.connected_codec = Some(ConnectedCodec { addr: addr(0x77), word: long_codec_word, nominal_bitrate_bps: 328_000 });
    emit_snapshot_fixture("max-length-multibyte-names", &model, &presets, Instant::from_micros(6_000_000), 17);
}

#[test]
fn emit_home_trailing_bytes() {
    // Proves the append-only-proto contract at the fixture level: a future
    // proto's appended bytes past `HOME_SNAPSHOT_LEN` must decode
    // identically to the same payload without them (design: "trailing
    // bytes past len (append-only rule)"). The `.bin` carries 16 extra
    // 0xAA bytes; the `.json` is the decode of the FIRST `HOME_SNAPSHOT_
    // LEN` bytes only, exactly what `decode_home_snapshot` returns.
    let mut model = BtModel::default();
    let presets = PresetStore::new();
    model.volume = Some(VolumeState { level: 10, muted: false, source: VolumeSource::Host });
    let now = Instant::from_micros(7_000_000);
    let base = encode_home_snapshot(&model, &presets, now, 21, &no_extras());
    let snap = decode_home_snapshot(&base).expect("a well-formed encode must always decode");

    let mut bytes = base.to_vec();
    bytes.extend_from_slice(&[0xAA; 16]);
    assert_eq!(decode_home_snapshot(&bytes), Some(snap.clone()), "trailing bytes must not change the decode");

    check_or_write_bytes("home-trailing-bytes.bin", &bytes);
    check_or_write_text("home-trailing-bytes.json", &snapshot_json(&base, &snap));
}

#[test]
fn emit_home_library_extras() {
    // The `163..169` append itself (bead `pico-link-jyhk.18`, design
    // section 8): every `flags2` bit set, a nonzero `library_rev`/
    // `device_editor_effect_id`, and a placeholder nonzero
    // `codec_fallback_reason` byte to prove the field round-trips even
    // though nothing in `App` sources a real one yet (see
    // `HomeSnapshotExtras::codec_fallback_reason`'s doc comment).
    let model = BtModel::default();
    let presets = PresetStore::new();
    let extras = HomeSnapshotExtras { library_rev: 0xBEEF, host_preview_active: true, device_editor_open: true, device_editor_effect_id: 7, presets_ready: true, codec_fallback_reason: 1 };
    emit_snapshot_fixture_with_extras("library-extras", &model, &presets, Instant::from_micros(8_000_000), 23, &extras);
}

#[test]
fn header_constants_match_this_proto() {
    // Cheap belt-and-braces: the fixtures above are only meaningful if
    // these two constants are what this module's doc comment (and every
    // `home-*.bin`'s first four bytes) assumes.
    assert_eq!(TELEMETRY_PROTO, 1);
    assert_eq!(TELEMETRY_PAGE_HOME, 0);
    assert_eq!(HOME_SNAPSHOT_LEN, 169);
}

fn sample_out_level(received_at: Instant) -> OutLevelSample {
    OutLevelSample {
        peak_l: 200,
        peak_r: 190,
        rms_l: 120,
        rms_r: 110,
        hold_l: 200,
        hold_r: 190,
        hold_l_at: received_at,
        hold_r_at: received_at,
        received_at,
        attack_peak_l: 200,
        attack_peak_r: 190,
        attack_peak_l_at: received_at,
        attack_peak_r_at: received_at,
    }
}

// --- Meter ballistics trace: on_levels_changed + decay_peak -------------

/// One scripted `LevelsChanged`-style fold, at `at_ms` on the app's own
/// clock.
struct Sample {
    at_ms: u64,
    peak_l: u8,
    peak_r: u8,
    rms_l: u8,
    rms_r: u8,
}

/// One render-time query: "if the host rendered right now, at `at_ms`,
/// against whatever the last fold above produced, what would it show?"
/// Deliberately NOT coupled to `App::tick` -- these read the already-
/// folded [`OutLevelSample`] fields directly and recompute the exact same
/// way [`crate::render::hero::HeroStatusView`] does at render time (see
/// [`decay_peak`]'s doc comment: pure, render-time, never mutates stored
/// state), so a query instant can freely fall between, or after, any
/// sample without perturbing the fold history.
struct Query {
    at_ms: u64,
}

fn query_json(at_ms: u64, out_level: &OutLevelSample) -> String {
    let now = Instant::from_micros(at_ms * 1_000);
    let elapsed_l = now.saturating_duration_since(out_level.attack_peak_l_at);
    let elapsed_r = now.saturating_duration_since(out_level.attack_peak_r_at);
    let displayed_l = decay_peak(out_level.attack_peak_l, elapsed_l);
    let displayed_r = decay_peak(out_level.attack_peak_r, elapsed_r);
    let received_ms = out_level.received_at.as_micros() / 1_000;
    #[allow(clippy::cast_possible_truncation)]
    let stale = now.saturating_duration_since(out_level.received_at) >= OUT_LEVEL_STALE_AFTER;
    // Deliberately NOT emitting a segment/zone count here: the vertical
    // meter's segment count and colour zones are about to change (bead
    // pico-link-5ful) and this trace must not bake in geometry that will
    // go stale the moment that lands. Ballistics (decay_peak's displayed
    // peak, the hold cap, staleness) is the stable contract a host port
    // needs; segment mapping is a separate, still-moving fixture.
    std::format!(
        "{{\"at_ms\": {}, \"received_ms\": {}, \"stale\": {}, \"displayed_peak_l\": {}, \"displayed_peak_r\": {}, \"hold_l\": {}, \"hold_r\": {}}}",
        at_ms,
        received_ms,
        json_bool(stale),
        displayed_l,
        displayed_r,
        out_level.hold_l,
        out_level.hold_r,
    )
}

#[test]
fn emit_meter_trace() {
    // A hand-picked timeline exercising: the very first sample (anchors
    // start at 0), an instantaneous attack on both channels, a big drop on
    // one channel while the other barely moves (attack anchor NOT
    // re-triggered -- `on_levels_changed`'s "only a rise re-anchors"
    // rule), continued release, and a gap long enough to both go stale
    // (`OUT_LEVEL_STALE_AFTER`, 200ms) and expire the peak-hold cap
    // (`OUT_LEVEL_HOLD_DURATION`, 1500ms).
    let samples = [
        Sample { at_ms: 0, peak_l: 50, peak_r: 40, rms_l: 30, rms_r: 25 },
        Sample { at_ms: 100, peak_l: 200, peak_r: 180, rms_l: 150, rms_r: 140 },
        Sample { at_ms: 250, peak_l: 30, peak_r: 170, rms_l: 20, rms_r: 130 },
        Sample { at_ms: 600, peak_l: 10, peak_r: 10, rms_l: 5, rms_r: 5 },
        Sample { at_ms: 2_000, peak_l: 5, peak_r: 5, rms_l: 2, rms_r: 2 },
    ];
    let queries = [
        Query { at_ms: 0 },
        Query { at_ms: 50 },
        Query { at_ms: 150 },
        Query { at_ms: 260 },
        Query { at_ms: 400 },
        Query { at_ms: 700 },
        Query { at_ms: 900 },
        Query { at_ms: 2_000 },
        Query { at_ms: 2_100 },
        Query { at_ms: 2_300 },
    ];

    let mut app = App::new(240, 240);
    // One (sample, [queries strictly before the next sample]) step at a
    // time, so each query reads the fold state that was actually current
    // at that instant -- not a later one.
    let mut query_idx = 0usize;
    let mut steps: Vec<String> = Vec::new();
    for (i, sample) in samples.iter().enumerate() {
        app.tick(sample.at_ms * 1_000);
        app.on_levels_changed(sample.peak_l, sample.peak_r, sample.rms_l, sample.rms_r);
        let next_sample_at = samples.get(i + 1).map(|s| s.at_ms);
        while query_idx < queries.len() && next_sample_at.is_none_or(|next| queries[query_idx].at_ms < next) {
            let q = &queries[query_idx];
            assert!(q.at_ms >= sample.at_ms, "query {} precedes the sample it's meant to read", q.at_ms);
            let out_level = app.model.borrow().out_level.expect("on_levels_changed always sets out_level");
            steps.push(query_json(q.at_ms, &out_level));
            query_idx += 1;
        }
    }
    assert_eq!(query_idx, queries.len(), "every query must be consumed by some step in the script");

    let sample_json: Vec<String> = samples
        .iter()
        .map(|s| std::format!("{{\"at_ms\": {}, \"peak_l\": {}, \"peak_r\": {}, \"rms_l\": {}, \"rms_r\": {}}}", s.at_ms, s.peak_l, s.peak_r, s.rms_l, s.rms_r))
        .collect();

    let json = std::format!(
        "{{\n  \"_comment\": \"Scripted App::on_levels_changed timeline + render-time decay_peak queries (bead pico-link-jyhk.9). samples[] are fed to on_levels_changed in order via App::tick(at_ms*1000); queries[] read the OutLevelSample last folded at or before their at_ms and recompute exactly what crate::render::hero::HeroStatusView's ballistics would show if it rendered at that instant -- displayed_peak_* is decay_peak(attack_peak_*, now - attack_peak_*_at), stale is (now - received_ms) >= OUT_LEVEL_STALE_AFTER_MS, hold_* is the raw peak-hold cap (never itself decayed). Deliberately NOT a segment/zone count: the vertical meter's segment count and colour zones are about to change (bead pico-link-5ful), so this fixture sticks to the stable ballistics contract. A host port must match every displayed_peak_*/hold_* bit-exactly -- see constants.json's RELEASE_RATIO_PER_MS_Q16.\",\n  \"samples\": [\n    {}\n  ],\n  \"queries\": [\n    {}\n  ]\n}}\n",
        sample_json.join(",\n    "),
        steps.join(",\n    "),
    );
    check_or_write_text("meter-trace.json", &json);
}

// --- constants.json ------------------------------------------------------
//
// Deliberately NOT emitting VERTICAL_METER_SEGMENT_COUNT/_DBFS_THRESHOLDS or
// a colour-zone map here: the vertical meter's segment count and theme
// palette are about to change (bead pico-link-5ful), so this fixture sticks
// to ballistics and wire constants that are stable across that change.

fn volume_percent_samples() -> Vec<(u8, u8)> {
    // Representative `VolumeState::percent()` inputs -- both documented
    // exact endpoints (0->0, 127->100) plus a handful of interior
    // half-up-rounding cases, all computed from the real method, not
    // hand-derived, so a rounding-rule change here fails this test.
    [0u8, 1, 32, 63, 64, 65, 100, 126, 127]
        .into_iter()
        .map(|level| (level, VolumeState { level, muted: false, source: VolumeSource::Host }.percent()))
        .collect()
}

#[test]
fn emit_constants() {
    let vol_samples = volume_percent_samples();
    let vol_json: Vec<String> = vol_samples.iter().map(|(level, pct)| std::format!("{{\"level\": {level}, \"percent\": {pct}}}")).collect();
    let fault_key_labels: Vec<&str> = FaultKey::ALL.iter().map(|k| k.name()).collect();

    let json = std::format!(
        concat!(
            "{{\n",
            "  \"_comment\": \"Named constants a host-side ballistics/decode port must use verbatim rather than retype (bead pico-link-jyhk.9, FERN DESIGN section 5). Regenerate with UPDATE_FIXTURES=1.\",\n",
            "  \"TELEMETRY_PROTO\": {},\n",
            "  \"HOME_SNAPSHOT_LEN\": {},\n",
            "  \"OUT_LEVEL_STALE_AFTER_MS\": {},\n",
            "  \"OUT_LEVEL_REFRESH_INTERVAL_MS\": {},\n",
            "  \"OUT_LEVEL_HOLD_DURATION_MS\": {},\n",
            "  \"RELEASE_RATIO_PER_MS_Q16\": {},\n",
            "  \"FAULT_LIVE_WINDOW_MS\": {},\n",
            "  \"FAULT_RETIRE_MS\": {},\n",
            "  \"FAULT_KEY_ORDER\": {},\n",
            "  \"VOLUME_PERCENT_SAMPLES\": [\n    {}\n  ]\n",
            "}}\n"
        ),
        TELEMETRY_PROTO,
        HOME_SNAPSHOT_LEN,
        OUT_LEVEL_STALE_AFTER.as_millis(),
        OUT_LEVEL_REFRESH_INTERVAL.as_millis(),
        OUT_LEVEL_HOLD_DURATION.as_millis(),
        RELEASE_RATIO_PER_MS_Q16,
        FAULT_LIVE_WINDOW.as_millis(),
        FAULT_RETIRE.as_millis(),
        json_str_array(&fault_key_labels),
        vol_json.join(",\n    "),
    );
    check_or_write_text("constants.json", &json);
}
