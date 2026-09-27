// Page-0 Home telemetry snapshot decoder. Mirrors the wire layout owned by
// `core/src/app/telemetry.rs` (its module doc comment has the authoritative
// byte table; `encode_home_snapshot`/`decode_home_snapshot` are the Rust
// side of this same contract, and `FaultKey::ALL` in
// `core/src/app/fault.rs` fixes the six fault slots' order). This is a
// hand-ported TS mirror per FERN DESIGN section 5 ("core emits, JS
// asserts") -- fixture-backed round-trip tests belong on jyhk.9/.11; this
// file is the typed decoder those tests exercise.
//
// Requested over `iface 6`'s CLASS `GET_TELEMETRY` (`PL_CFG_REQ_GET_TELEMETRY
// = 0x03`, `firmware/src/usb_config_itf.h`), `wValue` = page id. Only page 0
// ("Home") exists today.

export const TELEMETRY_PROTO = 1;
export const TELEMETRY_PAGE_HOME = 0;
export const HOME_SNAPSHOT_LEN = 163;

const CODEC_WORD_CAP = 8;
const DEVICE_NAME_CAP = 32;
const FX_NAME_CAP = 16;
const FAULT_SLOT_COUNT = 6;
const FAULT_SLOT_LEN = 13;

const OFF_PROTO = 0;
const OFF_PAGE = 1;
const OFF_LEN = 2;
const OFF_UPTIME_MS = 4;
const OFF_SNAP_SEQ = 8;
const OFF_LINK = 12;
const OFF_FLAGS = 13;
const OFF_KBPS = 14;
const OFF_CODEC_LEN = 16;
const OFF_CODEC_BYTES = OFF_CODEC_LEN + 1;
const OFF_NAME_LEN = OFF_CODEC_BYTES + CODEC_WORD_CAP;
const OFF_NAME_BYTES = OFF_NAME_LEN + 1;
const OFF_FX_LEN = OFF_NAME_BYTES + DEVICE_NAME_CAP;
const OFF_FX_BYTES = OFF_FX_LEN + 1;
const OFF_VOL_LEVEL = OFF_FX_BYTES + FX_NAME_CAP;
const OFF_VOL_SOURCE = OFF_VOL_LEVEL + 1;
const OFF_PEAK_L = OFF_VOL_SOURCE + 1;
const OFF_PEAK_R = OFF_PEAK_L + 1;
const OFF_RMS_L = OFF_PEAK_R + 1;
const OFF_RMS_R = OFF_RMS_L + 1;
const OFF_RECEIVED_MS = OFF_RMS_R + 1;
const OFF_FAULTS = OFF_RECEIVED_MS + 4;

const FLAG_ADAPTIVE = 1 << 0;
const FLAG_KBPS_IS_LIVE = 1 << 1;
const FLAG_VOLUME_PRESENT = 1 << 2;
const FLAG_MUTED = 1 << 3;
const FLAG_LEVEL_PRESENT = 1 << 4;

const VALUE_KIND_NONE = 0;
const VALUE_KIND_RATIO = 1;
const VALUE_KIND_COUNT = 2;
const VALUE_KIND_MILLIS = 3;

/** `FaultKey::ALL` order (`core/src/app/fault.rs`). Append-only ordinals. */
export const FAULT_KEYS = ["buf_starved", "buf_overflow", "usb_supply_low", "air_congested", "air_link_lost", "enc_resync"] as const;

export type FaultKey = (typeof FAULT_KEYS)[number];

export type FaultValue = { kind: "ratio"; value: number } | { kind: "count"; value: number } | { kind: "millis"; value: number } | { kind: "none" };

function decodeFaultValue(kind: number, value: number): FaultValue {
  switch (kind) {
    case VALUE_KIND_RATIO:
      return { kind: "ratio", value };
    case VALUE_KIND_COUNT:
      return { kind: "count", value };
    case VALUE_KIND_MILLIS:
      return { kind: "millis", value };
    default:
      // Unknown kind (VALUE_KIND_NONE or a future proto's ordinal) decodes
      // to "no value" -- never matched as a discriminant. Mirrors
      // `value_from_kind` in telemetry.rs.
      return { kind: "none" };
  }
}

export interface DecodedFault {
  count: number;
  firstSeenMs: number;
  lastSeenMs: number;
  value: FaultValue;
}

export type VolumeSource = "host" | "sink" | "device";

function decodeVolumeSource(source: number): VolumeSource {
  switch (source) {
    case 1:
      return "sink";
    case 2:
      return "device";
    default:
      return "host";
  }
}

