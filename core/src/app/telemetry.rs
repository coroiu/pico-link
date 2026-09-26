//! Page-0 Home telemetry snapshot: the versioned, fixed-layout wire
//! payload the web companion polls over `iface 6`'s `GET_TELEMETRY`
//! control request (bead `pico-link-jyhk.2`, "ADA DESIGN" comment on
//! `pico-link-jyhk.1`, section 4 -- that comment is the spec this module
//! implements; this doc comment plus [`golden_bytes_layout_is_stable`]
//! (below) is the durable, checked-in copy of it).
//!
//! `dead_code` is allowed crate-wide for this module: `pico-link-jyhk.2`'s
//! scope is deliberately this file only (encode/decode + tests, per the
//! design's own task split) -- the `ui-ffi` call site
//! (`pl_ui_telemetry`) that will make [`encode_home_snapshot`] a live
//! export is `pico-link-jyhk.3`, not yet built.
#![allow(dead_code)]
//!
//! # What this is, and isn't
//!
//! [`encode_home_snapshot`] projects [`BtModel`] + [`PresetStore`] + a
//! render-time [`Instant`] into `HOME_SNAPSHOT_LEN` raw bytes -- Home's
//! *inputs*, not its rendered output. Ballistics (hold/decay/attack),
//! staleness and fault-retirement tiers are NOT computed here: the design
//! is explicit that the host recomputes those against its own clock, the
//! same way `render::hero` computes them at render time from
//! `BtModel::out_level`/`FaultLog`. This module owns exactly the encode/
//! decode round trip and nothing downstream of it -- no ui-ffi call site,
//! no C, no `HomeInputs` refactor of `render::home::project_hero` (those
//! are `pico-link-jyhk.3`/`.7`).
//!
//! `decode_home_snapshot` exists for the same reason `core` owns the
//! schema at all (design: "C only copies opaque bytes"): whichever side
//! ends up decoding this wire format on the host (the wasm build per
//! Fern's design, or a native Rust tool) should decode with the same
//! crate that encoded it, not a hand-ported copy.
//!
//! # Layout, page 0, proto 1
//!
//! Little-endian, fixed offsets, [`HOME_SNAPSHOT_LEN`] (163) bytes -- three
//! EP0 packets at the USB full-speed 64-byte control-transfer size. Fields
//! are append-only within a proto (design section 4's versioning rule): a
//! future field is added at the end and `len` grows; changing an existing
//! field's offset or width requires bumping [`TELEMETRY_PROTO`] instead.
//!
//! | Bytes | Field | Notes |
//! |---|---|---|
//! | 0..1 | `proto` (`u8`) | [`TELEMETRY_PROTO`] |
//! | 1..2 | `page` (`u8`) | [`TELEMETRY_PAGE_HOME`] |
//! | 2..4 | `len` (`u16`) | Always [`HOME_SNAPSHOT_LEN`] as of proto 1 |
//! | 4..8 | `uptime_ms` (`u32`) | `now`'s microsecond count, truncated to ms |
//! | 8..12 | `snap_seq` (`u32`) | Caller-supplied; `0` means "not ready" |
//! | 12..13 | `link` (`u8`) | `0` none, `1` connected |
//! | 13..14 | `flags` (`u8`) | bit0 adaptive, bit1 `kbps_is_live`, bit2 `volume_present`, bit3 muted, bit4 `level_present` |
//! | 14..16 | `kbps` (`u16`) | Resolved the way `render::home::project_hero` does |
//! | 16..17 | `codec_word_len` (`u8`) | |
//! | 17..25 | `codec_word` (`[u8; 8]`) | Zero-padded past `codec_word_len` |
//! | 25..26 | `device_name_len` (`u8`) | |
//! | 26..58 | `device_name` (`[u8; 32]`) | Zero-padded past `device_name_len` |
//! | 58..59 | `fx_preset_name_len` (`u8`) | `0` means Off |
//! | 59..75 | `fx_preset_name` (`[u8; 16]`) | Zero-padded past `fx_preset_name_len` |
//! | 75..76 | `volume_level` (`u8`) | 0..127, `0` when absent |
//! | 76..77 | `volume_source` (`u8`) | `0` host, `1` sink, `2` device |
//! | 77..78 | `peak_l` (`u8`) | |
//! | 78..79 | `peak_r` (`u8`) | |
//! | 79..80 | `rms_l` (`u8`) | |
//! | 80..81 | `rms_r` (`u8`) | |
//! | 81..85 | `received_ms` (`u32`) | Same clock domain as `uptime_ms` |
//! | 85..163 | 6x fault slot, [`FaultKey::ALL`] order | 13 bytes each, see below |
//!
//! Each fault slot: `count` (`u16`, `0` = absent), `first_seen_ms`
//! (`u32`), `last_seen_ms` (`u32`), `value_kind` (`u8`: `0` none, `1`
//! ratio, `2` count, `3` millis), `value` (`u16`).
//!
//! ## Deviation from the ADA DESIGN comment: `value_kind == 3` (millis)
//!
//! The design comment's payload section enumerates `value_kind` as only
//! `0`/`1`/`2` (none/ratio/count), but [`FaultValue`] has a THIRD variant,
//! [`FaultValue::Millis`], which is live today --
//! `FaultKey::BufStarved`'s "ring dry, min fill `N`ms" reading
//! (`screens/why_page.rs`). Dropping it on the wire would silently
//! corrupt exactly the one fault the design's own worked example
//! (`BufStarved`) carries a value for. `ui-ffi::PlFaultValueKind` already
//! establishes `3 == Millis` as this project's wire convention for this
//! exact enum (`lib.rs`'s `TryFrom<u8>` impl), so this module reuses that
//! ordinal rather than inventing a new one -- a one-ordinal extension of
//! the spec, not a redesign.
//!
//! # Why `snap_seq` and `now` are parameters, not read from a clock here
//!
//! This module has no [`Clock`](crate::platform::Clock) of its own (it's
//! `core`, platform-free) and no opinion on polling cadence or
//! not-ready gating -- both belong to the superloop call site the design
//! describes (`main.c`, after `pl_dsp_service()`), which is `ui-ffi`/C
//! territory (`pico-link-jyhk.3`). `encode_home_snapshot` is a pure
//! projection: same inputs, same bytes, every time -- which is exactly
//! what makes [`golden_bytes_layout_is_stable`] a meaningful regression
//! guard.

