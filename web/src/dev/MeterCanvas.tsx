import * as React from "react";
import type { HomeSnapshot } from "../proto/telemetry";

/**
 * Renders the stereo peak/RMS bars straight to a canvas via
 * requestAnimationFrame, reading the latest snapshot out of a ref --
 * never through React state (design requirement, pico-link-jyhk.10: "Hot
 * path: meters ... draw on canvas via refs/requestAnimationFrame, never
 * through React state at 30Hz"). This is a placeholder debug meter, not
 * Home's real visual design -- that's Uma's pass on jyhk.12.
 */
export function MeterCanvas({ snapshotRef }: { snapshotRef: React.RefObject<HomeSnapshot | null> }) {
  const canvasRef = React.useRef<HTMLCanvasElement | null>(null);

  React.useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    let raf = 0;
    const draw = () => {
      const { width, height } = canvas;
      ctx.clearRect(0, 0, width, height);

      const style = getComputedStyle(document.documentElement);
      ctx.fillStyle = style.getPropertyValue("--card") || "#19203a";
      ctx.fillRect(0, 0, width, height);

      const snapshot = snapshotRef.current;
      const barW = width / 4;
      const bars: Array<[number, string]> = snapshot?.levelPresent
        ? [
            [snapshot.peakL, style.getPropertyValue("--primary") || "#195dde"],
            [snapshot.rmsL, style.getPropertyValue("--accent") || "#3a82f7"],
            [snapshot.peakR, style.getPropertyValue("--primary") || "#195dde"],
            [snapshot.rmsR, style.getPropertyValue("--accent") || "#3a82f7"],
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
