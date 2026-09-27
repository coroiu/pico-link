//! `HOST_OP` (`0x06`) / `GET_OP_STATUS` (`0x07`) -- the web companion's EQ
//! management protocol write side (bead `pico-link-jyhk.19`, "ADA DESIGN"
//! comment on `pico-link-jyhk.17`, sections 4/6/7). One mailbox, one op per
//! transfer, one status: every mutation, preview and parse the web page can
//! ask for goes through [`App::host_op`]; the host polls the result back
//! with [`App::host_op_status`] (`GET_OP_STATUS`).
//!
//! # Ops
//!
//! `op_proto` `1`. Request header 4 bytes: `op_proto` (`u8`), `op` (`u8`),
//! `seq` (`u8`, host-picked, C never parses it -- design section 4), `flags`
//! (`u8`). Body per op (design section 4's table):
//!
//! | op | Name | Body |
//! |---|---|---|
//! | 1 | `SAVE_EFFECT` | `id` (`u16`, `0` = create), `base_seq` (`u16`), `blob[80]` |
//! | 2 | `DELETE_EFFECT` | `id` (`u16`), `base_seq` (`u16`) |
//! | 3 | `ASSIGN` | `addr[6]`, `effect_id` (`u16`, `0` = Off) |
//! | 4 | `PREVIEW` | `effect_id` (`u16`, `0` = unsaved draft), `blob[80]`; `flags` bit0 = bypass |
//! | 5 | `PREVIEW_END` | (none) |
//! | 6 | `PARSE_APO` | `name_len` (`u8`), `name`, APO text (rest of the transfer) |
//!
//! # Semantics kept from the design (section 4)
//!
//! - `SAVE` create: [`App::presets_ready`] gate, `STORE_FULL` at
//!   [`crate::dsp::MAX_PRESETS`], [`crate::dsp::PresetStore::create`]. `SAVE`
//!   update: `NOT_FOUND` unless the id already exists (never
//!   create-with-host-id -- Ada's `pico-link-ryw.14` contract), `CONFLICT`
//!   if `base_seq` doesn't match the live `persisted_seq`, `EDITOR_OPEN` if
//!   the on-device effects editor has that id open
//!   ([`super::App::editor_preview`]). Both: `NAME_TAKEN` (byte-exact,
//!   excluding self), `NAME_INVALID` (empty or not UTF-8, caught by
//!   [`crate::dsp::Preset::from_wire_checked`]).
//! - Host blobs are decoded with [`crate::dsp::Preset::from_wire_checked`]
//!   (never the tolerant [`crate::dsp::Preset::from_wire`]) and range-checked
//!   with [`crate::dsp::validate_preset`] -- reject, never clamp.
//! - Lock rule (decision 1 on `pico-link-jyhk.14`): `core` ignores the
//!   host's `eq_locked` bit and computes
//!   `locked = previous.locked || bands differ || preamp differs`
//!   (`previous` is empty bands / [`crate::dsp::Preamp::Auto`] on create).
//! - `DELETE`: `NOT_FOUND`, `CONFLICT`, `EDITOR_OPEN`; queues
//!   [`super::Command::DeletePreset`] -- does NOT delete locally, same as
//!   the on-device delete-confirm flow (`crate::app::screens::effects`'s
//!   `build_delete_confirm_screen`): the store only actually loses the
//!   entry on the real [`super::Event::PresetDeleted`] echo.
//! - `ASSIGN`: `UNKNOWN_DEVICE` if `addr` isn't paired; `effect_id` `0` or
//!   an existing id only; queues [`super::Command::AssignPreset`] -- does
//!   NOT mutate [`super::BtModel::paired`] locally, same as the device
//!   page's own `EFFECT` picker (`crate::app::screens::device_page`'s
//!   `on_pick`): the check follows the `PairedDeviceUpserted` echo.
//! - `PREVIEW`: validates, sets [`super::App`]'s host-preview mailbox --
//!   no flash write, no [`super::Command`]. `PREVIEW_END` clears it.
//! - `PARSE_APO`: [`crate::dsp::import::parse_and_convert`] only, no store
//!   mutation -- returns the blob, the id of a same-name effect (`0` none)
//!   and the lowest-free `" N"` copy name, so the web page implements
//!   neither the parser nor the suffix rule.
//!
//! `GET_OP_STATUS` reply: `op_proto` (`u8`), `seq` (`u8`), `op` (`u8`),
//! `state` (`u8`: `0` none, `1` done, `2` rejected), `error` (`u8`),
//! reserved (`u8`), `effect_id`/`library_rev`/`persisted_seq`/`line`/`band`
//! (`u16` each), `value` (`f32`), `payload_len` (`u8`), `payload`. `line`/
//! `band`/`value` carry a rejection's offending line number ([`PARSE_APO`
//! parse failures](OpError::ParseError)) or band index/value ([`GAIN`/
//! `FREQ`/`Q`/`PREAMP` range failures](OpError)) -- `0`/`0.0` otherwise.
//! `library_rev` is always the current [`super::App::library_snapshot`]
//! revision (design section 6's "the page shows Saved when that id's
//! `persisted_seq` in the library passes the op status's `persisted_seq`"
//! needs a library rev to key its next re-read off, regardless of which op
//! ran).

use alloc::vec::Vec;

use super::events::Command;
use super::App;
use crate::dsp::preset::{Band, Preamp, Preset, MAX_NAME_BYTES};
use crate::dsp::store::{PresetStore, NO_PRESET_ID};
use crate::dsp::validate::{validate_preset, ValidateError};
use crate::dsp::{import, MAX_PRESETS};
use crate::dsp::preset::{PresetBlobError, BLOB_LEN};

/// This request/reply payload's protocol version -- design section 4's
/// `op_proto 1`. `pub(crate)` for [`super::host_op_fixtures`]'s request
/// builders.
pub(crate) const OP_PROTO: u8 = 1;

const REQ_HEADER_LEN: usize = 4;
const SAVE_BODY_LEN: usize = 2 + 2 + BLOB_LEN;
const DELETE_BODY_LEN: usize = 2 + 2;
const ASSIGN_BODY_LEN: usize = 6 + 2;
const PREVIEW_BODY_LEN: usize = 2 + BLOB_LEN;

/// `flags` bit0 on a `PREVIEW` request -- design section 4: "flags bit0 =
/// bypass". `pub(crate)` for [`super::host_op_fixtures`].
pub(crate) const FLAG_BYPASS: u8 = 1 << 0;

