import * as React from "react";
import type { HomeSnapshot } from "../proto/telemetry";
import { useTheme } from "../theme/ThemeProvider";
import { levelToSegmentCount, METER_SEGMENT_COUNT, segmentZone } from "../meter/segments";

interface MeterColors {
  card: string;
  border: string;
  meterSafe: string;
  meterWarn: string;
  meterErr: string;
}

const FALLBACK_COLORS: MeterColors = {
  card: "#191821",
  border: "#313142",
  meterSafe: "#f7f7f7",
  meterWarn: "#ffb221",
  meterErr: "#ff595a",
};

function readMeterColors(): MeterColors {
  if (typeof document === "undefined") return FALLBACK_COLORS;
  const style = getComputedStyle(document.documentElement);
  const read = (name: string, fallback: string) => style.getPropertyValue(name).trim() || fallback;
  return {
    card: read("--card", FALLBACK_COLORS.card),
    border: read("--border", FALLBACK_COLORS.border),
    meterSafe: read("--meter-safe", FALLBACK_COLORS.meterSafe),
    meterWarn: read("--meter-warn", FALLBACK_COLORS.meterWarn),
    meterErr: read("--meter-err", FALLBACK_COLORS.meterErr),
  };
}

function zoneColor(colors: MeterColors, index: number): string {
  switch (segmentZone(index)) {
    case "err":
      return colors.meterErr;
    case "warn":
      return colors.meterWarn;
    default:
      return colors.meterSafe;
  }
}

const SEGMENT_HEIGHT = 2;
const SEGMENT_GAP = 1;
const COLUMN_WIDTH = 14;
const COLUMN_GAP = 6;
const DRAWN_HEIGHT = METER_SEGMENT_COUNT * SEGMENT_HEIGHT + (METER_SEGMENT_COUNT - 1) * SEGMENT_GAP;

/**
 * DEMO: web-only stereo OUT-meter, straight to canvas via
 * requestAnimationFrame, reading the latest snapshot out of a ref -- never
 * through React state (design requirement, pico-link-jyhk.10: "Hot path:
 * meters ... draw on canvas via refs/requestAnimationFrame, never through
 * React state at 30Hz"). 48 segments per channel (design
 * `.planning/design/2026-09-27-visual-identity.md` §6), 1 dB/segment, same
 * zone proportions as the device meter -- geometry and thresholds are
 * web-owned (`../meter/segments`), not imported from `core` (the device
 * meter is reverting to 16 segments separately; the two are intentionally
 * decoupled).
 *
 * This draws `peakL`/`peakR` directly (no ballistics/hold yet -- this is
 * still a debug meter, not Home's full visual design per Uma's pass,
 * jyhk.12).
 *
 * Theme colours are read via `getComputedStyle` once per theme change (a
 * `useTheme()`-triggered re-render), cached in a ref for the rAF loop to
 * read -- review follow-up on pico-link-jyhk.11: "MeterCanvas calls
 * getComputedStyle per frame - cache on theme change."
 */
export function MeterCanvas({ snapshotRef }: { snapshotRef: React.RefObject<HomeSnapshot | null> }) {
  const canvasRef = React.useRef<HTMLCanvasElement | null>(null);
  const { theme } = useTheme();
  const colorsRef = React.useRef<MeterColors>(FALLBACK_COLORS);

  React.useEffect(() => {
    colorsRef.current = readMeterColors();
  }, [theme]);

  React.useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    let raf = 0;
    const draw = () => {
      const { width, height } = canvas;
      ctx.clearRect(0, 0, width, height);

      const colors = colorsRef.current;
      ctx.fillStyle = colors.card;
      ctx.fillRect(0, 0, width, height);

      const snapshot = snapshotRef.current;
      const levels = snapshot?.levelPresent ? [snapshot.peakL, snapshot.peakR] : [0, 0];
      const topPad = 6;
      const bottom = topPad + DRAWN_HEIGHT;

      levels.forEach((level, col) => {
        const filled = levelToSegmentCount(level);
        const x = col * (COLUMN_WIDTH + COLUMN_GAP) + COLUMN_GAP / 2;
        for (let i = 0; i < METER_SEGMENT_COUNT; i++) {
          const y = bottom - (i + 1) * SEGMENT_HEIGHT - i * SEGMENT_GAP;
          ctx.fillStyle = i < filled ? zoneColor(colors, i) : colors.border;
          ctx.fillRect(x, y, COLUMN_WIDTH, SEGMENT_HEIGHT);
        }
      });

      raf = requestAnimationFrame(draw);
    };
    raf = requestAnimationFrame(draw);
    return () => cancelAnimationFrame(raf);
  }, [snapshotRef]);

  return (
    <canvas
      ref={canvasRef}
      width={2 * COLUMN_WIDTH + 3 * COLUMN_GAP}
      height={DRAWN_HEIGHT + 12}
      className="rounded-md border border-border"
      data-testid="meter-canvas"
    />
  );
}
