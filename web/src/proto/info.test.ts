import { describe, expect, it } from "vitest";
import { decodeDeviceInfo, encodeDeviceInfoForTest, INFO_WIRE_LEN } from "./info";

describe("decodeDeviceInfo / encodeDeviceInfoForTest round-trip", () => {
  it("round-trips a populated GET_INFO reply", () => {
    const input = { infoVer: 1, importProto: 1, statusVer: 1, telemetryProto: 1, telemetryPageMask: 0b101, version: "1.2.3-dev" };
    const encoded = encodeDeviceInfoForTest(input);
    expect(encoded.byteLength).toBe(INFO_WIRE_LEN);
    expect(decodeDeviceInfo(encoded)).toEqual(input);
  });

  it("returns null when the buffer is shorter than INFO_WIRE_LEN", () => {
    expect(decodeDeviceInfo(new Uint8Array(INFO_WIRE_LEN - 1))).toBeNull();
  });

  it("truncates a version string longer than 32 bytes", () => {
    const encoded = encodeDeviceInfoForTest({ infoVer: 1, importProto: 1, statusVer: 1, telemetryProto: 1, telemetryPageMask: 0, version: "v".repeat(40) });
    expect(decodeDeviceInfo(encoded)!.version.length).toBe(32);
  });
});
