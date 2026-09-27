//! Fixture-emitting tests for the web companion's `HOST_OP` (`0x06`) /
//! `GET_OP_STATUS` (`0x07`) write side (bead `pico-link-jyhk.19`, "ADA
//! DESIGN" comment on `pico-link-jyhk.17`, sections 4/6/7), mirroring the
//! shape [`super::library_fixtures`]/[`super::telemetry_fixtures`] already
//! established ("core emits, JS asserts"). The code-review on this bead
//! (`pico-link-jyhk.19`) flagged that this fixture set never existed even
//! though it was in the task's own scope line -- see that comment for why
//! it matters: `pico-link-jyhk.22`'s hand-written `OpError` TypeScript enum
//! had already drifted from [`super::host_op::OpError`] on nearly every
//! ordinal before this file existed to catch it.
//!
//! Three kinds of fixture, all under `fixtures/host_op/`:
//!
//! - `op-errors.json` -- every [`super::host_op::OpError`] name mapped to
//!   its real `as u8` ordinal, plus the six op codes
//!   ([`super::host_op::HostOpCode::wire`]) and the `flags` bits. This is
//!   the file `pico-link-jyhk.22`'s enum should have been checked against.
//! - `request-*.bin`/`.json` -- a canonical `HOST_OP` request per op,
//!   proven valid by actually feeding it through [`super::App::host_op`]
//!   (never just hand-assembled and left unexercised).
//! - `status-*.bin`/`.json` -- a canonical `GET_OP_STATUS` reply per
//!   scenario, produced by the real [`super::App::host_op_status`] encoder
//!   after a real [`super::App::host_op`] call, never hand-built.
//!
//! # Check mode vs. regenerate mode
//!
//! Same contract as [`super::telemetry_fixtures`]/[`super::library_fixtures`]:
//! plain `cargo test` checks every fixture byte-for-byte/text-for-text
//! against `fixtures/host_op/*`; a stale fixture is a test failure. To
//! regenerate after a deliberate change:
//!
//! ```text
//! UPDATE_FIXTURES=1 cargo test -p pico-link-core host_op_fixtures
//! ```
//!
//! and review the resulting `git diff` before committing.

#![cfg(test)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::host_op::{HostOpCode, OpError, FLAG_BYPASS, OP_PROTO};
use super::model::PairedDevice;
use super::test_support::ready_presets;
use super::{App, Event};
use crate::dsp::preset::{Band, BandKind, Preset, BLOB_LEN, MAX_NAME_BYTES};

// --- Fixture I/O: check by default, regenerate under UPDATE_FIXTURES=1 -

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("fixtures").join("host_op")
}

/// Same contract as [`super::telemetry_fixtures::check_or_write_bytes`].
fn check_or_write_bytes(rel_name: &str, actual: &[u8]) {
    let path = fixtures_dir().join(rel_name);
    if env::var_os("UPDATE_FIXTURES").is_some() {
        fs::create_dir_all(path.parent().expect("fixtures dir has a parent")).expect("create fixtures/host_op");
        fs::write(&path, actual).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
        return;
    }
    let expected = fs::read(&path).unwrap_or_else(|e| {
        panic!("missing fixture {} ({e}) -- run `UPDATE_FIXTURES=1 cargo test -p pico-link-core host_op_fixtures` and commit the result", path.display());
    });
    assert_eq!(
        actual,
        expected.as_slice(),
        "fixture {} is stale -- rerun `UPDATE_FIXTURES=1 cargo test -p pico-link-core host_op_fixtures`, review the diff, and commit it",
        path.display()
    );
}

