// GET_LIBRARY (0x05) snapshot decoder + the shared effect-blob (v1/v2)
// codec, per ADA DESIGN on pico-link-jyhk.17 (`.planning/design/
// 2026-09-27-iface6-eq-management-protocol.md` section 3) and its core
// implementation, `core/src/app/library.rs` + `core/src/dsp/preset.rs`
// (`Preset::to_wire`/`from_wire`). Same "core emits, JS asserts" discipline
// as `telemetry.ts`/`info.ts`: `library.fixture.test.ts` checks this
// decoder against every `fixtures/library/*.bin+json` pair `cargo test`
// generated at bead `pico-link-jyhk.18`.
//
// The effect blob (`BLOB_LEN` = 80 bytes) IS `Preset::to_wire`'s wire
// format, embedded verbatim in each library effect record (design section
// 3: "The blob IS the effect format on the wire"). This module owns
// decoding/encoding it because `ops.ts`'s `SAVE_EFFECT`/`PREVIEW` HOST_OP
// requests need to build the same 80 bytes the library record carries.

export const LIBRARY_PROTO = 1;

const HEADER_LEN = 14;
export const EFFECT_RECORD_LEN = 84;
export const DEVICE_RECORD_LEN = 42;
const DEVICE_NAME_CAP = 32;

const OFF_LIB_PROTO = 0;
const OFF_LEN = 2;
const OFF_LIBRARY_REV = 4;
const OFF_FLAGS = 6;
const OFF_EFFECT_COUNT = 7;
const OFF_EFFECT_REC_LEN = 8;
const OFF_DEVICE_COUNT = 9;
const OFF_DEVICE_REC_LEN = 10;
const OFF_MAX_EFFECTS = 11;
const OFF_MAX_DEVICES = 12;

const FLAG_PRESETS_READY = 1 << 0;
const DEVICE_FLAG_CONNECTED = 1 << 0;

// --- The effect blob: Preset::to_wire/from_wire (core/src/dsp/preset.rs) -

export const BLOB_LEN = 80;
export const MAX_BANDS = 10;
export const MAX_NAME_BYTES = 16;

const BLOB_VERSION_V1 = 1;
const BLOB_VERSION_V2 = 2;

const V1_BAND_RECORD_LEN = 5;
const V2_BAND_RECORD_LEN = 6;

// v1 layout: {version, name_len, name[16], crossfeed, band_count, band[10]}
const V1_NAME_LEN_OFF = 1;
const V1_NAME_OFF = 2;
const V1_CROSSFEED_OFF = V1_NAME_OFF + MAX_NAME_BYTES; // 18
const V1_BAND_COUNT_OFF = V1_CROSSFEED_OFF + 1; // 19
const V1_BANDS_OFF = V1_BAND_COUNT_OFF + 1; // 20

// v2 layout: {version, name[16], flags, preamp_cdb[2], band[10]}
const V2_NAME_OFF = 1;
const V2_FLAGS_OFF = V2_NAME_OFF + MAX_NAME_BYTES; // 17
const V2_PREAMP_OFF = V2_FLAGS_OFF + 1; // 18
const V2_BANDS_OFF = V2_PREAMP_OFF + 2; // 20

const FLAGS_CROSSFEED_MASK = 0b0000_0011;
const FLAGS_PREAMP_EXPLICIT_BIT = 0b0000_0100;
const FLAGS_EQ_LOCKED_BIT = 0b0000_1000;
const FLAGS_BAND_COUNT_SHIFT = 4;

const KIND_GAIN_GAIN_BITS = 13;
const KIND_GAIN_GAIN_MASK = (1 << KIND_GAIN_GAIN_BITS) - 1; // 0x1FFF
const KIND_GAIN_GAIN_SIGN_BIT = 1 << (KIND_GAIN_GAIN_BITS - 1); // 0x1000
const KIND_GAIN_GAIN_MAX = KIND_GAIN_GAIN_SIGN_BIT - 1; // 4095
const KIND_GAIN_GAIN_MIN = -KIND_GAIN_GAIN_MAX; // -4095

