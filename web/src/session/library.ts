// The EQ library/ops session API: `GET_LIBRARY` refresh driven by
// telemetry's `library_rev`, a `HOST_OP`/`GET_OP_STATUS` queue, preview
// keepalive, and save confirmation -- per ADA DESIGN on pico-link-jyhk.17
// (`.planning/design/2026-09-27-iface6-eq-management-protocol.md`, this
// task is its section 11 task 5). Firmware does not implement 0x05-0x07
// yet; every network call here is gated on `DeviceInfo.v2.opMask` (design
// section 8) so this never issues a request real hardware can't answer --
// `FakeTransport({ enableLibrary: true })` emulates the device for tests
// and the dev page.
//
// Every network call goes through `Session.enqueueCommand` (FERN DESIGN
// section 4: "commands ... FIFO and served first") so library reads/writes
// always jump ahead of the next telemetry poll, the same as any future
// IMPORT_PRESET-style command.
import type { Session } from "./session";
import { createStore } from "./store";
import type { Store } from "./store";
import { decodeLibrarySnapshot, encodePresetBlob } from "../proto/library";
import type { LibrarySnapshot, Preset } from "../proto/library";
import { opMaskSupports } from "../proto/info";
import { decodeOpStatus, encodeAssignRequest, encodeDeleteEffectRequest, encodeParseApoRequest, encodePreviewEndRequest, encodePreviewRequest, encodeSaveEffectRequest, OpError } from "../proto/ops";
import type { OpStatus } from "../proto/ops";
import { PL_CFG_REQ_GET_LIBRARY, PL_CFG_REQ_GET_OP_STATUS, PL_CFG_REQ_HOST_OP } from "../transport/types";

export type LibraryPhase = "unavailable" | "loading" | "ready" | "error";

export interface LibraryState {
  phase: LibraryPhase;
  snapshot: LibrarySnapshot | null;
}

export type OpOutcome =
  | { kind: "done"; status: OpStatus }
  | { kind: "conflict"; currentPersistedSeq: number }
  | { kind: "editorOpen" }
  | { kind: "rejected"; error: OpError }
  | { kind: "timeout" }
  | { kind: "unavailable" }
  /** `previewStart`/`previewEnd` refused to send because `document.hidden` -- not a real failure, just nothing sent this call. */
  | { kind: "hidden" };

export type SaveOutcome = Exclude<OpOutcome, { kind: "done" }> | { kind: "queued"; effectId: number; persistedSeq: number; confirmed: boolean };

export interface LibraryControllerOptions {
  /** How often `HOST_OP`'s in-flight `GET_OP_STATUS` is repolled while waiting for `state != none`. Default 15ms. */
  opStatusPollMs?: number;
  /** How long to wait for a `HOST_OP` to leave `state == none` before giving up (design section 9: a flash write can NAK EP0 for "tens of ms"). Default 3000ms. */
  opStatusTimeoutMs?: number;
  /** How often `checkForLibraryChange` should be called by a caller-driven interval; this class does not start its own timer -- see `start()`'s doc comment. Default 100ms. */
  changeCheckIntervalMs?: number;
  /** How long `saveEffect` waits for `persisted_seq` to confirm before reporting `confirmed: false` (design section 6: "a 5s timeout reads 'not confirmed'"). Default 5000ms. */
  saveConfirmTimeoutMs?: number;
  /** How often to re-check confirmation while waiting. Default 100ms. */
  confirmPollMs?: number;
  /** Re-send interval for an active host preview -- must stay comfortably under the device's 2s auto-revert lease (design section 7). Default 800ms. */
  previewKeepaliveMs?: number;
  /** Injectable for tests; defaults to `performance.now`. */
  now?: () => number;
  /** Injectable for tests; defaults to the global `document` if present. */
  visibilityDocument?: Document;
  /** Injectable for tests. */
  setIntervalFn?: typeof setInterval;
  clearIntervalFn?: typeof clearInterval;
}

const DEFAULTS: Required<Omit<LibraryControllerOptions, "now" | "visibilityDocument" | "setIntervalFn" | "clearIntervalFn">> = {
  opStatusPollMs: 15,
  opStatusTimeoutMs: 3_000,
  changeCheckIntervalMs: 100,
  saveConfirmTimeoutMs: 5_000,
  confirmPollMs: 100,
  previewKeepaliveMs: 800,
};

const OP_GET_LIBRARY_BIT = 5;
const OP_HOST_OP_BIT = 6;
const OP_GET_OP_STATUS_BIT = 7;

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