/// Design section 5: "APO text above ~1000 B is rejected with a clear
/// message."
const MAX_APO_TEXT_LEN: usize = 1000;

/// `GET_OP_STATUS`'s fixed 21-byte header (see [`encode_op_status`] --
/// `op_proto`/`seq`/`op`/`state`/`error`/reserved (1 byte each), `effect_id`/
/// `library_rev`/`persisted_seq`/`line`/`band` (`u16` each), `value` (`f32`),
/// `payload_len` (`u8`)).
const OP_STATUS_HEADER_LEN: usize = 21;

/// The largest payload any [`HostOpStatus`] can carry: `PARSE_APO`'s
/// `blob[BLOB_LEN] + collides_with(u16) + copy_name_len(u8) + copy_name[MAX_NAME_BYTES]`
/// (module doc comment's `PARSE_APO` row; see the payload built in
/// [`App::host_op`]'s `op 6` arm).
const MAX_OP_STATUS_PAYLOAD_LEN: usize = BLOB_LEN + 2 + 1 + MAX_NAME_BYTES;

/// The largest a [`super::App::host_op_status`] reply can ever be --
/// [`OP_STATUS_HEADER_LEN`] plus the largest possible payload
/// ([`MAX_OP_STATUS_PAYLOAD_LEN`], from `PARSE_APO`). `pub` (re-exported from
/// [`super`]) so `ui-ffi`'s `pl_ui_host_op` can reject a too-small `out_cap`
/// BEFORE calling [`super::App::host_op`], rather than mutating state it
/// then can't report (bead `pico-link-jyhk.20` review fix).
pub const MAX_OP_STATUS_LEN: usize = OP_STATUS_HEADER_LEN + MAX_OP_STATUS_PAYLOAD_LEN;

#[allow(dead_code)] // Referenced only from tests/doc comments -- a fresh App's HostOpStatus::default() already IS 0 without naming this constant at the runtime call site.
const STATE_NONE: u8 = 0;
const STATE_DONE: u8 = 1;
const STATE_REJECTED: u8 = 2;

/// The six `HOST_OP` mutations -- design section 4's table. `#[repr(u8)]`
/// with explicit discriminants so [`Self::wire`] and [`Self::from_wire`]
/// stay a single source of truth for the wire value -- [`super::host_op_fixtures`]'s
/// `op-errors.json` reads [`Self::wire`] rather than hand-duplicating these
/// numbers, so a renumbering here shows up as a fixture-drift test failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum HostOpCode {
    SaveEffect = 1,
    DeleteEffect = 2,
    Assign = 3,
    Preview = 4,
    PreviewEnd = 5,
    ParseApo = 6,
}

impl HostOpCode {
    fn from_wire(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::SaveEffect),
            2 => Some(Self::DeleteEffect),
            3 => Some(Self::Assign),
            4 => Some(Self::Preview),
            5 => Some(Self::PreviewEnd),
            6 => Some(Self::ParseApo),
            _ => None,
        }
    }

    /// This op's wire byte -- the inverse of [`Self::from_wire`].
    #[cfg(test)]
    pub(crate) const fn wire(self) -> u8 {
        self as u8
    }
}

/// `GET_OP_STATUS`'s `error` byte -- one flat vocabulary across every op, so
/// the web page has one table to render a message from regardless of which
/// op failed. `0` ([`Self::None`]) never appears on a real reply: `state`
/// is `0` (none)/`1` (done) whenever there is no error to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum OpError {
    #[allow(dead_code)] // Never constructed -- `state` is 0/1 (none/done) whenever there's no error; kept as `0` purely so no other error code is ever misread as "no error".
    None = 0,
    /// The request itself was malformed (wrong `op_proto`, a body shorter
    /// than the op requires, invalid UTF-8 where text is expected).
    InvalidRequest = 1,
    /// `op` didn't match any of the six known ops.
    UnknownOp = 2,
    /// Ada's preset-id-allocation contract (`pico-link-ryw.14`): C's
    /// boot-time high-water mark hasn't arrived yet -- see
    /// [`super::App::presets_ready`].
    NotReady = 3,
    /// A `SAVE` create against an already-full store.
    StoreFull = 4,
    /// A `SAVE` update, `DELETE`, or `ASSIGN`'s `effect_id` referenced an id
    /// that doesn't exist.
    NotFound = 5,
    /// A `SAVE`/`DELETE`'s `base_seq` didn't match the live `persisted_seq`
    /// -- someone else (the on-device editor, or a second web tab) changed
    /// this effect first.
    Conflict = 6,
    /// A `SAVE`/`DELETE` targeted an effect the on-device effects editor
    /// currently has open.
    EditorOpen = 7,
    /// A `SAVE`'s name byte-exactly matches a different effect's name.
    NameTaken = 8,
    /// A `SAVE`/`PREVIEW`'s name was empty or not valid UTF-8.
    NameInvalid = 9,
    /// The blob's version byte wasn't the `v2` this build writes.
    BlobVersion = 10,
    /// The blob's `band_count` exceeded [`crate::dsp::MAX_BANDS`].
    BandCount = 11,
    /// A band's `kind` bits were one of the reserved (unused) values.
    ReservedBandKind = 12,
    GainRange = 13,
    FreqRange = 14,
    QRange = 15,
    PreampRange = 16,
    /// An `ASSIGN`'s `addr` isn't a currently-paired device.
    UnknownDevice = 17,
    /// A `PARSE_APO` document failed to parse (see `line` for the 1-based
    /// line number, when the parser could attribute one).
    ParseError = 18,
    /// A `PARSE_APO` document's text exceeded [`MAX_APO_TEXT_LEN`].
    ApoTooLarge = 19,
}

/// Translates a strict-decode failure ([`crate::dsp::Preset::from_wire_checked`])
/// into the flat `error` vocabulary.
fn op_error_from_blob_error(err: PresetBlobError) -> OpError {
    match err {
        PresetBlobError::UnsupportedVersion { .. } => OpError::BlobVersion,
        PresetBlobError::TooManyBands { .. } => OpError::BandCount,
        PresetBlobError::ReservedBandKind { .. } => OpError::ReservedBandKind,
        PresetBlobError::InvalidNameUtf8 => OpError::NameInvalid,
    }
}