export type BandKind = "peak" | "lowShelf" | "highShelf";

function bandKindToWire(kind: BandKind): number {
  switch (kind) {
    case "lowShelf":
      return 1;
    case "highShelf":
      return 2;
    default:
      return 0;
  }
}

/** Any wire value other than `{0, 1, 2}` falls back to `"peak"` (mirrors `BandKind::from_wire`). */
function bandKindFromWire(value: number): BandKind {
  switch (value & 0x7) {
    case 1:
      return "lowShelf";
    case 2:
      return "highShelf";
    default:
      return "peak";
  }
}

export interface Band {
  kind: BandKind;
  /** 0.5Hz steps. */
  freqHalfHz: number;
  /** 0.01dB steps (centi-dB). */
  gainCdb: number;
  /** 0.001 steps (milli-Q). */
  qMilli: number;
}

export type CrossfeedLevel = "off" | "weak" | "medium" | "strong";

function crossfeedToWire(level: CrossfeedLevel): number {
  switch (level) {
    case "weak":
      return 1;
    case "medium":
      return 2;
    case "strong":
      return 3;
    default:
      return 0;
  }
}

/** Any wire value other than `{0, 1, 2, 3}` falls back to `"off"` (mirrors `CrossfeedLevel::from_wire`). */
function crossfeedFromWire(value: number): CrossfeedLevel {
  switch (value & FLAGS_CROSSFEED_MASK) {
    case 1:
      return "weak";
    case 2:
      return "medium";
    case 3:
      return "strong";
    default:
      return "off";
  }
}

export type Preamp = { kind: "auto" } | { kind: "explicit"; cdb: number };

export interface Preset {
  name: string;
  crossfeed: CrossfeedLevel;
  bands: Band[];
  preamp: Preamp;
  eqLocked: boolean;
}

const utf8Decoder = new TextDecoder("utf-8");
const utf8Encoder = new TextEncoder();

function packKindGain(kind: BandKind, gainCdb: number): number {
  const clamped = Math.max(KIND_GAIN_GAIN_MIN, Math.min(KIND_GAIN_GAIN_MAX, gainCdb));
  const gainBits = clamped & KIND_GAIN_GAIN_MASK;
  const kindBits = bandKindToWire(kind) << KIND_GAIN_GAIN_BITS;
  return (kindBits | gainBits) & 0xffff;
}

function unpackKindGain(raw: number): { kind: BandKind; gainCdb: number } {
  const kind = bandKindFromWire((raw >> KIND_GAIN_GAIN_BITS) & 0x7);
  const gainBits = raw & KIND_GAIN_GAIN_MASK;
  const gainCdb = gainBits & KIND_GAIN_GAIN_SIGN_BIT ? gainBits - (1 << KIND_GAIN_GAIN_BITS) : gainBits;
  return { kind, gainCdb };
}

function bandFromWireV2(view: DataView, off: number): Band {
  const kindGain = view.getUint16(off, true);
  const { kind, gainCdb } = unpackKindGain(kindGain);
  const freqHalfHz = view.getUint16(off + 2, true);
  const qMilli = view.getUint16(off + 4, true);
  return { kind, freqHalfHz, gainCdb, qMilli };
}

function bandToWireV2(view: DataView, off: number, band: Band): void {
  view.setUint16(off, packKindGain(band.kind, band.gainCdb), true);
  view.setUint16(off + 2, band.freqHalfHz, true);
  view.setUint16(off + 4, band.qMilli, true);
}

/** Widens a v1 band record to v2's exact representation (mirrors `Band::from_wire_v1`: exact scale, `freq*2`, `gain_half_db*50`, `Q_TABLE[q_idx]*1000`). */
const Q_TABLE_MILLI = [400, 600, 710, 1_000, 1_400, 2_000, 3_200, 8_000];

function qMilliFromIndex(qIdx: number): number {
  return Q_TABLE_MILLI[Math.min(qIdx, Q_TABLE_MILLI.length - 1)];
}

