// GET_INFO (0x04) decoder. Mirrors `pl_cfg_info_wire_t`
// (firmware/src/usb_config_itf.h:199-207) -- this struct has no Rust
// owner (that file's comment), so this is a hand-written fixture-backed
// mirror per FERN DESIGN section 5's "GET_INFO has no Rust owner" note.
//
// Layout (packed, little-endian):
//   0..1  info_ver       (u8)
//   1..2  import_proto   (u8)
//   2..3  status_ver     (u8)
//   3..4  telemetry_proto(u8)
//   4..8  telemetry_page_mask (u32)
//   8..9  version_len    (u8)
//   9..41 version        ([u8; 32], zero-padded past version_len)
export const INFO_WIRE_LEN = 41;

export interface DeviceInfo {
  infoVer: number;
  importProto: number;
  statusVer: number;
  telemetryProto: number;
  telemetryPageMask: number;
  version: string;
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
  return {
    infoVer: view.getUint8(0),
    importProto: view.getUint8(1),
    statusVer: view.getUint8(2),
    telemetryProto: view.getUint8(3),
    telemetryPageMask: view.getUint32(4, true),
    version,
  };
}

/** Test helper: the inverse of `decodeDeviceInfo`, for `FakeTransport`. */
export function encodeDeviceInfoForTest(input: DeviceInfo): Uint8Array {
  const u8 = new Uint8Array(INFO_WIRE_LEN);
  const view = new DataView(u8.buffer);
  view.setUint8(0, input.infoVer);
  view.setUint8(1, input.importProto);
  view.setUint8(2, input.statusVer);
  view.setUint8(3, input.telemetryProto);
  view.setUint32(4, input.telemetryPageMask, true);
  const encoded = new TextEncoder().encode(input.version).slice(0, 32);
  view.setUint8(8, encoded.length);
  u8.set(encoded, 9);
  return u8;
}
