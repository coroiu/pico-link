// HOST_OP (0x06) request encoders + GET_OP_STATUS (0x07) reply decoder, per
// ADA DESIGN on pico-link-jyhk.17 (`.planning/design/
// 2026-09-27-iface6-eq-management-protocol.md` section 4) and core's write
// side (bead pico-link-jyhk.19, `core/src/app/host_op.rs`). Every layout
// and ordinal below is cross-checked against `fixtures/host_op/*` by
// `ops.fixture.test.ts` -- `OpError`'s values in particular are core's,
// emitted verbatim into `fixtures/host_op/op-errors.json`
// (`core/src/app/host_op_fixtures.rs`); do not hand-edit them without
// re-running that check.
import { BLOB_LEN } from "./library";

export const OP_PROTO = 1;

export const HOST_OP_SAVE_EFFECT = 1;
export const HOST_OP_DELETE_EFFECT = 2;
export const HOST_OP_ASSIGN = 3;
export const HOST_OP_PREVIEW = 4;
export const HOST_OP_PREVIEW_END = 5;
export const HOST_OP_PARSE_APO = 6;

const FLAG_PREVIEW_BYPASS = 1 << 0;

const REQ_HEADER_LEN = 4; // op_proto, op, seq, flags

function parseAddr(addr: string): Uint8Array {
  return new Uint8Array(addr.split(":").map((h) => parseInt(h, 16)));
}

const utf8Encoder = new TextEncoder();
const utf8Decoder = new TextDecoder("utf-8");

function writeReqHeader(out: Uint8Array, op: number, seq: number, flags: number): void {
  out[0] = OP_PROTO;
  out[1] = op;
  out[2] = seq;
  out[3] = flags;
}

/** `id: 0` means create. `blob` must be exactly `BLOB_LEN` (80) bytes -- see `encodePresetBlob` in `library.ts`. */
export function encodeSaveEffectRequest(seq: number, id: number, baseSeq: number, blob: Uint8Array): Uint8Array {
  if (blob.length !== BLOB_LEN) throw new RangeError(`SAVE_EFFECT blob must be ${BLOB_LEN} bytes, got ${blob.length}`);
  const out = new Uint8Array(REQ_HEADER_LEN + 4 + BLOB_LEN);
  const view = new DataView(out.buffer);
  writeReqHeader(out, HOST_OP_SAVE_EFFECT, seq, 0);
  view.setUint16(4, id, true);
  view.setUint16(6, baseSeq, true);
  out.set(blob, 8);
  return out;
}

export function encodeDeleteEffectRequest(seq: number, id: number, baseSeq: number): Uint8Array {
  const out = new Uint8Array(REQ_HEADER_LEN + 4);
  const view = new DataView(out.buffer);
  writeReqHeader(out, HOST_OP_DELETE_EFFECT, seq, 0);
  view.setUint16(4, id, true);
  view.setUint16(6, baseSeq, true);
  return out;
}

/** `effectId: 0` means Off. */
export function encodeAssignRequest(seq: number, addr: string, effectId: number): Uint8Array {
  const out = new Uint8Array(REQ_HEADER_LEN + 8);
  const view = new DataView(out.buffer);
  writeReqHeader(out, HOST_OP_ASSIGN, seq, 0);
  out.set(parseAddr(addr), 4);
  view.setUint16(10, effectId, true);
  return out;
}

/** `effectId: 0` means an unsaved draft. `blob` must be exactly `BLOB_LEN` bytes. */
export function encodePreviewRequest(seq: number, effectId: number, blob: Uint8Array, bypass: boolean): Uint8Array {
  if (blob.length !== BLOB_LEN) throw new RangeError(`PREVIEW blob must be ${BLOB_LEN} bytes, got ${blob.length}`);
  const out = new Uint8Array(REQ_HEADER_LEN + 2 + BLOB_LEN);
  const view = new DataView(out.buffer);
  writeReqHeader(out, HOST_OP_PREVIEW, seq, bypass ? FLAG_PREVIEW_BYPASS : 0);
  view.setUint16(4, effectId, true);
  out.set(blob, 6);
  return out;
}

export function encodePreviewEndRequest(seq: number): Uint8Array {
  const out = new Uint8Array(REQ_HEADER_LEN);
  writeReqHeader(out, HOST_OP_PREVIEW_END, seq, 0);
  return out;
}

/** `name` and `apoText` are both encoded as UTF-8; total request size is `5 + name.length + apoText.length` in bytes -- see design section 5's ~1000 B practical limit on `apoText`. */
export function encodeParseApoRequest(seq: number, name: string, apoText: string): Uint8Array {
  const nameBytes = utf8Encoder.encode(name);
  const textBytes = utf8Encoder.encode(apoText);
  if (nameBytes.length > 255) throw new RangeError("PARSE_APO name must fit a u8 length prefix");
  const out = new Uint8Array(REQ_HEADER_LEN + 1 + nameBytes.length + textBytes.length);
  writeReqHeader(out, HOST_OP_PARSE_APO, seq, 0);
  out[4] = nameBytes.length;
  out.set(nameBytes, 5);
  out.set(textBytes, 5 + nameBytes.length);
  return out;
}

// --- GET_OP_STATUS (0x07) -------------------------------------------------

export type OpStatusState = "none" | "done" | "rejected";

/**
 * `core::app::host_op::OpError`'s ordinals verbatim -- design section 4:
 * "Error enum lives in core and is emitted to fixtures/ as constants".
 * Cross-checked byte-for-byte against `fixtures/host_op/op-errors.json`
 * (`core/src/app/host_op_fixtures.rs`) by `ops.fixture.test.ts`; a
 * renumbering in core fails that test rather than silently drifting here
 * the way this table once did (bead pico-link-jyhk.22 review finding).
 */