/// Translates a [`validate_preset`] failure into the flat `error`
/// vocabulary, alongside the `(band, value)` pair `GET_OP_STATUS` reports
/// for it (design: "`line`/`band`/`value` ... [carry] a rejection's ...
/// band index/value").
fn op_error_from_validate_error(err: ValidateError) -> (OpError, u16, f32) {
    match err {
        ValidateError::TooManyBands { band_count } => (OpError::BandCount, truncate_u16(band_count), 0.0),
        ValidateError::GainOutOfRange { band_index, gain_db } => (OpError::GainRange, truncate_u16(band_index), gain_db),
        ValidateError::FreqOutOfRange { band_index, freq_hz } => (OpError::FreqRange, truncate_u16(band_index), freq_hz),
        ValidateError::QOutOfRange { band_index, q } => (OpError::QRange, truncate_u16(band_index), q),
        ValidateError::PreampOutOfRange { preamp_db } => (OpError::PreampRange, 0, preamp_db),
    }
}

#[allow(clippy::cast_possible_truncation)] // every caller passes a value that already fits u16 (a band index <= MAX_BANDS, or a small count)
fn truncate_u16(v: usize) -> u16 {
    v.min(u16::MAX as usize) as u16
}

/// One [`App::host_op`] result -- the state [`App::host_op_status`] encodes
/// on every `GET_OP_STATUS` poll until the next `HOST_OP` overwrites it.
/// `Default`'s all-zero value (`state` [`STATE_NONE`]) is exactly what a
/// fresh `App` (no `HOST_OP` ever received) reports -- design section 4:
/// "the host polls `GET_OP_STATUS` until `seq` matches its own and `state
/// != 0`", so an untouched `state == 0` can never be mistaken for a real
/// result at `seq == 0`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct HostOpStatus {
    seq: u8,
    op: u8,
    state: u8,
    error: u8,
    effect_id: u16,
    library_rev: u16,
    persisted_seq: u16,
    line: u16,
    band: u16,
    value: f32,
    payload: Vec<u8>,
}

impl HostOpStatus {
    fn rejected(seq: u8, op: u8, error: OpError) -> Self {
        Self { seq, op, state: STATE_REJECTED, error: error as u8, ..Self::default() }
    }

    fn rejected_conflict(seq: u8, op: u8, effect_id: u16, persisted_seq: u16) -> Self {
        Self { seq, op, state: STATE_REJECTED, error: OpError::Conflict as u8, effect_id, persisted_seq, ..Self::default() }
    }

    fn rejected_at(seq: u8, op: u8, error: OpError, line: u16, band: u16, value: f32) -> Self {
        Self { seq, op, state: STATE_REJECTED, error: error as u8, line, band, value, ..Self::default() }
    }

    fn done(seq: u8, op: u8) -> Self {
        Self { seq, op, state: STATE_DONE, ..Self::default() }
    }

    fn done_effect(seq: u8, op: u8, effect_id: u16, persisted_seq: u16) -> Self {
        Self { seq, op, state: STATE_DONE, effect_id, persisted_seq, ..Self::default() }
    }

    fn done_payload(seq: u8, op: u8, payload: Vec<u8>) -> Self {
        Self { seq, op, state: STATE_DONE, payload, ..Self::default() }
    }
}

/// Encodes a [`HostOpStatus`] into the `GET_OP_STATUS` wire layout -- see
/// this module's doc comment for the field order.
fn encode_op_status(status: &HostOpStatus) -> Vec<u8> {
    let mut out = Vec::with_capacity(OP_STATUS_HEADER_LEN + status.payload.len());
    out.push(OP_PROTO);
    out.push(status.seq);
    out.push(status.op);
    out.push(status.state);
    out.push(status.error);
    out.push(0); // reserved
    out.extend_from_slice(&status.effect_id.to_le_bytes());
    out.extend_from_slice(&status.library_rev.to_le_bytes());
    out.extend_from_slice(&status.persisted_seq.to_le_bytes());
    out.extend_from_slice(&status.line.to_le_bytes());
    out.extend_from_slice(&status.band.to_le_bytes());
    out.extend_from_slice(&status.value.to_le_bytes());
    #[allow(clippy::cast_possible_truncation)] // payload is built by this module and never exceeds ~99 bytes (PARSE_APO's blob+collides_with+copy_name)
    let payload_len = status.payload.len() as u8;
    out.push(payload_len);
    out.extend_from_slice(&status.payload);
    out
}

/// Finds another preset (not `excluding`) whose name byte-exactly matches
/// `name` -- the `NAME_TAKEN` check, which (unlike
/// [`import::find_by_name`]) must not treat a `SAVE` update's own current
/// name as a collision with itself.
fn find_other_by_name(presets: &PresetStore, name: &str, excluding: Option<u16>) -> Option<u16> {
    presets.iter().find(|(id, p)| p.name == name && Some(*id) != excluding).map(|(id, _)| id)
}

/// Decision 1 on `pico-link-jyhk.14`: `locked = previous.locked || bands
/// differ || preamp differs`, ignoring whatever `eq_locked` bit the host
/// blob carried. `previous` is `None` on a `SAVE` create, which this reads
/// as "empty bands, [`Preamp::Auto`]" (design section 4) -- so a create
/// that already carries real bands or an explicit preamp locks immediately,
/// same as an import.
fn apply_lock_rule(previous: Option<&Preset>, candidate: &mut Preset) {
    let prev_locked = previous.is_some_and(|p| p.eq_locked);
    let prev_bands: &[Band] = previous.map_or(&[], |p| p.bands.as_slice());
    let prev_preamp = previous.map_or(Preamp::Auto, |p| p.preamp);
    let bands_differ = prev_bands != candidate.bands.as_slice();
    let preamp_differs = prev_preamp != candidate.preamp;
    candidate.eq_locked = prev_locked || bands_differ || preamp_differs;
}

impl App {
    /// Whether the on-device effects editor currently has `id` open --
    /// design section 6's `EDITOR_OPEN` check, reading the same
    /// [`Self::editor_preview`] mailbox `pico-link-jyhk.18`'s telemetry
    /// append reads.
    fn host_op_editor_open(&self, id: u16) -> bool {
        self.editor_preview.borrow().as_ref().is_some_and(|(open_id, _, _)| *open_id == id)
    }

    /// Decodes and executes one `HOST_OP` (`0x06`) request, storing the
    /// result for the next [`Self::host_op_status`] (`GET_OP_STATUS`,
    /// `0x07`) poll to read -- `ui-ffi`'s `pl_ui_host_op`'s core-side
    /// implementation (design section 11, Task 3). Never panics on a
    /// malformed/truncated `request`: every length/bounds check runs before
    /// any field read, same discipline every other wire decode in this
    /// crate uses.
    pub fn host_op(&mut self, request: &[u8]) {
        let status = self.decode_and_run_host_op(request);
        let mut status = status;
        status.library_rev = self.current_library_rev();
        self.host_op_status = status;
    }

