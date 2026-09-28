//! `GET_RADIO` (`0x08`, IN) -- a snapshot of the Bluetooth radio's live
//! session state (discovery + connect progress) for the web companion,
//! design `.planning/design/2026-09-27-iface6-eq-management-protocol.md`
//! sec 13.4 (bead `pico-link-jyhk.27`, the design's R2, "off by one" from
//! its own sec 13.8 numbering -- see the INVESTIGATION comment on that
//! bead). Mirrors [`super::library::encode_library_snapshot`]'s shape:
//! Rust encodes in the superloop under the poll-recency gate, C publishes,
//! SETUP `memcpy`s.
//!
//! # Why a third snapshot, not folded into `GET_LIBRARY` or a telemetry page
//!
//! Design sec 13.4: not a telemetry page (page 0 already owns the one
//! generate-on-request buffer `usb_config_itf.c` maintains --
//! alternating page 0 and a radio page would serve the wrong one), and not
//! folded into `GET_LIBRARY` (scan churn bumping `library_rev` would force
//! a 1 KB effect re-read on every inquiry result).
//!
//! # Layout, `radio_proto` 1
//!
//! Little-endian. Header [`HEADER_LEN`] (36) bytes, then up to
//! [`super::model::MAX_SCAN_LIST_ITEMS`] (12) scan records of
//! [`SCAN_RECORD_LEN`] (42) bytes each -- design sec 13.4's "Max 36 + 12 x
//! 42 = 540 B".
//!
//! | Bytes | Field | Notes |
//! |---|---|---|
//! | 0..1 | `radio_proto` (`u8`) | [`RADIO_PROTO`] |
//! | 1..2 | reserved (`u8`) | Always `0` |
//! | 2..4 | `len` (`u16`) | `HEADER_LEN + scan_count * SCAN_RECORD_LEN` |
//! | 4..6 | `radio_rev` (`u16`) | Bumped only on a byte difference from the last encode -- see [`super::App::radio_snapshot`] |
//! | 6..7 | `flags` (`u8`) | bit0 discovering, bit1 connecting, bit2 `device_wizard_open`, bit3 `paired_full`, bit4 `store_ready` |
//! | 7..8 | `scan_owner` (`u8`) | `0` none, `1` device, `2` host -- [`scan_owner_wire`] |
//! | 8..10 | `scan_seq` (`u16`) | [`super::model::BtModel::scan_seq`] |
//! | 10..12 | `attempt_seq` (`u16`) | `0` when [`super::model::BtModel::attempt`] is `None` |
//! | 12..13 | `initiator` (`u8`) | `0` none, `1` device, `2` host, `3` auto-reconnect -- [`initiator_wire`] |
//! | 13..14 | `step` (`u8`) | `0` none, else [`connect_step_wire`] |
//! | 14..20 | attempt `addr` (`[u8; 6]`) | All-zero when no attempt in flight |
//! | 20..21 | `retries` (`u8`) | |
//! | 21..22 | reserved (`u8`) | Always `0` |
//! | 22..24 | `outcome_seq` (`u16`) | The `attempt_seq` this outcome concludes; `0` when [`super::model::BtModel::last_outcome`] is `None` |
//! | 24..25 | `outcome` (`u8`) | `0` none, `1` ok, `2` `ok_degraded`, `3` failed, `4` cancelled -- [`outcome_wire`] |
//! | 25..26 | `reason` (`u8`) | `0` none, else [`super::events::ConnectFailureReason::wire`] |
//! | 26..32 | outcome `addr` (`[u8; 6]`) | All-zero when no last outcome |
//! | 32..33 | `scan_count` (`u8`) | |
//! | 33..34 | `scan_rec_len` (`u8`) | [`SCAN_RECORD_LEN`] -- lets an old host skip a future tail |
//! | 34..35 | `scan_total_audio` (`u8`) | Audio-sink count before the [`super::model::MAX_SCAN_LIST_ITEMS`] cap |
//! | 35..36 | reserved (`u8`) | Always `0` |
//!
//! Scan record ([`SCAN_RECORD_LEN`], 42 B): `addr` (`[u8; 6]`), `bars`
//! (`u8`, 0..4, [`super::model::signal_bar_level`]), `flags` (`u8`: bit0
//! `already_paired`), `name_len` (`u8`), `name` (`[u8; 32]`, zero-padded
//! past `name_len`), reserved (`u8`). The list is EXACTLY what the wizard
//! shows -- [`super::model::scan_list_view`], the one core fn both this
//! encoder and `crate::render::wizard::build_scan_list` call (design sec
//! 13.4).
//!
//! # `outcome`'s `4 cancelled` -- an extension past the design comment
//!
//! The design comment (written before bead `pico-link-chc3`, "cancel-
//! connect v2") only enumerates `outcome` as `0`/`1`/`2`/`3`
//! (`none`/`ok`/`ok_degraded`/`failed`). `chc3` added
//! [`super::model::ConnectOutcomeResult::Cancelled`] as a fourth, real
//! outcome ([`super::radio_actions::cancel_connect`]'s doc comment) --
//! omitting it from this wire format would make a cancelled attempt
//! indistinguishable from a failed one to a web client, which is exactly
//! the ambiguity `chc3` introduced `Cancelled` to resolve on-device.
//! `4 cancelled` is a one-ordinal, append-only extension of the design's
//! own enumeration, not a redesign -- see [`outcome_wire`].
//!
//! # Why a caller-provided rev, not a clock or a stored counter here
//!
//! Same reasoning as [`super::library::encode_library_snapshot`]'s own doc
//! comment: this module has no opinion on *when* the radio session state
//! changed or how the revision counter advances -- both belong to the call
//! site ([`super::App::radio_snapshot`]) that can compare successive
//! encodes. [`encode_radio_snapshot`] is a pure projection: same inputs
//! (including `radio_rev`), same bytes, every time.

