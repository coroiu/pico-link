//! `GET_LIBRARY` (`0x05`) snapshot: one atomic, consistent view of every
//! stored DSP effect plus every paired device's assignment, for the web
//! companion's `iface 6` control request (bead `pico-link-jyhk.18`, "ADA
//! DESIGN" comment on `pico-link-jyhk.17`, section 3 -- that comment is the
//! spec this module implements; this doc comment plus
//! [`golden_bytes_layout_is_stable`] (below) is the durable, checked-in
//! copy of it).
//!
//! [`encode_library_snapshot`] is live: [`super::App::library_snapshot`]'s
//! call site. [`decode_library_snapshot`]/[`LibrarySnapshot`] have no call
//! site yet -- same "host-side decode helper" status
//! [`super::telemetry::decode_home_snapshot`] has, and exist for the same
//! reason (a future web decoder should decode with the same crate that
//! encoded it, not a hand-ported copy), plus feeding this module's own
//! fixture-emitting tests.
//!
//! # What this is, and isn't
//!
//! One snapshot with one `library_rev` gives an atomic view: effects and
//! the devices that reference them never disagree (design section 3, "one
//! snapshot with one rev also gives the page an atomic view"). This module
//! owns exactly the encode/decode round trip and the effect blob's
//! wrapping record -- it never reinterprets [`Preset::to_wire`]'s bytes,
//! which is already the versioned, core-owned effect wire format.
//!
//! # Layout, `lib_proto` 1
//!
//! Little-endian. Header 14 bytes, then up to
//! [`crate::dsp::MAX_PRESETS`] (8) effect records, then up to
//! [`super::model::MAX_PAIRED_DEVICES`] (8) device records -- design
//! section 3's "Max 14 + 8x84 + 8x42 = 1022 B" ceiling.
//!
//! | Bytes | Field | Notes |
//! |---|---|---|
//! | 0..1 | `lib_proto` (`u8`) | [`LIBRARY_PROTO`] |
//! | 1..2 | reserved (`u8`) | Always `0` |
//! | 2..4 | `len` (`u16`) | `HEADER_LEN + effect_count * EFFECT_RECORD_LEN + device_count * DEVICE_RECORD_LEN` |
//! | 4..6 | `library_rev` (`u16`) | Bumped only on a byte difference from the last encode -- see [`super::App::library_snapshot`] |
//! | 6..7 | `flags` (`u8`) | bit0 `presets_ready` |
//! | 7..8 | `effect_count` (`u8`) | |
//! | 8..9 | `effect_rec_len` (`u8`) | [`EFFECT_RECORD_LEN`] -- lets an old host skip a future tail |
//! | 9..10 | `device_count` (`u8`) | |
//! | 10..11 | `device_rec_len` (`u8`) | [`DEVICE_RECORD_LEN`] |
//! | 11..12 | `max_effects` (`u8`) | [`crate::dsp::MAX_PRESETS`] |
//! | 12..13 | `max_devices` (`u8`) | [`super::model::MAX_PAIRED_DEVICES`] |
//! | 13..14 | reserved (`u8`) | Always `0` |
//!
//! Effect record ([`EFFECT_RECORD_LEN`], 84 B): `id` (`u16`),
//! `persisted_seq` (`u16`), `blob` (`[u8; 80]` -- [`Preset::to_wire`],
//! verbatim). Emitted in [`crate::dsp::PresetStore::iter`]'s id-ascending
//! order.
//!
//! Device record ([`DEVICE_RECORD_LEN`], 42 B): `addr` (`[u8; 6]`),
//! `preset_id` (`u16`), `flags` (`u8`: bit0 `connected`), `name_len`
//! (`u8`), `name` (`[u8; 32]`, zero-padded past `name_len`). Emitted in
//! [`super::model::BtModel::paired`]'s stored order.
//!
//! # Why a caller-provided rev, not a clock or a stored counter here
//!
//! Like [`super::telemetry::encode_home_snapshot`], this module has no
//! opinion on *when* the library changed or how the revision counter
//! advances -- both belong to the call site
//! ([`super::App::library_snapshot`]) that can compare successive encodes.
//! [`encode_library_snapshot`] is a pure projection: same inputs (including
//! `library_rev`), same bytes, every time.

