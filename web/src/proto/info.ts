// GET_INFO (0x04) decoder. Mirrors `pl_cfg_info_wire_t`
// (firmware/src/usb_config_itf.h:199-207) -- this struct has no Rust
// owner (that file's comment), so this is a hand-written fixture-backed
// mirror per FERN DESIGN section 5's "GET_INFO has no Rust owner" note.
//
// v1 layout (packed, little-endian, `INFO_WIRE_LEN` = 41 B):
//   0..1  info_ver       (u8)
//   1..2  import_proto   (u8)
//   2..3  status_ver     (u8)
//   3..4  telemetry_proto(u8)
//   4..8  telemetry_page_mask (u32)
//   8..9  version_len    (u8)
//   9..41 version        ([u8; 32], zero-padded past version_len)
//
// v2 appends (ADA DESIGN on pico-link-jyhk.17, `.planning/design/
// 2026-09-27-iface6-eq-management-protocol.md` section 8), `INFO_WIRE_LEN_V2`
// = 51 B total:
//   41..42 lib_proto      (u8, `1`)
//   42..43 op_proto       (u8, `1`)
//   43..45 mailbox_len    (u16, `1024`)
//   45..49 op_mask        (u32, bit N = HOST_OP `N` implemented)
//   49..51 library_max_len(u16)
//
// FIRMWARE STATUS: v2 has not landed on either side yet -- neither C
// (usb_config_itf.c, that design's task 4) nor a fixture generator (info-v1
// is hand-written; there's no Rust owner to emit info-v2 either). This
// decoder is coded directly from the design doc; `info.test.ts`'s v2 tests
// are marked "(design-derived)". Callers MUST check `infoVer >= 2` before
// trusting any v2 field (a v1 device never sets them; `decodeDeviceInfo`
// leaves them `undefined` on a payload shorter than `INFO_WIRE_LEN_V2`).
export const INFO_WIRE_LEN = 41;
export const INFO_WIRE_LEN_V2 = 51;

/** The v2 op_mask's bit-per-HOST_OP contract (design section 8: "bit N = op N"). */
export function opMaskSupports(opMask: number, op: number): boolean {
  return (opMask & (1 << op)) !== 0;
}

export interface DeviceInfoV2Fields {
  libProto: number;
  opProto: number;
  mailboxLen: number;
  opMask: number;
  libraryMaxLen: number;
}

export interface DeviceInfo {
  infoVer: number;
  importProto: number;
  statusVer: number;
  telemetryProto: number;
  telemetryPageMask: number;
  version: string;
  /** `undefined` on a pre-v2 (41-byte) reply -- design section 8: "info_ver 1 -> Home only". */
  v2: DeviceInfoV2Fields | undefined;
}

const utf8Decoder = new TextDecoder("utf-8");

export function decodeDeviceInfo(bytes: ArrayBuffer | Uint8Array): DeviceInfo | null {
  const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  if (u8.byteLength < INFO_WIRE_LEN) {
    return null;
  }
  const view = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
  const versionLen = Math.min(view.getUint8(8), 32);
  const version = utf8Decoder.decode(new Uint8Array(u8.buffer, u8.byteOffset + 9, versionLen));

  let v2: DeviceInfoV2Fields | undefined;
  if (u8.byteLength >= INFO_WIRE_LEN_V2) {
    v2 = {
      libProto: view.getUint8(41),
      opProto: view.getUint8(42),
      mailboxLen: view.getUint16(43, true),
      opMask: view.getUint32(45, true),
      libraryMaxLen: view.getUint16(49, true),
    };
  }

  return {
    infoVer: view.getUint8(0),
    importProto: view.getUint8(1),
    statusVer: view.getUint8(2),
    telemetryProto: view.getUint8(3),
    telemetryPageMask: view.getUint32(4, true),
    version,
    v2,
  };
}

/** `DeviceInfo` with `v2` optional -- what a caller builds (`FakeTransport`'s options, test fixtures) before `encodeDeviceInfoForTest` fills in the "not present" case as a shorter wire length rather than an explicit `undefined` field. */
export type DeviceInfoInput = Omit<DeviceInfo, "v2"> & { v2?: DeviceInfoV2Fields };

/** Test helper: the inverse of `decodeDeviceInfo`, for `FakeTransport`. Omitting `v2` produces a v1-length (41-byte) reply. */
export function encodeDeviceInfoForTest(input: DeviceInfoInput): Uint8Array {
  const wireLen = input.v2 ? INFO_WIRE_LEN_V2 : INFO_WIRE_LEN;
  const u8 = new Uint8Array(wireLen);
  const view = new DataView(u8.buffer);
  view.setUint8(0, input.infoVer);
  view.setUint8(1, input.importProto);
  view.setUint8(2, input.statusVer);
  view.setUint8(3, input.telemetryProto);
  view.setUint32(4, input.telemetryPageMask, true);
  const encoded = new TextEncoder().encode(input.version).slice(0, 32);
  view.setUint8(8, encoded.length);
  u8.set(encoded, 9);

  if (input.v2) {
    view.setUint8(41, input.v2.libProto);
    view.setUint8(42, input.v2.opProto);
    view.setUint16(43, input.v2.mailboxLen, true);
    view.setUint32(45, input.v2.opMask, true);
    view.setUint16(49, input.v2.libraryMaxLen, true);
  }

  return u8;
}
