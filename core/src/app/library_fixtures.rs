//! Fixture-emitting tests for the web companion's `GET_LIBRARY` (`0x05`)
//! snapshot (bead `pico-link-jyhk.18`, "ADA DESIGN" comment on
//! `pico-link-jyhk.17`, section 3), mirroring the shape
//! [`super::telemetry_fixtures`] already established for page-0 telemetry
//! (FERN DESIGN section 5: "core emits, JS asserts"). Every fixture below
//! is generated straight from [`super::library::encode_library_snapshot`],
//! the same code the wire format actually runs, so a layout change shows up
//! as a failing `cargo test` here, not as a silent drift a browser
//! discovers first.
//!
//! # Check mode vs. regenerate mode
//!
//! Same contract as [`super::telemetry_fixtures`]: plain `cargo test`
//! checks every fixture byte-for-byte/text-for-text against
//! `fixtures/library/*`; a stale fixture is a test failure. To regenerate
//! after a deliberate change:
//!
//! ```text
//! UPDATE_FIXTURES=1 cargo test -p pico-link-core library_fixtures
//! ```
//!
//! and review the resulting `git diff` before committing.
//!
//! # Why a separate `fixtures/library/` directory
//!
//! `GET_LIBRARY` is its own request (`lib_proto`, not `TELEMETRY_PROTO`)
//! with its own web session API landing later (`pico-link-jyhk.22`), so its
//! fixtures live in their own directory rather than growing
//! `fixtures/telemetry/`'s existing `home-*` naming convention with a
//! second, unrelated prefix.

#![cfg(test)]

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use super::library::{decode_library_snapshot, encode_library_snapshot, DecodedDevice, DecodedEffect, LibrarySnapshot, DEVICE_RECORD_LEN, EFFECT_RECORD_LEN, LIBRARY_PROTO, LIBRARY_SNAPSHOT_MAX_LEN};
use super::model::{PairedDevice, MAX_PAIRED_DEVICES};
use crate::dsp::{Preset, PresetStore, MAX_PRESETS};

// --- Fixture I/O: check by default, regenerate under UPDATE_FIXTURES=1 -

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("fixtures").join("library")
}

/// Same contract as [`super::telemetry_fixtures::check_or_write_bytes`].
fn check_or_write_bytes(rel_name: &str, actual: &[u8]) {
    let path = fixtures_dir().join(rel_name);
    if env::var_os("UPDATE_FIXTURES").is_some() {
        fs::create_dir_all(path.parent().expect("fixtures dir has a parent")).expect("create fixtures/library");
        fs::write(&path, actual).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
        return;
    }
    let expected = fs::read(&path).unwrap_or_else(|e| {
        panic!("missing fixture {} ({e}) -- run `UPDATE_FIXTURES=1 cargo test -p pico-link-core library_fixtures` and commit the result", path.display());
    });
    assert_eq!(
        actual,
        expected.as_slice(),
        "fixture {} is stale -- rerun `UPDATE_FIXTURES=1 cargo test -p pico-link-core library_fixtures`, review the diff, and commit it",
        path.display()
    );
}

/// Same contract as [`super::telemetry_fixtures::check_or_write_text`].
fn check_or_write_text(rel_name: &str, actual: &str) {
    let path = fixtures_dir().join(rel_name);
    if env::var_os("UPDATE_FIXTURES").is_some() {
        fs::create_dir_all(path.parent().expect("fixtures dir has a parent")).expect("create fixtures/library");
        fs::write(&path, actual).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("missing fixture {} ({e}) -- run `UPDATE_FIXTURES=1 cargo test -p pico-link-core library_fixtures` and commit the result", path.display());
    });
    assert_eq!(
        actual,
        expected,
        "fixture {} is stale -- rerun `UPDATE_FIXTURES=1 cargo test -p pico-link-core library_fixtures`, review the diff, and commit it",
        path.display()
    );
}

