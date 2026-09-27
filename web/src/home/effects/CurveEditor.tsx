import * as React from "react";
import type { Band } from "../../proto/library";
import {
  DB_RANGE,
  DEFAULT_PAD,
  bandFreqHz,
  bandGainDb,
  bandQ,
  biquadForBand,
  clamp,
  computeDrag,
  computeNudge,
  computeWheelQ,
  dbToY,
  fmtFreqLabel,
  freqToX,
  hitTestBand,
  magnitudeDb,
  responseDb,
} from "./math";
import type { CurveGeom, NudgeKey } from "./math";

interface CurveEditorProps {
  bands: Band[];
  selected: number;
  bypass: boolean;
  onSelect: (index: number) => void;
  /** Replaces one band's freq/gain (drag, dblclick-add uses `onAdd` instead). */
  onChangeBand: (index: number, patch: Partial<Pick<Band, "freqHalfHz" | "gainCdb" | "qMilli">>) => void;
  onAddBand: (freqHz: number, gainDb: number) => void;
  onRemoveBand: (index: number) => void;
  onCycle: (dir: 1 | -1) => void;
}

function readCssColor(name: string, fallback: string): string {
  if (typeof document === "undefined") return fallback;
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

const N_CURVE_STEPS = 240;

/**
 * The Effects editor's frequency-response curve: log 20Hz-20kHz x-axis,
 * +-15dB y-axis, composite response line plus the selected band's own
 * shape filled underneath. Drag = freq+gain (Shift: fine), wheel = Q,
 * double-click empty space adds a peak band, arrows nudge, Tab cycles,
 * Delete removes. Design of record: UMA DESIGN on pico-link-jyhk.8
 * section 3; math ported from the mock's `drawCurve`/`coeffs`/`magDb`.
 */
export function CurveEditor({ bands, selected, bypass, onSelect, onChangeBand, onAddBand, onRemoveBand, onCycle }: CurveEditorProps) {
  const canvasRef = React.useRef<HTMLCanvasElement | null>(null);
  const wrapRef = React.useRef<HTMLDivElement | null>(null);
  const [readout, setReadout] = React.useState<{ x: number; y: number; text: string } | null>(null);
  const dragRef = React.useRef<{ index: number; startFreqHz: number; startGainDb: number; x0: number; y0: number } | null>(null);
  const bandsRef = React.useRef(bands);
  bandsRef.current = bands;

  const geom = React.useCallback((): CurveGeom => {
    const el = canvasRef.current;
    const rect = el?.getBoundingClientRect() ?? { width: 640, height: 260 };
    return { width: rect.width, height: rect.height, ...DEFAULT_PAD };
  }, []);

  const draw = React.useCallback(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const g = geom();
    const dpr = window.devicePixelRatio || 1;
    canvas.width = Math.max(1, Math.round(g.width * dpr));
    canvas.height = Math.max(1, Math.round(g.height * dpr));
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, g.width, g.height);

    const divider = readCssColor("--border", "#313142");
    const text2 = readCssColor("--muted-foreground", "#a5a2bd");
    const zero = readCssColor("--zero", "#4a4a5e");
    const wash = readCssColor("--wash", "rgba(90,73,214,0.15)");
    const brandBright = readCssColor("--accent", "#948aff");
    const surface2 = readCssColor("--popover", "#29283a");
    const fg = readCssColor("--foreground", "#f7f7f7");

    ctx.font = "11px system-ui, sans-serif";
    ctx.textBaseline = "middle";
    ctx.lineWidth = 1;

    for (const f of [20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000]) {
      const x = Math.round(freqToX(f, g)) + 0.5;
      ctx.strokeStyle = divider;
      ctx.beginPath();
      ctx.moveTo(x, g.padTop);
      ctx.lineTo(x, g.height - g.padBottom);
      ctx.stroke();
      ctx.fillStyle = text2;
      ctx.textAlign = "center";
      ctx.fillText(f >= 1000 ? `${f / 1000}k` : String(f), x, g.height - g.padBottom + 12);
    }
    for (let d = -DB_RANGE; d <= DB_RANGE; d += 5) {
      const y = Math.round(dbToY(d, g)) + 0.5;
      ctx.strokeStyle = d === 0 ? zero : divider;
      ctx.beginPath();
      ctx.moveTo(g.padLeft, y);
      ctx.lineTo(g.width - g.padRight, y);
      ctx.stroke();
      ctx.fillStyle = text2;
      ctx.textAlign = "right";
      ctx.fillText(`${d > 0 ? "+" : ""}${d}`, g.padLeft - 8, y);
    }

    const sel = bands[selected];
    if (sel) {
      const c = biquadForBand(sel);
      ctx.beginPath();
      ctx.moveTo(freqToX(20, g), dbToY(0, g));
      for (let i = 0; i <= N_CURVE_STEPS; i++) {
        const f = 20 * 1000 ** (i / N_CURVE_STEPS);
        ctx.lineTo(freqToX(f, g), dbToY(clamp(magnitudeDb(c, f), -DB_RANGE, DB_RANGE), g));
      }
      ctx.lineTo(freqToX(20000, g), dbToY(0, g));
      ctx.closePath();
      ctx.fillStyle = wash;
      ctx.fill();
    }

    ctx.beginPath();
    for (let i = 0; i <= N_CURVE_STEPS; i++) {
      const f = 20 * 1000 ** (i / N_CURVE_STEPS);
      const r = responseDb(bands, f);
      const x = freqToX(f, g);
      const y = dbToY(clamp(r, -DB_RANGE, DB_RANGE), g);
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    }
    ctx.strokeStyle = bypass ? text2 : brandBright;
    ctx.lineWidth = 2.5;
    ctx.setLineDash(bypass ? [6, 5] : []);
    ctx.stroke();
    ctx.setLineDash([]);

    bands.forEach((b, i) => {
      const x = freqToX(bandFreqHz(b), g);
      const y = dbToY(clamp(bandGainDb(b), -DB_RANGE, DB_RANGE), g);
      const isSel = i === selected;
      ctx.beginPath();
      ctx.arc(x, y, isSel ? 11 : 9, 0, Math.PI * 2);
      ctx.fillStyle = isSel ? brandBright : surface2;
      ctx.fill();
      ctx.lineWidth = 2;
      ctx.strokeStyle = isSel ? fg : brandBright;
      ctx.stroke();
      ctx.fillStyle = fg;
      ctx.textAlign = "center";
      ctx.font = "bold 11px system-ui, sans-serif";
      ctx.fillText(String(i + 1), x, y + 0.5);
      ctx.font = "11px system-ui, sans-serif";
    });
  }, [bands, selected, bypass, geom]);

  React.useEffect(() => {
    draw();
    const onResize = () => draw();
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, [draw]);

  const localXY = (e: { clientX: number; clientY: number }): [number, number] => {
    const rect = canvasRef.current!.getBoundingClientRect();
    return [e.clientX - rect.left, e.clientY - rect.top];
  };

  const onPointerDown: React.PointerEventHandler<HTMLCanvasElement> = (e) => {
    const [x, y] = localXY(e);
    const g = geom();
    const i = hitTestBand(bands, x, y, g);
    if (i < 0) return;
    onSelect(i);
    const b = bands[i];
    dragRef.current = { index: i, startFreqHz: bandFreqHz(b), startGainDb: bandGainDb(b), x0: x, y0: y };
    canvasRef.current?.setPointerCapture(e.pointerId);
  };

  const onPointerMove: React.PointerEventHandler<HTMLCanvasElement> = (e) => {
    const [x, y] = localXY(e);
    const g = geom();
    const drag = dragRef.current;
    if (drag) {
      const result = computeDrag({ freqHz: drag.startFreqHz, gainDb: drag.startGainDb, x0: drag.x0, y0: drag.y0 }, x, y, g, e.shiftKey);
      onChangeBand(drag.index, { freqHalfHz: Math.round(result.freqHz * 2), gainCdb: Math.round(result.gainDb * 100) });
    }
    const i = drag ? drag.index : hitTestBand(bands, x, y, g);
    const wrap = wrapRef.current;
    if (wrap) wrap.style.cursor = i >= 0 ? (drag ? "grabbing" : "grab") : "crosshair";
    const freqHz = i >= 0 ? bandFreqHz(bands[i]) : (() => {
      const u = clamp((x - g.padLeft) / (g.width - g.padLeft - g.padRight), 0, 1);
      return 20 * 1000 ** u;
    })();
    const db = i >= 0 ? bandGainDb(bands[i]) : responseDb(bands, freqHz);
    const qText = i >= 0 ? ` · Q ${bandQ(bands[i]).toFixed(2)}` : "";
    setReadout({ x, y, text: `${i >= 0 ? `${i + 1} · ` : ""}${fmtFreqLabel(freqHz)} · ${db > 0 ? "+" : ""}${db.toFixed(1)} dB${qText}` });
  };

  const onPointerUp: React.PointerEventHandler<HTMLCanvasElement> = () => {
    dragRef.current = null;
  };

  const onPointerLeave = () => setReadout(null);

  const onWheel: React.WheelEventHandler<HTMLCanvasElement> = (e) => {
    const [x, y] = localXY(e);
    const g = geom();
    let i = hitTestBand(bands, x, y, g);
    if (i < 0) i = selected;
    const b = bands[i];
    if (!b) return;
    e.preventDefault();
    const nextQ = computeWheelQ(bandQ(b), e.deltaY < 0, e.shiftKey);
    onSelect(i);
    onChangeBand(i, { qMilli: Math.round(nextQ * 1000) });
  };

  const onDoubleClick: React.MouseEventHandler<HTMLCanvasElement> = (e) => {
    const [x, y] = localXY(e);
    const g = geom();
    if (hitTestBand(bands, x, y, g) >= 0 || bands.length >= 10) return;
    const u = clamp((x - g.padLeft) / (g.width - g.padLeft - g.padRight), 0, 1);
    const freqHz = Math.round(20 * 1000 ** u);
    const gainDb = Math.round((((1 - (y - g.padTop) / (g.height - g.padTop - g.padBottom)) * 2 * DB_RANGE - DB_RANGE) * 10)) / 10;
    onAddBand(freqHz, gainDb);
  };

  const onKeyDown: React.KeyboardEventHandler<HTMLDivElement> = (e) => {
    const b = bands[selected];
    if (!b) return;
    const key = e.key;
    if (key === "Delete" || key === "Backspace") {
      e.preventDefault();
      onRemoveBand(selected);
      return;
    }
    if (key === "Tab") {
      e.preventDefault();
      onCycle(e.shiftKey ? -1 : 1);
      return;
    }
    if (key !== "ArrowLeft" && key !== "ArrowRight" && key !== "ArrowUp" && key !== "ArrowDown") return;
    e.preventDefault();
    const result = computeNudge(key as NudgeKey, bandFreqHz(b), bandGainDb(b), e.shiftKey);
    onChangeBand(selected, { freqHalfHz: Math.round(result.freqHz * 2), gainCdb: Math.round(result.gainDb * 100) });
  };

  return (
    <div
      ref={wrapRef}
      className="relative rounded-md border border-border bg-card p-1"
      tabIndex={0}
      role="group"
      aria-label="EQ curve editor"
      onKeyDown={onKeyDown}
      data-testid="curve-editor"
    >
      <canvas
        ref={canvasRef}
        className="block h-64 w-full"
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerLeave={onPointerLeave}
        onWheel={onWheel}
        onDoubleClick={onDoubleClick}
        data-testid="curve-canvas"
      />
      {readout ? (
        <div
          className="pointer-events-none absolute rounded border border-border bg-popover px-2 py-1 text-xs tabular-nums text-foreground"
          style={{ left: Math.min(readout.x + 14, 400), top: Math.max(4, readout.y - 30) }}
        >
          {readout.text}
        </div>
      ) : null}
    </div>
  );
}