export interface HomeSnapshot {
  uptimeMs: number;
  /** `0` means "not ready" (design section 3, flow step (c)). */
  snapSeq: number;
  linkConnected: boolean;
  codecWord: string;
  kbps: number;
  kbpsAdaptive: boolean;
  kbpsIsLive: boolean;
  deviceName: string;
  /** Empty means Off. */
  fxPresetName: string;
  volumeLevel: number;
  volumeMuted: boolean;
  volumeSource: VolumeSource;
  volumePresent: boolean;
  peakL: number;
  peakR: number;
  rmsL: number;
  rmsR: number;
  receivedMs: number;
  levelPresent: boolean;
  /** `FAULT_KEYS` order, `null` for a key never raised. */
  faults: Record<FaultKey, DecodedFault | null>;
}

const utf8Decoder = new TextDecoder("utf-8");

function readFixedStr(view: DataView, lenOff: number, bytesOff: number, cap: number): string {
  const len = Math.min(view.getUint8(lenOff), cap);
  const bytes = new Uint8Array(view.buffer, view.byteOffset + bytesOff, len);
  return utf8Decoder.decode(bytes);
}

/**
 * Decodes a page-0 payload `encode_home_snapshot` (core/src/app/
 * telemetry.rs) produced. Returns `null` if `bytes` is too short, or if
 * `proto`/`page` don't match -- the design's "on an unknown proto, show
 * update-needed and stop polling" rule starts here.
 */
export function decodeHomeSnapshot(bytes: ArrayBuffer | Uint8Array): HomeSnapshot | null {
  const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  if (u8.byteLength < HOME_SNAPSHOT_LEN) {
    return null;
  }
  const view = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);

  if (view.getUint8(OFF_PROTO) !== TELEMETRY_PROTO || view.getUint8(OFF_PAGE) !== TELEMETRY_PAGE_HOME) {
    return null;
  }

  const uptimeMs = view.getUint32(OFF_UPTIME_MS, true);
  const snapSeq = view.getUint32(OFF_SNAP_SEQ, true);
  const linkConnected = view.getUint8(OFF_LINK) !== 0;
  const flags = view.getUint8(OFF_FLAGS);
  const kbps = view.getUint16(OFF_KBPS, true);
  const codecWord = readFixedStr(view, OFF_CODEC_LEN, OFF_CODEC_BYTES, CODEC_WORD_CAP);
  const deviceName = readFixedStr(view, OFF_NAME_LEN, OFF_NAME_BYTES, DEVICE_NAME_CAP);
  const fxPresetName = readFixedStr(view, OFF_FX_LEN, OFF_FX_BYTES, FX_NAME_CAP);
  const volumeLevel = view.getUint8(OFF_VOL_LEVEL);
  const volumeSource = decodeVolumeSource(view.getUint8(OFF_VOL_SOURCE));
  const peakL = view.getUint8(OFF_PEAK_L);
  const peakR = view.getUint8(OFF_PEAK_R);
  const rmsL = view.getUint8(OFF_RMS_L);
  const rmsR = view.getUint8(OFF_RMS_R);
  const receivedMs = view.getUint32(OFF_RECEIVED_MS, true);

  const faults = {} as Record<FaultKey, DecodedFault | null>;
  for (let i = 0; i < FAULT_SLOT_COUNT; i++) {
    const slotOff = OFF_FAULTS + i * FAULT_SLOT_LEN;
    const count = view.getUint16(slotOff, true);
    const key = FAULT_KEYS[i];
    if (count === 0) {
      faults[key] = null;
      continue;
    }
    const firstSeenMs = view.getUint32(slotOff + 2, true);
    const lastSeenMs = view.getUint32(slotOff + 6, true);
    const kind = view.getUint8(slotOff + 10);
    const value = view.getUint16(slotOff + 11, true);
    faults[key] = { count, firstSeenMs, lastSeenMs, value: decodeFaultValue(kind, value) };
  }

  return {
    uptimeMs,
    snapSeq,
    linkConnected,
    codecWord,
    kbps,
    kbpsAdaptive: (flags & FLAG_ADAPTIVE) !== 0,
    kbpsIsLive: (flags & FLAG_KBPS_IS_LIVE) !== 0,
    deviceName,
    fxPresetName,
    volumeLevel,
    volumeMuted: (flags & FLAG_MUTED) !== 0,
    volumeSource,
    volumePresent: (flags & FLAG_VOLUME_PRESENT) !== 0,
    peakL,
    peakR,
    rmsL,
    rmsR,
    receivedMs,
    levelPresent: (flags & FLAG_LEVEL_PRESENT) !== 0,
    faults,
  };
}

