// Pure validation/edit helpers for the Effects editor: band field clamps
// (mirrors the mock's `bands` table inputs: freq 20-20000Hz, gain
// +-24dB, Q 0.1-20), the 16-byte name-counter rule, and duplicate-name
// detection against a library snapshot. The device (`core/src/dsp/
// validate.rs`) is authoritative -- a rejected `SAVE_EFFECT` still surfaces
// its own `GainRange`/`FreqRange`/`QRange`/`NameTaken` error -- these are a
// client-side pre-check so the UI doesn't wait for a round-trip to disable
// obviously-invalid input.
import type { LibraryEffect } from "../../proto/library";
import { MAX_NAME_BYTES } from "../../proto/library";
import { clamp } from "./math";

export const BAND_FREQ_MIN = 20;
export const BAND_FREQ_MAX = 20_000;
export const BAND_GAIN_MIN = -24;
export const BAND_GAIN_MAX = 24;
export const BAND_Q_MIN = 0.1;
export const BAND_Q_MAX = 20;

export function clampFreqHz(v: number): number {
  return clamp(v, BAND_FREQ_MIN, BAND_FREQ_MAX);
}

export function clampGainDb(v: number): number {
  return clamp(v, BAND_GAIN_MIN, BAND_GAIN_MAX);
}

export function clampQ(v: number): number {
  return clamp(v, BAND_Q_MIN, BAND_Q_MAX);
}

export function utf8ByteLength(s: string): number {
  return new TextEncoder().encode(s).length;
}

export interface NameCheck {
  byteLength: number;
  /** `true` when `byteLength` exceeds `MAX_NAME_BYTES` or another effect already has this (trimmed) name. */
  invalid: boolean;
  reason: "ok" | "tooLong" | "taken" | "empty";
}

/** Mirrors the mock's `nameState`: trims before comparing, `MAX_NAME_BYTES` (16) counter, duplicate check excludes `excludeId`. */
export function checkName(name: string, effects: LibraryEffect[], excludeId: number | null): NameCheck {
  const trimmed = name.trim();
  const byteLength = utf8ByteLength(name);
  if (!trimmed) return { byteLength, invalid: true, reason: "empty" };
  if (byteLength > MAX_NAME_BYTES) return { byteLength, invalid: true, reason: "tooLong" };
  const taken = effects.some((e) => e.id !== excludeId && e.preset.name === trimmed);
  if (taken) return { byteLength, invalid: true, reason: "taken" };
  return { byteLength, invalid: false, reason: "ok" };
}

/** `base` trimmed/suffixed until it doesn't collide with any existing name and fits `MAX_NAME_BYTES` -- mirrors the mock's `uniqueName`. */
export function uniqueName(base: string, existingNames: string[]): string {
  let candidate = base;
  const stem = base.replace(/ \d+$/, "");
  let suffix = 2;
  const names = new Set(existingNames);
  while (names.has(candidate)) {
    candidate = `${stem} ${suffix}`;
    suffix += 1;
  }
  return candidate;
}

export function truncateToNameBudget(base: string): string {
  let s = base;
  while (utf8ByteLength(s) > MAX_NAME_BYTES) s = s.slice(0, -1);
  return s;
}