use alloc::string::String;
use alloc::vec::Vec;

use super::events::{ConnectFailureReason, ConnectStep};
use super::model::{scan_list_view, signal_bar_level, BtModel, ConnectInitiator, ConnectOutcomeResult, DeviceAddr, MAX_SCAN_LIST_ITEMS};

/// This payload's protocol version -- design sec 13.4's `radio_proto 1`.
pub(crate) const RADIO_PROTO: u8 = 1;

/// The scan record's name field's fixed wire width -- matches
/// [`super::model`]'s private `MAX_DEVICE_NAME_BYTES` on-flash/on-wire cap
/// exactly, the same deliberately-duplicated-constant shape
/// [`super::library::DEVICE_NAME_CAP`]/[`super::telemetry::DEVICE_NAME_CAP`]
/// already use for the identical reason.
const DEVICE_NAME_CAP: usize = 32;

const HEADER_LEN: usize = 36;
/// `addr` (6) + `bars` (1) + `flags` (1) + `name_len` (1) + `name` ([`DEVICE_NAME_CAP`], 32) + reserved (1).
pub(crate) const SCAN_RECORD_LEN: usize = 6 + 1 + 1 + 1 + DEVICE_NAME_CAP + 1;

/// The largest a [`encode_radio_snapshot`] payload can ever be: header +
/// [`MAX_SCAN_LIST_ITEMS`] scan records (design sec 13.4: "Max 36 + 12 x 42
/// = 540 B").
pub(crate) const RADIO_SNAPSHOT_MAX_LEN: usize = HEADER_LEN + MAX_SCAN_LIST_ITEMS * SCAN_RECORD_LEN;

// --- Fixed byte offsets, header only (scan record offsets are computed) -

const OFF_RADIO_PROTO: usize = 0;
const OFF_RESERVED1: usize = 1;
const OFF_LEN: usize = 2;
pub(crate) const OFF_RADIO_REV: usize = 4;
const OFF_FLAGS: usize = 6;
const OFF_SCAN_OWNER: usize = 7;
const OFF_SCAN_SEQ: usize = 8;
const OFF_ATTEMPT_SEQ: usize = 10;
const OFF_INITIATOR: usize = 12;
const OFF_STEP: usize = 13;
const OFF_ATTEMPT_ADDR: usize = 14;
const OFF_RETRIES: usize = 20;
const OFF_RESERVED2: usize = 21;
const OFF_OUTCOME_SEQ: usize = 22;
const OFF_OUTCOME: usize = 24;
const OFF_REASON: usize = 25;
const OFF_OUTCOME_ADDR: usize = 26;
const OFF_SCAN_COUNT: usize = 32;
const OFF_SCAN_REC_LEN: usize = 33;
const OFF_SCAN_TOTAL_AUDIO: usize = 34;
const OFF_RESERVED3: usize = 35;

/// `flags` bit positions (design sec 13.4).
const FLAG_DISCOVERING: u8 = 1 << 0;
const FLAG_CONNECTING: u8 = 1 << 1;
const FLAG_DEVICE_WIZARD_OPEN: u8 = 1 << 2;
const FLAG_PAIRED_FULL: u8 = 1 << 3;
const FLAG_STORE_READY: u8 = 1 << 4;

/// Scan record `flags` bit positions (design sec 13.4).
const SCAN_FLAG_ALREADY_PAIRED: u8 = 1 << 0;

