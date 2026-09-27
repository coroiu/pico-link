import * as React from "react";
import type { HomeSnapshot } from "../../proto/telemetry";
import type { OutLevelBallistics } from "../../meter/ballistics";
import type { ClockOffsetEstimator } from "../../session/clock";
import { computeChannelView } from "../meterView";

interface MiniLiveStripProps {
  text: HomeSnapshot;
  snapshotRef: React.RefObject<HomeSnapshot | null>;
  ballistics: OutLevelBallistics;
  clock: ClockOffsetEstimator;
  onClickHome: () => void;
}

/**
 * The title bar's small live-codec + 2-column meter strip, shown on every
 * non-Home tab (design of record: UMA DESIGN on pico-link-jyhk.8 section 1
 * -- "so a Bypass A/B is visibly confirmed by the post-DSP meter without
 * leaving the editor"; mirrors the mock's `#mini`). Clicking it goes Home.
 * The two bars are driven directly by refs from `requestAnimationFrame`
 * (same hot-path rule as `OutMeter`), not React state.
 */
export function MiniLiveStrip({ text, snapshotRef, ballistics, clock, onClickHome }: MiniLiveStripProps) {
  const barLRef = React.useRef<HTMLDivElement | null>(null);
  const barRRef = React.useRef<HTMLDivElement | null>(null);

  React.useEffect(() => {
    let raf = 0;
    const draw = () => {
      const nowMs = clock.toDeviceMs(performance.now());
      const query = nowMs === null ? { stale: true, displayedPeakL: 0, displayedPeakR: 0, holdL: 0, holdR: 0 } : ballistics.query(nowMs);
      const linkOk = snapshotRef.current?.linkConnected ?? false;
      const left = linkOk ? computeChannelView(query, query.displayedPeakL, query.holdL) : { live: false, filled: 0, holdIndex: null };
      const right = linkOk ? computeChannelView(query, query.displayedPeakR, query.holdR) : { live: false, filled: 0, holdIndex: null };
      if (barLRef.current) barLRef.current.style.width = `${left.live ? (left.filled / 48) * 100 : 0}%`;
      if (barRRef.current) barRRef.current.style.width = `${right.live ? (right.filled / 48) * 100 : 0}%`;
      raf = requestAnimationFrame(draw);
    };
    raf = requestAnimationFrame(draw);
    return () => cancelAnimationFrame(raf);
  }, [snapshotRef, ballistics, clock]);

  const codec = text.linkConnected ? text.codecWord || "–" : "NO LINK";
  const kbps = text.linkConnected && text.kbpsIsLive ? `${text.kbps}` : "";

  return (
    <button
      type="button"
      onClick={onClickHome}
      title="Live from the dongle. Click for Home."
      className="flex items-center gap-2 rounded-md border border-border px-2 py-1 text-xs font-bold"
      data-testid="mini-live-strip"
    >
      <b className={text.linkConnected ? "" : "text-destructive"}>{codec}</b>
      <span className="tabular-nums text-muted-foreground">{kbps}</span>
      <div className="flex h-3.5 w-8 flex-col justify-center gap-0.5">
        <div className="h-1 w-full overflow-hidden rounded-sm bg-border">
          <div ref={barLRef} className="h-full bg-accent" style={{ width: 0 }} />
        </div>
        <div className="h-1 w-full overflow-hidden rounded-sm bg-border">
          <div ref={barRRef} className="h-full bg-accent" style={{ width: 0 }} />
        </div>
      </div>
    </button>
  );
}