/// Same contract as [`super::telemetry_fixtures::check_or_write_text`].
fn check_or_write_text(rel_name: &str, actual: &str) {
    let path = fixtures_dir().join(rel_name);
    if env::var_os("UPDATE_FIXTURES").is_some() {
        fs::create_dir_all(path.parent().expect("fixtures dir has a parent")).expect("create fixtures/host_op");
        fs::write(&path, actual).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("missing fixture {} ({e}) -- run `UPDATE_FIXTURES=1 cargo test -p pico-link-core host_op_fixtures` and commit the result", path.display());
    });
    assert_eq!(
        actual,
        expected,
        "fixture {} is stale -- rerun `UPDATE_FIXTURES=1 cargo test -p pico-link-core host_op_fixtures`, review the diff, and commit it",
        path.display()
    );
}

// --- A ~40-line hand-rolled JSON writer (design: "no serde in core") ----
// Duplicated from `library_fixtures`/`telemetry_fixtures` rather than
// shared across a `#[cfg(test)]`-only module boundary, same one-caller-each
// reason those two duplicate it from each other.

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

// --- op-errors.json: the ordinal table `pico-link-jyhk.22` should have --
// been built against. Every value below is read straight off the real
// enums (`as u8` / `HostOpCode::wire`), never hand-copied -- a renumbering
// of either enum changes `actual` here and fails the fixture check.

#[test]
fn emit_op_errors() {
    let errors = [
        ("NONE", OpError::None as u8),
        ("INVALID_REQUEST", OpError::InvalidRequest as u8),
        ("UNKNOWN_OP", OpError::UnknownOp as u8),
        ("NOT_READY", OpError::NotReady as u8),
        ("STORE_FULL", OpError::StoreFull as u8),
        ("NOT_FOUND", OpError::NotFound as u8),
        ("CONFLICT", OpError::Conflict as u8),
        ("EDITOR_OPEN", OpError::EditorOpen as u8),
        ("NAME_TAKEN", OpError::NameTaken as u8),
        ("NAME_INVALID", OpError::NameInvalid as u8),
        ("BLOB_VERSION", OpError::BlobVersion as u8),
        ("BAND_COUNT", OpError::BandCount as u8),
        ("RESERVED_BAND_KIND", OpError::ReservedBandKind as u8),
        ("GAIN_RANGE", OpError::GainRange as u8),
        ("FREQ_RANGE", OpError::FreqRange as u8),
        ("Q_RANGE", OpError::QRange as u8),
        ("PREAMP_RANGE", OpError::PreampRange as u8),
        ("UNKNOWN_DEVICE", OpError::UnknownDevice as u8),
        ("PARSE_ERROR", OpError::ParseError as u8),
        ("APO_TOO_LARGE", OpError::ApoTooLarge as u8),
    ];
    let ops = [
        ("SAVE_EFFECT", HostOpCode::SaveEffect.wire()),
        ("DELETE_EFFECT", HostOpCode::DeleteEffect.wire()),
        ("ASSIGN", HostOpCode::Assign.wire()),
        ("PREVIEW", HostOpCode::Preview.wire()),
        ("PREVIEW_END", HostOpCode::PreviewEnd.wire()),
        ("PARSE_APO", HostOpCode::ParseApo.wire()),
    ];
    let flags = [("BYPASS", FLAG_BYPASS)];

    let errors_json: Vec<String> = errors.iter().map(|(name, v)| std::format!("    {}: {v}", json_string(name))).collect();
    let ops_json: Vec<String> = ops.iter().map(|(name, v)| std::format!("    {}: {v}", json_string(name))).collect();
    let flags_json: Vec<String> = flags.iter().map(|(name, v)| std::format!("    {}: {v}", json_string(name))).collect();

    let json = std::format!(
        "{{\n  \"op_proto\": {OP_PROTO},\n  \"errors\": {{\n{}\n  }},\n  \"ops\": {{\n{}\n  }},\n  \"flags\": {{\n{}\n  }}\n}}\n",
        errors_json.join(",\n"),
        ops_json.join(",\n"),
        flags_json.join(",\n"),
    );
    check_or_write_text("op-errors.json", &json);
}

// --- HOST_OP request fixtures: canonical bytes per op, each proven -----
// valid by round-tripping through the real `App::host_op`/`host_op_status`.

