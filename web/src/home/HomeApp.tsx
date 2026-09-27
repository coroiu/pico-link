import * as React from "react";
import { Session } from "../session/session";
import type { SessionState } from "../session/session";
import { createStore, useStore } from "../session/store";
import { FakeTransport } from "../transport/fake";
import { isWebUsbSupported } from "../transport/webusb";
import { WebUsbSessionManager } from "../session/webusbSession";
import { emptyHomeSnapshot } from "../proto/telemetry";
import type { HomeSnapshot } from "../proto/telemetry";
import { emptyLibrarySnapshot } from "../proto/library";
import type { LibrarySnapshot } from "../proto/library";
import { TopBar } from "./TopBar";
import type { HomeTab } from "./TopBar";
import { HomeStack } from "./HomeStack";
import { ConnectScreens } from "./ConnectScreens";
import { UnpluggedOverlay } from "./UnpluggedOverlay";
import { EffectsTab } from "./effects/EffectsTab";
import { MiniLiveStrip } from "./effects/MiniLiveStrip";
import { LibraryController } from "../session/library";
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
    // `extras` must be present (with a `libraryRev` matching `goldenLibrary()`)
    // for `LibraryController.checkForLibraryChange` to ever fetch -- without
    // it the fake Effects tab never loads (found while screenshotting).
    extras: { libraryRev: GOLDEN_LIBRARY_REV, hostPreviewActive: false, deviceEditorOpen: false, deviceEditorEffectId: 0, presetsReady: true, codecFallbackReason: 0 },
  };
}

const GOLDEN_LIBRARY_REV = 1;

/** DEMO: a starter effect library for `?fake=1`, exercising the list/lock-badge/assignment/usage-count UI without hardware. */
function goldenLibrary(): LibrarySnapshot {
  return {
    ...emptyLibrarySnapshot(GOLDEN_LIBRARY_REV),
    presetsReady: true,
    effects: [
      {
        id: 1,
        persistedSeq: 1,
        preset: {
          name: "Warm",
          crossfeed: "weak",
          preamp: { kind: "auto" },
          eqLocked: true,
          bands: [
            { kind: "lowShelf", freqHalfHz: 200, gainCdb: 200, qMilli: 700 },
            { kind: "peak", freqHalfHz: 6000, gainCdb: -150, qMilli: 1000 },
          ],
        },
      },
      { id: 2, persistedSeq: 1, preset: { name: "Bright", crossfeed: "off", preamp: { kind: "auto" }, eqLocked: false, bands: [{ kind: "highShelf", freqHalfHz: 16000, gainCdb: 300, qMilli: 700 }] } },
    ],
    devices: [{ addr: "94:DB:56:54:7C:F2", presetId: 1, connected: true, name: "Sony WH-1000XM5" }],
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
  // DEMO: `?tab=fx` opens straight to Effects -- headless screenshot tooling
  // has no way to click a tab after a one-shot page load.
  const initialTab = React.useMemo<HomeTab>(() => (typeof window !== "undefined" && new URLSearchParams(window.location.search).get("tab") === "fx" ? "fx" : "home"), []);

  const [session, setSession] = React.useState<Session | null>(null);
  const [tab, setTab] = React.useState<HomeTab>(initialTab);
  const [everConnected, setEverConnected] = React.useState(false);
  const [chooserCancelled, setChooserCancelled] = React.useState(false);
  const [text, setText] = React.useState<HomeSnapshot>(emptyHomeSnapshot());
  const [library, setLibrary] = React.useState<LibraryController | null>(null);
  const managerRef = React.useRef<WebUsbSessionManager | null>(null);

  const idleStore = React.useMemo(() => createStore<SessionState>({ phase: "idle", info: null }), []);
  const status = useStore(session?.statusStore ?? idleStore);

  // Per jyhk.22's review finding: the `LibraryController` is constructed
  // per session and its start()/stop() bracket the session's own lifetime
  // (a reconnect disposes the old controller and mints a fresh one).
  React.useEffect(() => {
    if (!session) {
      setLibrary(null);
      return;
    }
    const controller = new LibraryController(session);
    controller.start();
    setLibrary(controller);
    return () => controller.stop();
  }, [session]);

  // DEMO: `?nolib=1` forces the pre-v2-firmware gate (`effects-firmware-gate`)
  // so it's screenshot/QA-able without a real old-firmware dongle.
  const noLib = React.useMemo(() => typeof window !== "undefined" && new URLSearchParams(window.location.search).get("nolib") === "1", []);

  React.useEffect(() => {
    if (useFake) {
      const fakeSession = new Session(new FakeTransport({ snapshot: () => goldenSnapshot(performance.now()), enableLibrary: !noLib, library: goldenLibrary() }));
      setSession(fakeSession);
      void fakeSession.start();
      return () => void fakeSession.stop();
    }

    const manager = new WebUsbSessionManager({ onSession: (s) => setSession(s) });
    managerRef.current = manager;
    void manager.start();
    return () => manager.stop();
  }, [useFake, noLib]);

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
      // FERN DESIGN section 6 / LibraryController doc comment: driven at
      // the same cadence the page already reads `snapshotRef` on, not its
      // own timer.
      if (library) void library.checkForLibraryChange();
    }, TEXT_REFRESH_MS);
    return () => clearInterval(timer);
  }, [session, library]);

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

  const connectedAddr = library?.store.getSnapshot().snapshot?.devices.find((d) => d.connected)?.addr ?? null;
  const mini = session ? <MiniLiveStrip text={text} snapshotRef={session.snapshotRef} ballistics={session.ballistics} clock={session.clock} onClickHome={() => setTab("home")} /> : null;

  return (
    <div className="min-h-svh bg-background text-foreground">
      <TopBar connected={connected} usbLabel={usbLabel} tab={tab} onTabChange={setTab} snapshot={connected ? text : null} mini={mini} />
      <main className="mx-auto max-w-[1180px] px-3.5 pt-4.5 pb-30 sm:px-6 sm:pt-7">
        {screen.kind !== "home" || !session ? (
          <ConnectScreens screen={screen.kind === "home" ? { kind: "connect", note: "" } : screen} onConnect={onConnect} />
        ) : tab === "home" ? (
          <HomeStack snapshotRef={session.snapshotRef} ballistics={session.ballistics} clock={session.clock} text={text} onEditFx={() => setTab("fx")} />
        ) : library ? (
          <EffectsTab library={library} connectedAddr={connectedAddr} />
        ) : null}
      </main>
      {connected && screen.kind === "home" && screen.unplugged ? <UnpluggedOverlay /> : null}
    </div>
  );
}
