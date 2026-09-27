// The Home fault strip's row model: freshness tiers, retirement, sorting,
// row cap and text formatting -- a port of `core/src/app/fault.rs`'s
// `FaultLog::is_live`/`has_visible_entry` and `core/src/render/hero.rs`'s
// fault-strip drawing (row cap `FAULT_ROW_CAP = 4`, glyph/severity tables in
// `FaultKey`). Design of record: UMA DESIGN on pico-link-jyhk.8, mock
// `.planning/design/mocks/2026-09-27-web-companion-v2-identity.html`'s
// `renderHome`'s fault-row block.
//
// All ages/tiers are computed against the *device* clock (`HomeSnapshot.
// uptimeMs`), not host wall-clock -- both `firstSeenMs`/`lastSeenMs` and
// `uptimeMs` are the same clock domain, so no offset estimation is needed
// here (unlike the meter, which must interpolate between polls).
import type { DecodedFault, FaultKey } from "../proto/telemetry";
import { FAULT_KEYS } from "../proto/telemetry";
import { FAULT_LIVE_WINDOW_MS, FAULT_RETIRE_MS } from "../meter/constants";

export const FAULT_ROW_CAP = 4;

/** `<=16 char, uppercase ASCII` names -- `FaultKey::name` (fault.rs:33-40). */
export const FAULT_NAMES: Record<FaultKey, string> = {
  buf_starved: "BUF STARVED",
  buf_overflow: "BUF OVERFLOW",
  usb_supply_low: "USB SUPPLY LOW",
  air_congested: "AIR CONGESTED",
  air_link_lost: "AIR LINK LOST",
  enc_resync: "ENC RESYNC",
};

/** `FaultKey::severity` (fault.rs:97-102): red (audible) vs amber (concealed). */
export const FAULT_SEVERITY: Record<FaultKey, "audible" | "concealed"> = {
  buf_starved: "audible",
  buf_overflow: "audible",
  usb_supply_low: "concealed",
  air_congested: "concealed",
  air_link_lost: "audible",
  enc_resync: "concealed",
};

/** `FaultKey::glyph` (fault.rs:71-76): the drawn primitive class. */
export const FAULT_GLYPH: Record<FaultKey, "up" | "down" | "square"> = {
  buf_starved: "down",
  buf_overflow: "up",
  usb_supply_low: "down",
  air_congested: "square",
  air_link_lost: "square",
  enc_resync: "square",
};

export type FaultTier = "live" | "recent";

export interface FaultRow {
  key: FaultKey;
  name: string;
  severity: "audible" | "concealed";
  glyph: "up" | "down" | "square";
  tier: FaultTier;
  count: number;
  ageMs: number;
  valueText: string;
}

function faultValueText(fault: DecodedFault): string {
  switch (fault.value.kind) {
    case "ratio":
      return `${Math.round(fault.value.value / 2.56)}%`;
    case "count":
      return `${fault.value.value}`;
    case "millis":
      return `min fill ${fault.value.value} ms`;
    default:
      return "";
  }
}

/**
 * Builds the fault strip's visible rows at device time `nowMs`: retired
 * entries (`now - lastSeen >= FAULT_RETIRE_MS`) dropped, sorted first-seen
 * descending (mock: newest-first, `.faults{flex-direction:column-reverse}`
 * lays them out growing upward from that order), capped at
 * `FAULT_ROW_CAP`. `age`/`tier` come from `lastSeenMs`, matching
 * `FaultLog::is_live`'s `< FAULT_LIVE_WINDOW_MS` test.
 */
export function buildFaultRows(faults: Record<FaultKey, DecodedFault | null>, nowMs: number): FaultRow[] {
  const rows: Array<FaultRow & { firstSeenMs: number }> = [];
  for (const key of FAULT_KEYS) {
    const entry = faults[key];
    if (!entry) continue;
    const age = Math.max(0, nowMs - entry.lastSeenMs);
    if (age >= FAULT_RETIRE_MS) continue;
    rows.push({
      key,
      name: FAULT_NAMES[key],
      severity: FAULT_SEVERITY[key],
      glyph: FAULT_GLYPH[key],
      tier: age < FAULT_LIVE_WINDOW_MS ? "live" : "recent",
      count: entry.count,
      ageMs: age,
      valueText: faultValueText(entry),
      firstSeenMs: entry.firstSeenMs,
    });
  }
  rows.sort((a, b) => b.firstSeenMs - a.firstSeenMs);
  return rows.slice(0, FAULT_ROW_CAP).map(({ firstSeenMs: _firstSeenMs, ...row }) => row);
}

/** "N s ago" / "N min ago" (mock's `ago()`). */
export function formatAge(ms: number): string {
  const totalSeconds = Math.round(ms / 1000);
  if (totalSeconds < 60) return `${totalSeconds} s ago`;
  return `${Math.round(totalSeconds / 60)} min ago`;
}