interface ActivePreview {
  effectId: number;
  preset: Preset;
  bypass: boolean;
}

/**
 * Owns the `GET_LIBRARY`/`HOST_OP`/`GET_OP_STATUS` traffic for one
 * `Session`. Construct after the session reaches `"ready"`; `start()`/
 * `stop()` bracket its lifetime the same as `Session.start()`/`stop()`.
 *
 * This class does not run its own polling timer for `library_rev` changes
 * -- unlike `Session`'s telemetry loop, there is no `setInterval` inside
 * this file driving `checkForLibraryChange()`. A caller (the dev page,
 * `HomeApp`, or a test) is expected to invoke it on the same cadence it
 * already reads `session.snapshotRef` (design section 6: "the page polls
 * at 30Hz already"), which keeps this class synchronously testable without
 * fake timers for the read path. The preview keepalive (a genuine
 * background repeat, not tied to a snapshot arriving) DOES own a real
 * `setInterval`, injectable via `setIntervalFn`/`clearIntervalFn`.
 */
export class LibraryController {
  readonly store: Store<LibraryState> = createStore<LibraryState>({ phase: "unavailable", snapshot: null });

  private readonly session: Session;
  private readonly opts: Required<Omit<LibraryControllerOptions, "now" | "visibilityDocument" | "setIntervalFn" | "clearIntervalFn">>;
  private readonly now: () => number;
  private readonly visibilityDocument: Document | undefined;
  private readonly setIntervalFn: typeof setInterval;
  private readonly clearIntervalFn: typeof clearInterval;

  private lastKnownRev: number | null = null;
  private nextSeq: number | null = null;
  private seqInitPromise: Promise<void> | null = null;

  private activePreview: ActivePreview | null = null;
  private previewTimer: ReturnType<typeof setInterval> | undefined;
  private visibilityHandler: (() => void) | undefined;
  private started = false;

  constructor(session: Session, options: LibraryControllerOptions = {}) {
    this.session = session;
    this.opts = { ...DEFAULTS, ...options };
    this.now = options.now ?? (() => performance.now());
    this.visibilityDocument = options.visibilityDocument ?? (typeof document !== "undefined" ? document : undefined);
    this.setIntervalFn = options.setIntervalFn ?? setInterval;
    this.clearIntervalFn = options.clearIntervalFn ?? clearInterval;
  }

  /** Registers the visibility handler that pauses/resumes the preview keepalive. Call once, after the session is ready. */
  start(): void {
    if (this.started) return;
    this.started = true;
    if (this.visibilityDocument) {
      this.visibilityHandler = () => this.onVisibilityChange();
      this.visibilityDocument.addEventListener("visibilitychange", this.visibilityHandler);
    }
  }

  /** Stops the preview keepalive and unregisters listeners. Does not send `PREVIEW_END` -- design section 7: on close/hidden the device's own 2s lease reverts audio; explicit `previewEnd()` is a caller decision ("leaving the editor"). */
  stop(): void {
    this.started = false;
    this.stopKeepalive();
    if (this.visibilityDocument && this.visibilityHandler) {
      this.visibilityDocument.removeEventListener("visibilitychange", this.visibilityHandler);
      this.visibilityHandler = undefined;
    }
  }

  /** `true` once `GET_INFO` reports `info_ver >= 2` and every op this controller needs (`GET_LIBRARY`, `HOST_OP`, `GET_OP_STATUS`) is set in `op_mask`. */
  opsAvailable(): boolean {
    const info = this.session.statusStore.getSnapshot().info;
    const v2 = info?.v2;
    if (!v2) return false;
    return opMaskSupports(v2.opMask, OP_GET_LIBRARY_BIT) && opMaskSupports(v2.opMask, OP_HOST_OP_BIT) && opMaskSupports(v2.opMask, OP_GET_OP_STATUS_BIT);
  }

  /**
   * Call on the same cadence the caller already reads
   * `session.snapshotRef` (design section 6). Compares the latest
   * telemetry `extras.libraryRev` against what this controller already
   * fetched, and refreshes only on a change (or on the very first call) --
   * the whole point of a caller-provided revision rather than polling
   * `GET_LIBRARY` at 30Hz "just in case".
   */
  async checkForLibraryChange(): Promise<void> {
    if (!this.opsAvailable()) {
      if (this.store.getSnapshot().phase !== "unavailable") this.store.set({ phase: "unavailable", snapshot: null });
      return;
    }
    const rev = this.session.snapshotRef.current?.extras?.libraryRev;
    if (rev === undefined) return;
    if (this.lastKnownRev !== null && rev === this.lastKnownRev) return;
    await this.refreshLibrary();
  }