export const OpError = {
  None: 0,
  InvalidRequest: 1,
  UnknownOp: 2,
  NotReady: 3,
  StoreFull: 4,
  NotFound: 5,
  Conflict: 6,
  EditorOpen: 7,
  NameTaken: 8,
  NameInvalid: 9,
  BlobVersion: 10,
  BandCount: 11,
  ReservedBandKind: 12,
  GainRange: 13,
  FreqRange: 14,
  QRange: 15,
  PreampRange: 16,
  UnknownDevice: 17,
  ParseError: 18,
  ApoTooLarge: 19,
  DeviceBusy: 20,
  RadioBusy: 21,
  NotConnected: 22,
  PairedFull: 23,
} as const;

export type OpError = (typeof OpError)[keyof typeof OpError];

const STATUS_HEADER_LEN = 21;

function stateFromWire(state: number): OpStatusState {
  switch (state) {
    case 1:
      return "done";
    case 2:
      return "rejected";
    default:
      return "none";
  }
}

export interface OpStatus {
  opProto: number;
  seq: number;
  op: number;
  state: OpStatusState;
  error: OpError;
  effectId: number;
  libraryRev: number;
  persistedSeq: number;
  line: number;
  band: number;
  value: number;
  payload: Uint8Array;
}

/**
 * Decodes a `GET_OP_STATUS` (0x07) reply (design section 4's ~101 B
 * layout). Returns `null` if `bytes` is shorter than the fixed header or
 * shorter than the header plus its own declared `payload_len` -- same
 * "bail on malformed/short" discipline as `decodeLibrarySnapshot`/
 * `decodeHomeSnapshot`.
 */
export function decodeOpStatus(bytes: ArrayBuffer | Uint8Array): OpStatus | null {
  const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  if (u8.byteLength < STATUS_HEADER_LEN) return null;
  const view = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);

  const opProto = view.getUint8(0);
  const seq = view.getUint8(1);
  const op = view.getUint8(2);
  const state = stateFromWire(view.getUint8(3));
  const error = view.getUint8(4) as OpError;
  const effectId = view.getUint16(6, true);
  const libraryRev = view.getUint16(8, true);
  const persistedSeq = view.getUint16(10, true);
  const line = view.getUint16(12, true);
  const band = view.getUint16(14, true);
  const value = view.getFloat32(16, true);
  const payloadLen = view.getUint8(20);

  if (u8.byteLength < STATUS_HEADER_LEN + payloadLen) return null;
  const payload = u8.slice(STATUS_HEADER_LEN, STATUS_HEADER_LEN + payloadLen);

  return { opProto, seq, op, state, error, effectId, libraryRev, persistedSeq, line, band, value, payload };
}

/** `PARSE_APO`'s result payload: `blob[80], u16 collides_with, u8 copy_name_len, copy_name[16]` (design section 4). */
export interface ParseApoResult {
  blob: Uint8Array;
  /** `0` = no same-name collision. */
  collidesWith: number;
  copyName: string;
}

/** Test/`FakeTransport` helper: the inverse of `decodeOpStatus` -- encodes a device's `GET_OP_STATUS` reply. */
export function encodeOpStatusForTest(status: Omit<OpStatus, "opProto">): Uint8Array {
  const out = new Uint8Array(STATUS_HEADER_LEN + status.payload.length);
  const view = new DataView(out.buffer);
  out[0] = OP_PROTO;
  out[1] = status.seq;
  out[2] = status.op;
  out[3] = status.state === "done" ? 1 : status.state === "rejected" ? 2 : 0;
  out[4] = status.error;
  view.setUint16(6, status.effectId, true);
  view.setUint16(8, status.libraryRev, true);
  view.setUint16(10, status.persistedSeq, true);
  view.setUint16(12, status.line, true);
  view.setUint16(14, status.band, true);
  view.setFloat32(16, status.value, true);
  out[20] = status.payload.length;
  out.set(status.payload, STATUS_HEADER_LEN);
  return out;
}

export function decodeParseApoResult(payload: Uint8Array): ParseApoResult | null {
  const MIN_LEN = BLOB_LEN + 2 + 1;
  if (payload.length < MIN_LEN) return null;
  const view = new DataView(payload.buffer, payload.byteOffset, payload.byteLength);
  const blob = payload.slice(0, BLOB_LEN);
  const collidesWith = view.getUint16(BLOB_LEN, true);
  const copyNameLenOff = BLOB_LEN + 2;
  const copyNameLen = Math.min(view.getUint8(copyNameLenOff), 16);
  const copyName = utf8Decoder.decode(payload.subarray(copyNameLenOff + 1, copyNameLenOff + 1 + copyNameLen));
  return { blob, collidesWith, copyName };
}

/** Test/`FakeTransport` helper: the inverse of `decodeParseApoResult`. */
export function encodeParseApoResultForTest(result: ParseApoResult): Uint8Array {
  const out = new Uint8Array(BLOB_LEN + 2 + 1 + 16);
  out.set(result.blob, 0);
  new DataView(out.buffer).setUint16(BLOB_LEN, result.collidesWith, true);
  const copyNameLenOff = BLOB_LEN + 2;
  const copyName = utf8Encoder.encode(result.copyName).slice(0, 16);
  out[copyNameLenOff] = copyName.length;
  out.set(copyName, copyNameLenOff + 1);
  return out;
}
