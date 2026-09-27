import { describe, expect, it, vi } from "vitest";
import { Session } from "./session";
import { FakeTransport } from "../transport/fake";
import { TransportError } from "../transport/types";
import type { Transport, Unsubscribe } from "../transport/types";
import { WebUsbBusyError } from "../transport/webusb";
import { emptyHomeSnapshot } from "../proto/telemetry";
import type { HomeSnapshot } from "../proto/telemetry";
import { PL_CFG_REQ_GET_TELEMETRY } from "../transport/types";

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function fakeDocument(initialHidden = false) {
  const listeners = new Set<() => void>();
  const doc = {
    hidden: initialHidden,
    addEventListener: (_type: string, cb: () => void) => listeners.add(cb),
    removeEventListener: (_type: string, cb: () => void) => listeners.delete(cb),
    setHidden(v: boolean) {
      doc.hidden = v;
      for (const cb of listeners) cb();
    },
  };
  return doc as unknown as Document & { setHidden: (v: boolean) => void };
}

/** Wraps a transport, tracking call order and max concurrency for assertions. */
function trackingTransport(inner: Transport) {
  const calls: string[] = [];
  let inFlight = 0;
  let maxInFlight = 0;
  return {
    calls,
    get maxInFlight() {
      return maxInFlight;
    },
    transport: {
      async open() {
        return inner.open();
      },
      async controlIn(bReq: number, wValue: number, len: number) {
        inFlight += 1;
        maxInFlight = Math.max(maxInFlight, inFlight);
        calls.push(bReq === PL_CFG_REQ_GET_TELEMETRY ? "poll" : "info");
        try {
          return await inner.controlIn(bReq, wValue, len);
        } finally {
          inFlight -= 1;
        }
      },
      async controlOut(bReq: number, wValue: number, bytes: Uint8Array) {
        inFlight += 1;
        maxInFlight = Math.max(maxInFlight, inFlight);
        calls.push("out");
        try {
          return await inner.controlOut(bReq, wValue, bytes);
        } finally {
          inFlight -= 1;
        }
      },
      async close() {
        return inner.close();
      },
      onDisconnect(cb: () => void): Unsubscribe {
        return inner.onDisconnect(cb);
      },
    } satisfies Transport,
  };
}

function goldenSnapshot(overrides: Partial<HomeSnapshot> = {}): HomeSnapshot {
  return { ...emptyHomeSnapshot(), snapSeq: 1, linkConnected: true, levelPresent: true, ...overrides };
}

