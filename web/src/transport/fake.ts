import { encodeHomeSnapshotForTest, emptyHomeSnapshot, TELEMETRY_PAGE_HOME } from "../proto/telemetry";
import type { HomeSnapshot } from "../proto/telemetry";
import { encodeDeviceInfoForTest } from "../proto/info";
import type { DeviceInfoInput } from "../proto/info";
import { decodePresetBlob, emptyLibrarySnapshot, encodeLibrarySnapshotForTest, encodePresetBlob } from "../proto/library";
import type { LibraryDevice, LibraryEffect, LibrarySnapshot, Preset } from "../proto/library";
import {
  encodeOpStatusForTest,
  encodeParseApoResultForTest,
  HOST_OP_ASSIGN,
  HOST_OP_CONNECT,
  HOST_OP_DELETE_EFFECT,
  HOST_OP_DISCONNECT,
  HOST_OP_FORGET,
  HOST_OP_PARSE_APO,
  HOST_OP_PREVIEW,
  HOST_OP_PREVIEW_END,
  HOST_OP_SAVE_EFFECT,
  HOST_OP_SCAN_START,
  HOST_OP_SCAN_STOP,
  HOST_OP_SET_DEVICE_QUALITY,
  OpError,
} from "../proto/ops";
import type { OpStatus } from "../proto/ops";
import { emptyRadioSnapshot, encodeRadioSnapshotForTest } from "../proto/radio";
import type { ConnectFailureReason, ConnectOutcomeResult, RadioScanEntry, RadioSnapshot } from "../proto/radio";
import { PL_CFG_REQ_GET_INFO, PL_CFG_REQ_GET_LIBRARY, PL_CFG_REQ_GET_OP_STATUS, PL_CFG_REQ_GET_RADIO, PL_CFG_REQ_GET_TELEMETRY, PL_CFG_REQ_HOST_OP, TransportError } from "./types";
import type { Transport, Unsubscribe } from "./types";

const DEFAULT_INFO: DeviceInfoInput = {
  infoVer: 1,
  importProto: 1,
  statusVer: 1,
  telemetryProto: 1,
  telemetryPageMask: 1 << TELEMETRY_PAGE_HOME,
  version: "dev",
};

/** Bits 5-7 of `op_mask` (`GET_LIBRARY`/`HOST_OP`/`GET_OP_STATUS`) -- what `enableLibrary: true` advertises via `GET_INFO` v2. */
const LIBRARY_OP_MASK = (1 << 5) | (1 << 6) | (1 << 7);

/** Bits 7-12 of `op_mask` (`SCAN_START`..`SET_DEVICE_QUALITY`, design sec 13.3) -- what `enableRadio: true` advertises via `GET_INFO` v2. */
const RADIO_OP_MASK = (1 << 7) | (1 << 8) | (1 << 9) | (1 << 10) | (1 << 11) | (1 << 12);

const LDAC_QUALITY_MIN = 1;
const LDAC_QUALITY_MAX = 4;

export interface FakeTransportOptions {
  /** Called each time GET_TELEMETRY page 0 is polled; return the live snapshot. */
  snapshot?: () => HomeSnapshot;
  info?: DeviceInfoInput;
  /**
   * DEMO: turns on the `GET_LIBRARY`/`HOST_OP`/`GET_OP_STATUS` (0x05-0x07)
   * emulation (design section 3-4 on pico-link-jyhk.17) for tests and the
   * dev page against firmware that doesn't exist yet -- e.g.
   * `new FakeTransport({ enableLibrary: true })` paired with a
   * `LibraryController` (`session/library.ts`) exercises the full
   * save/delete/assign/preview flow with no hardware and no firmware
   * changes. Advertised via `GET_INFO`'s v2 `op_mask` -- a caller must
   * still check it before issuing these requests, same as it would
   * against real hardware.
   */
  enableLibrary?: boolean;
  /** Seeds the emulated library (only meaningful with `enableLibrary: true`). Defaults to an empty, `presetsReady: true` library. */
  library?: LibrarySnapshot;
  /**
   * DEMO: turns on the `GET_RADIO`/`HOST_OP` ops 7..12 (0x08 +
   * `SCAN_START`..`SET_DEVICE_QUALITY`) emulation, design sec 13 (bead
   * pico-link-jyhk.24/.30) -- same shape as `enableLibrary`, advertised via
   * `GET_INFO`'s v2 `op_mask`. `RadioController` (`session/radio.ts`) is
   * the intended caller.
   */
  enableRadio?: boolean;
  /** Injected reply latency in ms (design section 3: "injectable latency"). */
  latencyMs?: number;
  /** If true, every controlIn/controlOut rejects with a stall (design: "stalls"). */
  stalled?: boolean;
}

