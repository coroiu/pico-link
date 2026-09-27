import * as React from "react";
import { Button } from "../components/ui/button";
import { THEME_NAMES, useTheme } from "../theme/ThemeProvider";
import { FakeTransport } from "../transport/fake";
import { isWebUsbSupported, WebUsbTransport } from "../transport/webusb";
import { emptyHomeSnapshot } from "../proto/telemetry";
import type { HomeSnapshot } from "../proto/telemetry";
import { Session } from "../session/session";
import type { SessionState } from "../session/session";
import { useStore } from "../session/store";
import { MeterCanvas } from "./MeterCanvas";

const DEBUG_REFRESH_MS = 250;

/** A synthesized "golden" snapshot, loosely modelled on the design's worked example. */
function goldenSnapshot(uptimeMs: number): HomeSnapshot {
  const t = uptimeMs / 1000;
  const wobble = (Math.sin(t * 2) + 1) / 2; // 0..1
  return {
    ...emptyHomeSnapshot(),
    uptimeMs,
    snapSeq: Math.floor(uptimeMs / 33) + 1,
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

function statusLine(status: SessionState): string {
  switch (status.phase) {
    case "idle":
      return "idle";
    case "opening":
      return "opening...";
    case "handshaking":
      return "handshaking...";
    case "ready":
      return `ready · fw ${status.info?.version ?? "?"} · telemetry proto ${status.info?.telemetryProto ?? "?"}`;
    case "lost":
      return "lost -- device disconnected";
    case "incompatible":
      return `incompatible firmware (telemetry proto ${status.info?.telemetryProto ?? "?"}) -- update firmware or reload`;
    case "busy-elsewhere":
      return "busy -- another tab or tool holds the config interface";
    default:
      return status.phase;
  }
}

/**
 * Dev page: runs against `FakeTransport` by default, with a "Connect
 * (WebUSB)" button that swaps in a real `Session` + `WebUsbTransport` (this
 * bead, pico-link-jyhk.11). Home visuals proper are `pico-link-jyhk.12`.
 * This is the page's headless run mode (FERN DESIGN section 7): agents
 * screenshot it via headless Chrome.
 */
export function DevPage() {
  const { theme, setTheme } = useTheme();
  const [session, setSession] = React.useState<Session>(() => new Session(new FakeTransport({ snapshot: () => goldenSnapshot(performance.now()) })));
  const status = useStore(session.statusStore);
  const [debug, setDebug] = React.useState<HomeSnapshot>(emptyHomeSnapshot());
  const [connectError, setConnectError] = React.useState<string | null>(null);

  React.useEffect(() => {
    void session.start();
    return () => {
      void session.stop();
    };
  }, [session]);

  React.useEffect(() => {
    const debugTimer = setInterval(() => {
      if (session.snapshotRef.current) setDebug(session.snapshotRef.current);
    }, DEBUG_REFRESH_MS);
    return () => clearInterval(debugTimer);
  }, [session]);

  const connectWebUsb = React.useCallback(() => {
    setConnectError(null);
    // Must run synchronously from this click handler, no `await` before it
    // -- `navigator.usb.requestDevice` (inside `WebUsbTransport.open()`)
    // requires an active user gesture.
    const nextSession = new Session(new WebUsbTransport());
    setSession(nextSession);
    nextSession.start().catch((err: unknown) => {
      setConnectError(err instanceof Error ? err.message : String(err));
    });
  }, []);

  const reconnectFake = React.useCallback(() => {
    setConnectError(null);
    setSession(new Session(new FakeTransport({ snapshot: () => goldenSnapshot(performance.now()) })));
  }, []);

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

      <div className="flex items-center gap-2">
        <Button size="sm" onClick={connectWebUsb} disabled={!isWebUsbSupported()} data-testid="connect-webusb">
          Connect (WebUSB)
        </Button>
        <Button size="sm" variant="outline" onClick={reconnectFake} data-testid="use-fake">
          Use Fake
        </Button>
        {!isWebUsbSupported() && <span className="text-xs text-muted-foreground">WebUSB unsupported in this browser</span>}
      </div>

      <p className="text-sm text-muted-foreground" data-testid="info-line">
        {statusLine(status)}
      </p>
      {connectError && (
        <p className="text-sm text-destructive" data-testid="connect-error">
          {connectError}
        </p>
      )}

      <section className="flex gap-6 items-start">
        <MeterCanvas snapshotRef={session.snapshotRef} />

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
