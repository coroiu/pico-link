// Asserts every `fixtures/telemetry/home-*.bin` against its `.json` golden
// (review follow-up on pico-link-jyhk.11: "switch web proto tests to assert
// against fixtures/telemetry/*.bin/.json"). `decodeHomeSnapshot`'s round-trip
// tests in `telemetry.test.ts` stay -- this file is the neutral-contract
// cross-check against what `cargo test` actually generated, per FERN DESIGN
// section 5 ("core emits, JS asserts").
import { describe, expect, it } from "vitest";
import { decodeHomeSnapshot, FAULT_KEYS } from "./telemetry";
import type { FaultValue, HomeSnapshot, VolumeSource } from "./telemetry";
import { fixtureBytes, fixtureJson, homeSnapshotFixtureNames } from "../test/fixtures";

interface FixtureFault {
  count: number;
  first_seen_ms: number;
  last_seen_ms: number;
  value_kind: "none" | "ratio" | "count" | "millis";
  value: number;
}

interface FixtureHomeSnapshot {
  wire_len: number;
  uptime_ms: number;
  snap_seq: number;
  link_connected: boolean;
  codec_word: string;
  kbps: number;
  kbps_adaptive: boolean;
  kbps_is_live: boolean;
  device_name: string;
  fx_preset_name: string;
  volume_present: boolean;
  volume_level: number;
  volume_muted: boolean;
  volume_source: VolumeSource;
  level_present: boolean;
  peak_l: number;
  peak_r: number;
  rms_l: number;
  rms_r: number;
  received_ms: number;
  faults: Array<FixtureFault | null>;
}

function faultValueFrom(fixture: FixtureFault): FaultValue {
  switch (fixture.value_kind) {
    case "ratio":
      return { kind: "ratio", value: fixture.value };
    case "count":
      return { kind: "count", value: fixture.value };
    case "millis":
      return { kind: "millis", value: fixture.value };
    default:
      return { kind: "none" };
  }
}

function expectedFromFixture(fixture: FixtureHomeSnapshot): HomeSnapshot {
  const faults = {} as HomeSnapshot["faults"];
  FAULT_KEYS.forEach((key, i) => {
    const entry = fixture.faults[i];
    faults[key] = entry ? { count: entry.count, firstSeenMs: entry.first_seen_ms, lastSeenMs: entry.last_seen_ms, value: faultValueFrom(entry) } : null;
  });
  return {
    uptimeMs: fixture.uptime_ms,
    snapSeq: fixture.snap_seq,
    linkConnected: fixture.link_connected,
    codecWord: fixture.codec_word,
    kbps: fixture.kbps,
    kbpsAdaptive: fixture.kbps_adaptive,
    kbpsIsLive: fixture.kbps_is_live,
    deviceName: fixture.device_name,
    fxPresetName: fixture.fx_preset_name,
    volumeLevel: fixture.volume_level,
    volumeMuted: fixture.volume_muted,
    volumeSource: fixture.volume_source,
    volumePresent: fixture.volume_present,
    peakL: fixture.peak_l,
    peakR: fixture.peak_r,
    rmsL: fixture.rms_l,
    rmsR: fixture.rms_r,
    receivedMs: fixture.received_ms,
    levelPresent: fixture.level_present,
    faults,
  };
}

describe("decodeHomeSnapshot vs fixtures/telemetry/home-*.bin+json", () => {
  const names = homeSnapshotFixtureNames();

  it("found the expected fixture set (guards against an empty/missing dir silently passing)", () => {
    expect(names.length).toBeGreaterThanOrEqual(8);
    expect(names).toContain("home-golden");
    expect(names).toContain("home-not-ready");
    expect(names).toContain("home-trailing-bytes");
  });

  it.each(names)("%s", (name) => {
    const bytes = fixtureBytes(`${name}.bin`);
    const fixture = fixtureJson<FixtureHomeSnapshot>(`${name}.json`);
    expect(fixture.wire_len).toBe(163);

    const decoded = decodeHomeSnapshot(bytes);
    expect(decoded).not.toBeNull();
    expect(decoded).toEqual(expectedFromFixture(fixture));
  });
});