use alloc::string::String;

use super::events::VolumeSource;
use super::fault::{FaultKey, FaultValue};
use super::model::BtModel;
use super::LDAC_QUALITY_ADAPTIVE;
use crate::dsp::PresetStore;
use crate::render::Instant;

/// This payload's protocol version. Bumping this is the only sanctioned
/// way to reorder or resize an existing field -- see this module's doc
/// comment.
pub(crate) const TELEMETRY_PROTO: u8 = 1;

/// `GET_TELEMETRY`'s `wValue` for the Home page -- the only page this
/// module encodes; a future F3 diagnostics page (design section 4) is a
/// separate payload under the same header.
pub(crate) const TELEMETRY_PAGE_HOME: u8 = 0;

/// The codec word field's fixed wire width (design: "codec word 8 B").
/// Every codec table entry in `firmware/src/codec_table.h`
/// ("SBC"/"AAC"/"LDAC"/...) fits comfortably inside this today.
const CODEC_WORD_CAP: usize = 8;

/// The device name field's fixed wire width -- matches `model`'s private
/// `MAX_DEVICE_NAME_BYTES` on-flash/on-wire cap exactly (that constant is
/// private to `model`, so this is a second, deliberately duplicated `32`
/// rather than a visibility change to serve one caller).
const DEVICE_NAME_CAP: usize = 32;

/// The FX preset name field's fixed wire width -- matches
/// [`crate::dsp::MAX_NAME_BYTES`] exactly (design: "FX preset name 16 B
/// (`MAX_NAME_BYTES`)").
const FX_NAME_CAP: usize = crate::dsp::MAX_NAME_BYTES;

/// Number of fault slots -- one per [`FaultKey`], in [`FaultKey::ALL`]
/// order.
const FAULT_SLOT_COUNT: usize = FaultKey::ALL.len();

/// One fault slot's wire width: `count` (2) + `first_seen_ms` (4) +
/// `last_seen_ms` (4) + `value_kind` (1) + `value` (2).
const FAULT_SLOT_LEN: usize = 13;

// --- Fixed byte offsets (see this module's doc comment table) ----------

const OFF_PROTO: usize = 0;
const OFF_PAGE: usize = 1;
const OFF_LEN: usize = 2;
const OFF_UPTIME_MS: usize = 4;
const OFF_SNAP_SEQ: usize = 8;
const OFF_LINK: usize = 12;
const OFF_FLAGS: usize = 13;
const OFF_KBPS: usize = 14;
const OFF_CODEC_LEN: usize = 16;
const OFF_CODEC_BYTES: usize = OFF_CODEC_LEN + 1;
const OFF_NAME_LEN: usize = OFF_CODEC_BYTES + CODEC_WORD_CAP;
const OFF_NAME_BYTES: usize = OFF_NAME_LEN + 1;
const OFF_FX_LEN: usize = OFF_NAME_BYTES + DEVICE_NAME_CAP;
const OFF_FX_BYTES: usize = OFF_FX_LEN + 1;
const OFF_VOL_LEVEL: usize = OFF_FX_BYTES + FX_NAME_CAP;
const OFF_VOL_SOURCE: usize = OFF_VOL_LEVEL + 1;
const OFF_PEAK_L: usize = OFF_VOL_SOURCE + 1;
const OFF_PEAK_R: usize = OFF_PEAK_L + 1;
const OFF_RMS_L: usize = OFF_PEAK_R + 1;
const OFF_RMS_R: usize = OFF_RMS_L + 1;
const OFF_RECEIVED_MS: usize = OFF_RMS_R + 1;
const OFF_FAULTS: usize = OFF_RECEIVED_MS + 4;

/// The whole payload's fixed length, in bytes -- `HOME_SNAPSHOT_LEN` bytes
/// as of proto 1, matching the design's "roughly 160 B" (163, computed
/// from the layout above, not hand-picked).
pub(crate) const HOME_SNAPSHOT_LEN: usize = OFF_FAULTS + FAULT_SLOT_COUNT * FAULT_SLOT_LEN;

/// `flags` bit positions (design section 4).
const FLAG_ADAPTIVE: u8 = 1 << 0;
const FLAG_KBPS_IS_LIVE: u8 = 1 << 1;
const FLAG_VOLUME_PRESENT: u8 = 1 << 2;
const FLAG_MUTED: u8 = 1 << 3;
const FLAG_LEVEL_PRESENT: u8 = 1 << 4;

/// `value_kind` ordinals -- see this module's doc comment for why `3`
/// (millis) exists even though the design comment's payload section only
/// lists `0`/`1`/`2`.
const VALUE_KIND_NONE: u8 = 0;
const VALUE_KIND_RATIO: u8 = 1;
const VALUE_KIND_COUNT: u8 = 2;
const VALUE_KIND_MILLIS: u8 = 3;

