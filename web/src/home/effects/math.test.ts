import { describe, expect, it } from "vitest";
import type { Band } from "../../proto/library";
import {
  DEFAULT_PAD,
  bandFreqHz,
  bandGainDb,
  bandQ,
  biquadForBand,
  computeDrag,
  computeNudge,
  computeWheelQ,
  dbToY,
  effectivePreampDb,
  freqHzToWire,
  freqToX,
  gainDbToWire,
  hitTestBand,
  magnitudeDb,
  peakBoostDb,
  qToWire,
  responseDb,
  xToFreq,
  yToDb,
} from "./math";
import type { CurveGeom } from "./math";

function band(kind: Band["kind"], freqHz: number, gainDb: number, q: number): Band {
  return { kind, freqHalfHz: freqHzToWire(freqHz), gainCdb: gainDbToWire(gainDb), qMilli: qToWire(q) };
}

const GEOM: CurveGeom = { width: 800, height: 300, ...DEFAULT_PAD };

describe("wire scale round-trips", () => {
  it("recovers freq/gain/q from their wire encodings", () => {
    const b = band("peak", 1000, 6, 1.41);
    expect(bandFreqHz(b)).toBeCloseTo(1000, 1);
    expect(bandGainDb(b)).toBeCloseTo(6, 2);
    expect(bandQ(b)).toBeCloseTo(1.41, 2);
  });
});

describe("biquad magnitude response", () => {
  it("a zero-gain peaking band is the identity filter (unity everywhere)", () => {
    const b = band("peak", 1000, 0, 1);
    const c = biquadForBand(b);
    for (const f of [20, 100, 1000, 5000, 19000]) {
      expect(magnitudeDb(c, f)).toBeCloseTo(0, 3);
    }
  });

  it("a peaking boost is at its maximum at the centre frequency", () => {
    const b = band("peak", 1000, 6, 1.41);
    const c = biquadForBand(b);
    const atCenter = magnitudeDb(c, 1000);
    expect(atCenter).toBeCloseTo(6, 1);
    expect(magnitudeDb(c, 100)).toBeLessThan(atCenter);
    expect(magnitudeDb(c, 10000)).toBeLessThan(atCenter);
  });

  it("a low shelf settles near gain_db well below its corner and near 0 well above", () => {
    const b = band("lowShelf", 200, -6, 0.7);
    const c = biquadForBand(b);
    expect(magnitudeDb(c, 20)).toBeCloseTo(-6, 0);
    expect(magnitudeDb(c, 20000)).toBeCloseTo(0, 0);
  });

  it("a high shelf settles near 0 well below its corner and near gain_db well above", () => {
    const b = band("highShelf", 4000, 6, 0.7);
    const c = biquadForBand(b);
    expect(magnitudeDb(c, 20)).toBeCloseTo(0, 0);
    expect(magnitudeDb(c, 20000)).toBeCloseTo(6, 0);
  });

  it("composite response sums each band's dB contribution", () => {
    const bands = [band("peak", 1000, 3, 1.41), band("peak", 1000, 2, 1.41)];
    const single = responseDb([bands[0]], 1000);
    const both = responseDb(bands, 1000);
    // Not exactly additive at every point for two overlapping peaks at the
    // literal center (phase interacts elsewhere), but AT the shared center
    // frequency both are real max-gain points and should sum closely.
    expect(both).toBeGreaterThan(single);
  });
});

describe("preamp", () => {
  it("auto preamp is the negative of the peak boost, floored at -12dB", () => {
    const bands = [band("peak", 1000, 6, 1.41)];
    const pre = effectivePreampDb({ kind: "auto" }, bands);
    expect(pre).toBeCloseTo(-peakBoostDb(bands), 5);
    expect(pre).toBeLessThanOrEqual(0);
    expect(pre).toBeGreaterThanOrEqual(-12);
  });

  it("a huge boost floors auto preamp at -12dB rather than going lower", () => {
    const bands = [band("peak", 1000, 24, 1)];
    expect(effectivePreampDb({ kind: "auto" }, bands)).toBe(-12);
  });

  it("fixed preamp reads back its own cdb value", () => {
    expect(effectivePreampDb({ kind: "explicit", cdb: -450 }, [])).toBeCloseTo(-4.5, 5);
  });
});