  /** Unconditionally re-fetches `GET_LIBRARY` and updates `store`. */
  async refreshLibrary(): Promise<void> {
    if (!this.opsAvailable()) return;
    if (this.store.getSnapshot().phase !== "ready") this.store.set({ phase: "loading", snapshot: this.store.getSnapshot().snapshot });
    try {
      const bytes = await this.session.enqueueCommand(async () => {
        const view = await this.session.transport.controlIn(PL_CFG_REQ_GET_LIBRARY, 0, 1536);
        return new Uint8Array(view.buffer, view.byteOffset, view.byteLength);
      });
      const snapshot = decodeLibrarySnapshot(bytes);
      if (!snapshot) {
        this.store.set({ phase: "error", snapshot: this.store.getSnapshot().snapshot });
        return;
      }
      this.lastKnownRev = snapshot.libraryRev;
      this.store.set({ phase: "ready", snapshot });
    } catch {
      this.store.set({ phase: "error", snapshot: this.store.getSnapshot().snapshot });
    }
  }

  /**
   * Creates (`existing` omitted) or updates (`existing` given) an effect.
   * `confirmed` in a `"queued"` result is `true` only once a subsequent
   * `GET_LIBRARY` shows `persisted_seq` has advanced to (at least) what
   * this save produced -- design section 6: "DONE on SAVE means queued,
   * not persisted... Saved when that id's persisted_seq in the library
   * passes the op status's persisted_seq." A `false` after
   * `saveConfirmTimeoutMs` reads as "not confirmed" per that same section,
   * not as a failure -- the save may still land.
   */
  async saveEffect(preset: Preset, existing?: { id: number; baseSeq: number }): Promise<SaveOutcome> {
    const blob = encodePresetBlob(preset);
    const id = existing?.id ?? 0;
    const baseSeq = existing?.baseSeq ?? 0;
    const outcome = await this.runOp((seq) => encodeSaveEffectRequest(seq, id, baseSeq, blob));
    if (outcome.kind !== "done") return outcome;

    const effectId = outcome.status.effectId;
    const persistedSeq = outcome.status.persistedSeq;
    const confirmed = await this.waitForPersisted(effectId, persistedSeq, blob);
    return { kind: "queued", effectId, persistedSeq, confirmed };
  }

  async deleteEffect(id: number, baseSeq: number): Promise<OpOutcome> {
    return this.runOp((seq) => encodeDeleteEffectRequest(seq, id, baseSeq));
  }

  /** `effectId: 0` unassigns (Off). No `base_seq` -- design section 4: "Last writer wins, no base_seq (a pick is idempotent)." */
  async assign(addr: string, effectId: number): Promise<OpOutcome> {
    return this.runOp((seq) => encodeAssignRequest(seq, addr, effectId));
  }

  /**
   * `effectId: 0` previews an unsaved draft. Arms the keepalive on success
   * (design section 7: coalesced at the caller's own rate, at most ~10/s --
   * this class does not itself rate-limit calls to `previewStart`, only the
   * background keepalive).
   *
   * While the document is hidden this refuses to send at all -- it records
   * `activePreview` so a return to the tab can resume it (`onVisibilityChange`),
   * but issues no `PREVIEW` traffic. This is the single gate every preview
   * send in the app must go through; a caller running its own resend loop
   * around this method would bypass it (see `EffectsTab`'s review fix on
   * this bead -- it used to run a parallel `setInterval` with no visibility
   * check at all).
   */
  async previewStart(effectId: number, preset: Preset, bypass: boolean): Promise<OpOutcome> {
    this.activePreview = { effectId, preset, bypass };
    if (this.isHidden()) {
      this.stopKeepalive();
      return { kind: "hidden" };
    }
    const outcome = await this.sendActivePreview();
    if (outcome.kind === "done" && !this.isHidden()) this.armKeepalive();
    return outcome;
  }

  /** Ends the host preview (design section 7: "leaving the editor sends PREVIEW_END"). */
  async previewEnd(): Promise<OpOutcome> {
    this.stopKeepalive();
    this.activePreview = null;
    return this.runOp((seq) => encodePreviewEndRequest(seq));
  }

