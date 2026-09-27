// DEMO: the web meter's own 48-segment geometry/zone table -- deliberately
// NOT sourced from `core/src/render/theme.rs`'s `VERTICAL_METER_*` constants
// (orchestrator correction, pico-link-5ful.2: the device meter is reverting
// to 16 segments in a separate bead, so web must own this independently
// rather than import a device constant that is about to change under it).
//
// Geometry and zones mirror the *design*, not the device build:
// `.planning/design/2026-09-27-visual-identity.md` §6 -- 48 segments over
// -48..0 dBFS, exactly 1 dB/segment. Amber (warn) zone starts at -18 dBFS
// (indices 30..41, the EBU alignment level), red (err) zone at -6 dBFS
// (indices 42..47); the bottom 30 (0..29) are the safe zone. Same 5/8, 2/8,
// 1/8 proportions the device meter has always used.
//
// Threshold table: `round(255 * 10 ** ((-48 + i + 1) / 20))` for i in 0..48,
// computed here (not hardcoded) since JS has `Math.pow`/no `no_std`
// constraint -- this is the exact formula the design doc gives for the
// device's own (now-independent) table.
export const METER_SEGMENT_COUNT = 48;

function computeThresholds(): readonly number[] {
  const thresholds: number[] = [];
  for (let i = 0; i < METER_SEGMENT_COUNT; i++) {
    const dbfs = -48 + i + 1;
    thresholds.push(Math.round(255 * 10 ** (dbfs / 20)));
  }
  return thresholds;
}

/** 48 entries, quietest (index 0) first, each the minimum linear 0-255 level that lights that segment. */
export const METER_DBFS_THRESHOLDS: readonly number[] = computeThresholds();

/** Maps a linear 0-255 level to a lit segment count (0..=48). */
export function levelToSegmentCount(level: number): number {
  let count = 0;
  for (const threshold of METER_DBFS_THRESHOLDS) {
    if (level >= threshold) count++;
  }
  return count;
}

export type MeterZone = "safe" | "warn" | "err";

/** Which zone segment `index` (0-based, quietest first) belongs to. Red = top 6 (-6..0 dBFS), amber = next 12 (-18..-6 dBFS), safe = the rest. */
export function segmentZone(index: number): MeterZone {
  if (index >= METER_SEGMENT_COUNT - 6) return "err";
  if (index >= METER_SEGMENT_COUNT - 18) return "warn";
  return "safe";
}