// --- A ~40-line hand-rolled JSON writer (design: "no serde in core") ----
// Duplicated from `telemetry_fixtures` rather than shared across a
// `#[cfg(test)]`-only module boundary for the same one-caller-each reason
// `library.rs`'s own `truncate_utf8`/`write_fixed_str` duplicate theirs.

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

// --- LibrarySnapshot -> JSON (the "expected decode" half of each pair) --

fn decoded_effect_json(effect: &DecodedEffect) -> String {
    std::format!(
        "{{\"id\": {}, \"persisted_seq\": {}, \"preset_name\": {}}}",
        effect.id,
        effect.persisted_seq,
        json_string(&effect.preset.name),
    )
}

fn decoded_device_json(device: &DecodedDevice) -> String {
    std::format!(
        "{{\"addr\": {}, \"preset_id\": {}, \"connected\": {}, \"name\": {}}}",
        json_string(&device.addr.iter().map(|b| std::format!("{b:02X}")).collect::<Vec<_>>().join(":")),
        device.preset_id,
        json_bool(device.connected),
        json_string(&device.name),
    )
}

/// Renders a decoded snapshot as deterministic JSON -- the "expected
/// output" half of each `library-*.bin`/`.json` pair: a web-side decoder
/// must produce exactly this from the paired `.bin`'s bytes.
fn snapshot_json(bytes: &[u8], snap: &LibrarySnapshot) -> String {
    let effects: Vec<String> = snap.effects.iter().map(decoded_effect_json).collect();
    let devices: Vec<String> = snap.devices.iter().map(decoded_device_json).collect();
    std::format!(
        "{{\n  \"wire_len\": {},\n  \"library_rev\": {},\n  \"presets_ready\": {},\n  \"max_effects\": {},\n  \"max_devices\": {},\n  \"effects\": [\n    {}\n  ],\n  \"devices\": [\n    {}\n  ]\n}}\n",
        bytes.len(),
        snap.library_rev,
        json_bool(snap.presets_ready),
        snap.max_effects,
        snap.max_devices,
        effects.join(",\n    "),
        devices.join(",\n    "),
    )
}

/// Encodes `presets`/`paired` at `(connected_addr, presets_ready,
/// persisted_seq, library_rev)`, decodes the result back (asserting the
/// round trip -- a bad fixture must never ship even in `UPDATE_FIXTURES=1`
/// mode), and checks/writes both halves of one `library-<name>.bin`/`.json`
/// pair.
#[allow(clippy::too_many_arguments)]
fn emit_library_fixture(name: &str, presets: &PresetStore, paired: &[PairedDevice], connected_addr: Option<[u8; 6]>, presets_ready: bool, persisted_seq: &BTreeMap<u16, u16>, library_rev: u16) {
    let bytes = encode_library_snapshot(presets, paired, connected_addr, presets_ready, persisted_seq, library_rev);
    let snap = decode_library_snapshot(&bytes).expect("a well-formed encode must always decode");
    check_or_write_bytes(&std::format!("library-{name}.bin"), &bytes);
    check_or_write_text(&std::format!("library-{name}.json"), &snapshot_json(&bytes, &snap));
}

fn addr(last_byte: u8) -> [u8; 6] {
    [0x94, 0xDB, 0x56, 0x54, 0x7C, last_byte]
}

#[test]
fn emit_library_empty() {
    let presets = PresetStore::new();
    let seqs = BTreeMap::new();
    emit_library_fixture("empty", &presets, &[], None, false, &seqs, 1);
}

#[test]
fn emit_library_golden() {
    // The same scenario `library::tests::golden_bytes_layout_is_stable`
    // locks byte-for-byte -- see that test's doc comment for why this
    // exact shape (one effect, one connected device assigned to it) is
    // "golden".
    let mut presets = PresetStore::new();
    let id = presets.create(Preset::new("Warm"));
    let mut seqs = BTreeMap::new();
    seqs.insert(id, 2u16);
    let paired = alloc::vec![PairedDevice { addr: addr(0xF2), name: "Pixel Buds".to_string(), mru_seq: 1, ldac_quality: 4, preset_id: id }];

    emit_library_fixture("golden", &presets, &paired, Some(addr(0xF2)), true, &seqs, 5);
}

