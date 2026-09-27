// Pure per-channel render-state for the Home OUT meter: absent-when-stale
// (design section 15 / hero.rs doc comments: "absent, never frozen and
// never faked") plus the filled-segment count and peak-hold segment index,
// derived from `OutLevelBallistics.query()`. Kept separate from the canvas
// drawing code (`OutMeter.tsx`) so staleness/segment-mapping logic is
// testable without a `<canvas>`.
import { levelToSegmentCount, METER_SEGMENT_COUNT, segmentZone } from "../meter/segments";
import type { MeterZone } from "../meter/segments";
import type { LevelQuery } from "../meter/ballistics";

export interface MeterChannelView {
  /** `false` means draw nothing for this channel at all (stale/no data). */
  live: boolean;
  /** Count of lit segments, quietest-first, 0..=48. */
  filled: number;
  /** Index (0-based) of the peak-hold marker segment, or `null` if the hold is at zero. */
  holdIndex: number | null;
}

const ABSENT: MeterChannelView = { live: false, filled: 0, holdIndex: null };

export function computeChannelView(query: LevelQuery, peak: number, hold: number): MeterChannelView {
  if (query.stale) return ABSENT;
  return {
    live: true,
    filled: levelToSegmentCount(peak),
    holdIndex: hold > 0 ? levelToSegmentCount(hold) - 1 : null,
  };
}

export { METER_SEGMENT_COUNT, segmentZone };
export type { MeterZone };