use alloc::vec::Vec;

use super::model::{PairedDevice, MAX_PAIRED_DEVICES};
use crate::dsp::{Preset, PresetStore, BLOB_LEN, MAX_PRESETS};

/// This payload's protocol version -- design section 3's `lib_proto 1`.
pub(crate) const LIBRARY_PROTO: u8 = 1;

/// The device record's name field's fixed wire width -- matches
/// [`super::model`]'s private `MAX_DEVICE_NAME_BYTES` on-flash/on-wire cap
/// exactly, the same deliberately-duplicated-constant shape
/// [`super::telemetry::DEVICE_NAME_CAP`] already uses for the identical
/// reason (that constant is private to `model`, so this is a second `32`
/// rather than a visibility change to serve two callers).
const DEVICE_NAME_CAP: usize = 32;

const HEADER_LEN: usize = 14;
/// `id` (2) + `persisted_seq` (2) + `blob` ([`BLOB_LEN`], 80).
pub(crate) const EFFECT_RECORD_LEN: usize = 2 + 2 + BLOB_LEN;
/// `addr` (6) + `preset_id` (2) + `flags` (1) + `name_len` (1) + `name` ([`DEVICE_NAME_CAP`], 32).
pub(crate) const DEVICE_RECORD_LEN: usize = 6 + 2 + 1 + 1 + DEVICE_NAME_CAP;

/// The largest a [`encode_library_snapshot`] payload can ever be: header +
/// [`MAX_PRESETS`] effect records + [`MAX_PAIRED_DEVICES`] device records
/// (design section 3: "Max 14 + 8x84 + 8x42 = 1022 B").
pub(crate) const LIBRARY_SNAPSHOT_MAX_LEN: usize = HEADER_LEN + MAX_PRESETS * EFFECT_RECORD_LEN + MAX_PAIRED_DEVICES * DEVICE_RECORD_LEN;

// --- Fixed byte offsets, header only (record offsets are computed) -----

const OFF_LIB_PROTO: usize = 0;
const OFF_RESERVED1: usize = 1;
const OFF_LEN: usize = 2;
pub(crate) const OFF_LIBRARY_REV: usize = 4;
const OFF_FLAGS: usize = 6;
const OFF_EFFECT_COUNT: usize = 7;
const OFF_EFFECT_REC_LEN: usize = 8;
const OFF_DEVICE_COUNT: usize = 9;
const OFF_DEVICE_REC_LEN: usize = 10;
const OFF_MAX_EFFECTS: usize = 11;
const OFF_MAX_DEVICES: usize = 12;
const OFF_RESERVED2: usize = 13;

/// `flags` bit positions (design section 3).
const FLAG_PRESETS_READY: u8 = 1 << 0;

/// Device record `flags` bit positions (design section 3).
const DEVICE_FLAG_CONNECTED: u8 = 1 << 0;

/// Truncates `s` to at most `cap` bytes, respecting a UTF-8 **character**
/// boundary -- the same discipline
/// [`super::telemetry::truncate_utf8`]/[`super::model::truncate_device_name`]
/// document, duplicated here rather than exposed across modules for one
/// caller.
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

/// Writes a fixed-width device name field: one length-prefix byte, then
/// `cap` bytes, zero-padded past the written length -- the same shape
/// [`super::telemetry::write_fixed_str`] uses.
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