interface HostPreview {
  effectId: number;
  preset: Preset;
  bypass: boolean;
}

/**
 * A scripted device model answering 0x02/0x03/0x04, per FERN DESIGN
 * section 3, plus (with `enableLibrary: true`) 0x05/0x06/0x07 per ADA
 * DESIGN on pico-link-jyhk.17. Backs the dev page's default run mode -- no
 * hardware, no capture file, just a live-generated snapshot (or a fixed
 * one) and an in-memory preset library.
 */
export class FakeTransport implements Transport {
  private disconnectCbs: Array<() => void> = [];
  private opened = false;

  private options: FakeTransportOptions;

  // --- Library/ops emulation state (only touched when `enableLibrary`). --
  private library: LibrarySnapshot;
  private nextEffectId: number;
  private lastStatus: OpStatus | null = null;
  private hostPreview: HostPreview | null = null;

  // --- Radio/ops emulation state (only touched when `enableRadio`). ------
  private radio: RadioSnapshot;
  private nextScanSeq = 0;
  private nextAttemptSeq = 0;
  private scanCandidates: RadioScanEntry[] = [];
  private connectedAddr: string | null = null;

  constructor(options: FakeTransportOptions = {}) {
    this.options = options;
    this.library = options.library ?? { ...emptyLibrarySnapshot(1), presetsReady: true };
    this.nextEffectId = 1 + this.library.effects.reduce((max, e) => Math.max(max, e.id), 0);
    this.radio = { ...emptyRadioSnapshot(1), storeReady: true };
  }

  async open(): Promise<void> {
    await this.delay();
    this.opened = true;
  }

  async close(): Promise<void> {
    this.opened = false;
  }

  onDisconnect(cb: () => void): Unsubscribe {
    this.disconnectCbs.push(cb);
    return () => {
      this.disconnectCbs = this.disconnectCbs.filter((registered) => registered !== cb);
    };
  }

  /** Test/dev-page hook: simulates the device unplugging. */
  simulateDisconnect(): void {
    this.opened = false;
    for (const cb of this.disconnectCbs) cb();
  }

  setStalled(stalled: boolean): void {
    this.options.stalled = stalled;
  }

  /** Test/dev-page hook: the current host preview, if any (`null` once `PREVIEW_END`d or auto-reverted -- this fake never auto-reverts on its own; the session layer's 2s lease is what a real device enforces). */
  currentHostPreview(): HostPreview | null {
    return this.hostPreview;
  }

  /** Test/dev-page hook: seeds what a subsequent `SCAN_START` reveals (design sec 13.4's scan list). */
  setScanCandidates(entries: RadioScanEntry[]): void {
    this.scanCandidates = entries;
  }

  /** Test hook: the live emulated `GET_LIBRARY` state, for radio ops (`FORGET`/`SET_DEVICE_QUALITY`) that mutate `library.devices` without a `LibraryController` in the test. */
  currentLibrarySnapshotForTest(): LibrarySnapshot {
    return this.library;
  }

  /** Test/dev-page hook: the live radio snapshot state, for assertions that don't want to round-trip `GET_RADIO`'s wire bytes. */
  currentRadioSnapshot(): RadioSnapshot {
    return this.radio;
  }

