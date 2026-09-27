// FERN DESIGN section 5: "nothing numeric is retyped from reading Rust" --
// this asserts every literal in constants.ts against the committed
// fixtures/telemetry/constants.json, so a future core change that bumps
// one of these fails here instead of silently drifting.
import { describe, expect, it } from "vitest";
import { FAULT_LIVE_WINDOW_MS, FAULT_RETIRE_MS, OUT_LEVEL_HOLD_DURATION_MS, OUT_LEVEL_STALE_AFTER_MS, RELEASE_RATIO_PER_MS_Q16 } from "./constants";
import { TELEMETRY_PROTO, HOME_SNAPSHOT_LEN, HOME_SNAPSHOT_FULL_LEN } from "../proto/telemetry";
import { fixtureJson } from "../test/fixtures";

interface FixtureConstants {
  TELEMETRY_PROTO: number;
  HOME_SNAPSHOT_LEN: number;
  OUT_LEVEL_STALE_AFTER_MS: number;
  OUT_LEVEL_HOLD_DURATION_MS: number;
  RELEASE_RATIO_PER_MS_Q16: number;
  FAULT_LIVE_WINDOW_MS: number;
  FAULT_RETIRE_MS: number;
}

describe("constants vs fixtures/telemetry/constants.json", () => {
  const fixture = fixtureJson<FixtureConstants>("constants.json");

  it("matches every named constant", () => {
    expect(TELEMETRY_PROTO).toBe(fixture.TELEMETRY_PROTO);
    // fixture's HOME_SNAPSHOT_LEN is the current *full* wire length (169, jyhk.18's
    // append); web's own HOME_SNAPSHOT_LEN stays the proto-1 minimum (163) so a
    // pre-append 163-byte snapshot still decodes -- see HOME_SNAPSHOT_FULL_LEN.
    expect(fixture.HOME_SNAPSHOT_LEN).toBe(HOME_SNAPSHOT_FULL_LEN);
    expect(HOME_SNAPSHOT_FULL_LEN).toBeGreaterThanOrEqual(HOME_SNAPSHOT_LEN);
    expect(OUT_LEVEL_STALE_AFTER_MS).toBe(fixture.OUT_LEVEL_STALE_AFTER_MS);
    expect(OUT_LEVEL_HOLD_DURATION_MS).toBe(fixture.OUT_LEVEL_HOLD_DURATION_MS);
    expect(RELEASE_RATIO_PER_MS_Q16).toBe(fixture.RELEASE_RATIO_PER_MS_Q16);
    expect(FAULT_LIVE_WINDOW_MS).toBe(fixture.FAULT_LIVE_WINDOW_MS);
    expect(FAULT_RETIRE_MS).toBe(fixture.FAULT_RETIRE_MS);
  });
});