function bandFromWireV1(view: DataView, off: number): Band {
  const kind = bandKindFromWire(view.getUint8(off));
  const freqHz = view.getUint16(off + 1, true);
  const gainHalfDb = view.getInt8(off + 3);
  const qIdx = view.getUint8(off + 4);
  return { kind, freqHalfHz: freqHz * 2, gainCdb: gainHalfDb * 50, qMilli: qMilliFromIndex(qIdx) };
}

function readName(u8: Uint8Array, nameOff: number): string {
  const nameBuf = u8.subarray(nameOff, nameOff + MAX_NAME_BYTES);
  let nameLen = nameBuf.indexOf(0);
  if (nameLen < 0) nameLen = MAX_NAME_BYTES;
  return utf8Decoder.decode(nameBuf.subarray(0, nameLen));
}

/**
 * Decodes an 80-byte effect blob (`Preset::from_wire`): dispatches on the
 * version byte -- `1` widens through `bandFromWireV1`, `2` reads v2
 * directly, anything else decodes to an empty, LOCKED preset (keeping only
 * the name bytes at v2's name offset), same per-field-fallback discipline
 * as core's `from_wire_unknown`. Never throws on a short/malformed buffer:
 * out-of-range reads are treated as `0` the way `raw.get(i).unwrap_or(0)`
 * does in Rust, by padding to `BLOB_LEN` first.
 */
export function decodePresetBlob(bytes: Uint8Array): Preset {
  const padded = new Uint8Array(BLOB_LEN);
  padded.set(bytes.subarray(0, Math.min(bytes.length, BLOB_LEN)));
  const view = new DataView(padded.buffer, padded.byteOffset, padded.byteLength);
  const version = padded[0];

  if (version === BLOB_VERSION_V1) {
    const nameLen = Math.min(padded[V1_NAME_LEN_OFF], MAX_NAME_BYTES);
    const name = utf8Decoder.decode(padded.subarray(V1_NAME_OFF, V1_NAME_OFF + nameLen));
    const crossfeed = crossfeedFromWire(padded[V1_CROSSFEED_OFF]);
    const bandCount = Math.min(padded[V1_BAND_COUNT_OFF], MAX_BANDS);
    const bands: Band[] = [];
    for (let i = 0; i < bandCount; i++) bands.push(bandFromWireV1(view, V1_BANDS_OFF + i * V1_BAND_RECORD_LEN));
    return { name, crossfeed, bands, preamp: { kind: "auto" }, eqLocked: false };
  }

  if (version === BLOB_VERSION_V2) {
    const name = readName(padded, V2_NAME_OFF);
    const flags = padded[V2_FLAGS_OFF];
    const crossfeed = crossfeedFromWire(flags);
    const preampExplicit = (flags & FLAGS_PREAMP_EXPLICIT_BIT) !== 0;
    const eqLocked = (flags & FLAGS_EQ_LOCKED_BIT) !== 0;
    const bandCount = Math.min((flags >> FLAGS_BAND_COUNT_SHIFT) & 0xf, MAX_BANDS);
    const preampCdb = view.getInt16(V2_PREAMP_OFF, true);
    const preamp: Preamp = preampExplicit ? { kind: "explicit", cdb: preampCdb } : { kind: "auto" };
    const bands: Band[] = [];
    for (let i = 0; i < bandCount; i++) bands.push(bandFromWireV2(view, V2_BANDS_OFF + i * V2_BAND_RECORD_LEN));
    return { name, crossfeed, bands, preamp, eqLocked };
  }

  // Unknown version: empty, locked, name kept at v2's offset.
  const name = readName(padded, V2_NAME_OFF);
  return { name, crossfeed: "off", bands: [], preamp: { kind: "auto" }, eqLocked: true };
}

/**
 * Encodes a preset into the exact `BLOB_LEN`-byte v2 wire format (mirrors
 * `Preset::to_wire` -- writes always emit v2, never v1). Used by `ops.ts`
 * to build `SAVE_EFFECT`/`PREVIEW` HOST_OP request bodies.
 */
