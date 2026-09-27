import * as React from "react";
import { Button } from "../components/ui/button";
import { THEME_NAMES, useTheme } from "../theme/ThemeProvider";
import { FakeTransport } from "../transport/fake";
import { decodeHomeSnapshot, emptyHomeSnapshot } from "../proto/telemetry";
import type { HomeSnapshot } from "../proto/telemetry";
import { decodeDeviceInfo } from "../proto/info";
import { PL_CFG_REQ_GET_INFO, PL_CFG_REQ_GET_TELEMETRY } from "../transport/types";
import { MeterCanvas } from "./MeterCanvas";

const POLL_MS = 33;
const DEBUG_REFRESH_MS = 250;

/** A synthesized "golden" snapshot, loosely modelled on the design's worked example. */
function goldenSnapshot(uptimeMs: number): HomeSnapshot {
  const t = uptimeMs / 1000;
  const wobble = (Math.sin(t * 2) + 1) / 2; // 0..1
  return {
    ...emptyHomeSnapshot(),
    uptimeMs,
    snapSeq: Math.floor(uptimeMs / POLL_MS) + 1,
    linkConnected: true,
    codecWord: "LDAC",
    kbps: Math.round(660 + wobble * 300),
    kbpsAdaptive: true,
    kbpsIsLive: true,
    deviceName: "Sony WH-1000XM5",
    fxPresetName: "Warm",
    volumePresent: true,
    volumeLevel: 96,
    volumeMuted: false,
    volumeSource: "host",
    levelPresent: true,
    peakL: Math.round(120 + wobble * 100),
    peakR: Math.round(110 + wobble * 110),
    rmsL: Math.round(70 + wobble * 60),
    rmsR: Math.round(65 + wobble * 65),
    receivedMs: uptimeMs,
  };
}

/**
 * Dev page: runs the app against `FakeTransport` with a theme switcher and
 * a minimal debug readout of the decoded Home snapshot. No Home visuals
 * yet -- that's `pico-link-jyhk.12`, pending Uma's design. This is the
 * page's headless run mode (FERN DESIGN section 7): agents screenshot it
 * via headless Chrome.
 */
export function DevPage() {
  const { theme, setTheme } = useTheme();
  const [transport] = React.useState(() => new FakeTransport({ snapshot: () => goldenSnapshot(performance.now()) }));
  const [debug, setDebug] = React.useState<HomeSnapshot>(emptyHomeSnapshot());
  const [infoLine, setInfoLine] = React.useState("connecting...");
  const snapshotRef = React.useRef<HomeSnapshot | null>(null);

  React.useEffect(() => {
    let cancelled = false;
    let pollTimer: ReturnType<typeof setInterval> | undefined;
    let debugTimer: ReturnType<typeof setInterval> | undefined;

    void (async () => {
      await transport.open();
      const infoView = await transport.controlIn(PL_CFG_REQ_GET_INFO, 0, 64);
      const info = decodeDeviceInfo(new Uint8Array(infoView.buffer, infoView.byteOffset, infoView.byteLength));
      if (cancelled) return;
      setInfoLine(info ? `fw ${info.version} · telemetry proto ${info.telemetryProto}` : "GET_INFO decode failed");

      pollTimer = setInterval(async () => {
        try {
          const view = await transport.controlIn(PL_CFG_REQ_GET_TELEMETRY, 0, 256);
          const snapshot = decodeHomeSnapshot(new Uint8Array(view.buffer, view.byteOffset, view.byteLength));
          if (snapshot) snapshotRef.current = snapshot;
        } catch {
          // Dev-page best-effort poll; the session layer (jyhk.11) owns
          // real reconnect/backoff behaviour.
        }
      }, POLL_MS);

      // Debug text panel refreshes far below 30Hz -- the only React state
      // in this component that mirrors the poll data (the constraint this
      // scaffold must honor is on the *canvas hot path*, not any state at
      // all).
      debugTimer = setInterval(() => {
        if (snapshotRef.current) setDebug(snapshotRef.current);
      }, DEBUG_REFRESH_MS);
    })();

    return () => {
      cancelled = true;
      if (pollTimer) clearInterval(pollTimer);
      if (debugTimer) clearInterval(debugTimer);
      void transport.close();
    };
  }, [transport]);

  return (
    <div className="min-h-svh bg-background text-foreground p-6 flex flex-col gap-6">
      <header className="flex items-center justify-between">
        <h1 className="text-lg font-semibold">Pico Link web companion -- dev</h1>
        <div className="flex gap-2" role="group" aria-label="theme">
          {THEME_NAMES.map((name) => (
            <Button key={name} variant={theme === name ? "default" : "outline"} size="sm" onClick={() => setTheme(name)} data-testid={`theme-${name}`}>
              {name}
            </Button>
          ))}
        </div>
      </header>

      <p className="text-sm text-muted-foreground" data-testid="info-line">
        {infoLine}
      </p>

      <section className="flex gap-6 items-start">
        <MeterCanvas snapshotRef={snapshotRef} />

        <dl className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1 text-sm bg-card text-card-foreground rounded-md border border-border p-4" data-testid="debug-readout">
          <dt className="text-muted-foreground">link</dt>
          <dd>{debug.linkConnected ? "connected" : "no link"}</dd>
          <dt className="text-muted-foreground">codec</dt>
          <dd>{debug.codecWord || "--"}</dd>
          <dt className="text-muted-foreground">kbps</dt>
          <dd>
            {debug.kbps}
            {debug.kbpsAdaptive ? " (adaptive)" : ""}
          </dd>
          <dt className="text-muted-foreground">device</dt>
          <dd>{debug.deviceName || "--"}</dd>
          <dt className="text-muted-foreground">fx preset</dt>
          <dd>{debug.fxPresetName || "Off"}</dd>
          <dt className="text-muted-foreground">volume</dt>
          <dd>
            {debug.volumePresent ? `${debug.volumeLevel}/127 (${debug.volumeSource})` : "--"}
            {debug.volumeMuted ? " muted" : ""}
          </dd>
          <dt className="text-muted-foreground">peak L/R</dt>
          <dd>
            {debug.peakL} / {debug.peakR}
          </dd>
          <dt className="text-muted-foreground">rms L/R</dt>
          <dd>
            {debug.rmsL} / {debug.rmsR}
          </dd>
          <dt className="text-muted-foreground">snap_seq</dt>
          <dd>{debug.snapSeq}</dd>
        </dl>
      </section>
    </div>
  );
}
