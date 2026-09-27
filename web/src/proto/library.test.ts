// Round-trip / edge-case tests for `library.ts`'s codec that aren't
// exercised by any `fixtures/library/*` pair (none of them carry EQ bands
// -- see `library.fixture.test.ts`'s module doc comment). Mirrors
// `core/src/dsp/preset.rs`'s own unit tests for the same blob format.
import { describe, expect, it } from "vitest";
import { decodeLibrarySnapshot, decodePresetBlob, emptyLibrarySnapshot, encodeLibrarySnapshotForTest, encodePresetBlob } from "./library";
import type { Band, Preset } from "./library";

describe("encodePresetBlob / decodePresetBlob round trip", () => {
  it("round-trips a preset with bands, crossfeed, explicit preamp and lock", () => {
    const bands: Band[] = [
      { kind: "peak", freqHalfHz: 200, gainCdb: -350, qMilli: 1000 },
      { kind: "lowShelf", freqHalfHz: 120, gainCdb: 400, qMilli: 710 },
      { kind: "highShelf", freqHalfHz: 20000, gainCdb: 0, qMilli: 400 },
    ];
    const preset: Preset = { name: "Bright", crossfeed: "strong", bands, preamp: { kind: "explicit", cdb: -600 }, eqLocked: true };

    const blob = encodePresetBlob(preset);
    expect(blob.length).toBe(80);
    expect(blob[0]).toBe(2); // always writes v2

    const decoded = decodePresetBlob(blob);
    expect(decoded).toEqual(preset);
  });

  it("round-trips a negative gain at the packed field's boundary", () => {
    const preset: Preset = { name: "Edge", crossfeed: "off", bands: [{ kind: "peak", freqHalfHz: 1, gainCdb: -4095, qMilli: 8000 }], preamp: { kind: "auto" }, eqLocked: false };
    const decoded = decodePresetBlob(encodePresetBlob(preset));
    expect(decoded.bands[0].gainCdb).toBe(-4095);
  });

  it("clamps an out-of-range gain rather than corrupting the packed field", () => {
    const preset: Preset = { name: "Clamp", crossfeed: "off", bands: [{ kind: "peak", freqHalfHz: 1, gainCdb: 9000, qMilli: 1000 }], preamp: { kind: "auto" }, eqLocked: false };
    const decoded = decodePresetBlob(encodePresetBlob(preset));
    expect(decoded.bands[0].gainCdb).toBe(4095);
  });

  it("decodes a v1 blob by widening exactly (freq*2, gain_half_db*50, Q_TABLE[q_idx]*1000)", () => {
    const blob = new Uint8Array(80);
    blob[0] = 1; // version 1
    blob[1] = 5; // name_len
    blob.set(new TextEncoder().encode("Hello"), 2);
    blob[18] = 2; // crossfeed: medium
    blob[19] = 1; // band_count
    // band: kind=1 (lowShelf), freq_hz=100 LE u16, gain_half_db=-4 (i8), q_idx=3
    const bandOff = 20;
    blob[bandOff] = 1;
    new DataView(blob.buffer).setUint16(bandOff + 1, 100, true);
    new DataView(blob.buffer).setInt8(bandOff + 3, -4);
    blob[bandOff + 4] = 3;

    const decoded = decodePresetBlob(blob);
    expect(decoded.name).toBe("Hello");
    expect(decoded.crossfeed).toBe("medium");
    expect(decoded.preamp).toEqual({ kind: "auto" });
    expect(decoded.eqLocked).toBe(false);
    expect(decoded.bands).toEqual([{ kind: "lowShelf", freqHalfHz: 200, gainCdb: -200, qMilli: 1000 }]);
  });

  it("decodes an unrecognised blob version as empty and LOCKED, keeping the v2-offset name", () => {
    const blob = new Uint8Array(80);
    blob[0] = 99;
    blob.set(new TextEncoder().encode("Future"), 1); // v2's NAME_OFF
    const decoded = decodePresetBlob(blob);
    expect(decoded).toEqual({ name: "Future", crossfeed: "off", bands: [], preamp: { kind: "auto" }, eqLocked: true });
  });

  it("never throws on a too-short buffer", () => {
    expect(() => decodePresetBlob(new Uint8Array(3))).not.toThrow();
  });
});

describe("encodeLibrarySnapshotForTest / decodeLibrarySnapshot round trip", () => {
  it("round-trips an empty library", () => {
    const snap = emptyLibrarySnapshot(1);
    const decoded = decodeLibrarySnapshot(encodeLibrarySnapshotForTest(snap));
    expect(decoded).toEqual(snap);
  });

  it("round-trips effects and devices, preserving blob content", () => {
    const preset: Preset = { name: "Warm", crossfeed: "weak", bands: [{ kind: "peak", freqHalfHz: 2000, gainCdb: 100, qMilli: 1400 }], preamp: { kind: "auto" }, eqLocked: false };
    const snap = {
      libraryRev: 12,
      presetsReady: true,
      maxEffects: 8,
      maxDevices: 8,
      effects: [{ id: 3, persistedSeq: 2, preset }],
      devices: [{ addr: "94:DB:56:54:7C:F2", presetId: 3, connected: true, name: "Pixel Buds" }],
    };
    const decoded = decodeLibrarySnapshot(encodeLibrarySnapshotForTest(snap));
    expect(decoded).toEqual(snap);
  });
});
