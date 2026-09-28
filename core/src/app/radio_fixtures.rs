//! Fixture-emitting tests for the web companion's `GET_RADIO` (`0x08`)
//! snapshot (design `.planning/design/2026-09-27-iface6-eq-management-
//! protocol.md` sec 13.4, bead `pico-link-jyhk.27`), mirroring the shape
//! [`super::library_fixtures`] already established for `GET_LIBRARY`.
//! Every `radio-*.bin`/`.json` pair below is generated straight from
//! [`super::radio::encode_radio_snapshot`], the same code the wire format
//! actually runs, so a layout change shows up as a failing `cargo test`
//! here, not as a silent drift a browser discovers first.
//!
//! Also emits `reasons.json`, the [`ConnectFailureReason`] wire-code +
//! `retryable` + text table design sec 13.4 calls for ("so JS never
//! retypes `events.rs:116-137`") -- every value read straight off
//! [`ConnectFailureReason::wire`]/[`ConnectFailureReason::retryable`]/
//! [`ConnectFailureReason::text`], never hand-copied, the same discipline
//! [`super::host_op_fixtures`]'s `op-errors.json` uses for [`super::host_op::OpError`].
//!
//! # Check mode vs. regenerate mode
//!
//! Same contract as [`super::library_fixtures`]:
//!
//! ```text
//! UPDATE_FIXTURES=1 cargo test -p pico-link-core radio_fixtures
//! ```

#![cfg(test)]

use alloc::string::{String, ToString};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use super::events::{ConnectFailureReason, ConnectStep};
use super::model::{BtModel, ConnectAttempt, ConnectInitiator, ConnectOutcome, ConnectOutcomeResult, DeviceEntry, PairedDevice, ScanOwner, MAX_SCAN_LIST_ITEMS};
use super::radio::{decode_radio_snapshot, encode_radio_snapshot, DecodedAttempt, DecodedOutcome, DecodedScanEntry, RadioSnapshot, RADIO_PROTO, RADIO_SNAPSHOT_MAX_LEN, SCAN_RECORD_LEN};

// --- Fixture I/O: check by default, regenerate under UPDATE_FIXTURES=1 -

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("fixtures").join("radio")
}

/// Same contract as [`super::library_fixtures::check_or_write_bytes`].
fn check_or_write_bytes(rel_name: &str, actual: &[u8]) {
    let path = fixtures_dir().join(rel_name);
    if env::var_os("UPDATE_FIXTURES").is_some() {
        fs::create_dir_all(path.parent().expect("fixtures dir has a parent")).expect("create fixtures/radio");
        fs::write(&path, actual).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
        return;
    }
    let expected = fs::read(&path).unwrap_or_else(|e| {
        panic!("missing fixture {} ({e}) -- run `UPDATE_FIXTURES=1 cargo test -p pico-link-core radio_fixtures` and commit the result", path.display());
    });
    assert_eq!(
        actual,
        expected.as_slice(),
        "fixture {} is stale -- rerun `UPDATE_FIXTURES=1 cargo test -p pico-link-core radio_fixtures`, review the diff, and commit it",
        path.display()
    );
}

/// Same contract as [`super::library_fixtures::check_or_write_text`].
fn check_or_write_text(rel_name: &str, actual: &str) {
    let path = fixtures_dir().join(rel_name);
    if env::var_os("UPDATE_FIXTURES").is_some() {
        fs::create_dir_all(path.parent().expect("fixtures dir has a parent")).expect("create fixtures/radio");
        fs::write(&path, actual).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("missing fixture {} ({e}) -- run `UPDATE_FIXTURES=1 cargo test -p pico-link-core radio_fixtures` and commit the result", path.display());
    });
    assert_eq!(
        actual,
        expected,
        "fixture {} is stale -- rerun `UPDATE_FIXTURES=1 cargo test -p pico-link-core radio_fixtures`, review the diff, and commit it",
        path.display()
    );
}