fn save_request(seq: u8, id: u16, base_seq: u16, blob: &[u8; BLOB_LEN]) -> Vec<u8> {
    let mut req = alloc::vec![OP_PROTO, HostOpCode::SaveEffect.wire(), seq, 0];
    req.extend_from_slice(&id.to_le_bytes());
    req.extend_from_slice(&base_seq.to_le_bytes());
    req.extend_from_slice(blob);
    req
}

fn delete_request(seq: u8, id: u16, base_seq: u16) -> Vec<u8> {
    let mut req = alloc::vec![OP_PROTO, HostOpCode::DeleteEffect.wire(), seq, 0];
    req.extend_from_slice(&id.to_le_bytes());
    req.extend_from_slice(&base_seq.to_le_bytes());
    req
}

fn assign_request(seq: u8, addr: [u8; 6], effect_id: u16) -> Vec<u8> {
    let mut req = alloc::vec![OP_PROTO, HostOpCode::Assign.wire(), seq, 0];
    req.extend_from_slice(&addr);
    req.extend_from_slice(&effect_id.to_le_bytes());
    req
}

fn preview_request(seq: u8, effect_id: u16, blob: &[u8; BLOB_LEN], bypass: bool) -> Vec<u8> {
    let flags = if bypass { FLAG_BYPASS } else { 0 };
    let mut req = alloc::vec![OP_PROTO, HostOpCode::Preview.wire(), seq, flags];
    req.extend_from_slice(&effect_id.to_le_bytes());
    req.extend_from_slice(blob);
    req
}

fn preview_end_request(seq: u8) -> Vec<u8> {
    alloc::vec![OP_PROTO, HostOpCode::PreviewEnd.wire(), seq, 0]
}

fn parse_apo_request(seq: u8, host_name: &str, text: &str) -> Vec<u8> {
    let mut req = alloc::vec![OP_PROTO, HostOpCode::ParseApo.wire(), seq, 0];
    #[allow(clippy::cast_possible_truncation)] // fixture-only names are always short
    req.push(host_name.len() as u8);
    req.extend_from_slice(host_name.as_bytes());
    req.extend_from_slice(text.as_bytes());
    req
}

fn addr(last_byte: u8) -> [u8; 6] {
    [0x94, 0xDB, 0x56, 0x54, 0x7C, last_byte]
}

fn new_preset_blob(name: &str) -> [u8; BLOB_LEN] {
    Preset::new(name).to_wire()
}

fn emit_request_fixture(name: &str, bytes: &[u8], decoded_json: &str) {
    check_or_write_bytes(&std::format!("request-{name}.bin"), bytes);
    check_or_write_text(&std::format!("request-{name}.json"), decoded_json);
}

#[test]
fn emit_request_save_create() {
    let blob = new_preset_blob("Warm");
    let req = save_request(1, 0, 0, &blob);
    let mut app = App::new(240, 240);
    ready_presets(&mut app);
    app.host_op(&req); // proves the bytes are actually accepted
    let mut buf = [0u8; 200];
    let n = app.host_op_status(&mut buf);
    assert_eq!(buf[3], 1, "SAVE create must be accepted by the real handler");

    let json = std::format!("{{\n  \"op\": \"SAVE_EFFECT\",\n  \"op_code\": {},\n  \"seq\": 1,\n  \"flags\": 0,\n  \"id\": 0,\n  \"base_seq\": 0,\n  \"blob_len\": {BLOB_LEN},\n  \"preset_name\": {}\n}}\n", HostOpCode::SaveEffect.wire(), json_string("Warm"));
    let _ = n;
    emit_request_fixture("save-create", &req, &json);
}

