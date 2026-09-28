// The web device-management session API: `GET_RADIO` refresh driven by
// telemetry's `radio_rev`, and the scan/connect/disconnect/forget/quality
// `HOST_OP`s -- per ADA DESIGN on pico-link-jyhk.24 (`.planning/design/
// 2026-09-27-iface6-eq-management-protocol.md` section 13, this task is its
// R6). Firmware does not implement 0x08 or ops 7..12 yet (that's this
// design's R4/R5, C + ui-ffi); every network call here is gated on
// `DeviceInfo.v2.opMask` (design section 13.3's op bits, same "info_ver
// v2's op_mask, bit N = op N" contract `LibraryController` already uses)
// so this never issues a request real hardware can't answer --
// `FakeTransport({ enableRadio: true })` emulates the device for tests and
// the dev page.
//
// Same "commands jump the queue ahead of the next telemetry poll" discipline
// as `LibraryController` (`session/library.ts`): every op/refresh goes
// through `Session.enqueueCommand`.
import type { Session } from "./session";
import { createStore } from "./store";
import type { Store } from "./store";
import { decodeRadioSnapshot, emptyRadioSnapshot } from "../proto/radio";
import type { RadioSnapshot } from "../proto/radio";
import { opMaskSupports } from "../proto/info";
import {
  decodeOpStatus,
  encodeConnectRequest,
  encodeDisconnectRequest,
  encodeForgetRequest,
  encodeScanStartRequest,
  encodeScanStopRequest,
  encodeSetDeviceQualityRequest,
  OpError,
} from "../proto/ops";
import type { OpStatus } from "../proto/ops";
import { PL_CFG_REQ_GET_OP_STATUS, PL_CFG_REQ_GET_RADIO, PL_CFG_REQ_HOST_OP } from "../transport/types";

export type RadioPhase = "unavailable" | "loading" | "ready" | "error";

export interface RadioState {
  phase: RadioPhase;
  snapshot: RadioSnapshot;
}

/**
 * `RadioOutcome`'s `word` is the reused `effect_id` wire slot -- design sec
 * 13.3: `SCAN_START` returns `scan_seq`, `CONNECT` returns `attempt_seq`,
 * every other op returns `0` (ignored). A caller correlates `word` against
 * the next `GET_RADIO`'s `scanSeq`/`attempt.seq` to know when *this*
 * specific scan/connect has actually progressed, rather than racing a
 * concurrent device- or auto-reconnect-owned one.
 */
export type RadioOpOutcome =
  | { kind: "done"; word: number }
  | { kind: "rejected"; error: OpError }
  | { kind: "timeout" }
  | { kind: "unavailable" };

export interface RadioControllerOptions {
  /** How often an in-flight `HOST_OP`'s `GET_OP_STATUS` is repolled. Default 15ms -- same as `LibraryController`. */
  opStatusPollMs?: number;
  /** How long to wait for a `HOST_OP` to leave `state == none`. Default 3000ms -- same as `LibraryController`. */
  opStatusTimeoutMs?: number;
  /** Injectable for tests; defaults to `performance.now`. */
  now?: () => number;
}

const DEFAULTS: Required<Omit<RadioControllerOptions, "now">> = {
  opStatusPollMs: 15,
  opStatusTimeoutMs: 3_000,
};

// Design sec 13.4/13.3: op_mask bit N = HOST_OP op N (`opMaskSupports`'s
// existing contract, `LibraryController`'s `OP_*_BIT` constants use the
// same numbering space). GET_RADIO itself has no op_mask bit of its own --
// design sec 13.4's GET_INFO v3 gates it on `radio_proto`/`radio_max_len`
// instead (a later bead's C/ui-ffi work); this controller gates its own
// GET_RADIO reads on the same op bits that gate the ops it exists to
// support, since a build old enough to lack ops 7..12 has no radio session
// state worth reading either.
const OP_SCAN_START_BIT = 7;
const OP_SCAN_STOP_BIT = 8;
const OP_CONNECT_BIT = 9;
const OP_DISCONNECT_BIT = 10;
const OP_FORGET_BIT = 11;
const OP_SET_DEVICE_QUALITY_BIT = 12;

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * Owns the `GET_RADIO`/`HOST_OP`/`GET_OP_STATUS` traffic (ops 7..12) for
 * one `Session`. Construct after the session reaches `"ready"`, same
 * lifecycle as `LibraryController`.
 *
 * Like `LibraryController`, this class runs no polling timer of its own for
 * `radio_rev` changes -- a caller invokes `checkForRadioChange()` on the
 * same cadence it already reads `session.snapshotRef` (design sec 13.4:
 * "the page re-reads GET_RADIO only when radio_rev moves").
 */