/// Reads a fixed device-name field back, lossily -- see
/// [`super::telemetry::read_fixed_str`]'s doc comment for why lossy
/// decoding is fine here (the wire is Rust-encoded UTF-8 by construction;
/// this only matters for corrupt/malicious input, which
/// [`decode_library_snapshot`]'s callers must already treat as untrusted).
#[allow(dead_code)] // Host-side decode helper; no call site yet (see this module's doc comment).
fn read_fixed_str(buf: &[u8], len_off: usize, bytes_off: usize, cap: usize) -> alloc::string::String {
    let len = (buf[len_off] as usize).min(cap);
    alloc::string::String::from_utf8_lossy(&buf[bytes_off..bytes_off + len]).into_owned()
}

/// One decoded effect record -- [`decode_library_snapshot`]'s per-effect
/// output.
#[allow(dead_code)] // Host-side decode output; no call site yet (see this module's doc comment).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DecodedEffect {
    pub(crate) id: u16,
    pub(crate) persisted_seq: u16,
    pub(crate) preset: Preset,
}

/// One decoded device record -- [`decode_library_snapshot`]'s per-device
/// output.
#[allow(dead_code)] // Host-side decode output; no call site yet (see this module's doc comment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedDevice {
    pub(crate) addr: [u8; 6],
    pub(crate) preset_id: u16,
    pub(crate) connected: bool,
    pub(crate) name: alloc::string::String,
}

/// The decoded form of a [`encode_library_snapshot`] payload -- see this
/// module's doc comment for the wire layout each field round-trips.
#[allow(dead_code)] // Host-side decode output; no call site yet (see this module's doc comment).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LibrarySnapshot {
    pub(crate) library_rev: u16,
    pub(crate) presets_ready: bool,
    pub(crate) max_effects: u8,
    pub(crate) max_devices: u8,
    pub(crate) effects: Vec<DecodedEffect>,
    pub(crate) devices: Vec<DecodedDevice>,
}

