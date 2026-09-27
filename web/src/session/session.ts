// The session layer (FERN DESIGN section 4, pico-link-jyhk.8): opens a
// `Transport`, runs the GET_INFO handshake, then a single-flight
// command/poll loop with adaptive pacing, visibility-aware pausing, and
// reboot detection. Owns exactly one `Transport` at a time -- reconnect
// across a browser-level USB disconnect/reconnect (getDevices()/the
// `connect` event) is the caller's job (see `webusbSession.ts`), since only
// the caller knows how to mint a fresh `WebUsbTransport` for the device
// that came back.
import { decodeDeviceInfo } from "../proto/info";
import type { DeviceInfo } from "../proto/info";
import { decodeHomeSnapshot, TELEMETRY_PAGE_HOME, TELEMETRY_PROTO } from "../proto/telemetry";
import type { HomeSnapshot } from "../proto/telemetry";
import { PL_CFG_REQ_GET_INFO, PL_CFG_REQ_GET_TELEMETRY } from "../transport/types";
import type { Transport, Unsubscribe } from "../transport/types";
import { WebUsbBusyError } from "../transport/webusb";
import { OutLevelBallistics } from "../meter/ballistics";
import { ClockOffsetEstimator } from "./clock";
import { createStore } from "./store";
import type { Store } from "./store";
import { nextPollIntervalMs, pushRttSample } from "./pacing";

export type SessionPhase = "idle" | "opening" | "handshaking" | "ready" | "lost" | "incompatible" | "busy-elsewhere";

export interface SessionState {
  phase: SessionPhase;
  info: DeviceInfo | null;
}

export interface SessionOptions {
  /** Start-to-start poll pacing at a healthy RTT (design section 4). Default 33ms (the 20Hz telemetry source, a2dp.c:241). */
  pollIntervalMs?: number;
  /** Pacing once the median RTT exceeds `rttBackoffThresholdMs`. Default 50ms. */
  backoffIntervalMs?: number;
  /** Window size for the median-RTT pacing decision. Default 30. */
  rttWindowSize?: number;
  /** Default 20ms. */
  rttBackoffThresholdMs?: number;
  /** How often to re-check `document.hidden` while paused. Default 250ms. */
  hiddenPollMs?: number;
  /** Injectable for tests; defaults to `performance.now`. */
  now?: () => number;
  /** Injectable for tests; defaults to the global `document` if present. */
  visibilityDocument?: Document;
}

interface QueueItem {
  run: () => Promise<unknown>;
  resolve: (value: unknown) => void;
  reject: (err: unknown) => void;
}

const DEFAULTS: Required<Omit<SessionOptions, "now" | "visibilityDocument">> = {
  pollIntervalMs: 33,
  backoffIntervalMs: 50,
  rttWindowSize: 30,
  rttBackoffThresholdMs: 20,
  hiddenPollMs: 250,
};

/**
 * Review fix-first (pico-link-jyhk.11): a transport whose device is already
 * gone can fail every `controlIn` forever without ever firing the browser's
 * `disconnect` event (seen with a stale/half-closed handle) -- swallowing
 * those errors indefinitely (design section 4's "only disconnect ends a
 * session") left the loop spinning against a dead device. After this many
 * *consecutive* poll failures, treat the session as lost rather than retrying
 * forever.
 */
const MAX_CONSECUTIVE_POLL_FAILURES = 5;

/**
 * One connected-device session. Construct with an already-instantiated
 * (but not yet opened) `Transport`; call `start()`. `statusStore` carries
 * the slow-changing session phase for React; `snapshotRef` and
 * `ballistics`/`clock` are the 30Hz-adjacent hot path a canvas reads
 * directly (design: never through React state).
 */
export class Session {
  readonly statusStore: Store<SessionState>;
  readonly snapshotRef: { current: HomeSnapshot | null } = { current: null };
  readonly ballistics = new OutLevelBallistics();
  readonly clock = new ClockOffsetEstimator();

  /**
   * The transport this session drives -- `readonly` (not `private`) so a
   * companion controller sharing this session's single-flight command
   * queue (`enqueueCommand`) can issue its own `controlIn`/`controlOut`
   * calls against the same device, e.g. `LibraryController`
   * (pico-link-jyhk.22) for `GET_LIBRARY`/`HOST_OP`/`GET_OP_STATUS`. Never
   * call `transport.open()`/`close()` from outside `Session` -- lifecycle
   * stays here.
   */
  readonly transport: Transport;
  private readonly opts: Required<Omit<SessionOptions, "now" | "visibilityDocument">>;
  private readonly now: () => number;
  private readonly visibilityDocument: Document | undefined;