// --- A ~40-line hand-rolled JSON writer (design: "no serde in core") ----
// Duplicated from `library_fixtures`/`telemetry_fixtures` rather than
// shared across a `#[cfg(test)]`-only module boundary, for the same
// one-caller-each reason those two duplicate it from each other.

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

fn json_addr(addr: [u8; 6]) -> String {
    json_string(&addr.iter().map(|b| std::format!("{b:02X}")).collect::<Vec<_>>().join(":"))
}

// --- RadioSnapshot -> JSON (the "expected decode" half of each pair) ---

fn scan_owner_label(owner: ScanOwner) -> &'static str {
    match owner {
        ScanOwner::None => "none",
        ScanOwner::Device => "device",
        ScanOwner::Host => "host",
    }
}

fn initiator_label(initiator: ConnectInitiator) -> &'static str {
    match initiator {
        ConnectInitiator::Device => "device",
        ConnectInitiator::Host => "host",
        ConnectInitiator::AutoReconnect => "auto_reconnect",
    }
}

fn step_label(step: ConnectStep) -> &'static str {
    match step {
        ConnectStep::Connecting => "connecting",
        ConnectStep::Pairing => "pairing",
        ConnectStep::SettingUpAudio => "setting_up_audio",
        ConnectStep::NegotiatingCodec => "negotiating_codec",
        ConnectStep::Disconnecting => "disconnecting",
    }
}

fn outcome_label(result: ConnectOutcomeResult) -> &'static str {
    match result {
        ConnectOutcomeResult::Ok => "ok",
        ConnectOutcomeResult::OkDegraded => "ok_degraded",
        ConnectOutcomeResult::Failed => "failed",
        ConnectOutcomeResult::Cancelled => "cancelled",
    }
}

fn attempt_json(attempt: Option<&DecodedAttempt>) -> String {
    match attempt {
        None => "null".to_string(),
        Some(a) => std::format!(
            "{{\"seq\": {}, \"addr\": {}, \"initiator\": {}, \"step\": {}, \"retries\": {}}}",
            a.seq,
            json_addr(a.addr),
            json_string(initiator_label(a.initiator)),
            a.step.map_or("null".to_string(), |s| json_string(step_label(s))),
            a.retries,
        ),
    }
}

fn outcome_json(outcome: Option<&DecodedOutcome>) -> String {
    match outcome {
        None => "null".to_string(),
        Some(o) => std::format!(
            "{{\"seq\": {}, \"addr\": {}, \"result\": {}, \"reason\": {}}}",
            o.seq,
            json_addr(o.addr),
            json_string(outcome_label(o.result)),
            o.reason.map_or("null".to_string(), |r| json_string(r.text())),
        ),
    }
}

fn scan_entry_json(entry: &DecodedScanEntry) -> String {
    std::format!(
        "{{\"addr\": {}, \"bars\": {}, \"already_paired\": {}, \"name\": {}}}",
        json_addr(entry.addr),
        entry.bars,
        json_bool(entry.already_paired),
        json_string(&entry.name),
    )
}

/// Renders a decoded snapshot as deterministic JSON -- the "expected
/// output" half of each `radio-*.bin`/`.json` pair.
fn snapshot_json(bytes: &[u8], snap: &RadioSnapshot) -> String {
    let scan: Vec<String> = snap.scan.iter().map(scan_entry_json).collect();
    std::format!(
        "{{\n  \"wire_len\": {},\n  \"radio_rev\": {},\n  \"discovering\": {},\n  \"connecting\": {},\n  \"device_wizard_open\": {},\n  \"paired_full\": {},\n  \"store_ready\": {},\n  \"scan_owner\": {},\n  \"scan_seq\": {},\n  \"attempt\": {},\n  \"last_outcome\": {},\n  \"scan_total_audio\": {},\n  \"scan\": [\n    {}\n  ]\n}}\n",
        bytes.len(),
        snap.radio_rev,
        json_bool(snap.discovering),
        json_bool(snap.connecting),
        json_bool(snap.device_wizard_open),
        json_bool(snap.paired_full),
        json_bool(snap.store_ready),
        json_string(scan_owner_label(snap.scan_owner)),
        snap.scan_seq,
        attempt_json(snap.attempt.as_ref()),
        outcome_json(snap.last_outcome.as_ref()),
        snap.scan_total_audio,
        scan.join(",\n    "),
    )
}

