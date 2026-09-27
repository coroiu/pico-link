// Pure client-side EQ math for the curve editor: the exact RBJ cookbook
// biquad formulas from `core/src/dsp/coeffs.rs` (`rbj_peaking`/
// `rbj_low_shelf`/`rbj_high_shelf`, Q-based form), a `magnitude_at`-style
// response, and the log-frequency / dB screen-space mapping the mock
// (`.planning/design/mocks/2026-09-27-web-companion-v2-identity.html`,
// `coeffs`/`magDb`/`fx2x`/`db2y`) uses for its curve canvas.
//
// This is a PREVIEW ONLY: the device is authoritative for the actual DSP
// (`core/src/dsp/coeffs.rs` running at the real stream `fs_hz`). `FS` below
// is a fixed assumption for drawing the curve, matching the mock and a
// typical A2DP rate; it does not need to match the device's exact runtime
// rate for the curve to be a useful visual guide.
import type { Band, BandKind, Preamp } from "../../proto/library";

export const FS = 48000;
export const DB_RANGE = 15;
export const FREQ_MIN = 20;
export const FREQ_MAX = 20_000;

export function bandFreqHz(band: Band): number {
  return band.freqHalfHz * 0.5;
}

export function bandGainDb(band: Band): number {
  return band.gainCdb * 0.01;
}

export function bandQ(band: Band): number {
  return band.qMilli * 0.001;
}

export function freqHzToWire(freqHz: number): number {
  return Math.round(clamp(freqHz, FREQ_MIN, FREQ_MAX) * 2);
}

export function gainDbToWire(gainDb: number): number {
  return Math.round(clamp(gainDb, -24, 24) * 100);
}

export function qToWire(q: number): number {
  return Math.round(clamp(q, 0.1, 20) * 1000);
}

export interface Biquad {
  b0: number;
  b1: number;
  b2: number;
  a1: number;
  a2: number;
}

/** Mirrors `core::dsp::coeffs::rbj_peaking`. */
export function rbjPeaking(freqHz: number, gainDb: number, q: number, fsHz: number): Biquad {
  const a = 10 ** (gainDb / 40);
  const w0 = (2 * Math.PI * freqHz) / fsHz;
  const sinW0 = Math.sin(w0);
  const cosW0 = Math.cos(w0);
  const alpha = sinW0 / (2 * q);

  const a0 = 1 + alpha / a;
  return {
    b0: (1 + alpha * a) / a0,
    b1: (-2 * cosW0) / a0,
    b2: (1 - alpha * a) / a0,
    a1: (-2 * cosW0) / a0,
    a2: (1 - alpha / a) / a0,
  };
}

/** Mirrors `core::dsp::coeffs::rbj_low_shelf`. */
export function rbjLowShelf(freqHz: number, gainDb: number, q: number, fsHz: number): Biquad {
  const a = 10 ** (gainDb / 40);
  const w0 = (2 * Math.PI * freqHz) / fsHz;
  const sinW0 = Math.sin(w0);
  const cosW0 = Math.cos(w0);
  const alpha = sinW0 / (2 * q);
  const sqrtA = Math.sqrt(a);
  const twoSqrtAAlpha = 2 * sqrtA * alpha;

  const a0 = a + 1 + (a - 1) * cosW0 + twoSqrtAAlpha;
  return {
    b0: (a * (a + 1 - (a - 1) * cosW0 + twoSqrtAAlpha)) / a0,
    b1: (2 * a * (a - 1 - (a + 1) * cosW0)) / a0,
    b2: (a * (a + 1 - (a - 1) * cosW0 - twoSqrtAAlpha)) / a0,
    a1: (-2 * (a - 1 + (a + 1) * cosW0)) / a0,
    a2: (a + 1 + (a - 1) * cosW0 - twoSqrtAAlpha) / a0,
  };
}

/** Mirrors `core::dsp::coeffs::rbj_high_shelf`. */
export function rbjHighShelf(freqHz: number, gainDb: number, q: number, fsHz: number): Biquad {
  const a = 10 ** (gainDb / 40);
  const w0 = (2 * Math.PI * freqHz) / fsHz;
  const sinW0 = Math.sin(w0);
  const cosW0 = Math.cos(w0);
  const alpha = sinW0 / (2 * q);
  const sqrtA = Math.sqrt(a);
  const twoSqrtAAlpha = 2 * sqrtA * alpha;

  const a0 = a + 1 - (a - 1) * cosW0 + twoSqrtAAlpha;
  return {
    b0: (a * (a + 1 + (a - 1) * cosW0 + twoSqrtAAlpha)) / a0,
    b1: (-2 * a * (a - 1 + (a + 1) * cosW0)) / a0,
    b2: (a * (a + 1 + (a - 1) * cosW0 - twoSqrtAAlpha)) / a0,
    a1: (2 * (a - 1 - (a + 1) * cosW0)) / a0,
    a2: (a + 1 - (a - 1) * cosW0 - twoSqrtAAlpha) / a0,
  };
}

