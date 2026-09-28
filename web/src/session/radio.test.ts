import { describe, expect, it, vi } from "vitest";
import { Session } from "./session";
import { RadioController } from "./radio";
import { FakeTransport } from "../transport/fake";
import type { FakeTransportOptions } from "../transport/fake";
import { emptyHomeSnapshot } from "../proto/telemetry";
import type { HomeSnapshot } from "../proto/telemetry";
import { OpError } from "../proto/ops";
import { PL_CFG_REQ_GET_RADIO } from "../transport/types";

function snapshotWithRadioRev(rev: number): HomeSnapshot {
  return { ...emptyHomeSnapshot(), snapSeq: 1, extras: { libraryRev: 0, hostPreviewActive: false, deviceEditorOpen: false, deviceEditorEffectId: 0, presetsReady: true, codecFallbackReason: 0, radioRev: rev } };
}

async function startedSession(fakeOpts: FakeTransportOptions = {}): Promise<{ session: Session; transport: FakeTransport }> {
  const transport = new FakeTransport({ enableRadio: true, snapshot: () => snapshotWithRadioRev(1), ...fakeOpts });
  const session = new Session(transport, { now: () => 0, visibilityDocument: undefined });
  await session.start();
  return { session, transport };
}

describe("RadioController.opsAvailable", () => {
  it("is false without enableRadio (info_ver 1, no op_mask)", async () => {
    const { session } = await startedSession({ enableRadio: false });
    const controller = new RadioController(session);
    expect(controller.opsAvailable()).toBe(false);
    await session.stop();
  });

  it("is true once GET_INFO reports v2 with the radio op bits (7..12) set", async () => {
    const { session } = await startedSession();
    const controller = new RadioController(session);
    expect(controller.opsAvailable()).toBe(true);
    await session.stop();
  });
});

describe("RadioController.checkForRadioChange / refreshRadio", () => {
  it("refreshes on the first check and populates the store", async () => {
    const { session } = await startedSession();
    const controller = new RadioController(session);
    await controller.checkForRadioChange();
    const state = controller.store.getSnapshot();
    expect(state.phase).toBe("ready");
    expect(state.snapshot.radioRev).toBe(1);
    await session.stop();
  });

  it("does not refetch when radio_rev is unchanged", async () => {
    const { session, transport } = await startedSession();
    const controller = new RadioController(session);
    // Prime `lastKnownRev` deterministically -- `checkForRadioChange`
    // itself races Session's own background telemetry poll for
    // `snapshotRef.current` (same race `LibraryController.
    // checkForLibraryChange` has), which this test isn't exercising.
    await controller.refreshRadio();
    const spy = vi.spyOn(transport, "controlIn");
    await controller.checkForRadioChange();
    const radioCalls = spy.mock.calls.filter((call) => call[0] === PL_CFG_REQ_GET_RADIO).length;
    expect(radioCalls).toBe(0); // no new GET_RADIO call -- radio_rev is still 1
    await session.stop();
  });

  it("goes to phase unavailable when ops aren't supported", async () => {
    const { session } = await startedSession({ enableRadio: false });
    const controller = new RadioController(session);
    await controller.checkForRadioChange();
    expect(controller.store.getSnapshot().phase).toBe("unavailable");
    await session.stop();
  });
});

describe("RadioController.startScan / stopScan", () => {
  it("starts a scan and surfaces the seeded candidates via a refresh", async () => {
    const { session, transport } = await startedSession();
    transport.setScanCandidates([{ addr: "94:DB:56:54:7C:20", bars: 3, alreadyPaired: false, name: "Cans" }]);
    const controller = new RadioController(session);

    const outcome = await controller.startScan();
    expect(outcome.kind).toBe("done");
    if (outcome.kind === "done") expect(outcome.word).toBeGreaterThan(0);

    await controller.refreshRadio();
    const snap = controller.store.getSnapshot().snapshot;
    expect(snap.discovering).toBe(true);
    expect(snap.scanOwner).toBe("host");
    expect(snap.scan).toHaveLength(1);
    expect(snap.scan[0].name).toBe("Cans");
    await session.stop();
  });

  it("SCAN_START is idempotent against its own running scan", async () => {
    const { session } = await startedSession();
    const controller = new RadioController(session);
    const first = await controller.startScan();
    const second = await controller.startScan();
    expect(first.kind).toBe("done");
    expect(second.kind).toBe("done");
    if (first.kind === "done" && second.kind === "done") expect(second.word).toBe(first.word);
    await session.stop();
  });

  it("stops a running scan", async () => {
    const { session } = await startedSession();
    const controller = new RadioController(session);
    await controller.startScan();
    const outcome = await controller.stopScan();
    expect(outcome.kind).toBe("done");
    await controller.refreshRadio();
    expect(controller.store.getSnapshot().snapshot.discovering).toBe(false);
    await session.stop();
  });
});

