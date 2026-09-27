import { describe, expect, it } from "vitest";
import { medianRtt, nextPollIntervalMs, pushRttSample } from "./pacing";

describe("medianRtt", () => {
  it("is 0 for an empty window", () => {
    expect(medianRtt([])).toBe(0);
  });
  it("is the middle value for an odd-length window", () => {
    expect(medianRtt([5, 1, 3])).toBe(3);
  });
  it("averages the two middle values for an even-length window", () => {
    expect(medianRtt([1, 2, 3, 4])).toBe(2.5);
  });
});

describe("pushRttSample", () => {
  it("caps the window at the given size, dropping the oldest", () => {
    const samples = [1, 2, 3];
    pushRttSample(samples, 4, 3);
    expect(samples).toEqual([2, 3, 4]);
  });
});

describe("nextPollIntervalMs", () => {
  const opts = { pollIntervalMs: 33, backoffIntervalMs: 50, rttBackoffThresholdMs: 20 };

  it("stays at the fast interval when RTT is healthy", () => {
    expect(nextPollIntervalMs([5, 6, 7], opts)).toBe(33);
  });

  it("backs off once the median RTT exceeds the threshold", () => {
    expect(nextPollIntervalMs([25, 30, 40], opts)).toBe(50);
  });

  it("stays at the fast interval with no samples yet", () => {
    expect(nextPollIntervalMs([], opts)).toBe(33);
  });
});