/// Encodes `model` at `(device_wizard_open, radio_rev)`, decodes the
/// result back (asserting the round trip -- a bad fixture must never ship
/// even in `UPDATE_FIXTURES=1` mode), and checks/writes both halves of one
/// `radio-<name>.bin`/`.json` pair.
fn emit_radio_fixture(name: &str, model: &BtModel, device_wizard_open: bool, radio_rev: u16) {
    let bytes = encode_radio_snapshot(model, device_wizard_open, radio_rev);
    let snap = decode_radio_snapshot(&bytes).expect("a well-formed encode must always decode");
    check_or_write_bytes(&std::format!("radio-{name}.bin"), &bytes);
    check_or_write_text(&std::format!("radio-{name}.json"), &snapshot_json(&bytes, &snap));
}

fn addr(last_byte: u8) -> [u8; 6] {
    [0x94, 0xDB, 0x56, 0x54, 0x7C, last_byte]
}

#[test]
fn emit_radio_idle() {
    let model = BtModel::default();
    emit_radio_fixture("idle", &model, false, 1);
}

#[test]
fn emit_radio_discovering_host_scan() {
    let mut model = BtModel { discovering: true, scan_owner: ScanOwner::Host, scan_seq: 4, ..Default::default() };
    model.discovered.push(DeviceEntry { addr: addr(0x10), name: "Cans".to_string(), rssi: -45, class_of_device: 0x04 << 8 });
    emit_radio_fixture("discovering-host-scan", &model, false, 2);
}

#[test]
fn emit_radio_scan_list_filters_non_audio_and_caps() {
    let mut model = BtModel { discovering: true, scan_owner: ScanOwner::Device, ..Default::default() };
    // One non-audio device (must be filtered) plus one more than the cap
    // of audio-classed devices (must be truncated, with the true total
    // still reported via `scan_total_audio`).
    model.discovered.push(DeviceEntry { addr: addr(0x00), name: "Phone".to_string(), rssi: -40, class_of_device: 0x01 << 8 });
    for i in 0..=MAX_SCAN_LIST_ITEMS {
        #[allow(clippy::cast_possible_truncation)]
        let last_byte = 0x20 + i as u8;
        model.discovered.push(DeviceEntry { addr: addr(last_byte), name: std::format!("Cans {i}"), rssi: -65, class_of_device: 0x04 << 8 });
    }
    emit_radio_fixture("scan-list-filters-and-caps", &model, false, 3);
}

#[test]
fn emit_radio_connecting_device_initiated() {
    let model = BtModel { connecting: true, attempt: Some(ConnectAttempt { seq: 12, addr: addr(0x01), initiator: ConnectInitiator::Device, step: Some(ConnectStep::Pairing), retries: 0 }), ..Default::default() };
    emit_radio_fixture("connecting-device-initiated", &model, true, 4);
}

#[test]
fn emit_radio_connecting_host_initiated_no_step_yet() {
    let model = BtModel { connecting: true, attempt: Some(ConnectAttempt { seq: 13, addr: addr(0x02), initiator: ConnectInitiator::Host, step: None, retries: 0 }), ..Default::default() };
    emit_radio_fixture("connecting-host-initiated-no-step", &model, false, 5);
}

#[test]
fn emit_radio_last_outcome_ok() {
    let model = BtModel { last_outcome: Some(ConnectOutcome { seq: 20, addr: addr(0x03), result: ConnectOutcomeResult::Ok, reason: None }), ..Default::default() };
    emit_radio_fixture("last-outcome-ok", &model, false, 6);
}