describe("RadioController.connect / disconnect / forget / setDeviceQuality", () => {
  it("connect to an unknown address rejects UNKNOWN_DEVICE", async () => {
    const { session } = await startedSession();
    const controller = new RadioController(session);
    const outcome = await controller.connect("94:DB:56:54:7C:99");
    expect(outcome).toEqual({ kind: "rejected", error: OpError.UnknownDevice });
    await session.stop();
  });

  it("connect to a discovered audio sink records an in-flight attempt with a nonzero attempt_seq", async () => {
    const { session, transport } = await startedSession();
    transport.setScanCandidates([{ addr: "94:DB:56:54:7C:20", bars: 3, alreadyPaired: false, name: "Cans" }]);
    const controller = new RadioController(session);

    const outcome = await controller.connect("94:DB:56:54:7C:20");
    expect(outcome.kind).toBe("done");
    if (outcome.kind === "done") expect(outcome.word).toBeGreaterThan(0);

    await controller.refreshRadio();
    const snap = controller.store.getSnapshot().snapshot;
    expect(snap.connecting).toBe(true);
    expect(snap.attempt?.addr).toBe("94:DB:56:54:7C:20");
    expect(snap.attempt?.initiator).toBe("host");
    await session.stop();
  });

  it("a second CONNECT while one is in flight is RADIO_BUSY", async () => {
    const { session, transport } = await startedSession();
    transport.setScanCandidates([
      { addr: "94:DB:56:54:7C:20", bars: 3, alreadyPaired: false, name: "Cans" },
      { addr: "94:DB:56:54:7C:21", bars: 3, alreadyPaired: false, name: "Other" },
    ]);
    const controller = new RadioController(session);
    await controller.connect("94:DB:56:54:7C:20");
    const second = await controller.connect("94:DB:56:54:7C:21");
    expect(second).toEqual({ kind: "rejected", error: OpError.RadioBusy });
    await session.stop();
  });

  it("resolving a connect attempt ok, then DISCONNECT, round-trips through GET_RADIO", async () => {
    const { session, transport } = await startedSession();
    transport.setScanCandidates([{ addr: "94:DB:56:54:7C:20", bars: 3, alreadyPaired: false, name: "Cans" }]);
    const controller = new RadioController(session);
    await controller.connect("94:DB:56:54:7C:20");
    transport.resolveConnectAttempt("ok");

    await controller.refreshRadio();
    let snap = controller.store.getSnapshot().snapshot;
    expect(snap.connecting).toBe(false);
    expect(snap.attempt).toBeUndefined();
    expect(snap.lastOutcome?.result).toBe("ok");

    const disconnectOutcome = await controller.disconnect("94:DB:56:54:7C:20");
    expect(disconnectOutcome.kind).toBe("done");

    const notConnected = await controller.disconnect("94:DB:56:54:7C:20");
    expect(notConnected).toEqual({ kind: "rejected", error: OpError.NotConnected });
    await session.stop();
  });

  it("FORGET an unknown device rejects UNKNOWN_DEVICE; a known one succeeds", async () => {
    const { session, transport } = await startedSession({
      library: { libraryRev: 1, presetsReady: true, maxEffects: 8, maxDevices: 8, effects: [], devices: [{ addr: "94:DB:56:54:7C:20", presetId: 0, connected: false, name: "Cans" }] },
    });
    const controller = new RadioController(session);
    const unknown = await controller.forget("94:DB:56:54:7C:99");
    expect(unknown).toEqual({ kind: "rejected", error: OpError.UnknownDevice });

    const known = await controller.forget("94:DB:56:54:7C:20");
    expect(known.kind).toBe("done");
    expect(transport.currentLibrarySnapshotForTest().devices).toHaveLength(0);
    await session.stop();
  });

  it("SET_DEVICE_QUALITY out of range rejects INVALID_REQUEST", async () => {
    const { session } = await startedSession({
      library: { libraryRev: 1, presetsReady: true, maxEffects: 8, maxDevices: 8, effects: [], devices: [{ addr: "94:DB:56:54:7C:20", presetId: 0, connected: false, name: "Cans" }] },
    });
    const controller = new RadioController(session);
    const outcome = await controller.setDeviceQuality("94:DB:56:54:7C:20", 9);
    expect(outcome).toEqual({ kind: "rejected", error: OpError.InvalidRequest });
    await session.stop();
  });

  it("unavailable when radio ops aren't advertised", async () => {
    const { session } = await startedSession({ enableRadio: false });
    const controller = new RadioController(session);
    expect(await controller.startScan()).toEqual({ kind: "unavailable" });
    expect(await controller.connect("94:DB:56:54:7C:20")).toEqual({ kind: "unavailable" });
    await session.stop();
  });
});