    fn decode_and_run_host_op(&mut self, request: &[u8]) -> HostOpStatus {
        if request.len() < REQ_HEADER_LEN {
            // Nothing to echo back yet -- `op`/`seq` themselves are
            // unreadable garbage at this point, so this is the one
            // rejection that can't carry a meaningful `op`/`seq` at all.
            return HostOpStatus::rejected(0, 0, OpError::InvalidRequest);
        }
        let op_proto = request[0];
        let op_byte = request[1];
        let seq = request[2];
        let flags = request[3];
        let body = &request[REQ_HEADER_LEN..];

        if op_proto != OP_PROTO {
            return HostOpStatus::rejected(seq, op_byte, OpError::InvalidRequest);
        }
        let Some(op) = HostOpCode::from_wire(op_byte) else {
            return HostOpStatus::rejected(seq, op_byte, OpError::UnknownOp);
        };

        match op {
            HostOpCode::SaveEffect => self.host_op_save(seq, op_byte, body),
            HostOpCode::DeleteEffect => self.host_op_delete(seq, op_byte, body),
            HostOpCode::Assign => self.host_op_assign(seq, op_byte, body),
            HostOpCode::Preview => self.host_op_preview(seq, op_byte, flags, body),
            HostOpCode::PreviewEnd => self.host_op_preview_end_request(seq, op_byte),
            HostOpCode::ParseApo => self.host_op_parse_apo(seq, op_byte, body),
        }
    }

    fn host_op_save(&mut self, seq: u8, op: u8, body: &[u8]) -> HostOpStatus {
        if body.len() < SAVE_BODY_LEN {
            return HostOpStatus::rejected(seq, op, OpError::InvalidRequest);
        }
        if !*self.presets_ready.borrow() {
            return HostOpStatus::rejected(seq, op, OpError::NotReady);
        }

        let id = u16::from_le_bytes([body[0], body[1]]);
        let base_seq = u16::from_le_bytes([body[2], body[3]]);
        let blob = &body[4..4 + BLOB_LEN];

        let mut preset = match Preset::from_wire_checked(blob) {
            Ok(preset) => preset,
            Err(err) => return HostOpStatus::rejected(seq, op, op_error_from_blob_error(err)),
        };
        if preset.name.is_empty() {
            return HostOpStatus::rejected(seq, op, OpError::NameInvalid);
        }
        if let Err(err) = validate_preset(&preset) {
            let (error, band, value) = op_error_from_validate_error(err);
            return HostOpStatus::rejected_at(seq, op, error, 0, band, value);
        }

        let is_create = id == NO_PRESET_ID;
        let previous = if is_create { None } else { self.presets.borrow().get(id).cloned() };

        if is_create {
            if self.presets.borrow().len() >= MAX_PRESETS {
                return HostOpStatus::rejected(seq, op, OpError::StoreFull);
            }
        } else {
            let Some(_) = previous.as_ref() else {
                return HostOpStatus::rejected(seq, op, OpError::NotFound);
            };
            let current_seq = self.preset_persisted_seq.get(&id).copied().unwrap_or(0);
            if current_seq != base_seq {
                return HostOpStatus::rejected_conflict(seq, op, id, current_seq);
            }
            if self.host_op_editor_open(id) {
                return HostOpStatus::rejected(seq, op, OpError::EditorOpen);
            }
        }

        let excluding = if is_create { None } else { Some(id) };
        if find_other_by_name(&self.presets.borrow(), &preset.name, excluding).is_some() {
            return HostOpStatus::rejected(seq, op, OpError::NameTaken);
        }

        apply_lock_rule(previous.as_ref(), &mut preset);

        let effect_id = if is_create {
            self.presets.borrow_mut().create(preset.clone())
        } else {
            self.presets.borrow_mut().update(id, preset.clone());
            id
        };

        self.commands.borrow_mut().push_back(Command::SavePreset { preset_id: effect_id, blob: preset.to_wire().to_vec() });
        self.mark_model_changed();

        let persisted_seq = self.preset_persisted_seq.get(&effect_id).copied().unwrap_or(0);
        HostOpStatus::done_effect(seq, op, effect_id, persisted_seq)
    }

    fn host_op_delete(&mut self, seq: u8, op: u8, body: &[u8]) -> HostOpStatus {
        if body.len() < DELETE_BODY_LEN {
            return HostOpStatus::rejected(seq, op, OpError::InvalidRequest);
        }
        let id = u16::from_le_bytes([body[0], body[1]]);
        let base_seq = u16::from_le_bytes([body[2], body[3]]);

        if self.presets.borrow().get(id).is_none() {
            return HostOpStatus::rejected(seq, op, OpError::NotFound);
        }
        let current_seq = self.preset_persisted_seq.get(&id).copied().unwrap_or(0);
        if current_seq != base_seq {
            return HostOpStatus::rejected_conflict(seq, op, id, current_seq);
        }
        if self.host_op_editor_open(id) {
            return HostOpStatus::rejected(seq, op, OpError::EditorOpen);
        }

        // Deliberately does NOT delete from `self.presets` here -- see this
        // module's doc comment: the store only loses the entry on the real
        // `PresetDeleted` echo, same as the on-device delete-confirm flow.
        self.commands.borrow_mut().push_back(Command::DeletePreset { preset_id: id });
        HostOpStatus::done_effect(seq, op, id, current_seq)
    }

    fn host_op_assign(&mut self, seq: u8, op: u8, body: &[u8]) -> HostOpStatus {
        if body.len() < ASSIGN_BODY_LEN {
            return HostOpStatus::rejected(seq, op, OpError::InvalidRequest);
        }
        let mut addr = [0u8; 6];
        addr.copy_from_slice(&body[0..6]);
        let effect_id = u16::from_le_bytes([body[6], body[7]]);

        let device_known = self.model.borrow().paired.iter().any(|d| d.addr == addr);
        if !device_known {
            return HostOpStatus::rejected(seq, op, OpError::UnknownDevice);
        }
        if effect_id != NO_PRESET_ID && self.presets.borrow().get(effect_id).is_none() {
            return HostOpStatus::rejected(seq, op, OpError::NotFound);
        }

        // Deliberately does NOT mutate `BtModel::paired` here -- see this
        // module's doc comment: the check follows the `PairedDeviceUpserted`
        // echo, same as the device page's own `EFFECT` picker.
        self.commands.borrow_mut().push_back(Command::AssignPreset { addr, preset_id: effect_id });
        HostOpStatus::done_effect(seq, op, effect_id, 0)
    }

