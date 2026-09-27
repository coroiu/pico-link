// Volume percent + banner logic, ported from `core/src/app/events.rs`'s
// `VolumeState::percent` and `core/src/render/hero.rs`'s `active_banner`
// (design of record: UMA DESIGN on pico-link-jyhk.8, mock
// `.planning/design/mocks/2026-09-27-web-companion-v2-identity.html`).
//
// DEVIATION: the wire `HomeSnapshot` (proto/telemetry.ts) has no
// `CodecStatus::Connected::fallback` reason string -- that field does not
// exist on the telemetry page yet (the mock's own "SBC fallback" state is
// annotated "needs a telemetry flag"). So the FALLBACK banner tier from
// hero.rs's `ActiveBanner` is not reachable here; only MUTED and VOLUME 0
// are implemented, in the same priority order (MUTED outranks VOLUME 0).
import type { HomeSnapshot, VolumeSource } from "../proto/telemetry";

/**
 * `VolumeState::percent` (events.rs:203): `(level * 100 + 63) / 127`, using
 * Rust's truncating **integer** division -- `Math.floor`, not
 * `Math.round`. `0 -> 0`, `127 -> 100`.
 */
export function volumeToPercent(level: number): number {
  return Math.floor((level * 100 + 63) / 127);
}

export interface HomeBanner {
  text: string;
  tone: "warn";
}

/**
 * Resolves the persistent banner slot from a decoded snapshot. `null` means
 * no banner (the common case) -- the caller must reserve the slot's height
 * regardless (mock's `.banner{min-height:1.6em}`) so nothing shifts.
 */
export function computeBanner(snapshot: HomeSnapshot): HomeBanner | null {
  if (!snapshot.linkConnected || !snapshot.volumePresent) return null;

  if (snapshot.volumeMuted) {
    return snapshot.volumeSource === "host" ? { text: "MUTED  Unmute on Mac", tone: "warn" } : { text: "MUTED", tone: "warn" };
  }

  if (volumeToPercent(snapshot.volumeLevel) === 0) {
    return { text: "HEADPHONE VOLUME 0", tone: "warn" };
  }

  return null;
}

/** Title-bar volume readout (mock's `#vol`): "" when absent, "MUTE" when muted, else a percent. */
export function chromeVolumeText(snapshot: HomeSnapshot): { text: string; warn: boolean } {
  if (!snapshot.volumePresent) return { text: "", warn: false };
  if (snapshot.volumeMuted) return { text: "MUTE", warn: true };
  return { text: `${volumeToPercent(snapshot.volumeLevel)}%`, warn: false };
}

export function isHostVolume(source: VolumeSource): boolean {
  return source === "host";
}