export class RadioController {
  readonly store: Store<RadioState> = createStore<RadioState>({ phase: "unavailable", snapshot: emptyRadioSnapshot() });

  private readonly session: Session;
  private readonly opts: Required<Omit<RadioControllerOptions, "now">>;
  private readonly now: () => number;

  private lastKnownRev: number | null = null;
  private nextSeq: number | null = null;
  private seqInitPromise: Promise<void> | null = null;

  constructor(session: Session, options: RadioControllerOptions = {}) {
    this.session = session;
    this.opts = { ...DEFAULTS, ...options };
    this.now = options.now ?? (() => performance.now());
  }

  /** `true` once `GET_INFO` reports `info_ver >= 2` and every radio op bit (7..12) is set in `op_mask`. */
  opsAvailable(): boolean {
    const info = this.session.statusStore.getSnapshot().info;
    const v2 = info?.v2;
    if (!v2) return false;
    return (
      opMaskSupports(v2.opMask, OP_SCAN_START_BIT) &&
      opMaskSupports(v2.opMask, OP_SCAN_STOP_BIT) &&
      opMaskSupports(v2.opMask, OP_CONNECT_BIT) &&
      opMaskSupports(v2.opMask, OP_DISCONNECT_BIT) &&
      opMaskSupports(v2.opMask, OP_FORGET_BIT) &&
      opMaskSupports(v2.opMask, OP_SET_DEVICE_QUALITY_BIT)
    );
  }

  /**
   * Call on the same cadence the caller already reads
   * `session.snapshotRef` (design sec 13.4). Compares the latest telemetry
   * `extras.radioRev` against what this controller already fetched, and
   * refreshes only on a change (or on the very first call).
   */
  async checkForRadioChange(): Promise<void> {
    if (!this.opsAvailable()) {
      if (this.store.getSnapshot().phase !== "unavailable") this.store.set({ phase: "unavailable", snapshot: emptyRadioSnapshot() });
      return;
    }
    const rev = this.session.snapshotRef.current?.extras?.radioRev;
    if (rev === undefined) return;
    if (this.lastKnownRev !== null && rev === this.lastKnownRev) return;
    await this.refreshRadio();
  }

  /** Unconditionally re-fetches `GET_RADIO` and updates `store`. */
  async refreshRadio(): Promise<void> {
    if (!this.opsAvailable()) return;
    const current = this.store.getSnapshot();
    if (current.phase !== "ready") this.store.set({ phase: "loading", snapshot: current.snapshot });
    try {
      const bytes = await this.session.enqueueCommand(async () => {
        const view = await this.session.transport.controlIn(PL_CFG_REQ_GET_RADIO, 0, 1536);
        return new Uint8Array(view.buffer, view.byteOffset, view.byteLength);
      });
      const snapshot = decodeRadioSnapshot(bytes);
      if (!snapshot) {
        this.store.set({ phase: "error", snapshot: this.store.getSnapshot().snapshot });
        return;
      }
      this.lastKnownRev = snapshot.radioRev;
      this.store.set({ phase: "ready", snapshot });
    } catch {
      this.store.set({ phase: "error", snapshot: this.store.getSnapshot().snapshot });
    }
  }

  /** `SCAN_START` (design sec 13.3, op 7) -- `done.word` is the resulting `scan_seq`. Idempotent against an already-running host scan (core's own no-op rule); never restarts an inquiry itself. */
  async startScan(): Promise<RadioOpOutcome> {
    return this.runOp((seq) => encodeScanStartRequest(seq));
  }