/// Truncates `s` to at most `cap` bytes, respecting a UTF-8 **character**
/// boundary -- the same discipline [`super::model::truncate_device_name`]
/// documents ("Rust owns text; C owns bytes"), generalised over `cap`
/// since this module truncates three differently-sized fields (codec
/// word, device name, FX preset name) rather than just one.
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

/// Writes `s` (truncated to `cap` bytes on a char boundary) into the
/// one-byte length prefix at `len_off` and the `cap`-byte fixed field at
/// `bytes_off` (zero-padded past the written length) -- the one shape all
/// three string fields share.
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

/// Reads a fixed string field back: `len` bytes from `bytes_off`, lossily
/// converted (the wire is Rust-encoded UTF-8 by construction, so this only
/// matters for a corrupt/malicious payload, which
/// [`decode_home_snapshot`]'s callers must already treat as untrusted
/// input off USB).
fn read_fixed_str(buf: &[u8], len_off: usize, bytes_off: usize, cap: usize) -> String {
    let len = (buf[len_off] as usize).min(cap);
    String::from_utf8_lossy(&buf[bytes_off..bytes_off + len]).into_owned()
}

fn value_kind_and_value(value: Option<FaultValue>) -> (u8, u16) {
    match value {
        None => (VALUE_KIND_NONE, 0),
        Some(FaultValue::Ratio(v)) => (VALUE_KIND_RATIO, v),
        Some(FaultValue::Count(v)) => (VALUE_KIND_COUNT, v),
        Some(FaultValue::Millis(v)) => (VALUE_KIND_MILLIS, v),
    }
}

fn value_from_kind(kind: u8, value: u16) -> Option<FaultValue> {
    match kind {
        VALUE_KIND_RATIO => Some(FaultValue::Ratio(value)),
        VALUE_KIND_COUNT => Some(FaultValue::Count(value)),
        VALUE_KIND_MILLIS => Some(FaultValue::Millis(value)),
        // `VALUE_KIND_NONE` and any unrecognised ordinal both decode to
        // "no value" -- an unknown kind from a newer proto/a corrupt
        // payload must never be matched on as a discriminant (the same
        // "checked, not trusted" rule `ui-ffi`'s `TryFrom` impls follow).
        _ => None,
    }
}

/// An [`Instant`]'s microsecond count, truncated to whole milliseconds and
/// saturated into `u32` -- `u32::MAX` ms is over 49 days, comfortably past
/// any realistic uptime between reboots on this hardware, so saturation
/// (rather than a wider field) is an acceptable, documented compromise
/// for a 163-byte-budget wire format.
#[allow(clippy::cast_possible_truncation)]
fn instant_to_millis_u32(instant: Instant) -> u32 {
    let ms = instant.as_micros() / 1_000;
    u32::try_from(ms).unwrap_or(u32::MAX)
}

fn volume_source_to_wire(source: VolumeSource) -> u8 {
    match source {
        VolumeSource::Host => 0,
        VolumeSource::Sink => 1,
        VolumeSource::Device => 2,
    }
}

fn volume_source_from_wire(source: u8) -> VolumeSource {
    match source {
        1 => VolumeSource::Sink,
        2 => VolumeSource::Device,
        // `0` and any unrecognised ordinal both decode to `Host` -- the
        // same "unknown decodes to the safest default, never panics"
        // discipline the rest of this module follows for wire input.
        _ => VolumeSource::Host,
    }
}

/// One decoded fault slot -- [`FaultEntry`](super::fault::FaultEntry)'s
/// wire shape, with device-clock millisecond timestamps rather than
/// [`Instant`]s (the host has no shared clock to build an `Instant`
/// from -- see this module's doc comment on `received_ms`'s clock
/// domain, which applies identically here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecodedFault {
    pub(crate) count: u16,
    pub(crate) first_seen_ms: u32,
    pub(crate) last_seen_ms: u32,
    pub(crate) value: Option<FaultValue>,
}

/// The decoded form of a [`HOME_SNAPSHOT_LEN`]-byte page-0 payload --
/// [`decode_home_snapshot`]'s output, and the type a future host-side
/// consumer (wasm or native) folds into [`super::Event`]s (design
/// section 4's "WASM: decode with the same core crate ... build Home from
/// the rest"). Deliberately flat/raw, matching what [`encode_home_
/// snapshot`] read out of [`BtModel`] -- this is Home's *inputs*, not its
/// rendered output; see this module's doc comment.
///
/// `clippy::struct_excessive_bools` is silenced deliberately: these are
/// five *independent* presence/state bits straight off the wire's own
/// `flags` byte (design section 4), not a state machine masquerading as
/// booleans -- collapsing them into an enum would just be re-deriving the
/// wire layout this struct exists to mirror.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HomeSnapshot {
    pub(crate) uptime_ms: u32,
    /// `0` means "not ready" (design section 3, flow step (c)).
    pub(crate) snap_seq: u32,
    pub(crate) link_connected: bool,
    pub(crate) codec_word: String,
    pub(crate) kbps: u16,
    pub(crate) kbps_adaptive: bool,
    pub(crate) kbps_is_live: bool,
    pub(crate) device_name: String,
    /// Empty means Off -- the design's own convention for this field.
    pub(crate) fx_preset_name: String,
    pub(crate) volume_level: u8,
    pub(crate) volume_muted: bool,
    pub(crate) volume_source: VolumeSource,
    pub(crate) volume_present: bool,
    pub(crate) peak_l: u8,
    pub(crate) peak_r: u8,
    pub(crate) rms_l: u8,
    pub(crate) rms_r: u8,
    pub(crate) received_ms: u32,
    pub(crate) level_present: bool,
    /// [`FaultKey::ALL`] order, `None` for a key never raised.
    pub(crate) faults: [Option<DecodedFault>; FAULT_SLOT_COUNT],
}

