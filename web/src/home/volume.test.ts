import { describe, expect, it } from "vitest";
import { chromeVolumeText, computeBanner, volumeToPercent } from "./volume";
import { emptyHomeSnapshot } from "../proto/telemetry";
import type { HomeSnapshot } from "../proto/telemetry";

function connected(overrides: Partial<HomeSnapshot> = {}): HomeSnapshot {
  return { ...emptyHomeSnapshot(), linkConnected: true, volumePresent: true, volumeLevel: 100, ...overrides };
}

describe("volumeToPercent", () => {
  it("maps the two endpoints exactly", () => {
    expect(volumeToPercent(0)).toBe(0);
    expect(volumeToPercent(127)).toBe(100);
  });

  it("matches the fixture's sampled values", () => {
    expect(volumeToPercent(32)).toBe(25);
    expect(volumeToPercent(63)).toBe(50);
    expect(volumeToPercent(64)).toBe(50);
    expect(volumeToPercent(65)).toBe(51);
    expect(volumeToPercent(100)).toBe(79);
    expect(volumeToPercent(126)).toBe(99);
  });
});

describe("computeBanner", () => {
  it("shows nothing when no volume reading is present", () => {
    expect(computeBanner(connected({ volumePresent: false }))).toBeNull();
  });

  it("shows nothing when not linked, even if muted", () => {
    expect(computeBanner(connected({ linkConnected: false, volumeMuted: true, volumeSource: "host" }))).toBeNull();
  });

  it("MUTED (host) outranks a simultaneous VOLUME 0 reading", () => {
    const banner = computeBanner(connected({ volumeMuted: true, volumeSource: "host", volumeLevel: 0 }));
    expect(banner).toEqual({ text: "MUTED  Unmute on Mac", tone: "warn" });
  });

  it("MUTED from a non-host source has no remedy text", () => {
    const banner = computeBanner(connected({ volumeMuted: true, volumeSource: "sink" }));
    expect(banner).toEqual({ text: "MUTED", tone: "warn" });
  });

  it("VOLUME 0 fires when not muted but the level rounds to 0%", () => {
    const banner = computeBanner(connected({ volumeLevel: 0, volumeMuted: false }));
    expect(banner).toEqual({ text: "HEADPHONE VOLUME 0", tone: "warn" });
  });

  it("no banner at a normal, unmuted, nonzero volume", () => {
    expect(computeBanner(connected({ volumeLevel: 96, volumeMuted: false }))).toBeNull();
  });
});

describe("chromeVolumeText", () => {
  it("is blank when absent", () => {
    expect(chromeVolumeText(connected({ volumePresent: false }))).toEqual({ text: "", warn: false });
  });

  it("reads MUTE, warned, when muted", () => {
    expect(chromeVolumeText(connected({ volumeMuted: true }))).toEqual({ text: "MUTE", warn: true });
  });

  it("reads a percent otherwise", () => {
    expect(chromeVolumeText(connected({ volumeLevel: 100 }))).toEqual({ text: "79%", warn: false });
  });
});
