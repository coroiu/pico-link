import { describe, expect, it } from "vitest";
import { ClockOffsetEstimator } from "./clock";

describe("ClockOffsetEstimator", () => {
  it("returns null before any sample", () => {
    const est = new ClockOffsetEstimator();
    expect(est.offsetMs()).toBeNull();
    expect(est.toDeviceMs(1000)).toBeNull();
  });

  it("takes the minimum offset over the window, not the latest", () => {
    const est = new ClockOffsetEstimator(2000);
    // A slow reply: hostReceive far ahead of uptime -> large offset.
    est.record(1100, 1000); // offset 100
    // A fast reply immediately after: small offset -- should win.
    est.record(1120, 1100); // offset 20
    expect(est.offsetMs()).toBe(20);

    // Another slow one shouldn't move it back up.
    est.record(1200, 1100); // offset 100
    expect(est.offsetMs()).toBe(20);
  });

  it("drops samples older than the window as the host clock advances", () => {
    const est = new ClockOffsetEstimator(2000);
    est.record(0, -20); // offset 20, the eventual minimum
    est.record(3000, 2950); // offset 50, now outside the window relative to the next sample
    est.record(5000, 4950); // offset 50; sample at t=0 is now 5000ms old, evicted
    expect(est.offsetMs()).toBe(50);
  });

  it("toDeviceMs converts a host reading using the current offset", () => {
    const est = new ClockOffsetEstimator();
    est.record(1000, 900); // offset 100
    expect(est.toDeviceMs(1500)).toBe(1400);
  });

  it("reset clears all samples (reboot detection)", () => {
    const est = new ClockOffsetEstimator();
    est.record(1000, 900);
    est.reset();
    expect(est.offsetMs()).toBeNull();
  });
});
