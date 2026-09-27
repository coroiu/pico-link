import { describe, expect, it } from "vitest";
import { buildFaultRows, formatAge, FAULT_ROW_CAP } from "./faults";
import type { FaultKey, DecodedFault } from "../proto/telemetry";
import { FAULT_KEYS } from "../proto/telemetry";
import { FAULT_LIVE_WINDOW_MS, FAULT_RETIRE_MS } from "../meter/constants";

function emptyFaults(): Record<FaultKey, DecodedFault | null> {
  const out = {} as Record<FaultKey, DecodedFault | null>;
  for (const key of FAULT_KEYS) out[key] = null;
  return out;
}

function fault(firstSeenMs: number, lastSeenMs: number, count = 1): DecodedFault {
  return { count, firstSeenMs, lastSeenMs, value: { kind: "none" } };
}

describe("buildFaultRows", () => {
  it("returns nothing when no fault has ever been raised", () => {
    expect(buildFaultRows(emptyFaults(), 100_000)).toEqual([]);
  });

  it("tiers a fault as live strictly under FAULT_LIVE_WINDOW_MS since last_seen", () => {
    const faults = emptyFaults();
    faults.buf_starved = fault(0, 1000);
    const now = 1000 + FAULT_LIVE_WINDOW_MS - 1;
    expect(buildFaultRows(faults, now)[0].tier).toBe("live");
  });

  it("tiers a fault as recent once FAULT_LIVE_WINDOW_MS has elapsed since last_seen", () => {
    const faults = emptyFaults();
    faults.buf_starved = fault(0, 1000);
    const now = 1000 + FAULT_LIVE_WINDOW_MS;
    expect(buildFaultRows(faults, now)[0].tier).toBe("recent");
  });

  it("drops (retires) a fault once FAULT_RETIRE_MS has elapsed since last_seen", () => {
    const faults = emptyFaults();
    faults.buf_starved = fault(0, 1000);
    const stillVisible = buildFaultRows(faults, 1000 + FAULT_RETIRE_MS - 1);
    const retired = buildFaultRows(faults, 1000 + FAULT_RETIRE_MS);
    expect(stillVisible).toHaveLength(1);
    expect(retired).toHaveLength(0);
  });

  it("sorts newest-first by first_seen and caps at FAULT_ROW_CAP", () => {
    const faults = emptyFaults();
    faults.buf_starved = fault(100, 5000);
    faults.buf_overflow = fault(300, 5000);
    faults.usb_supply_low = fault(200, 5000);
    faults.air_congested = fault(400, 5000);
    faults.air_link_lost = fault(500, 5000);
    faults.enc_resync = fault(600, 5000);

    const rows = buildFaultRows(faults, 5000);
    expect(rows).toHaveLength(FAULT_ROW_CAP);
    expect(rows.map((r) => r.key)).toEqual(["enc_resync", "air_link_lost", "air_congested", "buf_overflow"]);
  });

  it("carries count and severity through untouched", () => {
    const faults = emptyFaults();
    faults.buf_overflow = fault(0, 0, 7);
    const row = buildFaultRows(faults, 0)[0];
    expect(row.count).toBe(7);
    expect(row.severity).toBe("audible");
    expect(row.glyph).toBe("up");
  });
});

describe("formatAge", () => {
  it("renders under a minute in seconds", () => {
    expect(formatAge(45_000)).toBe("45 s ago");
  });

  it("renders a minute or more in minutes", () => {
    expect(formatAge(90_000)).toBe("2 min ago");
  });
});