  /**
   * Test/dev-page hook: resolves the in-flight `attempt` (if any) the way a
   * real device's `ConnectSucceeded`/`ConnectFailed`/cancel echo would --
   * this fake has no BTstack timing to simulate that on its own. On `"ok"`/
   * `"okDegraded"` also sets `connectedAddr` and marks the matching
   * `library.devices` entry (if any) `connected`, mirroring what a real
   * device's `PairedDeviceUpserted`/telemetry echo would surface through
   * `GET_LIBRARY`.
   */
  resolveConnectAttempt(result: ConnectOutcomeResult, reason?: ConnectFailureReason): void {
    const attempt = this.radio.attempt;
    if (!attempt) return;
    this.bumpRadioRev();
    this.radio = {
      ...this.radio,
      connecting: false,
      attempt: undefined,
      lastOutcome: { seq: attempt.seq, addr: attempt.addr, result, reason: result === "failed" ? reason : undefined },
    };
    if (result === "ok" || result === "okDegraded") {
      this.connectedAddr = attempt.addr;
      this.library = { ...this.library, devices: this.library.devices.map((d) => ({ ...d, connected: d.addr === attempt.addr })) };
    }
  }

  private bumpRadioRev(): void {
    this.radio = { ...this.radio, radioRev: this.radio.radioRev + 1 };
  }

  async controlIn(bRequest: number, wValue: number, length: number): Promise<DataView> {
    await this.delay();
    if (!this.opened) {
      throw new TransportError("fake device not open");
    }
    if (this.options.stalled) {
      throw new TransportError("stall (fake)");
    }

    if (bRequest === PL_CFG_REQ_GET_INFO) {
      const bytes = encodeDeviceInfoForTest(this.infoInput());
      return sliceView(bytes, length);
    }

    if (bRequest === PL_CFG_REQ_GET_TELEMETRY && wValue === TELEMETRY_PAGE_HOME) {
      const snapshot = this.options.snapshot ? this.options.snapshot() : emptyHomeSnapshot();
      const bytes = encodeHomeSnapshotForTest(snapshot);
      return sliceView(bytes, length);
    }

    if (bRequest === PL_CFG_REQ_GET_LIBRARY && this.options.enableLibrary) {
      const bytes = encodeLibrarySnapshotForTest(this.library);
      return sliceView(bytes, length);
    }

    if (bRequest === PL_CFG_REQ_GET_OP_STATUS && (this.options.enableLibrary || this.options.enableRadio)) {
      const status: OpStatus = this.lastStatus ?? { opProto: 1, seq: 0, op: 0, state: "none", error: OpError.None, effectId: 0, libraryRev: this.library.libraryRev, persistedSeq: 0, line: 0, band: 0, value: 0, payload: new Uint8Array(0) };
      const bytes = encodeOpStatusForTest(status);
      return sliceView(bytes, length);
    }

    if (bRequest === PL_CFG_REQ_GET_RADIO && this.options.enableRadio) {
      const bytes = encodeRadioSnapshotForTest(this.radio);
      return sliceView(bytes, length);
    }

    throw new TransportError(`FakeTransport: no scripted answer for bRequest 0x${bRequest.toString(16)} wValue ${wValue}`);
  }

  async controlOut(bRequest: number, _wValue: number, bytes: Uint8Array): Promise<void> {
    await this.delay();
    if (!this.opened) {
      throw new TransportError("fake device not open");
    }
    if (this.options.stalled) {
      throw new TransportError("stall (fake)");
    }

    if (bRequest === PL_CFG_REQ_HOST_OP && (this.options.enableLibrary || this.options.enableRadio)) {
      this.handleHostOp(bytes);
      return;
    }
    // No IMPORT_PRESET scripting yet -- jyhk.11+ territory.
  }

  private infoInput(): DeviceInfoInput {
    const info = this.options.info ?? DEFAULT_INFO;
    if ((!this.options.enableLibrary && !this.options.enableRadio) || info.v2) return info;
    const opMask = (this.options.enableLibrary ? LIBRARY_OP_MASK : 0) | (this.options.enableRadio ? RADIO_OP_MASK : 0);
    return { ...info, v2: { libProto: 1, opProto: 1, mailboxLen: 1024, opMask, libraryMaxLen: 1536 } };
  }