  /** `SCAN_STOP` (design sec 13.3, op 8). */
  async stopScan(): Promise<RadioOpOutcome> {
    return this.runOp((seq) => encodeScanStopRequest(seq));
  }

  /** `CONNECT` (design sec 13.3, op 9) -- `done.word` is the resulting `attempt_seq` (`0` if `addr` was already `connected_addr`, a no-op). */
  async connect(addr: string): Promise<RadioOpOutcome> {
    return this.runOp((seq) => encodeConnectRequest(seq, addr));
  }

  /** `DISCONNECT` (design sec 13.3, op 10). `addr` is an intent guard only -- core rejects `NOT_CONNECTED` unless it equals the live `connected_addr`. */
  async disconnect(addr: string): Promise<RadioOpOutcome> {
    return this.runOp((seq) => encodeDisconnectRequest(seq, addr));
  }

  /** `FORGET` (design sec 13.3, op 11). */
  async forget(addr: string): Promise<RadioOpOutcome> {
    return this.runOp((seq) => encodeForgetRequest(seq, addr));
  }

  /** `SET_DEVICE_QUALITY` (design sec 13.3, op 12). `ldacQuality`: 1..3, `4` = Adaptive. */
  async setDeviceQuality(addr: string, ldacQuality: number): Promise<RadioOpOutcome> {
    return this.runOp((seq) => encodeSetDeviceQualityRequest(seq, addr, ldacQuality));
  }

  // --- HOST_OP / GET_OP_STATUS plumbing (same shape as LibraryController's own) -

  private async ensureSeqInitialized(): Promise<void> {
    if (this.nextSeq !== null) return;
    if (!this.seqInitPromise) {
      this.seqInitPromise = this.session
        .enqueueCommand(async () => {
          const view = await this.session.transport.controlIn(PL_CFG_REQ_GET_OP_STATUS, 0, 256);
          const status = decodeOpStatus(new Uint8Array(view.buffer, view.byteOffset, view.byteLength));
          // Same "start at reply.seq + 1" contract `LibraryController` uses
          // -- both controllers share the one `seq` space `HOST_OP`/
          // `GET_OP_STATUS` define (design section 4), so a page running
          // both should really share a single sequencer; kept separate
          // here (own `nextSeq`) because nothing in either controller
          // assumes the other's `seq` values, and a shared sequencer is a
          // cross-controller wiring decision for whichever bead builds the
          // page that uses both.
          this.nextSeq = ((status?.seq ?? 0) + 1) & 0xff;
        })
        .catch(() => {
          this.nextSeq = 1;
        });
    }
    await this.seqInitPromise;
  }

  private async runOp(buildRequest: (seq: number) => Uint8Array): Promise<RadioOpOutcome> {
    if (!this.opsAvailable()) return { kind: "unavailable" };
    await this.ensureSeqInitialized();
    const seq = this.nextSeq!;
    this.nextSeq = (seq + 1) & 0xff;

    const result = await this.session.enqueueCommand(async () => {
      await this.session.transport.controlOut(PL_CFG_REQ_HOST_OP, 0, buildRequest(seq));
      return this.pollStatus(seq);
    });

    if (result === "timeout") return { kind: "timeout" };
    if (result.state === "rejected") return { kind: "rejected", error: result.error };
    return { kind: "done", word: result.effectId };
  }

  private async pollStatus(seq: number): Promise<OpStatus | "timeout"> {
    const deadline = this.now() + this.opts.opStatusTimeoutMs;
    for (;;) {
      const view = await this.session.transport.controlIn(PL_CFG_REQ_GET_OP_STATUS, 0, 256);
      const status = decodeOpStatus(new Uint8Array(view.buffer, view.byteOffset, view.byteLength));
      if (status && status.seq === seq && status.state !== "none") {
        return status;
      }
      if (this.now() >= deadline) return "timeout";
      await sleep(this.opts.opStatusPollMs);
    }
  }
}
