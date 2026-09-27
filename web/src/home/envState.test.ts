import { describe, expect, it } from "vitest";
import { selectEnvScreen } from "./envState";
import type { EnvStateInputs } from "./envState";

function inputs(overrides: Partial<EnvStateInputs>): EnvStateInputs {
  return { webUsbSupported: true, phase: "idle", everConnected: false, chooserCancelled: false, ...overrides };
}

describe("selectEnvScreen", () => {
  it("nochromium wins regardless of phase", () => {
    expect(selectEnvScreen(inputs({ webUsbSupported: false, phase: "ready" }))).toEqual({ kind: "nochromium" });
  });

  it("first visit shows the plain connect screen", () => {
    expect(selectEnvScreen(inputs({ phase: "idle" }))).toEqual({ kind: "connect", note: "" });
  });

  it("a cancelled chooser adds a note", () => {
    expect(selectEnvScreen(inputs({ phase: "idle", chooserCancelled: true }))).toEqual({
      kind: "connect",
      note: "No device was chosen. Is it plugged in? It shows up as Pico Link.",
    });
  });

  it("busy-elsewhere maps to the busy screen", () => {
    expect(selectEnvScreen(inputs({ phase: "busy-elsewhere" }))).toEqual({ kind: "busy" });
  });

  it("incompatible maps to the old-firmware screen", () => {
    expect(selectEnvScreen(inputs({ phase: "incompatible" }))).toEqual({ kind: "oldfw" });
  });

  it("ready shows Home with no overlay", () => {
    expect(selectEnvScreen(inputs({ phase: "ready" }))).toEqual({ kind: "home", unplugged: false });
  });

  it("lost after having been ready keeps showing Home, with the unplugged overlay", () => {
    expect(selectEnvScreen(inputs({ phase: "lost", everConnected: true }))).toEqual({ kind: "home", unplugged: true });
  });

  it("lost before ever being ready falls back to the connect screen", () => {
    expect(selectEnvScreen(inputs({ phase: "lost", everConnected: false }))).toEqual({
      kind: "connect",
      note: "No device was chosen. Is it plugged in? It shows up as Pico Link.",
    });
  });
});