#[test]
fn emit_request_save_update_rename() {
    let mut app = App::new(240, 240);
    ready_presets(&mut app);
    let id = app.presets_create_for_test(Preset::new("Warm"));
    app.handle_event(Event::PresetLoaded { id, blob: new_preset_blob("Warm").to_vec() }); // persisted_seq -> 1

    let blob = new_preset_blob("Warmer");
    let req = save_request(2, id, 1, &blob);
    app.host_op(&req);
    let mut buf = [0u8; 200];
    let n = app.host_op_status(&mut buf);
    assert_eq!(buf[3], 1, "SAVE update/rename must be accepted by the real handler");
    let _ = n;

    let json = std::format!(
        "{{\n  \"op\": \"SAVE_EFFECT\",\n  \"op_code\": {},\n  \"seq\": 2,\n  \"flags\": 0,\n  \"id\": {id},\n  \"base_seq\": 1,\n  \"blob_len\": {BLOB_LEN},\n  \"preset_name\": {}\n}}\n",
        HostOpCode::SaveEffect.wire(),
        json_string("Warmer"),
    );
    emit_request_fixture("save-update-rename", &req, &json);
}

#[test]
fn emit_request_delete() {
    let mut app = App::new(240, 240);
    ready_presets(&mut app);
    let id = app.presets_create_for_test(Preset::new("Doomed"));

    let req = delete_request(3, id, 0);
    app.host_op(&req);
    let mut buf = [0u8; 200];
    let n = app.host_op_status(&mut buf);
    assert_eq!(buf[3], 1, "DELETE must be accepted by the real handler");
    let _ = n;

    let json = std::format!("{{\n  \"op\": \"DELETE_EFFECT\",\n  \"op_code\": {},\n  \"seq\": 3,\n  \"flags\": 0,\n  \"id\": {id},\n  \"base_seq\": 0\n}}\n", HostOpCode::DeleteEffect.wire());
    emit_request_fixture("delete", &req, &json);
}

#[test]
fn emit_request_assign() {
    let mut app = App::new(240, 240);
    ready_presets(&mut app);
    let device_addr = addr(0xF2);
    app.handle_event(Event::PairedDeviceUpserted(PairedDevice { addr: device_addr, name: "Pixel Buds".to_string(), mru_seq: 1, ldac_quality: 4, preset_id: 0 }));
    let id = app.presets_create_for_test(Preset::new("Warm"));

    let req = assign_request(4, device_addr, id);
    app.host_op(&req);
    let mut buf = [0u8; 200];
    let n = app.host_op_status(&mut buf);
    assert_eq!(buf[3], 1, "ASSIGN must be accepted by the real handler");
    let _ = n;

    let json = std::format!(
        "{{\n  \"op\": \"ASSIGN\",\n  \"op_code\": {},\n  \"seq\": 4,\n  \"flags\": 0,\n  \"addr\": {},\n  \"effect_id\": {id}\n}}\n",
        HostOpCode::Assign.wire(),
        json_string(&device_addr.iter().map(|b| std::format!("{b:02X}")).collect::<Vec<_>>().join(":")),
    );
    emit_request_fixture("assign", &req, &json);
}

#[test]
fn emit_request_preview_no_bypass() {
    let mut app = App::new(240, 240);
    ready_presets(&mut app);
    let mut preset = Preset::new("Preview Me");
    preset.push_band(Band { kind: BandKind::Peak, freq_half_hz: 2000, gain_cdb: 300, q_milli: 1000 });
    let blob = preset.to_wire();

    let req = preview_request(5, 0, &blob, false);
    app.host_op(&req);
    let mut buf = [0u8; 200];
    let n = app.host_op_status(&mut buf);
    assert_eq!(buf[3], 1, "PREVIEW (no bypass) must be accepted by the real handler");
    let _ = n;

    let json = std::format!("{{\n  \"op\": \"PREVIEW\",\n  \"op_code\": {},\n  \"seq\": 5,\n  \"flags\": 0,\n  \"bypass\": false,\n  \"effect_id\": 0,\n  \"blob_len\": {BLOB_LEN}\n}}\n", HostOpCode::Preview.wire());
    emit_request_fixture("preview", &req, &json);
}