function writeFixedStr(u8: Uint8Array, view: DataView, lenOff: number, bytesOff: number, cap: number, s: string) {
  const encoded = new TextEncoder().encode(s).slice(0, cap);
  view.setUint8(lenOff, encoded.length);
  u8.set(encoded, bytesOff);
  u8.fill(0, bytesOff + encoded.length, bytesOff + cap);
}

/**
 * Test/fixture helper: encodes a snapshot's fields into the same wire
 * layout `decodeHomeSnapshot` reads, for round-trip tests and for
 * `FakeTransport`'s synthesized replies. Not a port of `encode_
 * home_snapshot` (that stays Rust-only, reading `BtModel`/`PresetStore`) --
 * this takes already-decoded fields, the inverse of `decodeHomeSnapshot`.
 */
export function encodeHomeSnapshotForTest(input: HomeSnapshot): Uint8Array {
  const u8 = new Uint8Array(HOME_SNAPSHOT_LEN);
  const view = new DataView(u8.buffer);

  view.setUint8(OFF_PROTO, TELEMETRY_PROTO);
  view.setUint8(OFF_PAGE, TELEMETRY_PAGE_HOME);
  view.setUint16(OFF_LEN, HOME_SNAPSHOT_LEN, true);
  view.setUint32(OFF_UPTIME_MS, input.uptimeMs, true);
  view.setUint32(OFF_SNAP_SEQ, input.snapSeq, true);
  view.setUint8(OFF_LINK, input.linkConnected ? 1 : 0);

  let flags = 0;
  if (input.kbpsAdaptive) flags |= FLAG_ADAPTIVE;
  if (input.kbpsIsLive) flags |= FLAG_KBPS_IS_LIVE;
  if (input.volumePresent) flags |= FLAG_VOLUME_PRESENT;
  if (input.volumeMuted) flags |= FLAG_MUTED;
  if (input.levelPresent) flags |= FLAG_LEVEL_PRESENT;
  view.setUint8(OFF_FLAGS, flags);

  view.setUint16(OFF_KBPS, input.kbps, true);
  writeFixedStr(u8, view, OFF_CODEC_LEN, OFF_CODEC_BYTES, CODEC_WORD_CAP, input.codecWord);
  writeFixedStr(u8, view, OFF_NAME_LEN, OFF_NAME_BYTES, DEVICE_NAME_CAP, input.deviceName);
  writeFixedStr(u8, view, OFF_FX_LEN, OFF_FX_BYTES, FX_NAME_CAP, input.fxPresetName);
  view.setUint8(OFF_VOL_LEVEL, input.volumeLevel);
  view.setUint8(OFF_VOL_SOURCE, input.volumeSource === "sink" ? 1 : input.volumeSource === "device" ? 2 : 0);
  view.setUint8(OFF_PEAK_L, input.peakL);
  view.setUint8(OFF_PEAK_R, input.peakR);
  view.setUint8(OFF_RMS_L, input.rmsL);
  view.setUint8(OFF_RMS_R, input.rmsR);
  view.setUint32(OFF_RECEIVED_MS, input.receivedMs, true);

  for (let i = 0; i < FAULT_SLOT_COUNT; i++) {
    const slotOff = OFF_FAULTS + i * FAULT_SLOT_LEN;
    const entry = input.faults[FAULT_KEYS[i]];
    if (!entry) continue;
    view.setUint16(slotOff, entry.count, true);
    view.setUint32(slotOff + 2, entry.firstSeenMs, true);
    view.setUint32(slotOff + 6, entry.lastSeenMs, true);
    const kind = entry.value.kind === "ratio" ? VALUE_KIND_RATIO : entry.value.kind === "count" ? VALUE_KIND_COUNT : entry.value.kind === "millis" ? VALUE_KIND_MILLIS : VALUE_KIND_NONE;
    view.setUint8(slotOff + 10, kind);
    view.setUint16(slotOff + 11, entry.value.kind === "none" ? 0 : entry.value.value, true);
  }

  return u8;
}

/** A snapshot with every field absent/zeroed -- "not ready" (`snapSeq === 0`). */
export function emptyHomeSnapshot(): HomeSnapshot {
  const faults = {} as Record<FaultKey, DecodedFault | null>;
  for (const key of FAULT_KEYS) faults[key] = null;
  return {
    uptimeMs: 0,
    snapSeq: 0,
    linkConnected: false,
    codecWord: "",
    kbps: 0,
    kbpsAdaptive: false,
    kbpsIsLive: false,
    deviceName: "",
    fxPresetName: "",
    volumeLevel: 0,
    volumeMuted: false,
    volumeSource: "host",
    volumePresent: false,
    peakL: 0,
    peakR: 0,
    rmsL: 0,
    rmsR: 0,
    receivedMs: 0,
    levelPresent: false,
    faults,
  };
}