    fn host_op_preview(&mut self, seq: u8, op: u8, flags: u8, body: &[u8]) -> HostOpStatus {
        if body.len() < PREVIEW_BODY_LEN {
            return HostOpStatus::rejected(seq, op, OpError::InvalidRequest);
        }
        let effect_id = u16::from_le_bytes([body[0], body[1]]);
        let blob = &body[2..2 + BLOB_LEN];

        let preset = match Preset::from_wire_checked(blob) {
            Ok(preset) => preset,
            Err(err) => return HostOpStatus::rejected(seq, op, op_error_from_blob_error(err)),
        };
        if let Err(err) = validate_preset(&preset) {
            let (error, band, value) = op_error_from_validate_error(err);
            return HostOpStatus::rejected_at(seq, op, error, 0, band, value);
        }

        let bypass = flags & FLAG_BYPASS != 0;
        self.host_preview = Some((effect_id, preset, bypass));
        HostOpStatus::done_effect(seq, op, effect_id, 0)
    }

    fn host_op_preview_end_request(&mut self, seq: u8, op: u8) -> HostOpStatus {
        self.host_preview = None;
        HostOpStatus::done(seq, op)
    }

    fn host_op_parse_apo(&mut self, seq: u8, op: u8, body: &[u8]) -> HostOpStatus {
        let Some(&name_len) = body.first() else {
            return HostOpStatus::rejected(seq, op, OpError::InvalidRequest);
        };
        let name_len = usize::from(name_len);
        if body.len() < 1 + name_len {
            return HostOpStatus::rejected(seq, op, OpError::InvalidRequest);
        }
        let Ok(host_name) = core::str::from_utf8(&body[1..=name_len]) else {
            return HostOpStatus::rejected(seq, op, OpError::InvalidRequest);
        };

        let text_bytes = &body[1 + name_len..];
        if text_bytes.len() > MAX_APO_TEXT_LEN {
            return HostOpStatus::rejected(seq, op, OpError::ApoTooLarge);
        }
        let Ok(text) = core::str::from_utf8(text_bytes) else {
            return HostOpStatus::rejected(seq, op, OpError::InvalidRequest);
        };

        match import::parse_and_convert(text, host_name) {
            Ok(preset) => {
                let presets = self.presets.borrow();
                let collides_with = import::find_by_name(&presets, &preset.name).unwrap_or(NO_PRESET_ID);
                let copy_name = import::lowest_free_suffixed_name(&presets, &preset.name);
                drop(presets);

                let mut payload = Vec::with_capacity(BLOB_LEN + 2 + 1 + MAX_NAME_BYTES);
                payload.extend_from_slice(&preset.to_wire());
                payload.extend_from_slice(&collides_with.to_le_bytes());
                let copy_bytes = copy_name.as_bytes();
                #[allow(clippy::cast_possible_truncation)] // `copy_name` is already truncated to MAX_NAME_BYTES by `lowest_free_suffixed_name`
                let copy_len = copy_bytes.len().min(MAX_NAME_BYTES) as u8;
                payload.push(copy_len);
                let mut name_buf = [0u8; MAX_NAME_BYTES];
                name_buf[..usize::from(copy_len)].copy_from_slice(&copy_bytes[..usize::from(copy_len)]);
                payload.extend_from_slice(&name_buf);

                HostOpStatus::done_payload(seq, op, payload)
            }
            Err(err) => host_op_status_from_import_error(seq, op, err),
        }
    }

    /// Encodes the current [`HostOpStatus`] for the web companion's
    /// `GET_OP_STATUS` poll (`0x07`) into `buf`. Returns the number of bytes
    /// written, or `0` (never a partial write) if `buf` is too small -- the
    /// same "`0` means don't publish this poll" contract
    /// [`Self::telemetry_snapshot`]/[`Self::library_snapshot`] use. Always
    /// reflects the LAST [`Self::host_op`] call's result (or the all-zero
    /// `state == 0` default if none has ever run) -- unlike
    /// [`Self::poll_command`], repeated calls don't drain anything, matching
    /// design section 4's "the host polls `GET_OP_STATUS` until `seq`
    /// matches its own and `state != 0`".
    #[must_use]
    pub fn host_op_status(&self, buf: &mut [u8]) -> usize {
        let bytes = encode_op_status(&self.host_op_status);
        if bytes.len() > buf.len() {
            return 0;
        }
        buf[..bytes.len()].copy_from_slice(&bytes);
        bytes.len()
    }

    /// Clears any active host preview -- `ui-ffi`'s `pl_ui_host_preview_end`,
    /// called by C after ~2 s with no `iface-6` traffic (design section 7's
    /// lease: "C stamps `s_last_host_setup_us` on EVERY `iface-6` SETUP ...
    /// if a host preview may be active and nothing arrived for 2 s, call
    /// `pl_ui_host_preview_end`"). Distinct from the `PREVIEW_END` op
    /// ([`Self::host_op`] with `op` `5`): this is the timeout path, so it
    /// has no request `seq` to answer and does not touch
    /// [`Self::host_op_status`].
    pub fn host_preview_end(&mut self) {
        self.host_preview = None;
    }
}

