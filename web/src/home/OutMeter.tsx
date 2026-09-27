import * as React from "react";
import type { HomeSnapshot } from "../proto/telemetry";
import type { OutLevelBallistics } from "../meter/ballistics";
import type { ClockOffsetEstimator } from "../session/clock";
import { computeChannelView, METER_SEGMENT_COUNT, segmentZone } from "./meterView";

interface OutMeterProps {
  snapshotRef: React.RefObject<HomeSnapshot | null>;
  ballistics: OutLevelBallistics;
  clock: ClockOffsetEstimator;
  /** Narrower geometry under the mock's 620px breakpoint. */
  compact: boolean;
}

interface Colors {
  divider: string;
  safe: string;
  warn: string;
  err: string;
  brandBright: string;
}

const FALLBACK: Colors = { divider: "#313142", safe: "#f7f7f7", warn: "#ffb221", err: "#ff595a", brandBright: "#948aff" };

function readColors(): Colors {
  if (typeof document === "undefined") return FALLBACK;
  const style = getComputedStyle(document.documentElement);
  const read = (name: string, fallback: string) => style.getPropertyValue(name).trim() || fallback;
  return {
    divider: read("--border", FALLBACK.divider),
    safe: read("--meter-safe", FALLBACK.safe),
    warn: read("--meter-warn", FALLBACK.warn),
    err: read("--meter-err", FALLBACK.err),
    brandBright: read("--accent", FALLBACK.brandBright),
  };
}

function zoneColor(colors: Colors, index: number): string {
  switch (segmentZone(index)) {
    case "err":
      return colors.err;
    case "warn":
      return colors.warn;
    default:
      return colors.safe;
  }
}

/**
 * The Home OUT level meter (design of record: UMA DESIGN on
 * pico-link-jyhk.8, mock `.meter`): a vertical 48-segment bar per channel,
 * quietest segment at the bottom, a distinct-coloured peak-hold marker, and
 * "absent, never frozen" staleness (`computeChannelView`'s `live` flag) --
 * the whole column blanks rather than showing a stuck last reading.
 *
 * Per the design's hot-path rule ("meters ... never through React state at
 * 30Hz"), the 48+48 segment `<div>`s are created once by React and then
 * driven directly via refs from a `requestAnimationFrame` loop -- no
 * per-frame React render.
 */
export function OutMeter({ snapshotRef, ballistics, clock, compact }: OutMeterProps) {
  const segRefsL = React.useRef<Array<HTMLDivElement | null>>([]);
  const segRefsR = React.useRef<Array<HTMLDivElement | null>>([]);
  const labRef = React.useRef<HTMLDivElement | null>(null);
  const lrRef = React.useRef<HTMLDivElement | null>(null);
  const colorsRef = React.useRef<Colors>(readColors());

  React.useEffect(() => {
    let raf = 0;
    const draw = () => {
      const colors = (colorsRef.current = readColors());
      const nowMs = clock.toDeviceMs(performance.now());
      const query = nowMs === null ? { stale: true, displayedPeakL: 0, displayedPeakR: 0, holdL: 0, holdR: 0 } : ballistics.query(nowMs);
      const snapshot = snapshotRef.current;
      const linkOk = snapshot?.linkConnected ?? false;

      const left = linkOk ? computeChannelView(query, query.displayedPeakL, query.holdL) : { live: false, filled: 0, holdIndex: null };
      const right = linkOk ? computeChannelView(query, query.displayedPeakR, query.holdR) : { live: false, filled: 0, holdIndex: null };
      const live = left.live || right.live;

      if (labRef.current) labRef.current.style.visibility = live ? "visible" : "hidden";
      if (lrRef.current) lrRef.current.style.visibility = live ? "visible" : "hidden";

      for (const [channel, refs] of [
        [left, segRefsL.current],
        [right, segRefsR.current],
      ] as const) {
        for (let i = 0; i < METER_SEGMENT_COUNT; i++) {
          const el = refs[i];
          if (!el) continue;
          if (!channel.live) {
            el.style.background = "transparent";
            continue;
          }
          if (channel.holdIndex === i) {
            el.style.background = i === METER_SEGMENT_COUNT - 1 ? colors.err : colors.brandBright;
          } else {
            el.style.background = i < channel.filled ? zoneColor(colors, i) : colors.divider;
          }
        }
      }

      raf = requestAnimationFrame(draw);
    };
    raf = requestAnimationFrame(draw);
    return () => cancelAnimationFrame(raf);
  }, [snapshotRef, ballistics, clock]);

  const colHeight = compact ? 170 : 288;
  const colWidth = compact ? 22 : 32;
  const colGap = compact ? 6 : 10;

  return (
    <div className="grid justify-items-center gap-2 pt-1.5" data-testid="out-meter">
      <div ref={labRef} className="text-xs font-bold tracking-[0.14em] text-muted-foreground">
        OUT
      </div>
      <div className="flex" style={{ gap: colGap, height: colHeight }}>
        {([segRefsL, segRefsR] as const).map((refs, colIdx) => (
          <div key={colIdx} className="flex flex-col-reverse gap-px" style={{ width: colWidth, height: colHeight }} data-testid={`meter-col-${colIdx === 0 ? "l" : "r"}`}>
            {Array.from({ length: METER_SEGMENT_COUNT }, (_, i) => (
              <div
                key={i}
                ref={(el) => {
                  refs.current[i] = el;
                }}
                className="flex-1 bg-border"
              />
            ))}
          </div>
        ))}
      </div>
      <div ref={lrRef} className="flex" style={{ gap: colGap }}>
        <span className="text-center text-xs font-bold text-muted-foreground" style={{ width: colWidth }}>
          L
        </span>
        <span className="text-center text-xs font-bold text-muted-foreground" style={{ width: colWidth }}>
          R
        </span>
      </div>
    </div>
  );
}