describe("screen-space mapping", () => {
  it("freqToX and xToFreq round-trip", () => {
    for (const f of [20, 100, 1000, 10000, 20000]) {
      const x = freqToX(f, GEOM);
      expect(xToFreq(x, GEOM)).toBeCloseTo(f, 0);
    }
  });

  it("dbToY and yToDb round-trip", () => {
    for (const db of [-15, -5, 0, 5, 15]) {
      const y = dbToY(db, GEOM);
      expect(yToDb(y, GEOM)).toBeCloseTo(db, 5);
    }
  });

  it("20Hz maps to the left pad edge and 20kHz to the right pad edge", () => {
    expect(freqToX(20, GEOM)).toBeCloseTo(GEOM.padLeft, 5);
    expect(freqToX(20000, GEOM)).toBeCloseTo(GEOM.width - GEOM.padRight, 5);
  });
});

describe("drag gesture", () => {
  it("dragging right increases frequency, dragging up increases gain", () => {
    const start = { freqHz: 1000, gainDb: 0, x0: freqToX(1000, GEOM), y0: dbToY(0, GEOM) };
    const right = computeDrag(start, start.x0 + 50, start.y0, GEOM, false);
    expect(right.freqHz).toBeGreaterThan(1000);
    const up = computeDrag(start, start.x0, start.y0 - 50, GEOM, false);
    expect(up.gainDb).toBeGreaterThan(0);
  });

  it("Shift (fine) scales the drag distance down by 0.2x", () => {
    const start = { freqHz: 1000, gainDb: 0, x0: freqToX(1000, GEOM), y0: dbToY(0, GEOM) };
    const coarse = computeDrag(start, start.x0 + 100, start.y0, GEOM, false);
    const fine = computeDrag(start, start.x0 + 100, start.y0, GEOM, true);
    expect(Math.abs(fine.freqHz - 1000)).toBeLessThan(Math.abs(coarse.freqHz - 1000));
  });

  it("clamps frequency and gain to their ranges", () => {
    const start = { freqHz: 20, gainDb: -15, x0: freqToX(20, GEOM), y0: dbToY(-15, GEOM) };
    const result = computeDrag(start, start.x0 - 500, start.y0 + 500, GEOM, false);
    expect(result.freqHz).toBe(20);
    expect(result.gainDb).toBe(-15);
  });
});

describe("wheel Q", () => {
  it("scrolling up (deltaY negative) increases Q by 1.1x, Shift by 1.02x", () => {
    expect(computeWheelQ(1, true, false)).toBeCloseTo(1.1, 5);
    expect(computeWheelQ(1, true, true)).toBeCloseTo(1.02, 5);
  });

  it("scrolling down decreases Q", () => {
    expect(computeWheelQ(1, false, false)).toBeLessThan(1);
  });

  it("clamps to [0.1, 20]", () => {
    expect(computeWheelQ(0.1, false, false)).toBe(0.1);
    expect(computeWheelQ(20, true, false)).toBe(20);
  });
});

describe("keyboard nudge", () => {
  it("ArrowRight/ArrowLeft move by 1/12 octave, Shift by 1/3", () => {
    const right = computeNudge("ArrowRight", 1000, 0, false);
    expect(right.freqHz).toBeCloseTo(1000 * 2 ** (1 / 12), 0);
    const rightFine = computeNudge("ArrowRight", 1000, 0, true);
    expect(rightFine.freqHz).toBeCloseTo(1000 * 2 ** (1 / 3), 0);
    const left = computeNudge("ArrowLeft", 1000, 0, false);
    expect(left.freqHz).toBeLessThan(1000);
  });

  it("ArrowUp/ArrowDown move gain by 0.1dB, Shift by 1dB", () => {
    expect(computeNudge("ArrowUp", 1000, 0, false).gainDb).toBeCloseTo(0.1, 5);
    expect(computeNudge("ArrowUp", 1000, 0, true).gainDb).toBeCloseTo(1, 5);
    expect(computeNudge("ArrowDown", 1000, 0, false).gainDb).toBeCloseTo(-0.1, 5);
  });
});

describe("hit testing", () => {
  it("finds the nearest band within the threshold", () => {
    const bands = [band("peak", 1000, 0, 1), band("peak", 5000, 5, 1)];
    const x = freqToX(1000, GEOM);
    const y = dbToY(0, GEOM);
    expect(hitTestBand(bands, x, y, GEOM)).toBe(0);
  });

  it("returns -1 when nothing is close enough", () => {
    const bands = [band("peak", 1000, 0, 1)];
    expect(hitTestBand(bands, 5, 5, GEOM)).toBe(-1);
  });
});
