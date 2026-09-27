import * as React from "react";
import type { HomeSnapshot } from "../proto/telemetry";
import { useTheme } from "../theme/ThemeProvider";

interface MeterColors {
  card: string;
  primary: string;
  accent: string;
}

const FALLBACK_COLORS: MeterColors = { card: "#19203a", primary: "#195dde", accent: "#3a82f7" };

function readMeterColors(): MeterColors {
  if (typeof document === "undefined") return FALLBACK_COLORS;
  const style = getComputedStyle(document.documentElement);
  return {
    card: style.getPropertyValue("--card").trim() || FALLBACK_COLORS.card,
    primary: style.getPropertyValue("--primary").trim() || FALLBACK_COLORS.primary,
    accent: style.getPropertyValue("--accent").trim() || FALLBACK_COLORS.accent,
  };
}

/**
 * Renders the stereo peak/RMS bars straight to a canvas via
 * requestAnimationFrame, reading the latest snapshot out of a ref --
 * never through React state (design requirement, pico-link-jyhk.10: "Hot
 * path: meters ... draw on canvas via refs/requestAnimationFrame, never
 * through React state at 30Hz"). This is a placeholder debug meter, not
 * Home's real visual design -- that's Uma's pass on jyhk.12.
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
      const barW = width / 4;
      const bars: Array<[number, string]> = snapshot?.levelPresent
        ? [
            [snapshot.peakL, colors.primary],
            [snapshot.rmsL, colors.accent],
            [snapshot.peakR, colors.primary],
            [snapshot.rmsR, colors.accent],
          ]
        : [];

      bars.forEach(([value, color], i) => {
        const h = (value / 255) * height;
        ctx.fillStyle = color;
        ctx.fillRect(i * barW + 4, height - h, barW - 8, h);
      });

      raf = requestAnimationFrame(draw);
    };
    raf = requestAnimationFrame(draw);
    return () => cancelAnimationFrame(raf);
  }, [snapshotRef]);

  return <canvas ref={canvasRef} width={160} height={120} className="rounded-md border border-border" data-testid="meter-canvas" />;
}