/// Builds the `GET_LIBRARY` snapshot from a live `&PresetStore` +
/// paired-device list, at the caller-supplied `library_rev` (design
/// section 3: "Rust encodes the library and compares with the last
/// encoding; rev++ only on a byte difference" -- that comparison is
/// [`super::App::library_snapshot`]'s job, not this pure function's; see
/// this module's doc comment).
///
/// `persisted_seq` looks up each effect's per-id sequence counter (design
/// section 3: "per-id u16 in `App`, bumped when a `PresetLoaded` echo for
/// that id is folded"); an id with no entry yet (should not happen once
/// [`super::App::on_preset_loaded`] has folded its own echo, but a defensive
/// default regardless) reads back as `0`.
///
/// Returns a `Vec<u8>` sized exactly to `len` (header + however many
/// records actually exist) -- never padded out to
/// [`LIBRARY_SNAPSHOT_MAX_LEN`], matching the design's "one consistent
/// snapshot," not a fixed-size wire record.
#[must_use]
pub(crate) fn encode_library_snapshot(presets: &PresetStore, paired: &[PairedDevice], connected_addr: Option<[u8; 6]>, presets_ready: bool, persisted_seq: &alloc::collections::BTreeMap<u16, u16>, library_rev: u16) -> Vec<u8> {
    #[allow(clippy::cast_possible_truncation)] // `PresetStore`/`paired` are already capped at MAX_PRESETS/MAX_PAIRED_DEVICES (8 each) well under `u8::MAX`.
    let effect_count = presets.len().min(MAX_PRESETS) as u8;
    #[allow(clippy::cast_possible_truncation)]
    let device_count = paired.len().min(MAX_PAIRED_DEVICES) as u8;

    let len = HEADER_LEN + usize::from(effect_count) * EFFECT_RECORD_LEN + usize::from(device_count) * DEVICE_RECORD_LEN;
    let mut buf = alloc::vec![0u8; len];

    buf[OFF_LIB_PROTO] = LIBRARY_PROTO;
    buf[OFF_RESERVED1] = 0;
    #[allow(clippy::cast_possible_truncation)] // `len` is well under `u16::MAX` (LIBRARY_SNAPSHOT_MAX_LEN is 1022).
    let len_u16 = len as u16;
    buf[OFF_LEN..OFF_LEN + 2].copy_from_slice(&len_u16.to_le_bytes());
    buf[OFF_LIBRARY_REV..OFF_LIBRARY_REV + 2].copy_from_slice(&library_rev.to_le_bytes());

    let mut flags = 0u8;
    if presets_ready {
        flags |= FLAG_PRESETS_READY;
    }
    buf[OFF_FLAGS] = flags;
    buf[OFF_EFFECT_COUNT] = effect_count;
    #[allow(clippy::cast_possible_truncation)] // EFFECT_RECORD_LEN (84) always fits u8.
    let effect_rec_len = EFFECT_RECORD_LEN as u8;
    buf[OFF_EFFECT_REC_LEN] = effect_rec_len;
    buf[OFF_DEVICE_COUNT] = device_count;
    #[allow(clippy::cast_possible_truncation)] // DEVICE_RECORD_LEN (42) always fits u8.
    let device_rec_len = DEVICE_RECORD_LEN as u8;
    buf[OFF_DEVICE_REC_LEN] = device_rec_len;
    #[allow(clippy::cast_possible_truncation)] // MAX_PRESETS (8) always fits u8.
    let max_effects = MAX_PRESETS as u8;
    buf[OFF_MAX_EFFECTS] = max_effects;
    #[allow(clippy::cast_possible_truncation)] // MAX_PAIRED_DEVICES (8) always fits u8.
    let max_devices = MAX_PAIRED_DEVICES as u8;
    buf[OFF_MAX_DEVICES] = max_devices;
    buf[OFF_RESERVED2] = 0;

    let mut off = HEADER_LEN;
    for (id, preset) in presets.iter().take(usize::from(effect_count)) {
        let seq = persisted_seq.get(&id).copied().unwrap_or(0);
        buf[off..off + 2].copy_from_slice(&id.to_le_bytes());
        buf[off + 2..off + 4].copy_from_slice(&seq.to_le_bytes());
        buf[off + 4..off + 4 + BLOB_LEN].copy_from_slice(&preset.to_wire());
        off += EFFECT_RECORD_LEN;
    }

    for device in paired.iter().take(usize::from(device_count)) {
        buf[off..off + 6].copy_from_slice(&device.addr);
        buf[off + 6..off + 8].copy_from_slice(&device.preset_id.to_le_bytes());
        let mut device_flags = 0u8;
        if Some(device.addr) == connected_addr {
            device_flags |= DEVICE_FLAG_CONNECTED;
        }
        buf[off + 8] = device_flags;
        write_fixed_str(&mut buf, off + 9, off + 10, DEVICE_NAME_CAP, &device.name);
        off += DEVICE_RECORD_LEN;
    }

    buf
}

