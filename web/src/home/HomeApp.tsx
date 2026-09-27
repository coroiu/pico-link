import * as React from "react";
import { Session } from "../session/session";
import type { SessionState } from "../session/session";
import { createStore, useStore } from "../session/store";
import { FakeTransport } from "../transport/fake";
import { isWebUsbSupported } from "../transport/webusb";
import { WebUsbSessionManager } from "../session/webusbSession";
import { emptyHomeSnapshot } from "../proto/telemetry";
import type { HomeSnapshot } from "../proto/telemetry";
import { TopBar } from "./TopBar";
import type { HomeTab } from "./TopBar";
import { HomeStack } from "./HomeStack";
import { ConnectScreens } from "./ConnectScreens";
import { UnpluggedOverlay } from "./UnpluggedOverlay";
import { EffectsPlaceholder } from "./EffectsPlaceholder";
import { selectEnvScreen } from "./envState";

const TEXT_REFRESH_MS = 250;

/** DEMO: the golden fake snapshot used when running without hardware (`?fake=1`, and the default when WebUSB is unsupported has nothing to fall back to -- this only backs manual/dev exploration, not the real app path). */
function goldenSnapshot(uptimeMs: number): HomeSnapshot {
  const t = uptimeMs / 1000;
  const wobble = (Math.sin(t * 2) + 1) / 2;
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
    volumeLevel: 100,
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
 * The real Home app (design of record: UMA DESIGN on pico-link-jyhk.8, mock
 * `2026-09-27-web-companion-v2-identity.html`). Owns the WebUSB session
 * lifecycle end to end: auto-reopen on load, the environment states when
 * there's no usable link, the Home/Effects tabs, and the unplugged overlay
 * once a device has been seen ready this page-load.
 *
 * `?fake=1` swaps in a `FakeTransport` (golden snapshot) for exploration
 * without hardware -- distinct from `?dev`'s separate debug page.
 */
export function HomeApp() {
  const useFake = React.useMemo(() => typeof window !== "undefined" && new URLSearchParams(window.location.search).get("fake") === "1", []);

  const [session, setSession] = React.useState<Session | null>(null);
  const [tab, setTab] = React.useState<HomeTab>("home");
  const [everConnected, setEverConnected] = React.useState(false);
  const [chooserCancelled, setChooserCancelled] = React.useState(false);
  const [text, setText] = React.useState<HomeSnapshot>(emptyHomeSnapshot());
  const managerRef = React.useRef<WebUsbSessionManager | null>(null);

  const idleStore = React.useMemo(() => createStore<SessionState>({ phase: "idle", info: null }), []);
  const status = useStore(session?.statusStore ?? idleStore);

  React.useEffect(() => {
    if (useFake) {
      const fakeSession = new Session(new FakeTransport({ snapshot: () => goldenSnapshot(performance.now()) }));
      setSession(fakeSession);
      void fakeSession.start();
      return () => void fakeSession.stop();
    }

    const manager = new WebUsbSessionManager({ onSession: (s) => setSession(s) });
    managerRef.current = manager;
    void manager.start();
    return () => manager.stop();
  }, [useFake]);

  React.useEffect(() => {
    if (status.phase === "ready") {
      setEverConnected(true);
      setChooserCancelled(false);
    }
  }, [status.phase]);

  React.useEffect(() => {
    if (!session) return;
    const timer = setInterval(() => {
      if (session.snapshotRef.current) setText(session.snapshotRef.current);
    }, TEXT_REFRESH_MS);
    return () => clearInterval(timer);
  }, [session]);

  const onConnect = React.useCallback(() => {
    setChooserCancelled(false);
    managerRef.current?.requestDevice().catch((err: unknown) => {
      // `navigator.usb.requestDevice` rejects with `NotFoundError` when the
      // user dismisses the chooser without picking a device (design section
      // 4's "busy-elsewhere"/incompatible are session phases; a cancelled
      // chooser never reaches the session at all).
      if (err instanceof DOMException && err.name === "NotFoundError") {
        setChooserCancelled(true);
      }
    });
  }, []);

  const screen = selectEnvScreen({
    webUsbSupported: useFake || isWebUsbSupported(),
    phase: status.phase,
    everConnected,
    chooserCancelled,
  });

  const connected = screen.kind === "home";
  const usbLabel = connected ? (screen.unplugged ? "Unplugged" : "Pico Link · USB") : "Not connected";

  return (
    <div className="min-h-svh bg-background text-foreground">
      <TopBar connected={connected} usbLabel={usbLabel} tab={tab} onTabChange={setTab} snapshot={connected ? text : null} />
      <main className="mx-auto max-w-[1180px] px-3.5 pt-4.5 pb-30 sm:px-6 sm:pt-7">
        {screen.kind !== "home" || !session ? (
          <ConnectScreens screen={screen.kind === "home" ? { kind: "connect", note: "" } : screen} onConnect={onConnect} />
        ) : tab === "home" ? (
          <HomeStack snapshotRef={session.snapshotRef} ballistics={session.ballistics} clock={session.clock} text={text} onEditFx={() => setTab("fx")} />
        ) : (
          <EffectsPlaceholder />
        )}
      </main>
      {connected && screen.kind === "home" && screen.unplugged ? <UnpluggedOverlay /> : null}
    </div>
  );
}
