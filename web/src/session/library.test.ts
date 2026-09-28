import { describe, expect, it, vi } from "vitest";
import { Session } from "./session";
import { LibraryController } from "./library";
import { FakeTransport } from "../transport/fake";
import type { FakeTransportOptions } from "../transport/fake";
import { emptyHomeSnapshot } from "../proto/telemetry";
import type { HomeSnapshot } from "../proto/telemetry";
import type { Preset } from "../proto/library";
import { OpError } from "../proto/ops";

const WARM: Preset = { name: "Warm", crossfeed: "off", bands: [], preamp: { kind: "auto" }, eqLocked: false };

function snapshotWithLibraryRev(rev: number): HomeSnapshot {
  return { ...emptyHomeSnapshot(), snapSeq: 1, extras: { libraryRev: rev, hostPreviewActive: false, deviceEditorOpen: false, deviceEditorEffectId: 0, presetsReady: true, codecFallbackReason: 0, radioRev: 0 } };
}

async function startedSession(fakeOpts: FakeTransportOptions = {}): Promise<{ session: Session; transport: FakeTransport }> {
  const transport = new FakeTransport({ enableLibrary: true, snapshot: () => snapshotWithLibraryRev(1), ...fakeOpts });
  const session = new Session(transport, { now: () => 0, visibilityDocument: undefined });
  await session.start();
  return { session, transport };
}

describe("LibraryController.opsAvailable", () => {
  it("is false without enableLibrary (info_ver 1, no op_mask)", async () => {
    const { session } = await startedSession({ enableLibrary: false });
    const controller = new LibraryController(session);
    expect(controller.opsAvailable()).toBe(false);
    await session.stop();
  });

  it("is true once GET_INFO reports v2 with the library bits set", async () => {
    const { session } = await startedSession();
    const controller = new LibraryController(session);
    expect(controller.opsAvailable()).toBe(true);
    await session.stop();
  });
});

describe("LibraryController.checkForLibraryChange / refreshLibrary", () => {
  it("refreshes on the first check and populates the store", async () => {
    const { session } = await startedSession();
    const controller = new LibraryController(session);
    await controller.checkForLibraryChange();
    const state = controller.store.getSnapshot();
    expect(state.phase).toBe("ready");
    expect(state.snapshot!.libraryRev).toBe(1);
    await session.stop();
  });

  it("does not refetch when library_rev is unchanged", async () => {
    const { session, transport } = await startedSession();
    const spy = vi.spyOn(transport, "controlIn");
    const controller = new LibraryController(session);
    await controller.checkForLibraryChange();
    const callsAfterFirst = spy.mock.calls.length;
    await controller.checkForLibraryChange();
    expect(spy.mock.calls.length).toBe(callsAfterFirst); // no new GET_LIBRARY call
    await session.stop();
  });

  it("goes to phase unavailable when ops aren't supported", async () => {
    const { session } = await startedSession({ enableLibrary: false });
    const controller = new LibraryController(session);
    await controller.checkForLibraryChange();
    expect(controller.store.getSnapshot().phase).toBe("unavailable");
    await session.stop();
  });
});

describe("LibraryController.saveEffect", () => {
  it("creates a new effect and confirms persistence", async () => {
    const { session } = await startedSession();
    const controller = new LibraryController(session);
    const outcome = await controller.saveEffect(WARM);
    expect(outcome.kind).toBe("queued");
    if (outcome.kind === "queued") {
      expect(outcome.effectId).toBeGreaterThan(0);
      expect(outcome.persistedSeq).toBe(1);
      expect(outcome.confirmed).toBe(true);
    }
    await session.stop();
  });

  it("surfaces a stale-base CONFLICT with the current persisted_seq", async () => {
    const { session } = await startedSession();
    const controller = new LibraryController(session);
    const created = await controller.saveEffect(WARM);
    if (created.kind !== "queued") throw new Error("expected queued");

    const stale = await controller.saveEffect({ ...WARM, name: "Bright" }, { id: created.effectId, baseSeq: 0 /* wrong: real seq is 1 */ });
    expect(stale).toEqual({ kind: "conflict", currentPersistedSeq: 1 });
    await session.stop();
  });

  it("updates an effect at the correct base_seq", async () => {
    const { session } = await startedSession();
    const controller = new LibraryController(session);
    const created = await controller.saveEffect(WARM);
    if (created.kind !== "queued") throw new Error("expected queued");

    const updated = await controller.saveEffect({ ...WARM, name: "Bright" }, { id: created.effectId, baseSeq: created.persistedSeq });
    expect(updated).toEqual({ kind: "queued", effectId: created.effectId, persistedSeq: 2, confirmed: true });
    await session.stop();
  });

  it("reports unavailable when ops aren't supported", async () => {
    const { session } = await startedSession({ enableLibrary: false });
    const controller = new LibraryController(session);
    expect(await controller.saveEffect(WARM)).toEqual({ kind: "unavailable" });
    await session.stop();
  });
});