/// Truncates `s` to at most `cap` bytes, respecting a UTF-8 **character**
/// boundary -- the same discipline every other snapshot encoder in this
/// crate documents (e.g. [`super::library::truncate_utf8`]), duplicated
/// here rather than exposed across modules for one caller.
fn truncate_utf8(s: &str, cap: usize) -> &str {
    if s.len() <= cap {
        return s;
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Writes a fixed-width name field: one length-prefix byte, then `cap`
/// bytes, zero-padded past the written length -- the same shape every
/// other snapshot encoder in this crate uses.
fn write_fixed_str(buf: &mut [u8], len_off: usize, bytes_off: usize, cap: usize, s: &str) {
    let truncated = truncate_utf8(s, cap);
    let bytes = truncated.as_bytes();
    #[allow(clippy::cast_possible_truncation)] // `cap` is always <= 32 in this module.
    let len = bytes.len() as u8;
    buf[len_off] = len;
    buf[bytes_off..bytes_off + bytes.len()].copy_from_slice(bytes);
    for b in &mut buf[bytes_off + bytes.len()..bytes_off + cap] {
        *b = 0;
    }
}

/// Reads a fixed name field back, lossily -- see
/// [`super::library::read_fixed_str`]'s doc comment for why lossy decoding
/// is fine here.
#[allow(dead_code)] // Host-side decode helper; no call site yet (see this module's doc comment).
fn read_fixed_str(buf: &[u8], len_off: usize, bytes_off: usize, cap: usize) -> String {
    let len = (buf[len_off] as usize).min(cap);
    String::from_utf8_lossy(&buf[bytes_off..bytes_off + len]).into_owned()
}

/// `ScanOwner` -> wire byte (design sec 13.4: "0 none, 1 device, 2 host").
fn scan_owner_wire(owner: super::model::ScanOwner) -> u8 {
    match owner {
        super::model::ScanOwner::None => 0,
        super::model::ScanOwner::Device => 1,
        super::model::ScanOwner::Host => 2,
    }
}

/// `ScanOwner` wire byte -> `ScanOwner` -- host-side decode counterpart of
/// [`scan_owner_wire`]. An unrecognised byte decodes to `None`, the same
/// "unknown decodes to the safest default, never panics" rule every other
/// wire decoder in this crate follows.
#[allow(dead_code)] // Host-side decode helper; no call site yet (see this module's doc comment).
fn scan_owner_from_wire(byte: u8) -> super::model::ScanOwner {
    match byte {
        1 => super::model::ScanOwner::Device,
        2 => super::model::ScanOwner::Host,
        _ => super::model::ScanOwner::None,
    }
}

/// `ConnectInitiator` -> wire byte (design sec 13.4: "0 none, 1 device, 2
/// host, 3 auto-reconnect"). `0` ("none") is never returned by this
/// function -- it is [`super::model::BtModel::attempt`] being `None`, the
/// same "the absent case is a `None`, not a variant" convention
/// [`super::model::ConnectInitiator`]'s own doc comment documents.
fn initiator_wire(initiator: ConnectInitiator) -> u8 {
    match initiator {
        ConnectInitiator::Device => 1,
        ConnectInitiator::Host => 2,
        ConnectInitiator::AutoReconnect => 3,
    }
}

/// Wire byte -> `ConnectInitiator`, for a decoded `initiator` byte already
/// known to be nonzero (`0`/unrecognised both map to `Device`, matching the
/// "unknown decodes to the safest default" rule -- there is no `None`
/// variant on [`ConnectInitiator`] to fall back to instead, since a nonzero
/// `initiator` byte always accompanies a real decoded attempt).
#[allow(dead_code)] // Host-side decode helper; no call site yet (see this module's doc comment).
fn initiator_from_wire(byte: u8) -> ConnectInitiator {
    match byte {
        2 => ConnectInitiator::Host,
        3 => ConnectInitiator::AutoReconnect,
        _ => ConnectInitiator::Device,
    }
}

/// `ConnectStep` -> this wire format's `step` byte (design sec 13.4: "step
/// (`ConnectStep` wire; 0 none)"). Note this is NOT the same ordinal space
/// `ui-ffi`'s `PlConnectStep` uses for `Event::ConnectStepChanged` (that
/// one is 0-based, `Connecting == 0`, with no "none" value because the
/// event only ever fires for a real step) -- `GET_RADIO`'s `step` byte
/// needs its own "none" (`0`, [`super::model::ConnectAttempt::step`] being
/// `None`), so every named step here is shifted up by one.
fn connect_step_wire(step: ConnectStep) -> u8 {
    match step {
        ConnectStep::Connecting => 1,
        ConnectStep::Pairing => 2,
        ConnectStep::SettingUpAudio => 3,
        ConnectStep::NegotiatingCodec => 4,
        ConnectStep::Disconnecting => 5,
    }
}

/// Wire byte -> `ConnectStep`, for a decoded `step` byte already known to
/// be nonzero. An unrecognised nonzero byte decodes to `Connecting` --
/// same "unknown decodes to the safest default" rule [`initiator_from_wire`]
/// documents.
#[allow(dead_code)] // Host-side decode helper; no call site yet (see this module's doc comment).
fn connect_step_from_wire(byte: u8) -> ConnectStep {
    match byte {
        2 => ConnectStep::Pairing,
        3 => ConnectStep::SettingUpAudio,
        4 => ConnectStep::NegotiatingCodec,
        5 => ConnectStep::Disconnecting,
        _ => ConnectStep::Connecting,
    }
}

/// `ConnectOutcomeResult` -> wire byte (design sec 13.4 plus this module's
/// doc comment on `4 cancelled` -- an append-only extension past the
/// pre-`chc3` design comment).
fn outcome_wire(result: ConnectOutcomeResult) -> u8 {
    match result {
        ConnectOutcomeResult::Ok => 1,
        ConnectOutcomeResult::OkDegraded => 2,
        ConnectOutcomeResult::Failed => 3,
        ConnectOutcomeResult::Cancelled => 4,
    }
}

/// Wire byte -> `ConnectOutcomeResult`, for a decoded `outcome` byte
/// already known to be nonzero. An unrecognised nonzero byte decodes to
/// `Failed` -- the most conservative "something went wrong" reading,
/// rather than silently claiming success for a byte this decoder doesn't
/// recognise.
#[allow(dead_code)] // Host-side decode helper; no call site yet (see this module's doc comment).
fn outcome_from_wire(byte: u8) -> ConnectOutcomeResult {
    match byte {
        1 => ConnectOutcomeResult::Ok,
        2 => ConnectOutcomeResult::OkDegraded,
        4 => ConnectOutcomeResult::Cancelled,
        _ => ConnectOutcomeResult::Failed,
    }
}

/// `ConnectFailureReason` wire byte -> `ConnectFailureReason`, for a
/// decoded `reason` byte already known to be nonzero. An unrecognised
/// nonzero byte decodes to `RadioError` -- the least specific, most
/// generic reason, rather than claiming a more specific (and wrong) cause.
#[allow(dead_code)] // Host-side decode helper; no call site yet (see this module's doc comment).
fn reason_from_wire(byte: u8) -> ConnectFailureReason {
    match byte {
        1 => ConnectFailureReason::Timeout,
        2 => ConnectFailureReason::Rejected,
        3 => ConnectFailureReason::NoA2dpSink,
        4 => ConnectFailureReason::NeedsPin,
        _ => ConnectFailureReason::RadioError,
    }
}

/// One decoded scan record -- [`decode_radio_snapshot`]'s per-device
/// output.
#[allow(dead_code)] // Host-side decode output; no call site yet (see this module's doc comment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedScanEntry {
    pub(crate) addr: DeviceAddr,
    pub(crate) bars: u8,
    pub(crate) already_paired: bool,
    pub(crate) name: String,
}

/// The decoded form of a [`encode_radio_snapshot`] payload -- see this
/// module's doc comment for the wire layout each field round-trips.
///
/// `clippy::struct_excessive_bools` is silenced deliberately, same
/// reasoning as [`super::telemetry::HomeSnapshot`]'s own doc comment:
/// these are independent presence/state bits straight off the wire's own
/// `flags` byte, not a state machine masquerading as booleans.
#[allow(clippy::struct_excessive_bools)]
#[allow(dead_code)] // Host-side decode output; no call site yet (see this module's doc comment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RadioSnapshot {
    pub(crate) radio_rev: u16,
    pub(crate) discovering: bool,
    pub(crate) connecting: bool,
    pub(crate) device_wizard_open: bool,
    pub(crate) paired_full: bool,
    pub(crate) store_ready: bool,
    pub(crate) scan_owner: super::model::ScanOwner,
    pub(crate) scan_seq: u16,
    /// `None` when `attempt_seq` decoded as `0` -- mirrors
    /// [`super::model::BtModel::attempt`] being `None`.
    pub(crate) attempt: Option<DecodedAttempt>,
    /// `None` when `outcome_seq`/`outcome` decoded as `0` -- mirrors
    /// [`super::model::BtModel::last_outcome`] being `None`.
    pub(crate) last_outcome: Option<DecodedOutcome>,
    pub(crate) scan_total_audio: u8,
    pub(crate) scan: Vec<DecodedScanEntry>,
}

