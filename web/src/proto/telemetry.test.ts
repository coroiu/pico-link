import { describe, expect, it } from "vitest";
import { decodeHomeSnapshot, emptyHomeSnapshot, encodeHomeSnapshotForTest, FAULT_KEYS, HOME_SNAPSHOT_LEN, TELEMETRY_PAGE_HOME, TELEMETRY_PROTO } from "./telemetry";

describe("decodeHomeSnapshot / encodeHomeSnapshotForTest round-trip", () => {
  it("round-trips the empty (not-ready) snapshot", () => {
    const encoded = encodeHomeSnapshotForTest(emptyHomeSnapshot());
    expect(encoded.byteLength).toBe(HOME_SNAPSHOT_LEN);
    const decoded = decodeHomeSnapshot(encoded);
    expect(decoded).not.toBeNull();
    expect(decoded).toEqual(emptyHomeSnapshot());
    expect(decoded!.snapSeq).toBe(0);
  });

  it("round-trips a fully populated snapshot, including all fault slots", () => {
    const input = {
      ...emptyHomeSnapshot(),
      uptimeMs: 123_456,
      snapSeq: 42,
      linkConnected: true,
      codecWord: "LDAC",
      kbps: 909,
      kbpsAdaptive: true,
      kbpsIsLive: true,
      deviceName: "Sony WH-1000XM5",
      fxPresetName: "Warm",
      volumeLevel: 96,
      volumeMuted: true,
      volumeSource: "sink" as const,
      volumePresent: true,
      peakL: 200,
      peakR: 190,
      rmsL: 100,
      rmsR: 95,
      receivedMs: 123_400,
      levelPresent: true,
      faults: Object.fromEntries(
        FAULT_KEYS.map((key, i) => [
          key,
          {
            count: i + 1,
            firstSeenMs: 1_000 + i,
            lastSeenMs: 2_000 + i,
            value:
              i % 3 === 0
                ? { kind: "ratio" as const, value: 50 }
                : i % 3 === 1
                  ? { kind: "count" as const, value: 7 }
                  : { kind: "millis" as const, value: 300 },
          },
        ]),
      ) as ReturnType<typeof emptyHomeSnapshot>["faults"],
    };

    const decoded = decodeHomeSnapshot(encodeHomeSnapshotForTest(input));
    expect(decoded).toEqual(input);
  });

  it("returns null when the buffer is shorter than HOME_SNAPSHOT_LEN", () => {
    const short = new Uint8Array(HOME_SNAPSHOT_LEN - 1);
    expect(decodeHomeSnapshot(short)).toBeNull();
  });

  it("returns null on a proto/page mismatch (the update-needed gate)", () => {
    const bytes = encodeHomeSnapshotForTest(emptyHomeSnapshot());
    const view = new DataView(bytes.buffer);
    view.setUint8(0, TELEMETRY_PROTO + 1);
    expect(decodeHomeSnapshot(bytes)).toBeNull();

    const bytes2 = encodeHomeSnapshotForTest(emptyHomeSnapshot());
    const view2 = new DataView(bytes2.buffer);
    view2.setUint8(1, TELEMETRY_PAGE_HOME + 1);
    expect(decodeHomeSnapshot(bytes2)).toBeNull();
  });

  it("decodes an absent fault slot (count 0) as null", () => {
    const decoded = decodeHomeSnapshot(encodeHomeSnapshotForTest(emptyHomeSnapshot()));
    for (const key of FAULT_KEYS) {
      expect(decoded!.faults[key]).toBeNull();
    }
  });

  it("truncates names longer than their wire cap", () => {
    const input = { ...emptyHomeSnapshot(), deviceName: "x".repeat(64), codecWord: "y".repeat(20), fxPresetName: "z".repeat(30) };
    const decoded = decodeHomeSnapshot(encodeHomeSnapshotForTest(input));
    expect(decoded!.deviceName.length).toBe(32);
    expect(decoded!.codecWord.length).toBe(8);
    expect(decoded!.fxPresetName.length).toBe(16);
  });
});