#[test]
fn emit_request_preview_bypass() {
    let mut app = App::new(240, 240);
    ready_presets(&mut app);
    let blob = new_preset_blob("Preview Me");

    let req = preview_request(6, 0, &blob, true);
    app.host_op(&req);
    let mut buf = [0u8; 200];
    let n = app.host_op_status(&mut buf);
    assert_eq!(buf[3], 1, "PREVIEW (bypass) must be accepted by the real handler");
    let _ = n;

    let json = std::format!("{{\n  \"op\": \"PREVIEW\",\n  \"op_code\": {},\n  \"seq\": 6,\n  \"flags\": {FLAG_BYPASS},\n  \"bypass\": true,\n  \"effect_id\": 0,\n  \"blob_len\": {BLOB_LEN}\n}}\n", HostOpCode::Preview.wire());
    emit_request_fixture("preview-bypass", &req, &json);
}

#[test]
fn emit_request_preview_end() {
    let mut app = App::new(240, 240);
    ready_presets(&mut app);
    app.host_op(&preview_request(7, 0, &new_preset_blob("X"), false));

    let req = preview_end_request(8);
    app.host_op(&req);
    let mut buf = [0u8; 200];
    let n = app.host_op_status(&mut buf);
    assert_eq!(buf[3], 1, "PREVIEW_END must be accepted by the real handler");
    let _ = n;

    let json = std::format!("{{\n  \"op\": \"PREVIEW_END\",\n  \"op_code\": {},\n  \"seq\": 8,\n  \"flags\": 0\n}}\n", HostOpCode::PreviewEnd.wire());
    emit_request_fixture("preview-end", &req, &json);
}

const APO_TEXT: &str = "Preamp: -1 dB\nFilter 1: ON PK Fc 1000 Hz Gain 2 dB Q 1";

#[test]
fn emit_request_parse_apo() {
    let mut app = App::new(240, 240);
    ready_presets(&mut app);

    let req = parse_apo_request(9, "Fallback", APO_TEXT);
    app.host_op(&req);
    let mut buf = [0u8; 400];
    let n = app.host_op_status(&mut buf);
    assert_eq!(buf[3], 1, "PARSE_APO must be accepted by the real handler");
    let _ = n;

    let json = std::format!(
        "{{\n  \"op\": \"PARSE_APO\",\n  \"op_code\": {},\n  \"seq\": 9,\n  \"flags\": 0,\n  \"name_len\": 8,\n  \"name\": {},\n  \"apo_text\": {}\n}}\n",
        HostOpCode::ParseApo.wire(),
        json_string("Fallback"),
        json_string(APO_TEXT),
    );
    emit_request_fixture("parse-apo", &req, &json);
}

// --- GET_OP_STATUS reply fixtures: real `App::host_op` + `host_op_status` --

/// `(op_proto, seq, op, state, error, effect_id, library_rev,
/// persisted_seq, line, band, value, payload)`.
#[allow(clippy::type_complexity)]
fn decode_status(buf: &[u8]) -> (u8, u8, u8, u8, u8, u16, u16, u16, u16, u16, f32, Vec<u8>) {
    let op_proto = buf[0];
    let seq = buf[1];
    let op = buf[2];
    let state = buf[3];
    let error = buf[4];
    let effect_id = u16::from_le_bytes([buf[6], buf[7]]);
    let library_rev = u16::from_le_bytes([buf[8], buf[9]]);
    let persisted_seq = u16::from_le_bytes([buf[10], buf[11]]);
    let line = u16::from_le_bytes([buf[12], buf[13]]);
    let band = u16::from_le_bytes([buf[14], buf[15]]);
    let value = f32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
    let payload_len = usize::from(buf[20]);
    let payload = buf[21..21 + payload_len].to_vec();
    (op_proto, seq, op, state, error, effect_id, library_rev, persisted_seq, line, band, value, payload)
}