describe("LibraryController.deleteEffect / assign", () => {
  it("NOT_FOUND on deleting an id that doesn't exist", async () => {
    const { session } = await startedSession();
    const controller = new LibraryController(session);
    const result = await controller.deleteEffect(999, 0);
    expect(result).toEqual({ kind: "rejected", error: OpError.NotFound });
    await session.stop();
  });

  it("UNKNOWN_DEVICE assigning to an unpaired address", async () => {
    const { session } = await startedSession();
    const controller = new LibraryController(session);
    const result = await controller.assign("00:11:22:33:44:55", 0);
    expect(result).toEqual({ kind: "rejected", error: OpError.UnknownDevice });
    await session.stop();
  });

  it("assigns and unassigns a paired device", async () => {
    const library = { libraryRev: 1, presetsReady: true, maxEffects: 8, maxDevices: 8, effects: [], devices: [{ addr: "94:DB:56:54:7C:F2", presetId: 0, connected: true, name: "Buds" }] };
    const { session } = await startedSession({ library });
    const controller = new LibraryController(session);
    const created = await controller.saveEffect(WARM);
    if (created.kind !== "queued") throw new Error("expected queued");

    const assigned = await controller.assign("94:DB:56:54:7C:F2", created.effectId);
    expect(assigned.kind).toBe("done");

    await controller.refreshLibrary();
    expect(controller.store.getSnapshot().snapshot!.devices[0].presetId).toBe(created.effectId);
    await session.stop();
  });
});

describe("LibraryController preview + keepalive", () => {
  it("previewStart arms a keepalive that re-sends on the interval", async () => {
    // Injects a no-op interval implementation (never actually fires) rather
    // than `vi.useFakeTimers()` -- `Session`'s own run loop schedules real
    // `setTimeout`s for its poll pacing, and faking timers globally here
    // would also freeze that loop's `sleep()`, wedging `enqueueCommand`
    // (this controller's every op goes through it) for the whole test.
    const { session } = await startedSession();
    let setIntervalCalls = 0;
    const noopInterval = (() => 0) as unknown as typeof setInterval;
    const controller = new LibraryController(session, {
      setIntervalFn: ((fn: () => void, ms: number) => {
        setIntervalCalls++;
        return noopInterval(fn, ms);
      }) as typeof setInterval,
      clearIntervalFn: () => {},
    });
    controller.start();

    const outcome = await controller.previewStart(0, WARM, false);
    expect(outcome.kind).toBe("done");
    expect(setIntervalCalls).toBe(1);

    await session.stop();
    controller.stop();
  });

  it("previewEnd stops the keepalive and clears the active preview", async () => {
    const { session } = await startedSession();
    const controller = new LibraryController(session);
    controller.start();
    await controller.previewStart(0, WARM, false);
    const outcome = await controller.previewEnd();
    expect(outcome.kind).toBe("done");
    await session.stop();
    controller.stop();
  });

  it("stops sending keepalive traffic while the document is hidden, and resumes on visible", async () => {
    const listeners = new Map<string, () => void>();
    const fakeDocImpl = {
      hidden: false,
      addEventListener(type: string, cb: () => void) {
        listeners.set(type, cb);
      },
      removeEventListener(type: string) {
        listeners.delete(type);
      },
    };
    const fakeDoc = fakeDocImpl as unknown as Document;

    const transport = new FakeTransport({ enableLibrary: true, snapshot: () => snapshotWithLibraryRev(1) });
    const session = new Session(transport, { now: () => 0, visibilityDocument: fakeDoc });
    await session.start();

    const controller = new LibraryController(session, { visibilityDocument: fakeDoc });
    controller.start();
    const controlOutSpy = vi.spyOn(transport, "controlOut");

    await controller.previewStart(0, WARM, false);
    const callsAfterStart = controlOutSpy.mock.calls.length;

    fakeDocImpl.hidden = true;
    listeners.get("visibilitychange")!();
    // Hidden: no automatic PREVIEW_END, no more keepalive sends expected without a real timer firing.
    expect(controlOutSpy.mock.calls.length).toBe(callsAfterStart);

    fakeDocImpl.hidden = false;
    listeners.get("visibilitychange")!();
    await vi.waitFor(() => expect(controlOutSpy.mock.calls.length).toBeGreaterThan(callsAfterStart));

    await session.stop();
    controller.stop();
  });

  it("previewStart itself refuses to send while hidden (not just the keepalive), and resumes on visible", async () => {
    const listeners = new Map<string, () => void>();
    const fakeDocImpl = {
      hidden: true,
      addEventListener(type: string, cb: () => void) {
        listeners.set(type, cb);
      },
      removeEventListener(type: string) {
        listeners.delete(type);
      },
    };
    const fakeDoc = fakeDocImpl as unknown as Document;

    const transport = new FakeTransport({ enableLibrary: true, snapshot: () => snapshotWithLibraryRev(1) });
    const session = new Session(transport, { now: () => 0, visibilityDocument: fakeDoc });
    await session.start();

    const controller = new LibraryController(session, { visibilityDocument: fakeDoc });
    controller.start();
    const controlOutSpy = vi.spyOn(transport, "controlOut");

    const outcome = await controller.previewStart(0, WARM, false);
    expect(outcome.kind).toBe("hidden");
    expect(controlOutSpy).not.toHaveBeenCalled();

    fakeDocImpl.hidden = false;
    listeners.get("visibilitychange")!();
    // Design: a return to visibility resumes the recorded `activePreview`.
    await vi.waitFor(() => expect(controlOutSpy).toHaveBeenCalled());

    await session.stop();
    controller.stop();
  });
});

describe("LibraryController.parseApo", () => {
  it("round-trips a PARSE_APO op", async () => {
    const { session } = await startedSession();
    const controller = new LibraryController(session);
    const outcome = await controller.parseApo("Warm", "Preamp: -6 dB\n");
    expect(outcome.kind).toBe("done");
    await session.stop();
  });
});