  /** Emulates the device's `HOST_OP` handling (design section 4) -- bumps `libraryRev` on any mutation, publishes a `GET_OP_STATUS` result `lastStatus` polls immediately (this fake has no async ACK/BUSY window). */
  private handleHostOp(req: Uint8Array): void {
    const view = new DataView(req.buffer, req.byteOffset, req.byteLength);
    const op = req[1];
    const seq = req[2];
    const flags = req[3];
    const base: Omit<OpStatus, "state" | "error" | "effectId" | "persistedSeq" | "line" | "band" | "value" | "payload"> = { opProto: 1, seq, op, libraryRev: this.library.libraryRev };
    const done = (extra: Partial<OpStatus> = {}): void => {
      this.lastStatus = { ...base, state: "done", error: OpError.None, effectId: 0, persistedSeq: 0, line: 0, band: 0, value: 0, payload: new Uint8Array(0), libraryRev: this.library.libraryRev, ...extra };
    };
    const rejected = (error: OpError, extra: Partial<OpStatus> = {}): void => {
      this.lastStatus = { ...base, state: "rejected", error, effectId: 0, persistedSeq: 0, line: 0, band: 0, value: 0, payload: new Uint8Array(0), libraryRev: this.library.libraryRev, ...extra };
    };
    const bumpRev = (): void => {
      this.library = { ...this.library, libraryRev: this.library.libraryRev + 1 };
    };

    switch (op) {
      case HOST_OP_SAVE_EFFECT: {
        const id = view.getUint16(4, true);
        const baseSeq = view.getUint16(6, true);
        const preset = decodePresetBlob(req.subarray(8, 8 + 80));
        if (id === 0) {
          const newId = this.nextEffectId++;
          const effect: LibraryEffect = { id: newId, persistedSeq: 1, preset };
          this.library = { ...this.library, effects: [...this.library.effects, effect] };
          bumpRev();
          done({ effectId: newId, persistedSeq: 1 });
          return;
        }
        const existing = this.library.effects.find((e) => e.id === id);
        if (!existing) {
          rejected(OpError.NotFound);
          return;
        }
        if (existing.persistedSeq !== baseSeq) {
          rejected(OpError.Conflict, { effectId: id, persistedSeq: existing.persistedSeq });
          return;
        }
        const nextSeq = existing.persistedSeq + 1;
        this.library = { ...this.library, effects: this.library.effects.map((e) => (e.id === id ? { id, persistedSeq: nextSeq, preset } : e)) };
        bumpRev();
        done({ effectId: id, persistedSeq: nextSeq });
        return;
      }
      case HOST_OP_DELETE_EFFECT: {
        const id = view.getUint16(4, true);
        const baseSeq = view.getUint16(6, true);
        const existing = this.library.effects.find((e) => e.id === id);
        if (!existing) {
          rejected(OpError.NotFound);
          return;
        }
        if (existing.persistedSeq !== baseSeq) {
          rejected(OpError.Conflict, { effectId: id, persistedSeq: existing.persistedSeq });
          return;
        }
        this.library = { ...this.library, effects: this.library.effects.filter((e) => e.id !== id) };
        bumpRev();
        done({ effectId: id });
        return;
      }
      case HOST_OP_ASSIGN: {
        const addr = formatAddrForFake(req.subarray(4, 10));
        const effectId = view.getUint16(10, true);
        const device = this.library.devices.find((d) => d.addr === addr);
        if (!device) {
          rejected(OpError.UnknownDevice);
          return;
        }
        const devices: LibraryDevice[] = this.library.devices.map((d) => (d.addr === addr ? { ...d, presetId: effectId } : d));
        this.library = { ...this.library, devices };
        bumpRev();
        done();
        return;
      }
      case HOST_OP_PREVIEW: {
        const effectId = view.getUint16(4, true);
        const preset = decodePresetBlob(req.subarray(6, 6 + 80));
        this.hostPreview = { effectId, preset, bypass: (flags & 1) !== 0 };
        done();
        return;
      }
      case HOST_OP_PREVIEW_END: {
        this.hostPreview = null;
        done();
        return;
      }
      case HOST_OP_PARSE_APO: {
        const nameLen = req[4];
        const name = new TextDecoder().decode(req.subarray(5, 5 + nameLen));
        const preset: Preset = { name, crossfeed: "off", bands: [], preamp: { kind: "auto" }, eqLocked: false };
        const collidesWith = this.library.effects.find((e) => e.preset.name === name)?.id ?? 0;
        const payload = encodeParseApoResultForTest({ blob: encodePresetBlob(preset), collidesWith, copyName: collidesWith ? `${name} 2` : "" });
        done({ payload });
        return;
      }
      case HOST_OP_SCAN_START: {
        if (this.radio.attempt) {
          rejected(OpError.RadioBusy);
          return;
        }
        if (this.radio.discovering && this.radio.scanOwner === "host") {
          done({ effectId: this.radio.scanSeq }); // idempotent, design sec 13.3
          return;
        }
        this.nextScanSeq = (this.nextScanSeq + 1) & 0xffff;
        this.bumpRadioRev();
        this.radio = { ...this.radio, discovering: true, scanOwner: "host", scanSeq: this.nextScanSeq, scan: this.scanCandidates, scanTotalAudio: this.scanCandidates.length };
        done({ effectId: this.nextScanSeq });
        return;
      }
      case HOST_OP_SCAN_STOP: {
        if (!this.radio.discovering) {
          done();
          return;
        }
        this.bumpRadioRev();
        this.radio = { ...this.radio, discovering: false, scanOwner: "none", scan: [], scanTotalAudio: 0 };
        done();
        return;
      }
      case HOST_OP_CONNECT: {
        const addr = formatAddrForFake(req.subarray(4, 10));
        if (addr === this.connectedAddr) {
          done({ effectId: 0 }); // no-op, design sec 13.3
          return;
        }
        if (this.radio.attempt) {
          rejected(OpError.RadioBusy);
          return;
        }
        const known = this.library.devices.some((d) => d.addr === addr) || this.scanCandidates.some((c) => c.addr === addr);
        if (!known) {
          rejected(OpError.UnknownDevice);
          return;
        }
        this.nextAttemptSeq = (this.nextAttemptSeq + 1) & 0xffff;
        this.bumpRadioRev();
        this.radio = { ...this.radio, connecting: true, discovering: false, scanOwner: "none", attempt: { seq: this.nextAttemptSeq, addr, initiator: "host", step: undefined, retries: 0 } };
        done({ effectId: this.nextAttemptSeq });
        return;
      }
      case HOST_OP_DISCONNECT: {
        const addr = formatAddrForFake(req.subarray(4, 10));
        if (addr !== this.connectedAddr) {
          rejected(OpError.NotConnected);
          return;
        }
        this.connectedAddr = null;
        this.library = { ...this.library, devices: this.library.devices.map((d) => (d.addr === addr ? { ...d, connected: false } : d)) };
        done();
        return;
      }
      case HOST_OP_FORGET: {
        const addr = formatAddrForFake(req.subarray(4, 10));
        if (!this.library.devices.some((d) => d.addr === addr)) {
          rejected(OpError.UnknownDevice);
          return;
        }
        if (this.radio.attempt?.addr === addr) {
          rejected(OpError.DeviceBusy);
          return;
        }
        this.library = { ...this.library, devices: this.library.devices.filter((d) => d.addr !== addr) };
        bumpRev();
        done();
        return;
      }
      case HOST_OP_SET_DEVICE_QUALITY: {
        const addr = formatAddrForFake(req.subarray(4, 10));
        const ldacQuality = req[10];
        if (!this.library.devices.some((d) => d.addr === addr)) {
          rejected(OpError.UnknownDevice);
          return;
        }
        if (ldacQuality < LDAC_QUALITY_MIN || ldacQuality > LDAC_QUALITY_MAX) {
          rejected(OpError.InvalidRequest);
          return;
        }
        done();
        return;
      }
      default:
        rejected(OpError.UnknownOp);
    }
  }

  private delay(): Promise<void> {
    const ms = this.options.latencyMs ?? 0;
    if (ms <= 0) return Promise.resolve();
    return new Promise((resolve) => setTimeout(resolve, ms));
  }
}

function sliceView(bytes: Uint8Array, length: number): DataView {
  const capped = bytes.subarray(0, Math.min(length, bytes.length));
  return new DataView(capped.buffer, capped.byteOffset, capped.byteLength);
}

function formatAddrForFake(bytes: Uint8Array): string {
  return Array.from(bytes)
    .map((b) => b.toString(16).toUpperCase().padStart(2, "0"))
    .join(":");
}