#[allow(clippy::type_complexity)] // same tuple shape as `decode_status`, which it takes by reference
fn status_json(op_name: &str, decoded: &(u8, u8, u8, u8, u8, u16, u16, u16, u16, u16, f32, Vec<u8>)) -> String {
    let (op_proto, seq, op, state, error, effect_id, library_rev, persisted_seq, line, band, value, payload) = decoded;
    std::format!(
        "{{\n  \"op_proto\": {op_proto},\n  \"op\": {},\n  \"op_code\": {op},\n  \"seq\": {seq},\n  \"state\": {state},\n  \"error\": {error},\n  \"effect_id\": {effect_id},\n  \"library_rev\": {library_rev},\n  \"persisted_seq\": {persisted_seq},\n  \"line\": {line},\n  \"band\": {band},\n  \"value\": {value},\n  \"payload_len\": {}\n}}\n",
        json_string(op_name),
        payload.len(),
    )
}

fn emit_status_fixture(name: &str, op_name: &str, buf: &[u8]) {
    check_or_write_bytes(&std::format!("status-{name}.bin"), buf);
    let decoded = decode_status(buf);
    check_or_write_text(&std::format!("status-{name}.json"), &status_json(op_name, &decoded));
}

#[test]
fn emit_status_save_success() {
    let mut app = App::new(240, 240);
    ready_presets(&mut app);
    app.host_op(&save_request(1, 0, 0, &new_preset_blob("Warm")));
    let mut buf = [0u8; 200];
    let n = app.host_op_status(&mut buf);
    emit_status_fixture("save-success", "SAVE_EFFECT", &buf[..n]);
}

#[test]
fn emit_status_delete_success() {
    let mut app = App::new(240, 240);
    ready_presets(&mut app);
    let id = app.presets_create_for_test(Preset::new("Doomed"));
    app.host_op(&delete_request(1, id, 0));
    let mut buf = [0u8; 200];
    let n = app.host_op_status(&mut buf);
    emit_status_fixture("delete-success", "DELETE_EFFECT", &buf[..n]);
}

#[test]
fn emit_status_assign_success() {
    let mut app = App::new(240, 240);
    ready_presets(&mut app);
    let device_addr = addr(0xF2);
    app.handle_event(Event::PairedDeviceUpserted(PairedDevice { addr: device_addr, name: "Pixel Buds".to_string(), mru_seq: 1, ldac_quality: 4, preset_id: 0 }));
    let id = app.presets_create_for_test(Preset::new("Warm"));
    app.host_op(&assign_request(1, device_addr, id));
    let mut buf = [0u8; 200];
    let n = app.host_op_status(&mut buf);
    emit_status_fixture("assign-success", "ASSIGN", &buf[..n]);
}

#[test]
fn emit_status_preview_success() {
    let mut app = App::new(240, 240);
    ready_presets(&mut app);
    app.host_op(&preview_request(1, 0, &new_preset_blob("Preview Me"), false));
    let mut buf = [0u8; 200];
    let n = app.host_op_status(&mut buf);
    emit_status_fixture("preview-success", "PREVIEW", &buf[..n]);
}

#[test]
fn emit_status_preview_end_success() {
    let mut app = App::new(240, 240);
    ready_presets(&mut app);
    app.host_op(&preview_request(1, 0, &new_preset_blob("X"), false));
    app.host_op(&preview_end_request(2));
    let mut buf = [0u8; 200];
    let n = app.host_op_status(&mut buf);
    emit_status_fixture("preview-end-success", "PREVIEW_END", &buf[..n]);
}

#[test]
fn emit_status_parse_apo_success_with_payload() {
    let mut app = App::new(240, 240);
    ready_presets(&mut app);
    app.host_op(&parse_apo_request(1, "Fallback", APO_TEXT));
    let mut buf = [0u8; 400];
    let n = app.host_op_status(&mut buf);
    assert!(n > 21, "PARSE_APO success must carry a payload");
    emit_status_fixture("parse-apo-success", "PARSE_APO", &buf[..n]);
}

