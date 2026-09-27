// Asserts info-v1.bin against info-v1.json (review follow-up on
// pico-link-jyhk.11). GET_INFO has no Rust owner (see info-v1.json's own
// `_comment`), so this is the only fixture cross-check for this decoder.
import { describe, expect, it } from "vitest";
import { decodeDeviceInfo, INFO_WIRE_LEN } from "./info";
import { fixtureBytes, fixtureJson } from "../test/fixtures";

interface FixtureDeviceInfo {
  wire_len: number;
  info_ver: number;
  import_proto: number;
  status_ver: number;
  telemetry_proto: number;
  telemetry_page_mask: number;
  version_len: number;
  version: string;
}

describe("decodeDeviceInfo vs fixtures/telemetry/info-v1.bin+json", () => {
  it("matches the committed golden", () => {
    const bytes = fixtureBytes("info-v1.bin");
    const fixture = fixtureJson<FixtureDeviceInfo>("info-v1.json");
    expect(fixture.wire_len).toBe(INFO_WIRE_LEN);

    const decoded = decodeDeviceInfo(bytes);
    expect(decoded).not.toBeNull();
    expect(decoded).toEqual({
      infoVer: fixture.info_ver,
      importProto: fixture.import_proto,
      statusVer: fixture.status_ver,
      telemetryProto: fixture.telemetry_proto,
      telemetryPageMask: fixture.telemetry_page_mask,
      version: fixture.version,
    });
    expect(decoded!.version.length).toBe(fixture.version_len);
  });
});
