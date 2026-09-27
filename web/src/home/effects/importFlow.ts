// Pure decision logic for the import flow (file / paste / drop), separated
// from `EffectsTab`'s network calls so the "same name -> offer Replace"
// branch (UMA DESIGN section 3, ADA DESIGN section 4's `PARSE_APO`) is
// testable without a `LibraryController`.
import type { OpError } from "../../proto/ops";

export function deriveImportBaseName(filename: string): string {
  const base = filename.replace(/\.txt$/i, "").replace(/ ParametricEQ$/i, "");
  const trimmed = base.trim();
  return trimmed || "Imported";
}

export type ImportPlan =
  | { kind: "createDirect" }
  | { kind: "confirmReplace"; collidesWithId: number; copyName: string }
  | { kind: "full" };

/**
 * `PARSE_APO` never touches the store (design section 4: "so the caller
 * decides") -- given its `collidesWith`/reply, decide what the UI should
 * do next: create outright, ask Replace-vs-Copy, or refuse because the
 * library is already full (8/8, per `pico-link-jyhk.14`'s "no browser
 * library" -- 8 on-device slots, checked against `maxEffects`).
 */
export function planImport(collidesWith: number, effectCount: number, maxEffects: number, copyName: string): ImportPlan {
  if (collidesWith !== 0) return { kind: "confirmReplace", collidesWithId: collidesWith, copyName };
  if (effectCount >= maxEffects) return { kind: "full" };
  return { kind: "createDirect" };
}

/** For the "Import as copy" branch of an already-full library. */
export function canImportCopy(effectCount: number, maxEffects: number): boolean {
  return effectCount < maxEffects;
}

const OP_ERROR_MESSAGES: Partial<Record<OpError, string>> = {
  4: "8 of 8 effects: delete one first",
  5: "That effect no longer exists.",
  6: "Someone else changed this effect first. Reload and try again.",
  7: "The device's own editor is open. Close it there first.",
  8: "That name is already used by another effect.",
  9: "That name isn't valid.",
  10: "This effect's data is from a newer firmware version.",
  11: "Too many bands (the dongle holds up to 10).",
  12: "Unsupported band type.",
  13: "Gain is out of range.",
  14: "Frequency is out of range.",
  15: "Q is out of range.",
  16: "Preamp is out of range.",
  17: "Unknown headphones.",
  18: "Couldn't parse that file. Expected lines like: Filter 1: ON PK Fc 105 Hz Gain 5.4 dB Q 0.70",
  19: "That file is too large.",
};

export function describeOpError(error: OpError): string {
  return OP_ERROR_MESSAGES[error] ?? "The dongle rejected that request.";
}