  /** `HOST_OP` `PARSE_APO` -- core does the parsing; this only round-trips the request/response (design section 4: "so JS implements neither the parser nor the suffix rule"). */
  async parseApo(name: string, apoText: string): Promise<OpOutcome> {
    return this.runOp((seq) => encodeParseApoRequest(seq, name, apoText));
  }

  // --- Preview keepalive ---------------------------------------------

  private armKeepalive(): void {
    this.stopKeepalive();
    this.previewTimer = this.setIntervalFn(() => {
      if (this.activePreview && !this.isHidden()) void this.sendActivePreview();
    }, this.opts.previewKeepaliveMs);
  }

  private stopKeepalive(): void {
    if (this.previewTimer !== undefined) {
      this.clearIntervalFn(this.previewTimer);
      this.previewTimer = undefined;
    }
  }

  private async sendActivePreview(): Promise<OpOutcome> {
    const preview = this.activePreview;
    if (!preview) return { kind: "unavailable" };
    const blob = encodePresetBlob(preview.preset);
    return this.runOp((seq) => encodePreviewRequest(seq, preview.effectId, blob, preview.bypass));
  }

  private onVisibilityChange(): void {
    if (this.isHidden()) {
      // Design section 7: "tab hidden (polling stops) ... audio reverts
      // within 2s. The draft stays in the page." Stop sending, but keep
      // `activePreview` so a return to the tab can resume it.
      this.stopKeepalive();
      return;
    }
    if (this.activePreview) {
      void this.sendActivePreview().then((outcome) => {
        if (outcome.kind === "done") this.armKeepalive();
      });
    }
  }

  private isHidden(): boolean {
    return this.visibilityDocument?.hidden ?? false;
  }

  // --- HOST_OP / GET_OP_STATUS plumbing -------------------------------

  private async ensureSeqInitialized(): Promise<void> {
    if (this.nextSeq !== null) return;
    if (!this.seqInitPromise) {
      this.seqInitPromise = this.session
        .enqueueCommand(async () => {
          const view = await this.session.transport.controlIn(PL_CFG_REQ_GET_OP_STATUS, 0, 256);
          const status = decodeOpStatus(new Uint8Array(view.buffer, view.byteOffset, view.byteLength));
          // Design section 4: "On session start the page reads
          // GET_OP_STATUS once and starts at reply.seq + 1, so a reload
          // cannot match a stale completion."
          this.nextSeq = ((status?.seq ?? 0) + 1) & 0xff;
        })
        .catch(() => {
          this.nextSeq = 1;
        });
    }
    await this.seqInitPromise;
  }

  private async runOp(buildRequest: (seq: number) => Uint8Array): Promise<OpOutcome> {
    if (!this.opsAvailable()) return { kind: "unavailable" };
    await this.ensureSeqInitialized();
    const seq = this.nextSeq!;
    this.nextSeq = (seq + 1) & 0xff;

    const result = await this.session.enqueueCommand(async () => {
      await this.session.transport.controlOut(PL_CFG_REQ_HOST_OP, 0, buildRequest(seq));
      return this.pollStatus(seq);
    });

    if (result === "timeout") return { kind: "timeout" };
    if (result.state === "rejected") {
      if (result.error === OpError.Conflict) return { kind: "conflict", currentPersistedSeq: result.persistedSeq };
      if (result.error === OpError.EditorOpen) return { kind: "editorOpen" };
      return { kind: "rejected", error: result.error };
    }
    return { kind: "done", status: result };
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

  private async waitForPersisted(effectId: number, expectedSeq: number, expectedBlob: Uint8Array): Promise<boolean> {
    const deadline = this.now() + this.opts.saveConfirmTimeoutMs;
    for (;;) {
      await this.refreshLibrary();
      const snap = this.store.getSnapshot().snapshot;
      const effect = snap?.effects.find((e) => e.id === effectId);
      if (effect && effect.persistedSeq >= expectedSeq) {
        // Belt-and-braces per design section 6: "A refusal arrives as the
        // truth echo (old blob...)". If persisted_seq is at least what we
        // expect but the encoded blob differs, treat as not-yet-confirmed
        // (a later save may have already advanced it further, which is
        // fine -- that's still confirmation of THIS controller's write
        // only when the content matches).
        const encoded = encodePresetBlob(effect.preset);
        if (bytesEqual(encoded, expectedBlob)) return true;
      }
      if (this.now() >= deadline) return false;
      await sleep(this.opts.confirmPollMs);
    }
  }
}

function bytesEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}