export function biquadForKind(kind: BandKind, freqHz: number, gainDb: number, q: number, fsHz = FS): Biquad {
  switch (kind) {
    case "lowShelf":
      return rbjLowShelf(freqHz, gainDb, q, fsHz);
    case "highShelf":
      return rbjHighShelf(freqHz, gainDb, q, fsHz);
    default:
      return rbjPeaking(freqHz, gainDb, q, fsHz);
  }
}

export function biquadForBand(band: Band, fsHz = FS): Biquad {
  return biquadForKind(band.kind, bandFreqHz(band), bandGainDb(band), bandQ(band), fsHz);
}

/** Mirrors `Biquad::magnitude_at`, but returns dB (`20*log10`) rather than linear magnitude. */
export function magnitudeDb(c: Biquad, freqHz: number, fsHz = FS): number {
  const w = (2 * Math.PI * freqHz) / fsHz;
  const cosW = Math.cos(w);
  const sinW = Math.sin(w);
  const cos2w = Math.cos(2 * w);
  const sin2w = Math.sin(2 * w);

  const numRe = c.b0 + c.b1 * cosW + c.b2 * cos2w;
  const numIm = -(c.b1 * sinW + c.b2 * sin2w);
  const denRe = 1 + c.a1 * cosW + c.a2 * cos2w;
  const denIm = -(c.a1 * sinW + c.a2 * sin2w);

  const numMagSq = numRe * numRe + numIm * numIm;
  const denMagSq = denRe * denRe + denIm * denIm;
  return 10 * Math.log10(numMagSq / denMagSq);
}

/** Composite response of every band at `freqHz`, in dB (sums each band's dB contribution, which is exact since dB(product) = sum(dB)). */
export function responseDb(bands: Band[], freqHz: number, fsHz = FS): number {
  let sum = 0;
  for (const band of bands) sum += magnitudeDb(biquadForBand(band, fsHz), freqHz, fsHz);
  return sum;
}

const PEAK_SCAN_STEPS = 200;

/** The composite response's maximum over 20Hz-20kHz, log-spaced -- mirrors the mock's `peakBoost`. */
export function peakBoostDb(bands: Band[], fsHz = FS): number {
  let max = 0;
  for (let i = 0; i <= PEAK_SCAN_STEPS; i++) {
    const f = FREQ_MIN * 1000 ** (i / PEAK_SCAN_STEPS);
    max = Math.max(max, responseDb(bands, f, fsHz));
  }
  return max;
}

/** `Preamp::Auto` = `-peakBoostDb` clamped to a floor of -12dB (never a boost); `Preamp::Fixed` is its own value. Mirrors the mock's `effPreamp`. */
export function effectivePreampDb(preamp: Preamp, bands: Band[], fsHz = FS): number {
  if (preamp.kind === "auto") return clamp(-peakBoostDb(bands, fsHz), -12, 0);
  return preamp.cdb * 0.01;
}

export function clamp(v: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, v));
}

// --- Screen-space mapping (log frequency x-axis, linear dB y-axis) --------

export interface CurveGeom {
  width: number;
  height: number;
  padLeft: number;
  padRight: number;
  padTop: number;
  padBottom: number;
}

export const DEFAULT_PAD = { padLeft: 40, padRight: 14, padTop: 14, padBottom: 24 };

function innerW(g: CurveGeom): number {
  return g.width - g.padLeft - g.padRight;
}

function innerH(g: CurveGeom): number {
  return g.height - g.padTop - g.padBottom;
}

/** `20Hz..20kHz` mapped log-linear across the plot's inner width (3 decades). */
export function freqToX(freqHz: number, g: CurveGeom): number {
  return g.padLeft + (Math.log10(freqHz / FREQ_MIN) / 3) * innerW(g);
}

export function xToFreq(x: number, g: CurveGeom): number {
  const u = clamp((x - g.padLeft) / innerW(g), 0, 1);
  return FREQ_MIN * 1000 ** u;
}