/// Translates an [`import::ImportError`] (from
/// [`import::parse_and_convert`]) into a `PARSE_APO` rejection.
/// [`import::ImportError::StoreFull`]/[`import::ImportError::NotReady`]
/// never reach here -- `parse_and_convert` has no store to be full and no
/// id-allocation gate to check (see its own doc comment) -- so both are
/// mapped defensively to [`OpError::InvalidRequest`] rather than treated as
/// unreachable, in case a future change to `parse_and_convert` ever starts
/// producing one.
fn host_op_status_from_import_error(seq: u8, op: u8, err: import::ImportError) -> HostOpStatus {
    use import::ImportError;
    match err {
        ImportError::Line(line_err) => HostOpStatus::rejected_at(seq, op, OpError::ParseError, truncate_u16(line_err.line), 0, 0.0),
        ImportError::Session(_) => HostOpStatus::rejected(seq, op, OpError::ParseError),
        ImportError::GainOutOfRange { band_index, gain_db } => HostOpStatus::rejected_at(seq, op, OpError::GainRange, 0, truncate_u16(band_index), gain_db),
        ImportError::FreqOutOfRange { band_index, freq_hz } => HostOpStatus::rejected_at(seq, op, OpError::FreqRange, 0, truncate_u16(band_index), freq_hz),
        ImportError::QOutOfRange { band_index, q } => HostOpStatus::rejected_at(seq, op, OpError::QRange, 0, truncate_u16(band_index), q),
        ImportError::PreampOutOfRange { preamp_db } => HostOpStatus::rejected_at(seq, op, OpError::PreampRange, 0, 0, preamp_db),
        ImportError::StoreFull | ImportError::NotReady => HostOpStatus::rejected(seq, op, OpError::InvalidRequest),
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec;

    use super::*;
    use crate::app::model::PairedDevice;
    use crate::app::test_support::ready_presets;
    use crate::app::{App, Event};
    use crate::dsp::preset::{BandKind, CrossfeedLevel};

    // --- Request/status wire shape ------------------------------------

    fn save_request(seq: u8, id: u16, base_seq: u16, blob: &[u8; BLOB_LEN]) -> Vec<u8> {
        let mut req = vec![OP_PROTO, 1, seq, 0];
        req.extend_from_slice(&id.to_le_bytes());
        req.extend_from_slice(&base_seq.to_le_bytes());
        req.extend_from_slice(blob);
        req
    }

    /// `(op_proto, seq, op, state, error, effect_id, library_rev,
    /// persisted_seq, line, band, value, payload)`.
    type DecodedStatus = (u8, u8, u8, u8, u8, u16, u16, u16, u16, u16, f32, Vec<u8>);

    fn decode_status(buf: &[u8]) -> DecodedStatus {
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

    fn new_preset_blob(name: &str) -> [u8; BLOB_LEN] {
        Preset::new(name).to_wire()
    }

    #[test]
    fn a_fresh_app_reports_state_none_before_any_host_op() {
        let app = App::new(240, 240);
        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (op_proto, _, _, state, ..) = decode_status(&buf[..n]);
        assert_eq!(op_proto, OP_PROTO);
        assert_eq!(state, STATE_NONE);
    }

    #[test]
    fn save_create_allocates_an_id_and_queues_save_preset() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        let blob = new_preset_blob("Warm");

        app.host_op(&save_request(7, 0, 0, &blob));

        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, seq, op, state, error, effect_id, _library_rev, persisted_seq, ..) = decode_status(&buf[..n]);
        assert_eq!(seq, 7);
        assert_eq!(op, 1);
        assert_eq!(state, STATE_DONE);
        assert_eq!(error, OpError::None as u8);
        assert_ne!(effect_id, 0, "create must allocate a real id");
        assert_eq!(persisted_seq, 0, "no PresetLoaded echo has folded yet");

        let saved_blob = app.presets_for_test().get(effect_id).map(|p| p.to_wire().to_vec());
        assert_eq!(app.poll_command(), saved_blob.map(|blob| Command::SavePreset { preset_id: effect_id, blob }));
    }

    #[test]
    fn save_create_before_presets_ready_is_rejected_not_ready() {
        let mut app = App::new(240, 240);
        let blob = new_preset_blob("Too Soon");
        app.host_op(&save_request(1, 0, 0, &blob));

        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, error, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_REJECTED);
        assert_eq!(error, OpError::NotReady as u8);
        assert!(app.poll_command().is_none());
    }

    #[test]
    fn save_create_on_a_full_store_is_rejected_store_full() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        for i in 0..MAX_PRESETS {
            let blob = new_preset_blob(&alloc::format!("Hand {i}"));
            app.host_op(&save_request(u8::try_from(i).unwrap(), 0, 0, &blob));
            app.poll_command();
        }
        let blob = new_preset_blob("One More");
        app.host_op(&save_request(99, 0, 0, &blob));

        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, error, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_REJECTED);
        assert_eq!(error, OpError::StoreFull as u8);
    }

    #[test]
    fn save_update_of_an_unknown_id_is_rejected_not_found() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        let blob = new_preset_blob("Ghost");
        app.host_op(&save_request(1, 12345, 0, &blob));

        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, error, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_REJECTED);
        assert_eq!(error, OpError::NotFound as u8);
    }

    #[test]
    fn save_update_with_a_stale_base_seq_is_rejected_conflict_with_the_current_seq() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        let blob = new_preset_blob("Warm");
        app.host_op(&save_request(1, 0, 0, &blob));
        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (.., effect_id, _, _persisted_seq, _, _, _, _) = decode_status(&buf[..n]);
        app.poll_command();

        // Simulate the flash echo bumping persisted_seq to 1.
        app.handle_event(Event::PresetLoaded { id: effect_id, blob: blob.to_vec() });

        let blob2 = new_preset_blob("Warmer");
        app.host_op(&save_request(2, effect_id, 0, &blob2)); // base_seq 0 is now stale

        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, error, _, _, persisted_seq, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_REJECTED);
        assert_eq!(error, OpError::Conflict as u8);
        assert_eq!(persisted_seq, 1, "the reply must carry the CURRENT persisted_seq so the page can resync");
    }

    #[test]
    fn save_update_of_an_id_the_device_editor_has_open_is_rejected_editor_open() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        let blob = new_preset_blob("Warm");
        app.host_op(&save_request(1, 0, 0, &blob));
        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (.., effect_id, _, _, _, _, _, _) = decode_status(&buf[..n]);
        app.poll_command();

        app.open_editor_for_test(effect_id);

        let blob2 = new_preset_blob("Warmer");
        app.host_op(&save_request(2, effect_id, 0, &blob2));
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, error, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_REJECTED);
        assert_eq!(error, OpError::EditorOpen as u8);
    }

    #[test]
    fn save_with_a_name_that_collides_with_a_different_effect_is_rejected_name_taken() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        app.host_op(&save_request(1, 0, 0, &new_preset_blob("Warm")));
        app.poll_command();

        app.host_op(&save_request(2, 0, 0, &new_preset_blob("Warm")));
        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, error, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_REJECTED);
        assert_eq!(error, OpError::NameTaken as u8);
    }

    #[test]
    fn save_update_may_keep_its_own_unchanged_name() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        app.host_op(&save_request(1, 0, 0, &new_preset_blob("Warm")));
        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (.., effect_id, _, _, _, _, _, _) = decode_status(&buf[..n]);
        app.poll_command();

        app.host_op(&save_request(2, effect_id, 0, &new_preset_blob("Warm")));
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_DONE, "renaming to the same name it already has must not be NAME_TAKEN");
    }

    #[test]
    fn save_with_an_empty_name_is_rejected_name_invalid() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        app.host_op(&save_request(1, 0, 0, &new_preset_blob("")));
        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, error, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_REJECTED);
        assert_eq!(error, OpError::NameInvalid as u8);
    }

    #[test]
    fn save_rejects_an_unsupported_blob_version() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        let mut blob = new_preset_blob("Warm");
        blob[0] = 1; // v1 -- host input must never be accepted as v1
        app.host_op(&save_request(1, 0, 0, &blob));
        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, error, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_REJECTED);
        assert_eq!(error, OpError::BlobVersion as u8);
    }

    #[test]
    fn save_rejects_an_out_of_range_band_with_its_index_and_value() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        let mut preset = Preset::new("Loud");
        preset.push_band(Band { kind: BandKind::Peak, freq_half_hz: 2000, gain_cdb: 3500, q_milli: 1000 }); // 35 dB, over the 30 dB limit
        app.host_op(&save_request(1, 0, 0, &preset.to_wire()));

        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, error, _, _, _, _, band, value, _) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_REJECTED);
        assert_eq!(error, OpError::GainRange as u8);
        assert_eq!(band, 1);
        assert!((value - 35.0).abs() < 0.01);
    }

    #[test]
    fn save_never_mutates_the_store_or_queues_a_command_on_any_rejection() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        app.host_op(&save_request(1, 0, 0, &new_preset_blob("")));
        assert!(app.poll_command().is_none());
    }

    #[test]
    fn a_crossfeed_only_edit_does_not_lock_a_hand_made_effect() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        // A hand-made, unlocked, empty-bands effect (the on-device New
        // effect default) already exists.
        let hand_made = Preset::new("Hand");
        let id = app.presets_create_for_test(hand_made);

        let mut edited = Preset::new("Hand");
        edited.crossfeed = CrossfeedLevel::Medium; // no bands, no explicit preamp -- unchanged from "empty/Auto"
        app.host_op(&save_request(1, id, 0, &edited.to_wire()));

        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_DONE);
        assert!(!app.presets_for_test().get(id).unwrap().eq_locked, "a crossfeed-only change must not lock a hand-made effect");
    }

    #[test]
    fn a_create_with_real_bands_locks_immediately() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        let mut preset = Preset::new("New");
        preset.push_band(Band { kind: BandKind::Peak, freq_half_hz: 2000, gain_cdb: 300, q_milli: 1000 });
        app.host_op(&save_request(1, 0, 0, &preset.to_wire()));

        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (.., effect_id, _, _, _, _, _, _) = decode_status(&buf[..n]);
        assert!(app.presets_for_test().get(effect_id).unwrap().eq_locked, "bands differing from the empty/Auto default lock on create");
    }

    // --- DELETE ---------------------------------------------------------

    fn delete_request(seq: u8, id: u16, base_seq: u16) -> Vec<u8> {
        let mut req = vec![OP_PROTO, 2, seq, 0];
        req.extend_from_slice(&id.to_le_bytes());
        req.extend_from_slice(&base_seq.to_le_bytes());
        req
    }

    #[test]
    fn delete_queues_the_command_but_never_mutates_the_store_locally() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        let id = app.presets_create_for_test(Preset::new("Doomed"));

        app.host_op(&delete_request(1, id, 0));

        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_DONE);
        assert!(app.presets_for_test().get(id).is_some(), "delete must wait for the PresetDeleted echo, same as the on-device confirm flow");
        assert_eq!(app.poll_command(), Some(Command::DeletePreset { preset_id: id }));
    }

    #[test]
    fn delete_of_an_unknown_id_is_rejected_not_found() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        app.host_op(&delete_request(1, 999, 0));
        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, error, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_REJECTED);
        assert_eq!(error, OpError::NotFound as u8);
    }

    #[test]
    fn delete_of_an_id_the_editor_has_open_is_rejected_editor_open() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        let id = app.presets_create_for_test(Preset::new("Open"));
        app.open_editor_for_test(id);
        app.host_op(&delete_request(1, id, 0));
        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, error, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_REJECTED);
        assert_eq!(error, OpError::EditorOpen as u8);
    }

    // --- ASSIGN -----------------------------------------------------

    fn assign_request(seq: u8, addr: [u8; 6], effect_id: u16) -> Vec<u8> {
        let mut req = vec![OP_PROTO, 3, seq, 0];
        req.extend_from_slice(&addr);
        req.extend_from_slice(&effect_id.to_le_bytes());
        req
    }

    #[test]
    fn assign_to_an_unpaired_device_is_rejected_unknown_device() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        app.host_op(&assign_request(1, [1, 2, 3, 4, 5, 6], 0));
        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, error, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_REJECTED);
        assert_eq!(error, OpError::UnknownDevice as u8);
    }

    #[test]
    fn assign_off_and_to_an_existing_effect_both_succeed_without_mutating_the_model() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        let addr = [9, 8, 7, 6, 5, 4];
        app.handle_event(Event::PairedDeviceUpserted(PairedDevice { addr, name: String::from("Buds"), mru_seq: 1, ldac_quality: 0, preset_id: 0 }));
        let id = app.presets_create_for_test(Preset::new("Warm"));

        app.host_op(&assign_request(1, addr, id));
        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_DONE);
        assert_eq!(app.poll_command(), Some(Command::AssignPreset { addr, preset_id: id }));
        assert_eq!(app.model().paired[0].preset_id, 0, "assign must not mutate the model locally -- it waits for the echo");

        app.host_op(&assign_request(2, addr, 0));
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_DONE, "assigning Off (id 0) must always be legal");
    }

    #[test]
    fn assign_to_a_nonexistent_effect_is_rejected_not_found() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        let addr = [9, 8, 7, 6, 5, 4];
        app.handle_event(Event::PairedDeviceUpserted(PairedDevice { addr, name: String::from("Buds"), mru_seq: 1, ldac_quality: 0, preset_id: 0 }));
        app.host_op(&assign_request(1, addr, 999));
        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, error, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_REJECTED);
        assert_eq!(error, OpError::NotFound as u8);
    }

    // --- PREVIEW / PREVIEW_END --------------------------------------

    fn preview_request(seq: u8, effect_id: u16, blob: &[u8; BLOB_LEN], bypass: bool) -> Vec<u8> {
        let flags = if bypass { FLAG_BYPASS } else { 0 };
        let mut req = vec![OP_PROTO, 4, seq, flags];
        req.extend_from_slice(&effect_id.to_le_bytes());
        req.extend_from_slice(blob);
        req
    }

    fn preview_end_request(seq: u8) -> Vec<u8> {
        vec![OP_PROTO, 5, seq, 0]
    }

    #[test]
    fn preview_wins_over_the_connected_devices_assignment_and_never_touches_flash() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        let mut preset = Preset::new("Preview Me");
        preset.push_band(Band { kind: BandKind::Peak, freq_half_hz: 2000, gain_cdb: 300, q_milli: 1000 });

        app.host_op(&preview_request(1, 0, &preset.to_wire(), false));
        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_DONE);
        assert!(app.poll_command().is_none(), "PREVIEW must never queue a command");

        let program = app.dsp_program(48_000);
        assert_eq!(program.biquads.len(), 1, "the preview's one band must be resolved, not Off");
    }

    #[test]
    fn preview_bypass_mutes_the_program_regardless_of_the_draft() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        let mut preset = Preset::new("Preview Me");
        preset.push_band(Band { kind: BandKind::Peak, freq_half_hz: 2000, gain_cdb: 300, q_milli: 1000 });
        app.host_op(&preview_request(1, 0, &preset.to_wire(), true));
        let program = app.dsp_program(48_000);
        assert!(program.biquads.is_empty(), "bypass previews Off");
    }

    #[test]
    fn preview_end_op_and_the_lease_timeout_both_clear_the_preview() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        app.host_op(&preview_request(1, 0, &new_preset_blob("X"), false));
        app.host_op(&preview_end_request(2));
        assert_eq!(app.dsp_program(48_000).biquads.len(), 0);

        app.host_op(&preview_request(3, 0, &new_preset_blob("X"), false));
        app.host_preview_end(); // the 2s lease timeout, called directly by C -- not an op
        assert_eq!(app.dsp_program(48_000).biquads.len(), 0);
    }

    #[test]
    fn preview_rejects_an_out_of_range_blob_and_leaves_any_prior_preview_untouched() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        app.host_op(&preview_request(1, 0, &new_preset_blob("Good"), false));

        let mut bad = Preset::new("Bad");
        bad.push_band(Band { kind: BandKind::Peak, freq_half_hz: 2000, gain_cdb: 3500, q_milli: 1000 });
        app.host_op(&preview_request(2, 0, &bad.to_wire(), false));

        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, error, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_REJECTED);
        assert_eq!(error, OpError::GainRange as u8);
    }

    // --- PARSE_APO ----------------------------------------------------

    fn parse_apo_request(seq: u8, host_name: &str, text: &str) -> Vec<u8> {
        let mut req = vec![OP_PROTO, 6, seq, 0];
        #[allow(clippy::cast_possible_truncation)]
        req.push(host_name.len() as u8);
        req.extend_from_slice(host_name.as_bytes());
        req.extend_from_slice(text.as_bytes());
        req
    }

    const APO_TEXT: &str = "Preamp: -1 dB\nFilter 1: ON PK Fc 1000 Hz Gain 2 dB Q 1";

    #[test]
    fn parse_apo_never_touches_the_store() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        app.host_op(&parse_apo_request(1, "Fallback", APO_TEXT));

        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, error, .., payload) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_DONE);
        assert_eq!(error, OpError::None as u8);
        assert!(app.poll_command().is_none(), "PARSE_APO must never queue a command");
        assert_eq!(app.presets_for_test().len(), 0, "PARSE_APO must never touch the store");

        // payload: blob[80], u16 collides_with, u8 copy_name_len, copy_name[16]
        assert_eq!(payload.len(), BLOB_LEN + 2 + 1 + MAX_NAME_BYTES);
        let collides_with = u16::from_le_bytes([payload[BLOB_LEN], payload[BLOB_LEN + 1]]);
        assert_eq!(collides_with, 0, "nothing in the (empty) store shares this name");
        let copy_name_len = usize::from(payload[BLOB_LEN + 2]);
        let copy_name = core::str::from_utf8(&payload[BLOB_LEN + 3..BLOB_LEN + 3 + copy_name_len]).unwrap();
        assert_eq!(copy_name, "Fallback 2", "core's own lowest-free-suffix rule -- the caller (a real SAVE, if the page chooses Copy) decides whether a suffix was actually needed by checking collides_with itself");
    }

    #[test]
    fn parse_apo_reports_a_malformed_lines_1_based_line_number() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        app.host_op(&parse_apo_request(1, "Fallback", "Preamp: not-a-number"));
        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, error, _, _, _, line, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_REJECTED);
        assert_eq!(error, OpError::ParseError as u8);
        assert_eq!(line, 1);
    }

    #[test]
    fn parse_apo_rejects_text_over_the_length_limit() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        let huge = "x".repeat(MAX_APO_TEXT_LEN + 1);
        app.host_op(&parse_apo_request(1, "F", &huge));
        let mut buf = [0u8; 2000];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, error, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_REJECTED);
        assert_eq!(error, OpError::ApoTooLarge as u8);
    }

    // --- General decode robustness -----------------------------------

    #[test]
    fn a_request_shorter_than_the_header_is_rejected_without_panicking() {
        let mut app = App::new(240, 240);
        app.host_op(&[]);
        app.host_op(&[OP_PROTO]);
        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, _, _, state, error, ..) = decode_status(&buf[..n]);
        assert_eq!(state, STATE_REJECTED);
        assert_eq!(error, OpError::InvalidRequest as u8);
    }

    #[test]
    fn an_unknown_op_byte_is_rejected_unknown_op() {
        let mut app = App::new(240, 240);
        app.host_op(&[OP_PROTO, 200, 5, 0]);
        let mut buf = [0u8; 200];
        let n = app.host_op_status(&mut buf);
        let (_, seq, op, state, error, ..) = decode_status(&buf[..n]);
        assert_eq!(seq, 5);
        assert_eq!(op, 200);
        assert_eq!(state, STATE_REJECTED);
        assert_eq!(error, OpError::UnknownOp as u8);
    }

    #[test]
    fn every_op_body_length_is_probed_without_panicking() {
        let mut app = App::new(240, 240);
        ready_presets(&mut app);
        for op in 1u8..=6 {
            for len in 0..90usize {
                let req = vec![OP_PROTO, op, 1, 0]
                    .into_iter()
                    .chain(core::iter::repeat(0u8).take(len))
                    .collect::<Vec<u8>>();
                app.host_op(&req);
            }
        }
    }
}
