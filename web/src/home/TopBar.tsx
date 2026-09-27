import type * as React from "react";
import type { HomeSnapshot } from "../proto/telemetry";
import { chromeVolumeText } from "./volume";
import { useIsCompact } from "./useIsCompact";

export type HomeTab = "home" | "fx";

interface TopBarProps {
  connected: boolean;
  usbLabel: string;
  tab: HomeTab;
  onTabChange: (tab: HomeTab) => void;
  /** `null` while no snapshot has arrived yet (mirrors the mock's chrome before first poll). */
  snapshot: HomeSnapshot | null;
  /** The mini live-codec + meter strip (mock's `#mini`), shown on non-Home tabs only. `null`/omitted on Home. */
  mini?: React.ReactNode;
}

/**
 * The device's title bar, grown up (mock's `.topbar`): wordmark, Home/
 * Effects tabs, a USB connection chip, and the volume/BT-glyph/link-dot
 * cluster the device's own chrome shows. Design of record: UMA DESIGN on
 * pico-link-jyhk.8 section on the top bar; mock's `renderChrome`.
 */
export function TopBar({ connected, usbLabel, tab, onTabChange, snapshot, mini }: TopBarProps) {
  const compact = useIsCompact();
  const link = connected && (snapshot?.linkConnected ?? false);
  const vol = snapshot ? chromeVolumeText(snapshot) : { text: "", warn: false };

  return (
    <header
      className="sticky top-0 z-10 flex h-13 items-center border-b border-border bg-card"
      style={{ gap: compact ? 10 : 20, padding: compact ? "0 12px" : "0 20px" }}
      data-testid="topbar"
    >
      <div className="whitespace-nowrap font-bold tracking-[0.01em]" style={{ fontSize: compact ? 15 : 17 }}>
        Pico Link
      </div>
      <nav className="flex h-full gap-1" role="tablist">
        <button
          role="tab"
          aria-selected={tab === "home"}
          disabled={!connected}
          onClick={() => onTabChange("home")}
          data-testid="tab-home"
          style={{ padding: compact ? "0 8px" : "0 14px", fontSize: compact ? 13 : 15 }}
          className={`h-full border-b-[3px] font-semibold disabled:cursor-default disabled:opacity-35 ${tab === "home" ? "border-accent text-foreground" : "border-transparent text-muted-foreground"}`}
        >
          Home
        </button>
        <button
          role="tab"
          aria-selected={tab === "fx"}
          disabled={!connected}
          onClick={() => onTabChange("fx")}
          data-testid="tab-fx"
          style={{ padding: compact ? "0 8px" : "0 14px", fontSize: compact ? 13 : 15 }}
          className={`h-full border-b-[3px] font-semibold disabled:cursor-default disabled:opacity-35 ${tab === "fx" ? "border-accent text-foreground" : "border-transparent text-muted-foreground"}`}
        >
          Effects
        </button>
      </nav>
      <div className="flex-1" />
      {tab === "fx" && connected ? mini : null}
      {!compact ? (
        <span
          className={`whitespace-nowrap rounded-full border border-border px-2.5 py-0.5 text-xs font-semibold ${connected ? "text-foreground" : "text-muted-foreground"}`}
          data-testid="usb-chip"
        >
          {usbLabel}
        </span>
      ) : null}
      <div className="flex items-center font-bold" style={{ gap: compact ? 8 : 14, fontSize: compact ? 13 : 15 }}>
        <span className={`tabular-nums ${vol.warn ? "text-warning" : ""}`} data-testid="chrome-volume">
          {vol.text}
        </span>
        <svg
          width={compact ? 11 : 14}
          height={compact ? 14 : 18}
          viewBox="0 0 14 18"
          className={link ? "text-accent" : "text-muted-foreground"}
          data-testid="chrome-bt"
        >
          <path d="M3 5l8 8-4 4V1l4 4-8 8" fill="none" stroke="currentColor" strokeWidth="2" strokeLinejoin="round" />
        </svg>
        <span className={`inline-block h-2.5 w-2.5 rounded-full ${link ? "bg-success" : "bg-muted-foreground"}`} data-testid="chrome-dot" />
      </div>
    </header>
  );
}
