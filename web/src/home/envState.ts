// Pure state-selection logic for the companion's top-level screen: which of
// the mock's `STATES` (nochromium/connect/cancelled/busy/oldfw), the Home
// page, or the Home page + unplugged overlay, to show. Kept as a plain
// function of inputs (no DOM/session reads) so state selection is unit-
// testable without a browser or a live `Session`.
import type { SessionPhase } from "../session/session";

export type EnvScreen = { kind: "nochromium" } | { kind: "connect"; note: string } | { kind: "busy" } | { kind: "oldfw" } | { kind: "home"; unplugged: boolean };

export interface EnvStateInputs {
  webUsbSupported: boolean;
  phase: SessionPhase;
  /** True once this page has ever reached `phase === "ready"` this session (survives a later `lost`). */
  everConnected: boolean;
  /** Set after an explicit `requestDevice()` call rejects with the chooser-cancelled error; cleared on the next successful open attempt. */
  chooserCancelled: boolean;
}

/**
 * Resolves the top-level screen. Mirrors the mock's `render()`:
 * `envOk = env==="ok" || env==="unplugged"` -- i.e. once a device has been
 * seen ready, a later `lost` (unplug) keeps showing Home (last-known
 * content, dimmed by the overlay) rather than bouncing back to a full-page
 * "Connect" screen, so effect edits in progress aren't torn down.
 */
export function selectEnvScreen(inputs: EnvStateInputs): EnvScreen {
  if (!inputs.webUsbSupported) return { kind: "nochromium" };

  switch (inputs.phase) {
    case "busy-elsewhere":
      return { kind: "busy" };
    case "incompatible":
      return { kind: "oldfw" };
    case "ready":
      return { kind: "home", unplugged: false };
    case "lost":
      if (inputs.everConnected) return { kind: "home", unplugged: true };
      return { kind: "connect", note: "No device was chosen. Is it plugged in? It shows up as Pico Link." };
    case "idle":
    case "opening":
    case "handshaking":
    default:
      if (inputs.chooserCancelled) {
        return { kind: "connect", note: "No device was chosen. Is it plugged in? It shows up as Pico Link." };
      }
      return { kind: "connect", note: "" };
  }
}