  private running = false;
  private unsubscribeDisconnect: Unsubscribe | undefined;
  private visibilityHandler: (() => void) | undefined;
  private commandQueue: QueueItem[] = [];
  private wakeResolvers: Array<() => void> = [];
  private rtts: number[] = [];
  private lastInfo: DeviceInfo | null = null;
  private lastUptimeMs: number | null = null;
  private lastSnapSeq: number | null = null;
  private lastLevelReceivedMs: number | null = null;
  private consecutivePollFailures = 0;

  constructor(transport: Transport, options: SessionOptions = {}) {
    this.transport = transport;
    this.opts = { ...DEFAULTS, ...options };
    this.now = options.now ?? (() => performance.now());
    this.visibilityDocument = options.visibilityDocument ?? (typeof document !== "undefined" ? document : undefined);
    this.statusStore = createStore<SessionState>({ phase: "idle", info: null });
  }

  /** Opens the transport, runs the GET_INFO handshake, then starts polling if compatible. */
  async start(): Promise<void> {
    this.running = true;
    this.setStatus({ phase: "opening", info: null });

    try {
      await this.transport.open();
    } catch (err) {
      this.running = false;
      if (err instanceof WebUsbBusyError) {
        this.setStatus({ phase: "busy-elsewhere", info: null });
        return;
      }
      this.setStatus({ phase: "lost", info: null });
      throw err;
    }

    this.unsubscribeDisconnect = this.transport.onDisconnect(() => this.handleDisconnected());
    if (this.visibilityDocument) {
      this.visibilityHandler = () => this.wake();
      this.visibilityDocument.addEventListener("visibilitychange", this.visibilityHandler);
    }

    const result = await this.performHandshake();
    if (result !== "ready") {
      this.running = false;
      return;
    }

    void this.runLoop();
  }

  /** Ends the session: stops polling, unsubscribes, closes the transport. */
  async stop(): Promise<void> {
    this.running = false;
    this.wake();
    if (this.unsubscribeDisconnect) {
      this.unsubscribeDisconnect();
      this.unsubscribeDisconnect = undefined;
    }
    if (this.visibilityDocument && this.visibilityHandler) {
      this.visibilityDocument.removeEventListener("visibilitychange", this.visibilityHandler);
      this.visibilityHandler = undefined;
    }
    await this.transport.close();
    this.setStatus({ phase: "idle", info: null });
  }