#[test]
fn emit_radio_last_outcome_failed_with_reason() {
    let model = BtModel { last_outcome: Some(ConnectOutcome { seq: 21, addr: addr(0x04), result: ConnectOutcomeResult::Failed, reason: Some(ConnectFailureReason::NoA2dpSink) }), ..Default::default() };
    emit_radio_fixture("last-outcome-failed-no-a2dp-sink", &model, false, 7);
}

#[test]
fn emit_radio_last_outcome_cancelled() {
    // The `chc3` extension (this module's/`radio.rs`'s doc comments): a
    // cancelled attempt must be a distinct `outcome` value from `failed`.
    let model = BtModel { last_outcome: Some(ConnectOutcome { seq: 22, addr: addr(0x05), result: ConnectOutcomeResult::Cancelled, reason: None }), ..Default::default() };
    emit_radio_fixture("last-outcome-cancelled", &model, false, 8);
}

#[test]
fn emit_radio_paired_full_and_store_ready() {
    let mut model = BtModel { store_status: Some(super::events::StoreStatus::Loaded), ..Default::default() };
    for i in 0..super::model::MAX_PAIRED_DEVICES {
        #[allow(clippy::cast_possible_truncation)]
        let last_byte = i as u8;
        model.paired.push(PairedDevice { addr: addr(last_byte), name: std::format!("Buds {i}"), mru_seq: 1, ldac_quality: 0, preset_id: 0 });
    }
    emit_radio_fixture("paired-full-and-store-ready", &model, false, 9);
}

#[test]
fn emit_radio_max_scan_list() {
    let mut model = BtModel::default();
    for i in 0..MAX_SCAN_LIST_ITEMS {
        #[allow(clippy::cast_possible_truncation)]
        let last_byte = i as u8;
        model.discovered.push(DeviceEntry { addr: addr(last_byte), name: std::format!("Cans {i}"), rssi: -40, class_of_device: 0x04 << 8 });
    }
    let model_ref = &model;
    emit_radio_fixture("max-scan-list", model_ref, false, 10);
}

#[test]
fn header_constants_match_this_proto() {
    // Cheap belt-and-braces: the fixtures above are only meaningful if
    // these constants are what this module's doc comment (and every
    // `radio-*.bin`'s first bytes) assumes.
    assert_eq!(RADIO_PROTO, 1);
    assert_eq!(SCAN_RECORD_LEN, 42);
    assert_eq!(RADIO_SNAPSHOT_MAX_LEN, 540);
}

// --- reasons.json: `ConnectFailureReason`'s wire-code/retryable/text -----
// table design sec 13.4 asks for. Every value below is read straight off
// the real enum's own `wire`/`retryable`/`text` methods, never hand-
// copied -- a renumbering of the enum changes `actual` here and fails the
// fixture check, the same discipline `host_op_fixtures::emit_op_errors`
// uses for `OpError`.

#[test]
fn emit_reasons() {
    let reasons = [
        ("TIMEOUT", ConnectFailureReason::Timeout),
        ("REJECTED", ConnectFailureReason::Rejected),
        ("NO_A2DP_SINK", ConnectFailureReason::NoA2dpSink),
        ("NEEDS_PIN", ConnectFailureReason::NeedsPin),
        ("RADIO_ERROR", ConnectFailureReason::RadioError),
    ];

    let entries: Vec<String> = reasons
        .iter()
        .map(|(name, reason)| {
            std::format!(
                "    {}: {{\"wire\": {}, \"retryable\": {}, \"text\": {}}}",
                json_string(name),
                reason.wire(),
                json_bool(reason.retryable()),
                json_string(reason.text()),
            )
        })
        .collect();

    let json = std::format!("{{\n  \"reasons\": {{\n{}\n  }}\n}}\n", entries.join(",\n"));
    check_or_write_text("reasons.json", &json);
}