/// Builds the page-0 Home snapshot from a live `&BtModel` read plus the
/// preset store, exactly the field set `render::home::HomeView::
/// project_hero` reads (design section 3: "Everything else comes from
/// `BtModel` plus `PresetStore`, exactly what `HomeView::project_hero`
/// reads"). `now` and `snap_seq` are the caller's (the design's superloop
/// call site owns both -- see this module's doc comment).
///
/// Pure and read-only: never mutates `model`/`presets`, never touches
/// `dirty`/damage/idle state -- the design's Rust-contract requirement
/// ("The borrow is read-only").
#[must_use]
pub(crate) fn encode_home_snapshot(model: &BtModel, presets: &PresetStore, now: Instant, snap_seq: u32) -> [u8; HOME_SNAPSHOT_LEN] {
    let mut buf = [0u8; HOME_SNAPSHOT_LEN];

    buf[OFF_PROTO] = TELEMETRY_PROTO;
    buf[OFF_PAGE] = TELEMETRY_PAGE_HOME;
    #[allow(clippy::cast_possible_truncation)] // `HOME_SNAPSHOT_LEN` (163) always fits `u16`.
    let len = HOME_SNAPSHOT_LEN as u16;
    buf[OFF_LEN..OFF_LEN + 2].copy_from_slice(&len.to_le_bytes());
    buf[OFF_UPTIME_MS..OFF_UPTIME_MS + 4].copy_from_slice(&instant_to_millis_u32(now).to_le_bytes());
    buf[OFF_SNAP_SEQ..OFF_SNAP_SEQ + 4].copy_from_slice(&snap_seq.to_le_bytes());

    let mut flags = 0u8;

    // `link`/`kbps`/codec word/device name all stay zeroed when there is
    // no connected codec -- "absent, never frozen or faked" (design
    // section 15, the same rule `project_hero`'s `NO LINK` arm follows).
    if let Some(codec) = &model.connected_codec {
        buf[OFF_LINK] = 1;

        // Same lookup/derivation `render::home::HomeView::project_
        // hero` performs -- see that function's doc comment for why
        // each piece is computed this way.
        let device = model.paired.iter().find(|d| d.addr == codec.addr);
        let is_ldac = codec.word == "LDAC";
        let device_ldac_quality = device.map_or(0, |d| d.ldac_quality);
        let adaptive = is_ldac && device_ldac_quality == LDAC_QUALITY_ADAPTIVE;
        let kbps_is_live = is_ldac && model.ldac_live_kbps.is_some();
        #[allow(clippy::cast_possible_truncation)] // Real-world kbps figures (SBC..LDAC) never approach `u16::MAX`.
        let kbps = if is_ldac { model.ldac_live_kbps.unwrap_or(codec.nominal_bitrate_bps / 1000) } else { codec.nominal_bitrate_bps / 1000 } as u16;

        if adaptive {
            flags |= FLAG_ADAPTIVE;
        }
        if kbps_is_live {
            flags |= FLAG_KBPS_IS_LIVE;
        }

        buf[OFF_KBPS..OFF_KBPS + 2].copy_from_slice(&kbps.to_le_bytes());
        write_fixed_str(&mut buf, OFF_CODEC_LEN, OFF_CODEC_BYTES, CODEC_WORD_CAP, &codec.word);

        let device_name = device.map(|d| d.name.as_str()).unwrap_or_default();
        write_fixed_str(&mut buf, OFF_NAME_LEN, OFF_NAME_BYTES, DEVICE_NAME_CAP, device_name);
    }

    // FX line: the connected device's assigned preset, resolved the same
    // way `project_hero`'s `fx_line` is -- empty wire string means Off
    // (design section 4), so an unresolved/absent name is never written
    // as the literal text "Off".
    let fx_name = model.connected_addr.and_then(|addr| model.paired.iter().find(|d| d.addr == addr)).map(|device| super::resolve_effect_name(presets, device.preset_id));
    let fx_name = fx_name.filter(|name| name != "Off").unwrap_or_default();
    write_fixed_str(&mut buf, OFF_FX_LEN, OFF_FX_BYTES, FX_NAME_CAP, &fx_name);

    if let Some(volume) = model.volume {
        flags |= FLAG_VOLUME_PRESENT;
        if volume.muted {
            flags |= FLAG_MUTED;
        }
        buf[OFF_VOL_LEVEL] = volume.level;
        buf[OFF_VOL_SOURCE] = volume_source_to_wire(volume.source);
    }

    if let Some(level) = model.out_level {
        flags |= FLAG_LEVEL_PRESENT;
        buf[OFF_PEAK_L] = level.peak_l;
        buf[OFF_PEAK_R] = level.peak_r;
        buf[OFF_RMS_L] = level.rms_l;
        buf[OFF_RMS_R] = level.rms_r;
        buf[OFF_RECEIVED_MS..OFF_RECEIVED_MS + 4].copy_from_slice(&instant_to_millis_u32(level.received_at).to_le_bytes());
    }

    buf[OFF_FLAGS] = flags;

    for (i, key) in FaultKey::ALL.into_iter().enumerate() {
        let slot_off = OFF_FAULTS + i * FAULT_SLOT_LEN;
        if let Some(entry) = model.fault_log.entry(key) {
            let (kind, value) = value_kind_and_value(entry.value);
            buf[slot_off..slot_off + 2].copy_from_slice(&entry.count.to_le_bytes());
            buf[slot_off + 2..slot_off + 6].copy_from_slice(&instant_to_millis_u32(entry.first_seen).to_le_bytes());
            buf[slot_off + 6..slot_off + 10].copy_from_slice(&instant_to_millis_u32(entry.last_seen).to_le_bytes());
            buf[slot_off + 10] = kind;
            buf[slot_off + 11..slot_off + 13].copy_from_slice(&value.to_le_bytes());
        }
        // A never-raised key's slot stays all-zero: `count == 0` already
        // means absent on the wire, matching `FaultLog::entry`'s own
        // `None` -- no separate "present" flag is needed per slot.
    }

    buf
}