  /**
   * Queues a command (e.g. a future IMPORT_PRESET/EQ write) ahead of the
   * next poll -- design section 4: "Commands ... FIFO and served first."
   * Wakes the loop immediately even if it's mid-sleep for a poll.
   */
  enqueueCommand<T>(run: () => Promise<T>): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      this.commandQueue.push({ run: run as () => Promise<unknown>, resolve: resolve as (value: unknown) => void, reject });
      this.wake();
    });
  }

  private setStatus(state: SessionState): void {
    this.statusStore.set(state);
  }

  private async performHandshake(): Promise<"ready" | "incompatible" | "failed"> {
    this.setStatus({ phase: "handshaking", info: this.lastInfo });
    try {
      const view = await this.transport.controlIn(PL_CFG_REQ_GET_INFO, 0, 64);
      const info = decodeDeviceInfo(new Uint8Array(view.buffer, view.byteOffset, view.byteLength));
      if (!info) {
        throw new Error("GET_INFO decode failed");
      }
      this.lastInfo = info;

      // Design section 4: "Unknown info_ver / telemetry_proto / import_proto
      // -> incompatible ... stop polling. Page mask gates features." Only
      // `telemetryProto` gates Home polling today; `infoVer`/`importProto`
      // have no consumer yet (no EQ-import UI on this bead).
      if (info.telemetryProto !== TELEMETRY_PROTO) {
        this.setStatus({ phase: "incompatible", info });
        return "incompatible";
      }

      this.setStatus({ phase: "ready", info });
      return "ready";
    } catch {
      this.setStatus({ phase: "lost", info: this.lastInfo });
      return "failed";
    }
  }

  private async runLoop(): Promise<void> {
    while (this.running) {
      while (this.running && this.commandQueue.length > 0) {
        const item = this.commandQueue.shift()!;
        try {
          item.resolve(await item.run());
        } catch (err) {
          item.reject(err);
        }
      }
      if (!this.running) return;

      if (this.isHidden()) {
        await this.sleep(this.opts.hiddenPollMs);
        continue;
      }

      const start = this.now();
      let rebooted = false;
      try {
        rebooted = await this.pollOnce();
        this.consecutivePollFailures = 0;
      } catch {
        // Slow/failed replies are normal (WebUSB has no per-transfer
        // timeout, design section 4: "Only the disconnect event ends a
        // session") -- swallow and retry next tick, unless they're
        // *consecutive*: a stale device that never fires `disconnect` (e.g.
        // a half-closed handle) would otherwise spin forever.
        this.consecutivePollFailures += 1;
        if (this.consecutivePollFailures >= MAX_CONSECUTIVE_POLL_FAILURES) {
          this.handleDisconnected();
          return;
        }
      }
      if (!this.running) return;

      if (rebooted) {
        const result = await this.performHandshake();
        if (result !== "ready") {
          this.running = false;
          return;
        }
        continue;
      }

      const rtt = this.now() - start;
      this.recordRtt(rtt);
      const delay = Math.max(0, this.currentPollIntervalMs() - rtt);
      await this.sleep(delay);
    }
  }

  /** Returns `true` if this poll detected a device reboot (uptime/snap_seq went backwards). */
  private async pollOnce(): Promise<boolean> {
    const view = await this.transport.controlIn(PL_CFG_REQ_GET_TELEMETRY, TELEMETRY_PAGE_HOME, 256);
    const hostReceiveMs = this.now();
    const snapshot = decodeHomeSnapshot(new Uint8Array(view.buffer, view.byteOffset, view.byteLength));
    if (!snapshot) {
      // A short/malformed reply -- ignore this tick rather than tearing
      // down the session over one bad read.
      return false;
    }

    if (this.isReboot(snapshot)) {
      this.clock.reset();
      this.ballistics.reset();
      this.lastUptimeMs = null;
      this.lastSnapSeq = null;
      this.lastLevelReceivedMs = null;
      this.snapshotRef.current = snapshot;
      return true;
    }

    this.clock.record(hostReceiveMs, snapshot.uptimeMs);

    if (snapshot.levelPresent && snapshot.receivedMs !== this.lastLevelReceivedMs) {
      this.ballistics.fold({ atMs: snapshot.receivedMs, peakL: snapshot.peakL, peakR: snapshot.peakR, rmsL: snapshot.rmsL, rmsR: snapshot.rmsR });
      this.lastLevelReceivedMs = snapshot.receivedMs;
    }

    this.lastUptimeMs = snapshot.uptimeMs;
    this.lastSnapSeq = snapshot.snapSeq;
    this.snapshotRef.current = snapshot;
    return false;
  }

  /** Design section 4: "Reboot detection: uptime_ms goes backwards or snap_seq restarts." `snapSeq === 0` ("not ready") is exempt -- it's the normal pre-link state, not a restart. */
  private isReboot(snapshot: HomeSnapshot): boolean {
    if (this.lastUptimeMs !== null && snapshot.uptimeMs < this.lastUptimeMs) return true;
    if (this.lastSnapSeq !== null && snapshot.snapSeq !== 0 && snapshot.snapSeq < this.lastSnapSeq) return true;
    return false;
  }

  private handleDisconnected(): void {
    this.running = false;
    this.wake();
    this.setStatus({ phase: "lost", info: this.lastInfo });
  }

  private isHidden(): boolean {
    return this.visibilityDocument?.hidden ?? false;
  }

  private recordRtt(rtt: number): void {
    pushRttSample(this.rtts, rtt, this.opts.rttWindowSize);
  }

  private currentPollIntervalMs(): number {
    return nextPollIntervalMs(this.rtts, this.opts);
  }

  private sleep(ms: number): Promise<void> {
    if (ms <= 0) return Promise.resolve();
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        this.wakeResolvers = this.wakeResolvers.filter((w) => w !== wake);
        resolve();
      }, ms);
      const wake = () => {
        clearTimeout(timer);
        resolve();
      };
      this.wakeResolvers.push(wake);
    });
  }

  /** Interrupts any in-progress `sleep` immediately (a queued command, a visibilitychange, stop()). */
  private wake(): void {
    for (const resolver of this.wakeResolvers.splice(0)) resolver();
  }
}