describe("Session", () => {
  it("handshakes then polls, populating snapshotRef", async () => {
    let seq = 0;
    const transport = new FakeTransport({ snapshot: () => goldenSnapshot({ snapSeq: ++seq, receivedMs: seq * 10 }) });
    const session = new Session(transport, { pollIntervalMs: 5, visibilityDocument: undefined });

    await session.start();
    expect(session.statusStore.getSnapshot().phase).toBe("ready");

    await sleep(40);
    expect(session.snapshotRef.current).not.toBeNull();
    expect(session.snapshotRef.current!.snapSeq).toBeGreaterThan(0);

    await session.stop();
    expect(session.statusStore.getSnapshot().phase).toBe("idle");
  });

  it("goes incompatible on an unknown telemetry proto and never polls", async () => {
    const transport = new FakeTransport({ info: { infoVer: 1, importProto: 1, statusVer: 1, telemetryProto: 99, telemetryPageMask: 1, version: "dev" } });
    const { transport: tracked, calls } = trackingTransport(transport);
    const session = new Session(tracked, { pollIntervalMs: 5 });

    await session.start();
    expect(session.statusStore.getSnapshot().phase).toBe("incompatible");

    await sleep(30);
    expect(calls.filter((c) => c === "poll")).toHaveLength(0);
  });

  it("reports busy-elsewhere when the transport throws WebUsbBusyError on open", async () => {
    const transport: Transport = {
      async open() {
        throw new WebUsbBusyError("claimed elsewhere");
      },
      async controlIn() {
        throw new TransportError("unreachable");
      },
      async controlOut() {},
      async close() {},
      onDisconnect: () => () => {},
    };
    const session = new Session(transport, { pollIntervalMs: 5 });
    await session.start();
    expect(session.statusStore.getSnapshot().phase).toBe("busy-elsewhere");
  });

  it("never has more than one transfer in flight, even with latency", async () => {
    const transport = new FakeTransport({ latencyMs: 8, snapshot: () => goldenSnapshot() });
    const tracked = trackingTransport(transport);
    const session = new Session(tracked.transport, { pollIntervalMs: 5 });

    await session.start();
    await sleep(60);
    await session.stop();

    expect(tracked.maxInFlight).toBe(1);
  });

  it("serves a queued command before the next poll, without stacking the poll slot", async () => {
    const transport = new FakeTransport({ snapshot: () => goldenSnapshot() });
    const { transport: tracked, calls } = trackingTransport(transport);
    const session = new Session(tracked, { pollIntervalMs: 20 });

    await session.start();
    await sleep(5); // let the handshake settle, before the first poll fires
    calls.length = 0;

    const commandDone = session.enqueueCommand(async () => {
      calls.push("cmd");
      return "ok";
    });
    await expect(commandDone).resolves.toBe("ok");

    expect(calls[0]).toBe("cmd");
    await session.stop();
  });

  it("pauses polling while the document is hidden and resumes on visibilitychange", async () => {
    const doc = fakeDocument(false);
    const transport = new FakeTransport({ snapshot: () => goldenSnapshot() });
    const { transport: tracked, calls } = trackingTransport(transport);
    const session = new Session(tracked, { pollIntervalMs: 5, hiddenPollMs: 5, visibilityDocument: doc });

    await session.start();
    await sleep(20);
    const pollsWhileVisible = calls.filter((c) => c === "poll").length;
    expect(pollsWhileVisible).toBeGreaterThan(0);

    doc.setHidden(true);
    calls.length = 0;
    await sleep(30);
    expect(calls.filter((c) => c === "poll")).toHaveLength(0);

    doc.setHidden(false);
    await sleep(20);
    expect(calls.filter((c) => c === "poll").length).toBeGreaterThan(0);

    await session.stop();
  });

  it("transitions to lost when the transport disconnects, and stops polling", async () => {
    const transport = new FakeTransport({ snapshot: () => goldenSnapshot() });
    const session = new Session(transport, { pollIntervalMs: 5 });

    await session.start();
    await sleep(10);
    transport.simulateDisconnect();

    expect(session.statusStore.getSnapshot().phase).toBe("lost");
  });

  it("transitions to lost after repeated consecutive poll failures, even with no disconnect event", async () => {
    // Review fix-first (pico-link-jyhk.11): a transport that never fires
    // `onDisconnect` but fails every poll (e.g. a stale/half-closed WebUSB
    // handle) must not spin forever -- N consecutive failures should also
    // reach `lost`.
    const inner = new FakeTransport({ snapshot: () => goldenSnapshot() });
    let pollsShouldFail = false;
    const flaky: Transport = {
      open: () => inner.open(),
      controlIn: (bReq, wValue, len) => {
        if (bReq === PL_CFG_REQ_GET_TELEMETRY && pollsShouldFail) {
          return Promise.reject(new TransportError("simulated poll failure"));
        }
        return inner.controlIn(bReq, wValue, len);
      },
      controlOut: (bReq, wValue, bytes) => inner.controlOut(bReq, wValue, bytes),
      close: () => inner.close(),
      onDisconnect: (cb) => inner.onDisconnect(cb),
    };
    const session = new Session(flaky, { pollIntervalMs: 5 });

    await session.start();
    await sleep(15);
    expect(session.statusStore.getSnapshot().phase).toBe("ready");

    pollsShouldFail = true;
    await sleep(100);

    expect(session.statusStore.getSnapshot().phase).toBe("lost");
  });

  it("detects a reboot (uptime going backwards), resets clock/ballistics, and re-handshakes", async () => {
    let uptime = 1000;
    const transport = new FakeTransport({ snapshot: () => goldenSnapshot({ uptimeMs: uptime, receivedMs: uptime, peakL: 200, peakR: 200 }) });
    const { transport: tracked, calls } = trackingTransport(transport);
    const session = new Session(tracked, { pollIntervalMs: 5 });

    await session.start();
    await sleep(20);
    expect(session.clock.offsetMs()).not.toBeNull();

    const clockResetSpy = vi.spyOn(session.clock, "reset");
    const ballisticsResetSpy = vi.spyOn(session.ballistics, "reset");
    const infoCallsBefore = calls.filter((c) => c === "info").length;
    uptime = 10; // goes backwards -> reboot
    await sleep(20);

    expect(clockResetSpy).toHaveBeenCalled();
    expect(ballisticsResetSpy).toHaveBeenCalled();
    const infoCallsAfter = calls.filter((c) => c === "info").length;
    expect(infoCallsAfter).toBeGreaterThan(infoCallsBefore);
    expect(session.statusStore.getSnapshot().phase).toBe("ready");

    await session.stop();
  });
});