/// [`RadioSnapshot::attempt`]'s decoded shape.
#[allow(dead_code)] // Host-side decode output; no call site yet (see this module's doc comment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecodedAttempt {
    pub(crate) seq: u16,
    pub(crate) addr: DeviceAddr,
    pub(crate) initiator: ConnectInitiator,
    /// `None` when the wire `step` byte was `0`.
    pub(crate) step: Option<ConnectStep>,
    pub(crate) retries: u8,
}

/// [`RadioSnapshot::last_outcome`]'s decoded shape.
#[allow(dead_code)] // Host-side decode output; no call site yet (see this module's doc comment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecodedOutcome {
    pub(crate) seq: u16,
    pub(crate) addr: DeviceAddr,
    pub(crate) result: ConnectOutcomeResult,
    /// `None` unless `result == Failed` and the wire `reason` byte was
    /// nonzero.
    pub(crate) reason: Option<ConnectFailureReason>,
}

/// Builds the `GET_RADIO` snapshot from a live `&BtModel`, at the caller-
/// supplied `radio_rev` (same "caller owns the rev counter" contract
/// [`super::library::encode_library_snapshot`]'s doc comment documents).
/// `device_wizard_open` is the one piece of state this module cannot read
/// off `BtModel` itself -- it is a `Navigator` stack query
/// ([`super::App::radio_snapshot`]'s call site, design sec 13.5: "device
/// wizard open ... is a navigator query").
///
/// Returns a `Vec<u8>` sized exactly to `len` (header + however many scan
/// records actually exist) -- never padded out to
/// [`RADIO_SNAPSHOT_MAX_LEN`], matching [`super::library::
/// encode_library_snapshot`]'s "one consistent snapshot," not a
/// fixed-size wire record.
#[must_use]
pub(crate) fn encode_radio_snapshot(model: &BtModel, device_wizard_open: bool, radio_rev: u16) -> Vec<u8> {
    let (scan_capped, scan_total_audio) = scan_list_view(&model.discovered);
    #[allow(clippy::cast_possible_truncation)] // `scan_list_view` already caps at `MAX_SCAN_LIST_ITEMS` (12), well under `u8::MAX`.
    let scan_count = scan_capped.len() as u8;
    #[allow(clippy::cast_possible_truncation)] // The pre-cap audio-sink count can in principle exceed 255 in a crowded room; saturate rather than wrap so "showing 12 of N" never reads back as a small N.
    let scan_total_audio_u8 = scan_total_audio.min(u8::MAX as usize) as u8;

    let len = HEADER_LEN + usize::from(scan_count) * SCAN_RECORD_LEN;
    let mut buf = alloc::vec![0u8; len];

    buf[OFF_RADIO_PROTO] = RADIO_PROTO;
    buf[OFF_RESERVED1] = 0;
    #[allow(clippy::cast_possible_truncation)] // `len` is well under `u16::MAX` (`RADIO_SNAPSHOT_MAX_LEN` is 540).
    let len_u16 = len as u16;
    buf[OFF_LEN..OFF_LEN + 2].copy_from_slice(&len_u16.to_le_bytes());
    buf[OFF_RADIO_REV..OFF_RADIO_REV + 2].copy_from_slice(&radio_rev.to_le_bytes());

    let mut flags = 0u8;
    if model.discovering {
        flags |= FLAG_DISCOVERING;
    }
    if model.connecting {
        flags |= FLAG_CONNECTING;
    }
    if device_wizard_open {
        flags |= FLAG_DEVICE_WIZARD_OPEN;
    }
    if model.paired.len() >= super::model::MAX_PAIRED_DEVICES {
        flags |= FLAG_PAIRED_FULL;
    }
    if model.store_status.is_some() {
        flags |= FLAG_STORE_READY;
    }
    buf[OFF_FLAGS] = flags;

    buf[OFF_SCAN_OWNER] = scan_owner_wire(model.scan_owner);
    buf[OFF_SCAN_SEQ..OFF_SCAN_SEQ + 2].copy_from_slice(&model.scan_seq.to_le_bytes());
    buf[OFF_RESERVED2] = 0;

    // `attempt_seq`/`initiator`/`step`/addr/`retries` all stay zeroed when
    // there is no attempt in flight -- `0` is "no attempt in flight" for
    // every one of these fields (this module's doc comment table).
    if let Some(attempt) = &model.attempt {
        buf[OFF_ATTEMPT_SEQ..OFF_ATTEMPT_SEQ + 2].copy_from_slice(&attempt.seq.to_le_bytes());
        buf[OFF_INITIATOR] = initiator_wire(attempt.initiator);
        buf[OFF_STEP] = attempt.step.map_or(0, connect_step_wire);
        buf[OFF_ATTEMPT_ADDR..OFF_ATTEMPT_ADDR + 6].copy_from_slice(&attempt.addr);
        #[allow(clippy::cast_possible_truncation)] // Retries realistically stay single-digit (one 0x0b page-timeout retry per design sec 13.1); saturate rather than wrap regardless.
        let retries = attempt.retries.min(u16::from(u8::MAX)) as u8;
        buf[OFF_RETRIES] = retries;
    }

    // `outcome_seq`/`outcome`/`reason`/addr all stay zeroed when there is
    // no concluded attempt yet -- same "0 means absent" convention.
    if let Some(outcome) = &model.last_outcome {
        buf[OFF_OUTCOME_SEQ..OFF_OUTCOME_SEQ + 2].copy_from_slice(&outcome.seq.to_le_bytes());
        buf[OFF_OUTCOME] = outcome_wire(outcome.result);
        buf[OFF_REASON] = outcome.reason.map_or(0, ConnectFailureReason::wire);
        buf[OFF_OUTCOME_ADDR..OFF_OUTCOME_ADDR + 6].copy_from_slice(&outcome.addr);
    }

    buf[OFF_SCAN_COUNT] = scan_count;
    #[allow(clippy::cast_possible_truncation)] // SCAN_RECORD_LEN (42) always fits u8.
    let scan_rec_len = SCAN_RECORD_LEN as u8;
    buf[OFF_SCAN_REC_LEN] = scan_rec_len;
    buf[OFF_SCAN_TOTAL_AUDIO] = scan_total_audio_u8;
    buf[OFF_RESERVED3] = 0;

    let mut off = HEADER_LEN;
    for device in &scan_capped {
        buf[off..off + 6].copy_from_slice(&device.addr);
        buf[off + 6] = signal_bar_level(device.rssi);
        let already_paired = model.paired.iter().any(|p| p.addr == device.addr);
        buf[off + 7] = if already_paired { SCAN_FLAG_ALREADY_PAIRED } else { 0 };
        write_fixed_str(&mut buf, off + 8, off + 9, DEVICE_NAME_CAP, &device.name);
        buf[off + 41] = 0;
        off += SCAN_RECORD_LEN;
    }

    buf
}