#[test]
fn emit_status_conflict_carries_current_persisted_seq() {
    let mut app = App::new(240, 240);
    ready_presets(&mut app);
    app.host_op(&save_request(1, 0, 0, &new_preset_blob("Warm")));
    let mut buf = [0u8; 200];
    let n = app.host_op_status(&mut buf);
    let (_, _, _, _, _, effect_id, ..) = decode_status(&buf[..n]);
    app.poll_command();
    app.handle_event(Event::PresetLoaded { id: effect_id, blob: new_preset_blob("Warm").to_vec() }); // persisted_seq -> 1

    app.host_op(&save_request(2, effect_id, 0, &new_preset_blob("Warmer"))); // stale base_seq 0
    let n = app.host_op_status(&mut buf);
    emit_status_fixture("conflict", "SAVE_EFFECT", &buf[..n]);
}

#[test]
fn emit_status_editor_open() {
    let mut app = App::new(240, 240);
    ready_presets(&mut app);
    let id = app.presets_create_for_test(Preset::new("Warm"));
    app.handle_event(Event::PresetLoaded { id, blob: new_preset_blob("Warm").to_vec() });
    app.open_editor_for_test(id);

    app.host_op(&save_request(1, id, 1, &new_preset_blob("Warmer")));
    let mut buf = [0u8; 200];
    let n = app.host_op_status(&mut buf);
    emit_status_fixture("editor-open", "SAVE_EFFECT", &buf[..n]);
}

#[test]
fn emit_status_range_error() {
    let mut app = App::new(240, 240);
    ready_presets(&mut app);
    let mut preset = Preset::new("Loud");
    preset.push_band(Band { kind: BandKind::Peak, freq_half_hz: 2000, gain_cdb: 3500, q_milli: 1000 }); // 35 dB, over the 30 dB limit
    app.host_op(&save_request(1, 0, 0, &preset.to_wire()));
    let mut buf = [0u8; 200];
    let n = app.host_op_status(&mut buf);
    emit_status_fixture("range-error", "SAVE_EFFECT", &buf[..n]);
}

#[test]
fn emit_status_not_ready() {
    let mut app = App::new(240, 240);
    app.host_op(&save_request(1, 0, 0, &new_preset_blob("Too Soon")));
    let mut buf = [0u8; 200];
    let n = app.host_op_status(&mut buf);
    emit_status_fixture("not-ready", "SAVE_EFFECT", &buf[..n]);
}

// --- Belt-and-braces: op-proto and MAX_NAME_BYTES the payload shape -----
// (`emit_status_parse_apo_success_with_payload`'s json) depends on.

#[test]
fn header_constants_match_this_proto() {
    assert_eq!(OP_PROTO, 1);
    assert_eq!(BLOB_LEN, 80);
    assert_eq!(MAX_NAME_BYTES, 16);
    assert_eq!(HostOpCode::SaveEffect.wire(), 1);
    assert_eq!(HostOpCode::ParseApo.wire(), 6);
}

/// Also serves as this bead's tamper-test proof, run by hand once (not part
/// of the checked-in suite): temporarily edit a committed value in
/// `fixtures/host_op/op-errors.json` (e.g. change `"CONFLICT"` from `6` to
/// `60`) and re-run `cargo test -p pico-link-core host_op_fixtures` --
/// `emit_op_errors` fails with the stale-fixture message above, proving
/// check mode actually catches drift rather than always regenerating.
#[test]
fn status_helpers_match_ready_presets_scenario() {
    let mut app = App::new(240, 240);
    ready_presets(&mut app);
    let mut buf = [0u8; 200];
    let n = app.host_op_status(&mut buf);
    let (_, _, _, state, ..) = decode_status(&buf[..n]);
    assert_eq!(state, 0, "a fresh, readied App with no HOST_OP yet must still report state NONE");
}