export function encodePresetBlob(preset: Preset): Uint8Array {
  const out = new Uint8Array(BLOB_LEN);
  const view = new DataView(out.buffer);
  out[0] = BLOB_VERSION_V2;

  const nameBytes = utf8Encoder.encode(preset.name).slice(0, MAX_NAME_BYTES);
  out.set(nameBytes, V2_NAME_OFF);

  const bandCount = Math.min(preset.bands.length, MAX_BANDS);
  let flags = crossfeedToWire(preset.crossfeed) & FLAGS_CROSSFEED_MASK;
  if (preset.preamp.kind === "explicit") flags |= FLAGS_PREAMP_EXPLICIT_BIT;
  if (preset.eqLocked) flags |= FLAGS_EQ_LOCKED_BIT;
  flags |= (bandCount << FLAGS_BAND_COUNT_SHIFT) & 0xff;
  out[V2_FLAGS_OFF] = flags;

  const preampCdb = preset.preamp.kind === "explicit" ? preset.preamp.cdb : 0;
  view.setInt16(V2_PREAMP_OFF, preampCdb, true);

  for (let i = 0; i < bandCount; i++) bandToWireV2(view, V2_BANDS_OFF + i * V2_BAND_RECORD_LEN, preset.bands[i]);

  return out;
}

// --- GET_LIBRARY snapshot ------------------------------------------------

export interface LibraryEffect {
  id: number;
  persistedSeq: number;
  preset: Preset;
}

export interface LibraryDevice {
  /** `"94:DB:56:54:7C:F2"` -- colon-separated uppercase hex, matching the fixture JSON's `addr` string. */
  addr: string;
  /** `0` means Off. */
  presetId: number;
  connected: boolean;
  name: string;
}

export interface LibrarySnapshot {
  libraryRev: number;
  presetsReady: boolean;
  maxEffects: number;
  maxDevices: number;
  effects: LibraryEffect[];
  devices: LibraryDevice[];
}

function formatAddr(bytes: Uint8Array): string {
  return Array.from(bytes)
    .map((b) => b.toString(16).toUpperCase().padStart(2, "0"))
    .join(":");
}

function parseAddr(addr: string): Uint8Array {
  return new Uint8Array(addr.split(":").map((h) => parseInt(h, 16)));
}

/**
 * Decodes a `GET_LIBRARY` (0x05) payload (mirrors
 * `core::app::library::decode_library_snapshot`). Returns `null` if
 * `bytes` is too short to hold its own declared header, `lib_proto`
 * doesn't match, the record lengths don't match this decoder's
 * `EFFECT_RECORD_LEN`/`DEVICE_RECORD_LEN` (a newer proto's wider records
 * are only skippable, not parseable, by this version), or the declared
 * counts don't fit inside `bytes` -- same "on an unknown/malformed
 * payload, the host bails" discipline as `decodeHomeSnapshot`.
 */