/// Decodes a [`encode_radio_snapshot`] payload. Returns `None` if `bytes`
/// is too short to hold its own declared header, `radio_proto` doesn't
/// match what this module encodes, or the declared `len`/`scan_count`
/// don't fit inside `bytes` -- the same "on an unknown/malformed payload,
/// the host bails" discipline [`super::library::decode_library_snapshot`]
/// follows.
#[must_use]
#[allow(dead_code)] // Host-side decode; no call site yet (see this module's doc comment).
pub(crate) fn decode_radio_snapshot(bytes: &[u8]) -> Option<RadioSnapshot> {
    if bytes.len() < HEADER_LEN {
        return None;
    }
    if bytes[OFF_RADIO_PROTO] != RADIO_PROTO {
        return None;
    }

    let radio_rev = u16::from_le_bytes(bytes[OFF_RADIO_REV..OFF_RADIO_REV + 2].try_into().ok()?);
    let flags = bytes[OFF_FLAGS];
    let scan_owner = scan_owner_from_wire(bytes[OFF_SCAN_OWNER]);
    let scan_seq = u16::from_le_bytes(bytes[OFF_SCAN_SEQ..OFF_SCAN_SEQ + 2].try_into().ok()?);

    let attempt_seq = u16::from_le_bytes(bytes[OFF_ATTEMPT_SEQ..OFF_ATTEMPT_SEQ + 2].try_into().ok()?);
    let attempt = if attempt_seq == 0 {
        None
    } else {
        let initiator = initiator_from_wire(bytes[OFF_INITIATOR]);
        let step_byte = bytes[OFF_STEP];
        let step = if step_byte == 0 { None } else { Some(connect_step_from_wire(step_byte)) };
        let mut addr = [0u8; 6];
        addr.copy_from_slice(&bytes[OFF_ATTEMPT_ADDR..OFF_ATTEMPT_ADDR + 6]);
        let retries = bytes[OFF_RETRIES];
        Some(DecodedAttempt { seq: attempt_seq, addr, initiator, step, retries })
    };

    let outcome_seq = u16::from_le_bytes(bytes[OFF_OUTCOME_SEQ..OFF_OUTCOME_SEQ + 2].try_into().ok()?);
    let outcome_byte = bytes[OFF_OUTCOME];
    let last_outcome = if outcome_seq == 0 || outcome_byte == 0 {
        None
    } else {
        let result = outcome_from_wire(outcome_byte);
        let reason_byte = bytes[OFF_REASON];
        let reason = if matches!(result, ConnectOutcomeResult::Failed) && reason_byte != 0 { Some(reason_from_wire(reason_byte)) } else { None };
        let mut addr = [0u8; 6];
        addr.copy_from_slice(&bytes[OFF_OUTCOME_ADDR..OFF_OUTCOME_ADDR + 6]);
        Some(DecodedOutcome { seq: outcome_seq, addr, result, reason })
    };

    let scan_count = usize::from(bytes[OFF_SCAN_COUNT]);
    let scan_rec_len = usize::from(bytes[OFF_SCAN_REC_LEN]);
    let scan_total_audio = bytes[OFF_SCAN_TOTAL_AUDIO];

    // Same "a newer proto's wider records are skippable via `*_rec_len`,
    // but this decoder only understands `radio_proto` 1's own record
    // shape" rule [`super::library::decode_library_snapshot`] documents.
    if scan_rec_len != SCAN_RECORD_LEN {
        return None;
    }

    let scan_end = HEADER_LEN.checked_add(scan_count.checked_mul(scan_rec_len)?)?;
    if bytes.len() < scan_end {
        return None;
    }

    let mut scan = Vec::with_capacity(scan_count);
    let mut off = HEADER_LEN;
    for _ in 0..scan_count {
        let mut addr = [0u8; 6];
        addr.copy_from_slice(&bytes[off..off + 6]);
        let bars = bytes[off + 6];
        let record_flags = bytes[off + 7];
        let name = read_fixed_str(bytes, off + 8, off + 9, DEVICE_NAME_CAP);
        scan.push(DecodedScanEntry { addr, bars, already_paired: record_flags & SCAN_FLAG_ALREADY_PAIRED != 0, name });
        off += scan_rec_len;
    }

    Some(RadioSnapshot {
        radio_rev,
        discovering: flags & FLAG_DISCOVERING != 0,
        connecting: flags & FLAG_CONNECTING != 0,
        device_wizard_open: flags & FLAG_DEVICE_WIZARD_OPEN != 0,
        paired_full: flags & FLAG_PAIRED_FULL != 0,
        store_ready: flags & FLAG_STORE_READY != 0,
        scan_owner,
        scan_seq,
        attempt,
        last_outcome,
        scan_total_audio,
        scan,
    })
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use super::*;
    use crate::app::model::{ConnectAttempt, ConnectOutcome, DeviceEntry, PairedDevice, ScanOwner};

    fn addr(last_byte: u8) -> [u8; 6] {
        [0x94, 0xDB, 0x56, 0x54, 0x7C, last_byte]
    }

    #[test]
    fn encode_empty_model_reports_zero_counts_and_header_only_length() {
        let model = BtModel::default();
        let bytes = encode_radio_snapshot(&model, false, 3);
        assert_eq!(bytes.len(), HEADER_LEN, "no attempt, no outcome, no scan results -- header only");
        let snap = decode_radio_snapshot(&bytes).expect("a well-formed encode must always decode");
        assert_eq!(snap.radio_rev, 3);
        assert!(!snap.discovering);
        assert!(!snap.connecting);
        assert!(!snap.device_wizard_open);
        assert!(!snap.paired_full);
        assert!(!snap.store_ready);
        assert_eq!(snap.scan_owner, ScanOwner::None);
        assert!(snap.attempt.is_none());
        assert!(snap.last_outcome.is_none());
        assert!(snap.scan.is_empty());
    }

    #[test]
    fn encode_round_trips_flags() {
        let mut model = BtModel { discovering: true, connecting: true, store_status: Some(super::super::events::StoreStatus::Loaded), ..Default::default() };
        for i in 0..super::super::model::MAX_PAIRED_DEVICES {
            model.paired.push(PairedDevice { addr: addr(i as u8), name: "Buds".to_string(), mru_seq: 1, ldac_quality: 0, preset_id: 0 });
        }

        let bytes = encode_radio_snapshot(&model, true, 1);
        let snap = decode_radio_snapshot(&bytes).unwrap();
        assert!(snap.discovering);
        assert!(snap.connecting);
        assert!(snap.device_wizard_open);
        assert!(snap.paired_full);
        assert!(snap.store_ready);
    }

    #[test]
    fn encode_round_trips_scan_owner_and_seq() {
        let model = BtModel { scan_owner: ScanOwner::Host, scan_seq: 7, ..Default::default() };

        let bytes = encode_radio_snapshot(&model, false, 1);
        let snap = decode_radio_snapshot(&bytes).unwrap();
        assert_eq!(snap.scan_owner, ScanOwner::Host);
        assert_eq!(snap.scan_seq, 7);
    }

    #[test]
    fn encode_round_trips_an_in_flight_attempt_with_a_step() {
        let model = BtModel { attempt: Some(ConnectAttempt { seq: 42, addr: addr(0x01), initiator: ConnectInitiator::Host, step: Some(ConnectStep::Pairing), retries: 2 }), ..Default::default() };

        let bytes = encode_radio_snapshot(&model, false, 1);
        let snap = decode_radio_snapshot(&bytes).unwrap();
        let attempt = snap.attempt.expect("attempt must round-trip");
        assert_eq!(attempt.seq, 42);
        assert_eq!(attempt.addr, addr(0x01));
        assert_eq!(attempt.initiator, ConnectInitiator::Host);
        assert_eq!(attempt.step, Some(ConnectStep::Pairing));
        assert_eq!(attempt.retries, 2);
    }

    #[test]
    fn encode_an_attempt_with_no_step_yet_decodes_to_none() {
        let model = BtModel { attempt: Some(ConnectAttempt { seq: 1, addr: addr(0x01), initiator: ConnectInitiator::Device, step: None, retries: 0 }), ..Default::default() };

        let bytes = encode_radio_snapshot(&model, false, 1);
        let snap = decode_radio_snapshot(&bytes).unwrap();
        assert_eq!(snap.attempt.expect("attempt must round-trip").step, None);
    }

    #[test]
    fn encode_round_trips_a_successful_outcome() {
        let model = BtModel { last_outcome: Some(ConnectOutcome { seq: 5, addr: addr(0x02), result: ConnectOutcomeResult::Ok, reason: None }), ..Default::default() };

        let bytes = encode_radio_snapshot(&model, false, 1);
        let snap = decode_radio_snapshot(&bytes).unwrap();
        let outcome = snap.last_outcome.expect("outcome must round-trip");
        assert_eq!(outcome.seq, 5);
        assert_eq!(outcome.addr, addr(0x02));
        assert_eq!(outcome.result, ConnectOutcomeResult::Ok);
        assert_eq!(outcome.reason, None);
    }

    #[test]
    fn encode_round_trips_a_failed_outcome_with_reason() {
        let model = BtModel { last_outcome: Some(ConnectOutcome { seq: 6, addr: addr(0x03), result: ConnectOutcomeResult::Failed, reason: Some(ConnectFailureReason::NoA2dpSink) }), ..Default::default() };

        let bytes = encode_radio_snapshot(&model, false, 1);
        let snap = decode_radio_snapshot(&bytes).unwrap();
        let outcome = snap.last_outcome.expect("outcome must round-trip");
        assert_eq!(outcome.result, ConnectOutcomeResult::Failed);
        assert_eq!(outcome.reason, Some(ConnectFailureReason::NoA2dpSink));
    }

    #[test]
    fn encode_round_trips_a_cancelled_outcome() {
        // The `chc3` extension this module's doc comment documents.
        let model = BtModel { last_outcome: Some(ConnectOutcome { seq: 9, addr: addr(0x04), result: ConnectOutcomeResult::Cancelled, reason: None }), ..Default::default() };

        let bytes = encode_radio_snapshot(&model, false, 1);
        assert_eq!(bytes[OFF_OUTCOME], 4, "cancelled is wire code 4");
        let snap = decode_radio_snapshot(&bytes).unwrap();
        assert_eq!(snap.last_outcome.unwrap().result, ConnectOutcomeResult::Cancelled);
    }

    #[test]
    fn encode_scan_list_matches_the_wizards_filter_and_cap() {
        let mut model = BtModel::default();
        // One audio-classed device, one non-audio (phone) -- only the
        // first should survive `scan_list_view`'s filter.
        model.discovered.push(DeviceEntry { addr: addr(0x10), name: "Cans".to_string(), rssi: -40, class_of_device: 0x04 << 8 });
        model.discovered.push(DeviceEntry { addr: addr(0x11), name: "Phone".to_string(), rssi: -40, class_of_device: 0x01 << 8 });
        model.paired.push(PairedDevice { addr: addr(0x10), name: "Cans".to_string(), mru_seq: 1, ldac_quality: 0, preset_id: 0 });

        let bytes = encode_radio_snapshot(&model, false, 1);
        let snap = decode_radio_snapshot(&bytes).unwrap();
        assert_eq!(snap.scan.len(), 1, "the phone must be filtered out");
        assert_eq!(snap.scan_total_audio, 1);
        assert_eq!(snap.scan[0].addr, addr(0x10));
        assert_eq!(snap.scan[0].bars, 4, "-40 dBm is >= -50, 4 bars");
        assert!(snap.scan[0].already_paired);
        assert_eq!(snap.scan[0].name, "Cans");
    }

    #[test]
    fn encode_caps_the_scan_list_at_max_scan_list_items() {
        let mut model = BtModel::default();
        for i in 0..(MAX_SCAN_LIST_ITEMS + 3) {
            #[allow(clippy::cast_possible_truncation)]
            let last_byte = i as u8;
            model.discovered.push(DeviceEntry { addr: addr(last_byte), name: "Cans".to_string(), rssi: -40, class_of_device: 0 });
        }

        let bytes = encode_radio_snapshot(&model, false, 1);
        assert_eq!(bytes.len(), HEADER_LEN + MAX_SCAN_LIST_ITEMS * SCAN_RECORD_LEN);
        let snap = decode_radio_snapshot(&bytes).unwrap();
        assert_eq!(snap.scan.len(), MAX_SCAN_LIST_ITEMS);
        assert_eq!(snap.scan_total_audio as usize, MAX_SCAN_LIST_ITEMS + 3);
    }

    #[test]
    fn decode_rejects_a_too_short_buffer() {
        assert!(decode_radio_snapshot(&[0u8; HEADER_LEN - 1]).is_none());
    }

    #[test]
    fn decode_rejects_an_unknown_radio_proto() {
        let model = BtModel::default();
        let mut bytes = encode_radio_snapshot(&model, false, 1);
        bytes[OFF_RADIO_PROTO] = 99;
        assert!(decode_radio_snapshot(&bytes).is_none());
    }

    #[test]
    fn decode_rejects_a_truncated_payload_shorter_than_its_own_declared_count() {
        let mut model = BtModel::default();
        model.discovered.push(DeviceEntry { addr: addr(0x01), name: "Cans".to_string(), rssi: -40, class_of_device: 0 });
        let mut bytes = encode_radio_snapshot(&model, false, 1);
        bytes.truncate(HEADER_LEN + 4); // header claims one scan record; body is chopped short.
        assert!(decode_radio_snapshot(&bytes).is_none());
    }

    // --- Layout stability (the "golden bytes" test) --------------------

    /// Locks the exact byte layout for a populated snapshot -- see this
    /// module's doc comment and
    /// [`super::library::tests::golden_bytes_layout_is_stable`]'s doc
    /// comment for why this kind of test exists.
    #[test]
    fn golden_bytes_layout_is_stable() {
        let mut model = BtModel {
            connecting: true,
            scan_owner: ScanOwner::Device,
            scan_seq: 11,
            attempt: Some(ConnectAttempt { seq: 42, addr: addr(0xF2), initiator: ConnectInitiator::Device, step: Some(ConnectStep::NegotiatingCodec), retries: 1 }),
            last_outcome: Some(ConnectOutcome { seq: 41, addr: addr(0xF1), result: ConnectOutcomeResult::Failed, reason: Some(ConnectFailureReason::Timeout) }),
            ..Default::default()
        };
        model.paired.push(PairedDevice { addr: addr(0xF2), name: "Pixel Buds".to_string(), mru_seq: 1, ldac_quality: 4, preset_id: 0 });
        model.discovered.push(DeviceEntry { addr: addr(0xF2), name: "Pixel Buds".to_string(), rssi: -55, class_of_device: 0x04 << 8 });

        let bytes = encode_radio_snapshot(&model, true, 9);

        assert_eq!(bytes.len(), HEADER_LEN + SCAN_RECORD_LEN);
        assert_eq!(HEADER_LEN, 36);
        assert_eq!(SCAN_RECORD_LEN, 42);

        // Header.
        assert_eq!(bytes[0], 1, "radio_proto");
        assert_eq!(bytes[1], 0, "reserved");
        assert_eq!(&bytes[2..4], &(bytes.len() as u16).to_le_bytes(), "len");
        assert_eq!(&bytes[4..6], &9u16.to_le_bytes(), "radio_rev");
        assert_eq!(bytes[6], 0b0000_0110, "flags: connecting | device_wizard_open");
        assert_eq!(bytes[7], 1, "scan_owner: device");
        assert_eq!(&bytes[8..10], &11u16.to_le_bytes(), "scan_seq");
        assert_eq!(&bytes[10..12], &42u16.to_le_bytes(), "attempt_seq");
        assert_eq!(bytes[12], 1, "initiator: device");
        assert_eq!(bytes[13], 4, "step: negotiating codec");
        assert_eq!(&bytes[14..20], &addr(0xF2));
        assert_eq!(bytes[20], 1, "retries");
        assert_eq!(bytes[21], 0, "reserved");
        assert_eq!(&bytes[22..24], &41u16.to_le_bytes(), "outcome_seq");
        assert_eq!(bytes[24], 3, "outcome: failed");
        assert_eq!(bytes[25], 1, "reason: timeout");
        assert_eq!(&bytes[26..32], &addr(0xF1));
        assert_eq!(bytes[32], 1, "scan_count");
        assert_eq!(bytes[33], 42, "scan_rec_len");
        assert_eq!(bytes[34], 1, "scan_total_audio");
        assert_eq!(bytes[35], 0, "reserved");

        // Scan record.
        let rec = &bytes[36..36 + SCAN_RECORD_LEN];
        assert_eq!(&rec[0..6], &addr(0xF2));
        assert_eq!(rec[6], 3, "-55 dBm is >= -60, 3 bars");
        assert_eq!(rec[7], 0b0000_0001, "already_paired");
        assert_eq!(rec[8], 10, "name_len");
        assert_eq!(&rec[9..19], b"Pixel Buds");
        assert!(rec[19..41].iter().all(|&b| b == 0), "name zero-padded past its length");
        assert_eq!(rec[41], 0, "reserved");

        // Full round trip.
        let snap = decode_radio_snapshot(&bytes).unwrap();
        assert_eq!(snap.radio_rev, 9);
        assert!(snap.connecting);
        assert!(snap.device_wizard_open);
        assert_eq!(snap.scan_owner, ScanOwner::Device);
        let attempt = snap.attempt.unwrap();
        assert_eq!(attempt.seq, 42);
        assert_eq!(attempt.step, Some(ConnectStep::NegotiatingCodec));
        let outcome = snap.last_outcome.unwrap();
        assert_eq!(outcome.result, ConnectOutcomeResult::Failed);
        assert_eq!(outcome.reason, Some(ConnectFailureReason::Timeout));
        assert_eq!(snap.scan[0].name, "Pixel Buds");
    }

    #[test]
    fn header_constants_match_this_proto() {
        assert_eq!(RADIO_PROTO, 1);
        assert_eq!(SCAN_RECORD_LEN, 42);
        assert_eq!(RADIO_SNAPSHOT_MAX_LEN, 540);
    }
}