/// Decodes a page-0 payload [`encode_home_snapshot`] produced. Returns
/// `None` if `bytes` is too short to hold [`HOME_SNAPSHOT_LEN`], or if
/// `proto`/`page` don't match what this module encodes -- the design's
/// "on an unknown proto, the host shows update-needed and stops polling"
/// rule starts here, at the one place the schema is owned. Trailing bytes
/// past `HOME_SNAPSHOT_LEN` (a future proto's appended fields) are
/// ignored, per the design's append-only versioning rule.
#[must_use]
pub(crate) fn decode_home_snapshot(bytes: &[u8]) -> Option<HomeSnapshot> {
    if bytes.len() < HOME_SNAPSHOT_LEN {
        return None;
    }
    if bytes[OFF_PROTO] != TELEMETRY_PROTO || bytes[OFF_PAGE] != TELEMETRY_PAGE_HOME {
        return None;
    }

    let uptime_ms = u32::from_le_bytes(bytes[OFF_UPTIME_MS..OFF_UPTIME_MS + 4].try_into().ok()?);
    let snap_seq = u32::from_le_bytes(bytes[OFF_SNAP_SEQ..OFF_SNAP_SEQ + 4].try_into().ok()?);
    let link_connected = bytes[OFF_LINK] != 0;
    let flags = bytes[OFF_FLAGS];
    let kbps = u16::from_le_bytes(bytes[OFF_KBPS..OFF_KBPS + 2].try_into().ok()?);

    let codec_word = read_fixed_str(bytes, OFF_CODEC_LEN, OFF_CODEC_BYTES, CODEC_WORD_CAP);
    let device_name = read_fixed_str(bytes, OFF_NAME_LEN, OFF_NAME_BYTES, DEVICE_NAME_CAP);
    let fx_preset_name = read_fixed_str(bytes, OFF_FX_LEN, OFF_FX_BYTES, FX_NAME_CAP);

    let volume_level = bytes[OFF_VOL_LEVEL];
    let volume_source = volume_source_from_wire(bytes[OFF_VOL_SOURCE]);

    let peak_l = bytes[OFF_PEAK_L];
    let peak_r = bytes[OFF_PEAK_R];
    let rms_l = bytes[OFF_RMS_L];
    let rms_r = bytes[OFF_RMS_R];
    let received_ms = u32::from_le_bytes(bytes[OFF_RECEIVED_MS..OFF_RECEIVED_MS + 4].try_into().ok()?);

    let mut faults = [None; FAULT_SLOT_COUNT];
    for (i, slot) in faults.iter_mut().enumerate() {
        let slot_off = OFF_FAULTS + i * FAULT_SLOT_LEN;
        let count = u16::from_le_bytes(bytes[slot_off..slot_off + 2].try_into().ok()?);
        if count == 0 {
            continue;
        }
        let first_seen_ms = u32::from_le_bytes(bytes[slot_off + 2..slot_off + 6].try_into().ok()?);
        let last_seen_ms = u32::from_le_bytes(bytes[slot_off + 6..slot_off + 10].try_into().ok()?);
        let kind = bytes[slot_off + 10];
        let value = u16::from_le_bytes(bytes[slot_off + 11..slot_off + 13].try_into().ok()?);
        *slot = Some(DecodedFault { count, first_seen_ms, last_seen_ms, value: value_from_kind(kind, value) });
    }

    Some(HomeSnapshot {
        uptime_ms,
        snap_seq,
        link_connected,
        codec_word,
        kbps,
        kbps_adaptive: flags & FLAG_ADAPTIVE != 0,
        kbps_is_live: flags & FLAG_KBPS_IS_LIVE != 0,
        device_name,
        fx_preset_name,
        volume_level,
        volume_muted: flags & FLAG_MUTED != 0,
        volume_source,
        volume_present: flags & FLAG_VOLUME_PRESENT != 0,
        peak_l,
        peak_r,
        rms_l,
        rms_r,
        received_ms,
        level_present: flags & FLAG_LEVEL_PRESENT != 0,
        faults,
    })
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;
    use alloc::vec::Vec;

    use super::*;
    use crate::app::model::{ConnectedCodec, OutLevelSample, PairedDevice};
    use crate::app::VolumeState;
    use crate::dsp::Preset;

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

    fn addr(last_byte: u8) -> [u8; 6] {
        [0x94, 0xDB, 0x56, 0x54, 0x7C, last_byte]
    }

    // --- Empty model: absent, not faked -------------------------------

    #[test]
    fn encode_empty_model_reports_no_link_no_volume_no_level_and_all_faults_absent() {
        let model = BtModel::default();
        let presets = PresetStore::new();
        let now = Instant::from_micros(1_500_000);

        let bytes = encode_home_snapshot(&model, &presets, now, 0);
        let snap = decode_home_snapshot(&bytes).expect("a well-formed encode must always decode");

        assert_eq!(snap.uptime_ms, 1_500);
        assert_eq!(snap.snap_seq, 0, "0 must mean not-ready, exactly what an unset caller-supplied seq encodes as");
        assert!(!snap.link_connected);
        assert_eq!(snap.codec_word, "");
        assert_eq!(snap.kbps, 0);
        assert!(!snap.kbps_adaptive);
        assert!(!snap.kbps_is_live);
        assert_eq!(snap.device_name, "");
        assert_eq!(snap.fx_preset_name, "", "no connected device means no FX line, not the literal word Off");
        assert!(!snap.volume_present);
        assert!(!snap.level_present);
        assert!(snap.faults.iter().all(Option::is_none));
    }

    // --- Connected LDAC, adaptive, live kbps, FX assigned -------------

    #[test]
    fn encode_connected_ldac_adaptive_live_kbps_matches_home_projection_rules() {
        let mut model = BtModel::default();
        let mut presets = PresetStore::new();
        let preset_id = presets.create(Preset::new("Warm"));

        model.paired.push(PairedDevice { addr: addr(0xF2), name: "Test Headphones".to_string(), mru_seq: 1, ldac_quality: 4, preset_id });
        model.connected_addr = Some(addr(0xF2));
        model.connected_codec = Some(ConnectedCodec { addr: addr(0xF2), word: "LDAC".to_string(), nominal_bitrate_bps: 990_000 });
        model.ldac_live_kbps = Some(909);

        let now = Instant::from_micros(2_000_000);
        let bytes = encode_home_snapshot(&model, &presets, now, 42);
        let snap = decode_home_snapshot(&bytes).unwrap();

        assert!(snap.link_connected);
        assert_eq!(snap.codec_word, "LDAC");
        assert_eq!(snap.device_name, "Test Headphones");
        assert_eq!(snap.kbps, 909, "the LIVE figure, not the nominal 990 -- same rule project_hero follows");
        assert!(snap.kbps_is_live);
        assert!(snap.kbps_adaptive, "ldac_quality 4 (Adaptive) on the connected device");
        assert_eq!(snap.fx_preset_name, "Warm");
        assert_eq!(snap.snap_seq, 42);
    }

    #[test]
    fn encode_connected_non_ldac_never_reports_live_or_adaptive() {
        let mut model = BtModel::default();
        let presets = PresetStore::new();

        model.paired.push(PairedDevice { addr: addr(0x01), name: "SBC Buds".to_string(), mru_seq: 1, ldac_quality: 4, preset_id: 0 });
        model.connected_addr = Some(addr(0x01));
        model.connected_codec = Some(ConnectedCodec { addr: addr(0x01), word: "SBC".to_string(), nominal_bitrate_bps: 328_000 });
        // A live LDAC figure left over from a prior LDAC connection must
        // never leak onto a non-LDAC codec's kbps -- `ldac_live_kbps` is
        // documented as cleared on codec change, but this projection must
        // not depend on that alone.
        model.ldac_live_kbps = Some(909);

        let bytes = encode_home_snapshot(&model, &presets, Instant::from_micros(0), 1);
        let snap = decode_home_snapshot(&bytes).unwrap();

        assert_eq!(snap.codec_word, "SBC");
        assert_eq!(snap.kbps, 328, "nominal_bitrate_bps / 1000, never the stale LDAC live figure");
        assert!(!snap.kbps_is_live);
        assert!(!snap.kbps_adaptive, "ldac_quality Adaptive on a non-LDAC codec must never tag as adaptive");
    }

    #[test]
    fn encode_fx_line_off_encodes_as_empty_not_the_word_off() {
        let mut model = BtModel::default();
        let presets = PresetStore::new(); // preset_id 7 is dangling -- resolves to Off.

        model.paired.push(PairedDevice { addr: addr(0x02), name: "Dangling FX".to_string(), mru_seq: 1, ldac_quality: 0, preset_id: 7 });
        model.connected_addr = Some(addr(0x02));
        model.connected_codec = Some(ConnectedCodec { addr: addr(0x02), word: "AAC".to_string(), nominal_bitrate_bps: 256_000 });

        let bytes = encode_home_snapshot(&model, &presets, Instant::from_micros(0), 1);
        let snap = decode_home_snapshot(&bytes).unwrap();

        assert_eq!(snap.fx_preset_name, "", "a dangling preset id resolves to Off, encoded as empty, per design section 4");
    }

    // --- Volume ---------------------------------------------------------

    #[test]
    fn encode_volume_present_and_muted_round_trips_level_source_and_muted() {
        let mut model = BtModel::default();
        let presets = PresetStore::new();
        model.volume = Some(VolumeState { level: 64, muted: true, source: VolumeSource::Sink });

        let bytes = encode_home_snapshot(&model, &presets, Instant::from_micros(0), 1);
        let snap = decode_home_snapshot(&bytes).unwrap();

        assert!(snap.volume_present);
        assert_eq!(snap.volume_level, 64);
        assert!(snap.volume_muted);
        assert_eq!(snap.volume_source, VolumeSource::Sink);
    }

    #[test]
    fn encode_volume_absent_is_distinguishable_from_a_zero_reading() {
        let model_absent = BtModel::default();
        let model_zero = BtModel { volume: Some(VolumeState { level: 0, muted: false, source: VolumeSource::Host }), ..Default::default() };
        let presets = PresetStore::new();

        let absent = decode_home_snapshot(&encode_home_snapshot(&model_absent, &presets, Instant::from_micros(0), 1)).unwrap();
        let zero = decode_home_snapshot(&encode_home_snapshot(&model_zero, &presets, Instant::from_micros(0), 1)).unwrap();

        assert!(!absent.volume_present);
        assert!(zero.volume_present, "level 0 is a real reading, not absence -- must not collapse to the same wire shape as no reading at all");
        assert_eq!(zero.volume_level, 0);
    }

    // --- Levels -----------------------------------------------------------

    #[test]
    fn encode_out_level_round_trips_peaks_rms_and_received_ms() {
        let mut model = BtModel::default();
        let presets = PresetStore::new();
        let received_at = Instant::from_micros(3_250_000);
        model.out_level = Some(sample_out_level(received_at));

        let bytes = encode_home_snapshot(&model, &presets, Instant::from_micros(3_400_000), 1);
        let snap = decode_home_snapshot(&bytes).unwrap();

        assert!(snap.level_present);
        assert_eq!(snap.peak_l, 200);
        assert_eq!(snap.peak_r, 190);
        assert_eq!(snap.rms_l, 120);
        assert_eq!(snap.rms_r, 110);
        assert_eq!(snap.received_ms, 3_250, "same clock domain as uptime_ms -- host derives staleness by subtracting the two");
        assert_eq!(snap.uptime_ms, 3_400);
    }

    // --- Faults -----------------------------------------------------------

    #[test]
    fn encode_faults_round_trip_all_three_value_kinds_and_leave_unraised_keys_absent() {
        let mut model = BtModel::default();
        let presets = PresetStore::new();
        let t1 = Instant::from_micros(1_000_000);
        let t2 = Instant::from_micros(4_000_000);

        model.fault_log.record(FaultKey::BufStarved, t1, Some(FaultValue::Millis(0)), 3);
        model.fault_log.record(FaultKey::UsbSupplyLow, t2, Some(FaultValue::Ratio(200)), 1);
        model.fault_log.record(FaultKey::EncResync, t2, Some(FaultValue::Count(9)), 9);
        // BufOverflow/AirCongested/AirLinkLost are left never-raised.

        let bytes = encode_home_snapshot(&model, &presets, t2, 1);
        let snap = decode_home_snapshot(&bytes).unwrap();

        let starved = snap.faults[0].expect("BufStarved is index 0 in FaultKey::ALL");
        assert_eq!(starved.count, 3);
        assert_eq!(starved.first_seen_ms, 1_000);
        assert_eq!(starved.last_seen_ms, 1_000);
        assert_eq!(starved.value, Some(FaultValue::Millis(0)));

        assert!(snap.faults[1].is_none(), "BufOverflow was never raised");

        let usb_supply = snap.faults[2].expect("UsbSupplyLow is index 2");
        assert_eq!(usb_supply.value, Some(FaultValue::Ratio(200)));

        assert!(snap.faults[3].is_none(), "AirCongested was never raised");
        assert!(snap.faults[4].is_none(), "AirLinkLost was never raised");

        let enc_resync = snap.faults[5].expect("EncResync is index 5");
        assert_eq!(enc_resync.value, Some(FaultValue::Count(9)));
        assert_eq!(enc_resync.first_seen_ms, 4_000);
    }

    #[test]
    fn encode_fault_value_none_round_trips_as_none_not_a_zero_count() {
        let mut model = BtModel::default();
        let presets = PresetStore::new();
        let t1 = Instant::from_micros(5_000_000);
        model.fault_log.record(FaultKey::AirLinkLost, t1, None, 2);

        let bytes = encode_home_snapshot(&model, &presets, t1, 1);
        let snap = decode_home_snapshot(&bytes).unwrap();

        let entry = snap.faults[4].expect("AirLinkLost is index 4");
        assert_eq!(entry.count, 2, "a raised fault with no accompanying value still has a nonzero count");
        assert_eq!(entry.value, None);
    }

    // --- Decode robustness --------------------------------------------

    #[test]
    fn decode_rejects_a_too_short_buffer() {
        let bytes = [0u8; HOME_SNAPSHOT_LEN - 1];
        assert!(decode_home_snapshot(&bytes).is_none());
    }

    #[test]
    fn decode_ignores_trailing_bytes_past_home_snapshot_len() {
        let model = BtModel::default();
        let presets = PresetStore::new();
        let mut bytes: Vec<u8> = encode_home_snapshot(&model, &presets, Instant::from_micros(0), 7).to_vec();
        bytes.extend_from_slice(&[0xAA; 16]); // a future proto's appended fields.

        let snap = decode_home_snapshot(&bytes).expect("trailing bytes must not fail decode");
        assert_eq!(snap.snap_seq, 7);
    }

    #[test]
    fn decode_rejects_an_unknown_proto() {
        let model = BtModel::default();
        let presets = PresetStore::new();
        let mut bytes = encode_home_snapshot(&model, &presets, Instant::from_micros(0), 1);
        bytes[OFF_PROTO] = 99;
        assert!(decode_home_snapshot(&bytes).is_none());
    }

    #[test]
    fn decode_rejects_an_unknown_page() {
        let model = BtModel::default();
        let presets = PresetStore::new();
        let mut bytes = encode_home_snapshot(&model, &presets, Instant::from_micros(0), 1);
        bytes[OFF_PAGE] = 1; // A future F3 diagnostics page, not Home.
        assert!(decode_home_snapshot(&bytes).is_none());
    }

    #[test]
    fn decode_unknown_fault_value_kind_decodes_to_none_rather_than_panicking() {
        let mut model = BtModel::default();
        let presets = PresetStore::new();
        model.fault_log.record(FaultKey::BufOverflow, Instant::from_micros(0), Some(FaultValue::Count(1)), 1);
        let mut bytes = encode_home_snapshot(&model, &presets, Instant::from_micros(0), 1);

        let slot_off = OFF_FAULTS + FAULT_SLOT_LEN; // BufOverflow is index 1.
        bytes[slot_off + 10] = 0xFF; // an ordinal no proto has ever defined.

        let snap = decode_home_snapshot(&bytes).unwrap();
        assert_eq!(snap.faults[1].unwrap().value, None, "an unrecognised value_kind must decode to None, never be matched as a discriminant");
    }

    #[test]
    fn decode_unknown_volume_source_decodes_to_host_rather_than_panicking() {
        let mut model = BtModel::default();
        let presets = PresetStore::new();
        model.volume = Some(VolumeState { level: 50, muted: false, source: VolumeSource::Device });
        let mut bytes = encode_home_snapshot(&model, &presets, Instant::from_micros(0), 1);
        bytes[OFF_VOL_SOURCE] = 0xFF;

        let snap = decode_home_snapshot(&bytes).unwrap();
        assert_eq!(snap.volume_source, VolumeSource::Host);
    }

    // --- Layout stability (the "golden bytes" test) --------------------

    /// Locks the exact byte layout for a fully-populated snapshot. This is
    /// the regression guard this module's doc comment promises ("its doc
    /// comment plus the golden test IS the spec") -- a change to any
    /// offset, width, or field order below must show up as a diff here,
    /// not as a silent wire-format break a future host-side decoder would
    /// discover the hard way. If this test ever needs to change for a
    /// reason OTHER than a genuine proto bump, that is the signal
    /// something violated the append-only rule.
    #[test]
    fn golden_bytes_layout_is_stable() {
        let mut model = BtModel::default();
        let mut presets = PresetStore::new();
        let preset_id = presets.create(Preset::new("Warm"));

        model.paired.push(PairedDevice { addr: addr(0xF2), name: "Pixel Buds".to_string(), mru_seq: 3, ldac_quality: 4, preset_id });
        model.connected_addr = Some(addr(0xF2));
        model.connected_codec = Some(ConnectedCodec { addr: addr(0xF2), word: "LDAC".to_string(), nominal_bitrate_bps: 990_000 });
        model.ldac_live_kbps = Some(909);
        model.volume = Some(VolumeState { level: 100, muted: false, source: VolumeSource::Host });
        let received_at = Instant::from_micros(12_345_000);
        model.out_level = Some(sample_out_level(received_at));
        model.fault_log.record(FaultKey::BufStarved, Instant::from_micros(1_000_000), Some(FaultValue::Millis(0)), 3);

        let now = Instant::from_micros(20_000_000);
        let bytes = encode_home_snapshot(&model, &presets, now, 0xDEAD_BEEF);

        assert_eq!(bytes.len(), HOME_SNAPSHOT_LEN);
        assert_eq!(HOME_SNAPSHOT_LEN, 163, "the layout table in this module's doc comment states 163 -- a change here must update that table too");

        // Header.
        assert_eq!(bytes[0], 1, "proto");
        assert_eq!(bytes[1], 0, "page");
        assert_eq!(&bytes[2..4], &163u16.to_le_bytes(), "len");
        assert_eq!(&bytes[4..8], &20_000u32.to_le_bytes(), "uptime_ms");
        assert_eq!(&bytes[8..12], &0xDEAD_BEEFu32.to_le_bytes(), "snap_seq");

        // Link/codec.
        assert_eq!(bytes[12], 1, "link connected");
        assert_eq!(bytes[13], 0b0001_0111, "flags: adaptive | kbps_is_live | volume_present | level_present");
        assert_eq!(&bytes[14..16], &909u16.to_le_bytes(), "kbps (live, not nominal 990)");

        // Codec word "LDAC".
        assert_eq!(bytes[16], 4);
        assert_eq!(&bytes[17..21], b"LDAC");
        assert_eq!(&bytes[21..25], &[0, 0, 0, 0], "codec word zero-padded past its length");

        // Device name "Pixel Buds" (10 bytes).
        assert_eq!(bytes[25], 10);
        assert_eq!(&bytes[26..36], b"Pixel Buds");
        assert!(bytes[36..58].iter().all(|&b| b == 0), "device name zero-padded past its length");

        // FX preset name "Warm".
        assert_eq!(bytes[58], 4);
        assert_eq!(&bytes[59..63], b"Warm");
        assert!(bytes[63..75].iter().all(|&b| b == 0));

        // Volume: level 100, source Host (0), not muted.
        assert_eq!(bytes[75], 100);
        assert_eq!(bytes[76], 0);

        // Levels.
        assert_eq!(bytes[77], 200, "peak_l");
        assert_eq!(bytes[78], 190, "peak_r");
        assert_eq!(bytes[79], 120, "rms_l");
        assert_eq!(bytes[80], 110, "rms_r");
        assert_eq!(&bytes[81..85], &12_345u32.to_le_bytes(), "received_ms");

        // Faults: only BufStarved (slot 0) raised.
        let slot0 = &bytes[85..98];
        assert_eq!(&slot0[0..2], &3u16.to_le_bytes(), "BufStarved count");
        assert_eq!(&slot0[2..6], &1_000u32.to_le_bytes(), "BufStarved first_seen_ms");
        assert_eq!(&slot0[6..10], &1_000u32.to_le_bytes(), "BufStarved last_seen_ms");
        assert_eq!(slot0[10], 3, "value_kind millis");
        assert_eq!(&slot0[11..13], &0u16.to_le_bytes(), "value 0ms");

        for slot_idx in 1..6 {
            let off = OFF_FAULTS + slot_idx * FAULT_SLOT_LEN;
            assert!(bytes[off..off + FAULT_SLOT_LEN].iter().all(|&b| b == 0), "an unraised fault slot must be all-zero");
        }

        // Full round trip against the same fixture.
        let snap = decode_home_snapshot(&bytes).unwrap();
        assert_eq!(snap.snap_seq, 0xDEAD_BEEF);
        assert_eq!(snap.codec_word, "LDAC");
        assert_eq!(snap.device_name, "Pixel Buds");
        assert_eq!(snap.fx_preset_name, "Warm");
        assert_eq!(snap.kbps, 909);
        assert!(snap.kbps_adaptive);
        assert!(snap.kbps_is_live);
    }
}
