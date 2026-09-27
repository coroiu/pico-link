import type { ReactNode } from "react";
import { buildFaultRows, formatAge } from "./faults";
import type { FaultRow } from "./faults";
import type { FaultKey, DecodedFault } from "../proto/telemetry";

const GLYPH_PATH: Record<FaultRow["glyph"], ReactNode> = {
  up: <path d="M7 2 L13 12 L1 12 Z" fill="currentColor" />,
  down: <path d="M1 2 L13 2 L7 12 Z" fill="currentColor" />,
  square: <rect x="3" y="3" width="8" height="8" fill="currentColor" />,
};

interface FaultStripProps {
  faults: Record<FaultKey, DecodedFault | null>;
  nowMs: number;
}

/**
 * The audio fault strip (design `.planning/design/2026-09-07-home-fault-
 * strip.md`, mock's `#hFaults`): device-authored fault names, value + "N s
 * ago", newest first, capped at 4 rows, reserving fixed height so a new
 * fault never shifts anything above it (mock: `.faults{min-height:calc(4 *
 * 30px)}`).
 */
export function FaultStrip({ faults, nowMs }: FaultStripProps) {
  const rows = buildFaultRows(faults, nowMs);

  return (
    <div className="col-span-full" data-testid="fault-strip">
      <div className="mt-3.5 mb-2.5 h-px bg-border md:mt-7" />
      <div className="flex min-h-[120px] flex-col-reverse" data-testid="fault-rows">
        {rows.map((row) => {
          const tone = row.severity === "audible" ? "text-destructive" : "text-warning";
          const dim = row.tier === "recent" ? "opacity-60" : "";
          return (
            <div
              key={row.key}
              className={`grid h-[30px] grid-cols-[18px_minmax(0,1fr)_auto_44px] items-center gap-3 text-[15px] font-bold tracking-[0.04em] ${tone} ${dim}`}
              data-testid={`fault-row-${row.key}`}
              title={`First seen ${formatAge(row.ageMs)}`}
            >
              <svg width="14" height="14" viewBox="0 0 14 14">
                {GLYPH_PATH[row.glyph]}
              </svg>
              <span>{row.name}</span>
              <span className={`text-[13px] font-normal tracking-normal ${row.tier === "recent" ? "text-[color:var(--text-3)]" : "text-muted-foreground"}`}>
                {row.valueText ? `${row.valueText} · ` : ""}
                {formatAge(row.ageMs)}
              </span>
              <span className="text-right">{row.count > 1 ? `x${row.count > 99 ? "99+" : row.count}` : ""}</span>
            </div>
          );
        })}
      </div>
    </div>
  );
}