/// Decodes a [`encode_library_snapshot`] payload. Returns `None` if
/// `bytes` is too short to hold its own declared header, `lib_proto`
/// doesn't match what this module encodes, or the declared `len`/counts
/// don't fit inside `bytes` -- the same "on an unknown/malformed payload,
/// the host bails" discipline
/// [`super::telemetry::decode_home_snapshot`] follows for page 0.
#[must_use]
#[allow(dead_code)] // Host-side decode; no call site yet (see this module's doc comment).
pub(crate) fn decode_library_snapshot(bytes: &[u8]) -> Option<LibrarySnapshot> {
    if bytes.len() < HEADER_LEN {
        return None;
    }
    if bytes[OFF_LIB_PROTO] != LIBRARY_PROTO {
        return None;
    }

    let library_rev = u16::from_le_bytes(bytes[OFF_LIBRARY_REV..OFF_LIBRARY_REV + 2].try_into().ok()?);
    let flags = bytes[OFF_FLAGS];
    let effect_count = usize::from(bytes[OFF_EFFECT_COUNT]);
    let effect_rec_len = usize::from(bytes[OFF_EFFECT_REC_LEN]);
    let device_count = usize::from(bytes[OFF_DEVICE_COUNT]);
    let device_rec_len = usize::from(bytes[OFF_DEVICE_REC_LEN]);
    let max_effects = bytes[OFF_MAX_EFFECTS];
    let max_devices = bytes[OFF_MAX_DEVICES];

    // A newer proto's wider records are skippable via `*_rec_len`, but this
    // decoder only understands `lib_proto` 1's own record shape -- reject
    // outright rather than misparse a wider record as if it were narrower.
    if effect_rec_len != EFFECT_RECORD_LEN || device_rec_len != DEVICE_RECORD_LEN {
        return None;
    }

    let effects_end = HEADER_LEN.checked_add(effect_count.checked_mul(effect_rec_len)?)?;
    let devices_end = effects_end.checked_add(device_count.checked_mul(device_rec_len)?)?;
    if bytes.len() < devices_end {
        return None;
    }

    let mut effects = Vec::with_capacity(effect_count);
    let mut off = HEADER_LEN;
    for _ in 0..effect_count {
        let id = u16::from_le_bytes(bytes[off..off + 2].try_into().ok()?);
        let persisted_seq = u16::from_le_bytes(bytes[off + 2..off + 4].try_into().ok()?);
        let preset = Preset::from_wire(&bytes[off + 4..off + 4 + BLOB_LEN]);
        effects.push(DecodedEffect { id, persisted_seq, preset });
        off += effect_rec_len;
    }

    let mut devices = Vec::with_capacity(device_count);
    for _ in 0..device_count {
        let mut addr = [0u8; 6];
        addr.copy_from_slice(&bytes[off..off + 6]);
        let preset_id = u16::from_le_bytes(bytes[off + 6..off + 8].try_into().ok()?);
        let device_flags = bytes[off + 8];
        let name = read_fixed_str(bytes, off + 9, off + 10, DEVICE_NAME_CAP);
        devices.push(DecodedDevice { addr, preset_id, connected: device_flags & DEVICE_FLAG_CONNECTED != 0, name });
        off += device_rec_len;
    }

    Some(LibrarySnapshot { library_rev, presets_ready: flags & FLAG_PRESETS_READY != 0, max_effects, max_devices, effects, devices })
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;
    use alloc::string::ToString;

    use super::*;
    use crate::dsp::Preset;

    fn addr(last_byte: u8) -> [u8; 6] {
        [0x94, 0xDB, 0x56, 0x54, 0x7C, last_byte]
    }

    #[test]
    fn encode_empty_library_reports_zero_counts_and_header_only_length() {
        let presets = PresetStore::new();
        let seqs = BTreeMap::new();
        let bytes = encode_library_snapshot(&presets, &[], None, false, &seqs, 3);
        assert_eq!(bytes.len(), HEADER_LEN, "no effects, no devices -- header only");
        let snap = decode_library_snapshot(&bytes).expect("a well-formed encode must always decode");
        assert_eq!(snap.library_rev, 3);
        assert!(!snap.presets_ready);
        assert!(snap.effects.is_empty());
        assert!(snap.devices.is_empty());
        assert_eq!(snap.max_effects, 8);
        assert_eq!(snap.max_devices, 8);
    }

    #[test]
    fn encode_round_trips_effects_with_persisted_seq_and_blob() {
        let mut presets = PresetStore::new();
        let id = presets.create(Preset::new("Warm"));
        let mut seqs = BTreeMap::new();
        seqs.insert(id, 7u16);

        let bytes = encode_library_snapshot(&presets, &[], None, true, &seqs, 1);
        let snap = decode_library_snapshot(&bytes).unwrap();

        assert!(snap.presets_ready);
        assert_eq!(snap.effects.len(), 1);
        assert_eq!(snap.effects[0].id, id);
        assert_eq!(snap.effects[0].persisted_seq, 7);
        assert_eq!(snap.effects[0].preset.name, "Warm");
    }

    #[test]
    fn encode_effect_with_no_persisted_seq_entry_defaults_to_zero() {
        let mut presets = PresetStore::new();
        let _id = presets.create(Preset::new("Bright"));
        let seqs = BTreeMap::new(); // no entry for `id`.

        let bytes = encode_library_snapshot(&presets, &[], None, false, &seqs, 1);
        let snap = decode_library_snapshot(&bytes).unwrap();
        assert_eq!(snap.effects[0].persisted_seq, 0);
    }

    #[test]
    fn encode_round_trips_devices_with_connected_flag() {
        let presets = PresetStore::new();
        let seqs = BTreeMap::new();
        let paired = alloc::vec![
            PairedDevice { addr: addr(0x01), name: "Buds A".to_string(), mru_seq: 1, ldac_quality: 0, preset_id: 0 },
            PairedDevice { addr: addr(0x02), name: "Buds B".to_string(), mru_seq: 2, ldac_quality: 4, preset_id: 9 },
        ];

        let bytes = encode_library_snapshot(&presets, &paired, Some(addr(0x02)), false, &seqs, 1);
        let snap = decode_library_snapshot(&bytes).unwrap();

        assert_eq!(snap.devices.len(), 2);
        assert!(!snap.devices[0].connected, "Buds A is not the connected address");
        assert_eq!(snap.devices[0].name, "Buds A");
        assert!(snap.devices[1].connected, "Buds B matches connected_addr");
        assert_eq!(snap.devices[1].preset_id, 9);
    }

    #[test]
    fn encode_multibyte_device_name_truncates_on_a_char_boundary() {
        let presets = PresetStore::new();
        let seqs = BTreeMap::new();
        let long_name: alloc::string::String = "\u{20AC}".repeat(11); // 33 bytes, cap is 32.
        let paired = alloc::vec![PairedDevice { addr: addr(0x03), name: long_name, mru_seq: 1, ldac_quality: 0, preset_id: 0 }];

        let bytes = encode_library_snapshot(&presets, &paired, None, false, &seqs, 1);
        let snap = decode_library_snapshot(&bytes).unwrap();
        assert_eq!(snap.devices[0].name.chars().count(), 10, "must back off a full character rather than keep a partial one");
    }

    #[test]
    fn encode_at_max_effects_and_devices_matches_the_1022_byte_ceiling() {
        let mut presets = PresetStore::new();
        for i in 0..MAX_PRESETS {
            presets.create(Preset::new(&alloc::format!("Effect {i}")));
        }
        let seqs = BTreeMap::new();
        let paired: Vec<PairedDevice> = (0..MAX_PAIRED_DEVICES).map(|i| PairedDevice { addr: addr(i as u8), name: "Buds".to_string(), mru_seq: 1, ldac_quality: 0, preset_id: 0 }).collect();

        let bytes = encode_library_snapshot(&presets, &paired, None, true, &seqs, 1);
        assert_eq!(bytes.len(), LIBRARY_SNAPSHOT_MAX_LEN);
        assert_eq!(LIBRARY_SNAPSHOT_MAX_LEN, 1022, "design section 3's own worked-out ceiling");

        let snap = decode_library_snapshot(&bytes).unwrap();
        assert_eq!(snap.effects.len(), MAX_PRESETS);
        assert_eq!(snap.devices.len(), MAX_PAIRED_DEVICES);
    }

    #[test]
    fn decode_rejects_a_too_short_buffer() {
        assert!(decode_library_snapshot(&[0u8; HEADER_LEN - 1]).is_none());
    }

    #[test]
    fn decode_rejects_an_unknown_lib_proto() {
        let presets = PresetStore::new();
        let seqs = BTreeMap::new();
        let mut bytes = encode_library_snapshot(&presets, &[], None, false, &seqs, 1);
        bytes[OFF_LIB_PROTO] = 99;
        assert!(decode_library_snapshot(&bytes).is_none());
    }

    #[test]
    fn decode_rejects_a_truncated_payload_shorter_than_its_own_declared_counts() {
        let mut presets = PresetStore::new();
        presets.create(Preset::new("Warm"));
        let seqs = BTreeMap::new();
        let mut bytes = encode_library_snapshot(&presets, &[], None, false, &seqs, 1);
        bytes.truncate(HEADER_LEN + 4); // header claims one effect record; body is chopped short.
        assert!(decode_library_snapshot(&bytes).is_none());
    }

    // --- Layout stability (the "golden bytes" test) --------------------

    /// Locks the exact byte layout for a populated snapshot -- see this
    /// module's doc comment and
    /// [`super::telemetry::tests::golden_bytes_layout_is_stable`]'s doc
    /// comment for why this kind of test exists.
    #[test]
    fn golden_bytes_layout_is_stable() {
        let mut presets = PresetStore::new();
        let id = presets.create(Preset::new("Warm"));
        let mut seqs = BTreeMap::new();
        seqs.insert(id, 2u16);
        let paired = alloc::vec![PairedDevice { addr: addr(0xF2), name: "Pixel Buds".to_string(), mru_seq: 1, ldac_quality: 4, preset_id: id }];

        let bytes = encode_library_snapshot(&presets, &paired, Some(addr(0xF2)), true, &seqs, 5);

        assert_eq!(bytes.len(), HEADER_LEN + EFFECT_RECORD_LEN + DEVICE_RECORD_LEN);
        assert_eq!(EFFECT_RECORD_LEN, 84);
        assert_eq!(DEVICE_RECORD_LEN, 42);

        // Header.
        assert_eq!(bytes[0], 1, "lib_proto");
        assert_eq!(bytes[1], 0, "reserved");
        assert_eq!(&bytes[2..4], &(bytes.len() as u16).to_le_bytes(), "len");
        assert_eq!(&bytes[4..6], &5u16.to_le_bytes(), "library_rev");
        assert_eq!(bytes[6], 0b0000_0001, "flags: presets_ready");
        assert_eq!(bytes[7], 1, "effect_count");
        assert_eq!(bytes[8], 84, "effect_rec_len");
        assert_eq!(bytes[9], 1, "device_count");
        assert_eq!(bytes[10], 42, "device_rec_len");
        assert_eq!(bytes[11], 8, "max_effects");
        assert_eq!(bytes[12], 8, "max_devices");
        assert_eq!(bytes[13], 0, "reserved");

        // Effect record.
        let eff = &bytes[14..14 + EFFECT_RECORD_LEN];
        assert_eq!(&eff[0..2], &id.to_le_bytes(), "effect id");
        assert_eq!(&eff[2..4], &2u16.to_le_bytes(), "persisted_seq");
        assert_eq!(&eff[4..4 + BLOB_LEN], &presets.get(id).unwrap().to_wire(), "blob is Preset::to_wire verbatim");

        // Device record.
        let dev = &bytes[14 + EFFECT_RECORD_LEN..14 + EFFECT_RECORD_LEN + DEVICE_RECORD_LEN];
        assert_eq!(&dev[0..6], &addr(0xF2));
        assert_eq!(&dev[6..8], &id.to_le_bytes(), "preset_id");
        assert_eq!(dev[8], 0b0000_0001, "connected flag");
        assert_eq!(dev[9], 10, "name_len");
        assert_eq!(&dev[10..20], b"Pixel Buds");
        assert!(dev[20..42].iter().all(|&b| b == 0), "device name zero-padded past its length");

        // Full round trip.
        let snap = decode_library_snapshot(&bytes).unwrap();
        assert_eq!(snap.library_rev, 5);
        assert_eq!(snap.effects[0].id, id);
        assert_eq!(snap.effects[0].preset.name, "Warm");
        assert_eq!(snap.devices[0].preset_id, id);
        assert!(snap.devices[0].connected);
    }
}
