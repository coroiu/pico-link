import { describe, expect, it } from "vitest";
import { computeChannelView } from "./meterView";
import type { LevelQuery } from "../meter/ballistics";

function query(overrides: Partial<LevelQuery>): LevelQuery {
  return { stale: false, displayedPeakL: 0, displayedPeakR: 0, holdL: 0, holdR: 0, ...overrides };
}

describe("computeChannelView", () => {
  it("blanks entirely when the query is stale, regardless of the peak/hold values passed in", () => {
    const view = computeChannelView(query({ stale: true }), 255, 255);
    expect(view).toEqual({ live: false, filled: 0, holdIndex: null });
  });

  it("is live and maps peak to a segment count when fresh", () => {
    const view = computeChannelView(query({ stale: false }), 255, 0);
    expect(view.live).toBe(true);
    expect(view.filled).toBe(48);
  });

  it("has no hold marker when hold is zero", () => {
    const view = computeChannelView(query({ stale: false }), 0, 0);
    expect(view.holdIndex).toBeNull();
  });

  it("places the hold marker at the segment the hold value lights", () => {
    const view = computeChannelView(query({ stale: false }), 0, 255);
    expect(view.holdIndex).toBe(47);
  });
});
