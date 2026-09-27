import { describe, expect, it } from "vitest";
import type { LibraryEffect, Preset } from "../../proto/library";
import { checkName, clampFreqHz, clampGainDb, clampQ, truncateToNameBudget, uniqueName, utf8ByteLength } from "./bandOps";

function effect(id: number, name: string): LibraryEffect {
  const preset: Preset = { name, crossfeed: "off", bands: [], preamp: { kind: "auto" }, eqLocked: true };
  return { id, persistedSeq: 1, preset };
}

describe("clamps", () => {
  it("clamps freq/gain/q to the mock's table ranges", () => {
    expect(clampFreqHz(5)).toBe(20);
    expect(clampFreqHz(50000)).toBe(20000);
    expect(clampGainDb(-100)).toBe(-24);
    expect(clampGainDb(100)).toBe(24);
    expect(clampQ(0)).toBe(0.1);
    expect(clampQ(999)).toBe(20);
  });
});

describe("checkName", () => {
  const effects = [effect(1, "Warm"), effect(2, "Bright")];

  it("flags empty names", () => {
    expect(checkName("   ", effects, null).reason).toBe("empty");
  });

  it("flags names over the 16-byte budget (multibyte counted in UTF-8 bytes)", () => {
    expect(checkName("This name is way too long", effects, null).reason).toBe("tooLong");
    // "café" x4 = 4*5 bytes (é is 2 bytes in utf-8) = 20 > 16
    expect(checkName("cafécafécafécafé", effects, null).byteLength).toBeGreaterThan(16);
  });

  it("flags a name already used by a different effect, but not by itself", () => {
    expect(checkName("Warm", effects, null).reason).toBe("taken");
    expect(checkName("Warm", effects, 1).reason).toBe("ok");
  });

  it("accepts a fresh, short, unique name", () => {
    const result = checkName("New EQ", effects, null);
    expect(result.invalid).toBe(false);
  });
});

describe("uniqueName", () => {
  it("returns the base name when it does not collide", () => {
    expect(uniqueName("Fresh", ["Warm", "Bright"])).toBe("Fresh");
  });

  it("appends the lowest free numeric suffix on collision", () => {
    expect(uniqueName("Warm", ["Warm", "Bright"])).toBe("Warm 2");
    expect(uniqueName("Warm", ["Warm", "Warm 2"])).toBe("Warm 3");
  });
});

describe("truncateToNameBudget", () => {
  it("leaves short names untouched", () => {
    expect(truncateToNameBudget("Short")).toBe("Short");
  });

  it("trims byte-for-byte until it fits 16 bytes", () => {
    const truncated = truncateToNameBudget("A very very long imported effect name");
    expect(utf8ByteLength(truncated)).toBeLessThanOrEqual(16);
  });
});