export function dbToY(db: number, g: CurveGeom, dbRange = DB_RANGE): number {
  return g.padTop + (1 - (db + dbRange) / (2 * dbRange)) * innerH(g);
}

export function yToDb(y: number, g: CurveGeom, dbRange = DB_RANGE): number {
  return (1 - (y - g.padTop) / innerH(g)) * 2 * dbRange - dbRange;
}

// --- Drag / wheel / keyboard interaction math (pure, unit-tested) --------

export interface DragStart {
  freqHz: number;
  gainDb: number;
  x0: number;
  y0: number;
}

export interface DragResult {
  freqHz: number;
  gainDb: number;
}

/**
 * The curve drag gesture: horizontal motion moves frequency (log-scaled),
 * vertical motion moves gain, `fine` (Shift held) scales both by 0.2x.
 * Mirrors the mock's `pointermove` handler exactly, generalised over
 * `CurveGeom`. Frequency snaps to 1 decimal below 100Hz, whole Hz above;
 * gain snaps to 0.1dB.
 */
export function computeDrag(start: DragStart, x: number, y: number, g: CurveGeom, fine: boolean, dbRange = DB_RANGE): DragResult {
  const k = fine ? 0.2 : 1;
  const x0 = freqToX(start.freqHz, g);
  const freqRaw = clamp(xToFreq(x0 + (x - start.x0) * k, g), FREQ_MIN, FREQ_MAX);
  const freqHz = freqRaw < 100 ? Math.round(freqRaw * 10) / 10 : Math.round(freqRaw);

  const dbAtStart = yToDb(start.y0, g, dbRange);
  const dbAtDragged = yToDb(start.y0 + (y - start.y0) * k, g, dbRange);
  const gainDb = Math.round(clamp(start.gainDb + (dbAtDragged - dbAtStart), -dbRange, dbRange) * 10) / 10;

  return { freqHz, gainDb };
}

/** Mirrors the mock's `wheel` handler: `q *= (fine ? 1.02 : 1.1) ** (deltaYNegative ? 1 : -1)`, clamped `[0.1, 20]`, rounded to 0.01. */
export function computeWheelQ(q: number, deltaYNegative: boolean, fine: boolean): number {
  const base = fine ? 1.02 : 1.1;
  const next = q * base ** (deltaYNegative ? 1 : -1);
  return Math.round(clamp(next, 0.1, 20) * 100) / 100;
}

export type NudgeKey = "ArrowLeft" | "ArrowRight" | "ArrowUp" | "ArrowDown";

/** Mirrors the mock's keyboard nudges: 1/12 octave (Shift: 1/3) for frequency, 0.1dB (Shift: 1dB) for gain. */
export function computeNudge(key: NudgeKey, freqHz: number, gainDb: number, fine: boolean, dbRange = DB_RANGE): DragResult {
  const oct = fine ? 1 / 3 : 1 / 12;
  if (key === "ArrowLeft" || key === "ArrowRight") {
    const dir = key === "ArrowRight" ? 1 : -1;
    const raw = clamp(freqHz * 2 ** (dir * oct), FREQ_MIN, FREQ_MAX);
    return { freqHz: raw < 100 ? Math.round(raw * 10) / 10 : Math.round(raw), gainDb };
  }
  const dir = key === "ArrowUp" ? 1 : -1;
  const step = fine ? 1 : 0.1;
  const next = Math.round(clamp(gainDb + dir * step, -dbRange, dbRange) * 10) / 10;
  return { freqHz, gainDb: next };
}

/** Nearest band to a pointer position within `thresholdPx`, or `-1`. Mirrors the mock's `hit`. */
export function hitTestBand(bands: Band[], x: number, y: number, g: CurveGeom, dbRange = DB_RANGE, thresholdPx = 16): number {
  let best = -1;
  let bestDist = thresholdPx;
  bands.forEach((band, i) => {
    const bx = freqToX(bandFreqHz(band), g);
    const by = dbToY(clamp(bandGainDb(band), -dbRange, dbRange), g, dbRange);
    const dist = Math.hypot(bx - x, by - y);
    if (dist < bestDist) {
      bestDist = dist;
      best = i;
    }
  });
  return best;
}

export function fmtFreq(f: number): string {
  return f < 100 ? f.toFixed(1).replace(/\.0$/, "") : String(Math.round(f));
}

export function fmtFreqLabel(f: number): string {
  return f >= 1000 ? `${(f / 1000).toFixed(2).replace(/\.?0+$/, "")} kHz` : `${Math.round(f)} Hz`;
}