#[test]
fn emit_library_effect_with_no_persisted_seq_entry() {
    // An id with no `persisted_seq` entry yet (design section 3: "an id
    // with no entry yet... reads back as 0") -- see
    // `super::App::on_preset_loaded`'s doc comment for why this should
    // never actually happen once the echo has folded, but the wire must
    // still have a defined shape for it.
    let mut presets = PresetStore::new();
    presets.create(Preset::new("Bright"));
    let seqs = BTreeMap::new();
    emit_library_fixture("effect-no-persisted-seq", &presets, &[], None, false, &seqs, 1);
}

#[test]
fn emit_library_unassigned_device() {
    // `preset_id: 0` means Off -- a paired device with no effect assigned.
    let presets = PresetStore::new();
    let seqs = BTreeMap::new();
    let paired = alloc::vec![PairedDevice { addr: addr(0x01), name: "SBC Buds".to_string(), mru_seq: 1, ldac_quality: 0, preset_id: 0 }];
    emit_library_fixture("unassigned-device", &presets, &paired, None, false, &seqs, 1);
}

#[test]
fn emit_library_dangling_assignment() {
    // A device's `preset_id` referencing an id that no longer exists in
    // the store (design section 4: "Devices dangle to Off" on delete) --
    // the wire carries the stale id verbatim; resolving it to Off is a
    // reader-side rule (design section 4 doc), not something this encoder
    // enforces.
    let presets = PresetStore::new();
    let seqs = BTreeMap::new();
    let paired = alloc::vec![PairedDevice { addr: addr(0x02), name: "Dangling".to_string(), mru_seq: 1, ldac_quality: 0, preset_id: 42 }];
    emit_library_fixture("dangling-assignment", &presets, &paired, None, false, &seqs, 1);
}

#[test]
fn emit_library_max_effects_and_devices() {
    // Design section 3's own worked-out ceiling: 8 effects + 8 devices,
    // 14 + 8*84 + 8*42 = 1022 B.
    let mut presets = PresetStore::new();
    for i in 0..MAX_PRESETS {
        presets.create(Preset::new(&alloc::format!("Effect {i}")));
    }
    let seqs = BTreeMap::new();
    let paired: alloc::vec::Vec<PairedDevice> = (0..MAX_PAIRED_DEVICES).map(|i| PairedDevice { addr: addr(i as u8), name: alloc::format!("Buds {i}"), mru_seq: 1, ldac_quality: 0, preset_id: 0 }).collect();
    emit_library_fixture("max-effects-and-devices", &presets, &paired, None, true, &seqs, 1);
}

#[test]
fn emit_library_multibyte_device_name_truncates() {
    // Same char-boundary truncation guard `telemetry_fixtures`'s
    // `emit_home_max_length_multibyte_names` exercises, but for the
    // library's own device-name field.
    let presets = PresetStore::new();
    let seqs = BTreeMap::new();
    let long_name: String = "\u{20AC}".repeat(11); // 33 bytes, cap is 32.
    let paired = alloc::vec![PairedDevice { addr: addr(0x03), name: long_name, mru_seq: 1, ldac_quality: 0, preset_id: 0 }];
    emit_library_fixture("multibyte-device-name", &presets, &paired, None, false, &seqs, 1);
}

#[test]
fn header_constants_match_this_proto() {
    // Cheap belt-and-braces: the fixtures above are only meaningful if
    // these constants are what this module's doc comment (and every
    // `library-*.bin`'s first bytes) assumes.
    assert_eq!(LIBRARY_PROTO, 1);
    assert_eq!(EFFECT_RECORD_LEN, 84);
    assert_eq!(DEVICE_RECORD_LEN, 42);
    assert_eq!(LIBRARY_SNAPSHOT_MAX_LEN, 1022);
}