export function decodeLibrarySnapshot(bytes: ArrayBuffer | Uint8Array): LibrarySnapshot | null {
  const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  if (u8.byteLength < HEADER_LEN) return null;
  const view = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);

  if (view.getUint8(OFF_LIB_PROTO) !== LIBRARY_PROTO) return null;

  const libraryRev = view.getUint16(OFF_LIBRARY_REV, true);
  const flags = view.getUint8(OFF_FLAGS);
  const effectCount = view.getUint8(OFF_EFFECT_COUNT);
  const effectRecLen = view.getUint8(OFF_EFFECT_REC_LEN);
  const deviceCount = view.getUint8(OFF_DEVICE_COUNT);
  const deviceRecLen = view.getUint8(OFF_DEVICE_REC_LEN);
  const maxEffects = view.getUint8(OFF_MAX_EFFECTS);
  const maxDevices = view.getUint8(OFF_MAX_DEVICES);

  if (effectRecLen !== EFFECT_RECORD_LEN || deviceRecLen !== DEVICE_RECORD_LEN) return null;

  const effectsEnd = HEADER_LEN + effectCount * effectRecLen;
  const devicesEnd = effectsEnd + deviceCount * deviceRecLen;
  if (u8.byteLength < devicesEnd) return null;

  const effects: LibraryEffect[] = [];
  let off = HEADER_LEN;
  for (let i = 0; i < effectCount; i++) {
    const id = view.getUint16(off, true);
    const persistedSeq = view.getUint16(off + 2, true);
    const preset = decodePresetBlob(u8.subarray(off + 4, off + 4 + BLOB_LEN));
    effects.push({ id, persistedSeq, preset });
    off += effectRecLen;
  }

  const devices: LibraryDevice[] = [];
  for (let i = 0; i < deviceCount; i++) {
    const addr = formatAddr(u8.subarray(off, off + 6));
    const presetId = view.getUint16(off + 6, true);
    const deviceFlags = view.getUint8(off + 8);
    const name = readFixedDeviceName(u8, off + 9, off + 10);
    devices.push({ addr, presetId, connected: (deviceFlags & DEVICE_FLAG_CONNECTED) !== 0, name });
    off += deviceRecLen;
  }

  return { libraryRev, presetsReady: (flags & FLAG_PRESETS_READY) !== 0, maxEffects, maxDevices, effects, devices };
}

function readFixedDeviceName(u8: Uint8Array, lenOff: number, bytesOff: number): string {
  const len = Math.min(u8[lenOff], DEVICE_NAME_CAP);
  return utf8Decoder.decode(u8.subarray(bytesOff, bytesOff + len));
}

/** Test/`FakeTransport` helper: the inverse of `decodeLibrarySnapshot`. */
export function encodeLibrarySnapshotForTest(input: LibrarySnapshot): Uint8Array {
  const len = HEADER_LEN + input.effects.length * EFFECT_RECORD_LEN + input.devices.length * DEVICE_RECORD_LEN;
  const u8 = new Uint8Array(len);
  const view = new DataView(u8.buffer);

  view.setUint8(OFF_LIB_PROTO, LIBRARY_PROTO);
  view.setUint16(OFF_LEN, len, true);
  view.setUint16(OFF_LIBRARY_REV, input.libraryRev, true);
  view.setUint8(OFF_FLAGS, input.presetsReady ? FLAG_PRESETS_READY : 0);
  view.setUint8(OFF_EFFECT_COUNT, input.effects.length);
  view.setUint8(OFF_EFFECT_REC_LEN, EFFECT_RECORD_LEN);
  view.setUint8(OFF_DEVICE_COUNT, input.devices.length);
  view.setUint8(OFF_DEVICE_REC_LEN, DEVICE_RECORD_LEN);
  view.setUint8(OFF_MAX_EFFECTS, input.maxEffects);
  view.setUint8(OFF_MAX_DEVICES, input.maxDevices);

  let off = HEADER_LEN;
  for (const effect of input.effects) {
    view.setUint16(off, effect.id, true);
    view.setUint16(off + 2, effect.persistedSeq, true);
    u8.set(encodePresetBlob(effect.preset), off + 4);
    off += EFFECT_RECORD_LEN;
  }

  for (const device of input.devices) {
    u8.set(parseAddr(device.addr), off);
    view.setUint16(off + 6, device.presetId, true);
    view.setUint8(off + 8, device.connected ? DEVICE_FLAG_CONNECTED : 0);
    const nameBytes = utf8Encoder.encode(device.name).slice(0, DEVICE_NAME_CAP);
    u8[off + 9] = nameBytes.length;
    u8.set(nameBytes, off + 10);
    off += DEVICE_RECORD_LEN;
  }

  return u8;
}

/** An empty library at the given `libraryRev` -- `FakeTransport`'s and tests' starting point. */
export function emptyLibrarySnapshot(libraryRev = 0): LibrarySnapshot {
  return { libraryRev, presetsReady: false, maxEffects: 8, maxDevices: 8, effects: [], devices: [] };
}
