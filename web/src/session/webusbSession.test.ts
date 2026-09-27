// Review fix-first (pico-link-jyhk.11): the reviewer proved the "unplug
// never reaches the Session" bug with a fake `navigator.usb` EventTarget
// dispatching a real `disconnect` event -- this is that regression test,
// plus coverage for the reconnect-leaks-a-stale-session half of the fix.
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { WebUsbSessionManager } from "./webusbSession";
import { Session } from "./session";
import { encodeDeviceInfoForTest } from "../proto/info";
import type { DeviceInfoInput } from "../proto/info";
import { encodeHomeSnapshotForTest, emptyHomeSnapshot, TELEMETRY_PAGE_HOME } from "../proto/telemetry";
import { PL_CFG_REQ_GET_INFO, PL_CFG_REQ_GET_TELEMETRY } from "../transport/types";
import { PL_USB_PRODUCT_ID, PL_USB_VENDOR_ID } from "../transport/webusb";

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

const DEFAULT_INFO: DeviceInfoInput = {
  infoVer: 1,
  importProto: 1,
  statusVer: 1,
  telemetryProto: 1,
  telemetryPageMask: 1 << TELEMETRY_PAGE_HOME,
  version: "dev",
};

/**
 * A fake `USBDevice`: answers GET_INFO/GET_TELEMETRY like `FakeTransport`
 * does, but through the real `WebUsbTransport`/`webusb-types.d.ts` shape so
 * this test exercises the actual `navigator.usb` wiring, not a shortcut
 * around it. Each fake device is a distinct object identity, matching how
 * `watchUsbConnectionEvents`/`WebUsbTransport.underlyingDevice` compare
 * real `USBDevice`s.
 */
function fakeUsbDevice(options: { openDelayMs?: number } = {}): USBDevice {
  let opened = false;
  const snapshot = () => encodeHomeSnapshotForTest({ ...emptyHomeSnapshot(), snapSeq: 1, linkConnected: true });
  return {
    vendorId: PL_USB_VENDOR_ID,
    productId: PL_USB_PRODUCT_ID,
    get configuration() {
      return opened ? { configurationValue: 1 } : null;
    },
    get opened() {
      return opened;
    },
    async open() {
      // Optional delay to occupy `openChain` for the length of one open --
      // used to prove a queued opener does not delay an unrelated
      // synchronous `navigator.usb.requestDevice()` call.
      if (options.openDelayMs) await sleep(options.openDelayMs);
      opened = true;
    },
    async close() {
      opened = false;
    },
    async selectConfiguration() {
      /* no-op */
    },
    async claimInterface() {
      /* no-op */
    },
    async releaseInterface() {
      /* no-op */
    },
    async controlTransferIn(setup: USBControlTransferParameters) {
      const bytes = setup.request === PL_CFG_REQ_GET_INFO ? encodeDeviceInfoForTest(DEFAULT_INFO) : setup.request === PL_CFG_REQ_GET_TELEMETRY ? snapshot() : new Uint8Array(0);
      const copy = new Uint8Array(bytes.length);
      copy.set(bytes);
      return { status: "ok", data: new DataView(copy.buffer) };
    },
    async controlTransferOut() {
      return { status: "ok", bytesWritten: 0 };
    },
  } as unknown as USBDevice;
}

/**
 * A minimal `navigator.usb` fake: a real `EventTarget` for connect/disconnect,
 * plus an authorized-devices list. `requestDevice` resolves whatever
 * `nextRequestedDevice` is currently set to -- tests that exercise
 * `WebUsbSessionManager.requestDevice()` set it before calling in, mirroring
 * the chooser resolving with the device the (fake) user picked.
 */
function fakeNavigatorUsb() {
  const target = new EventTarget();
  const authorized: USBDevice[] = [];
  let nextRequestedDevice: USBDevice | undefined;
  let nextRequestedRejection: unknown;
  let requestDeviceCallCount = 0;
  const usb = {
    addEventListener: (type: string, listener: EventListenerOrEventListenerObject) => target.addEventListener(type, listener),
    removeEventListener: (type: string, listener: EventListenerOrEventListenerObject) => target.removeEventListener(type, listener),
    getDevices: () => Promise.resolve([...authorized]),
    // Increments synchronously, before returning the promise -- callers
    // check this immediately after invoking `manager.requestDevice()`
    // (without awaiting) to prove the chooser call itself was not deferred
    // behind an unrelated in-flight open.
    requestDevice: () => {
      requestDeviceCallCount += 1;
      if (nextRequestedRejection !== undefined) {
        const err = nextRequestedRejection;
        nextRequestedRejection = undefined;
        return Promise.reject(err);
      }
      return nextRequestedDevice ? Promise.resolve(nextRequestedDevice) : Promise.reject(new Error("not used in this test"));
    },
  };
  return {
    usb: usb as unknown as USB,
    authorize(device: USBDevice) {
      authorized.push(device);
    },
    setNextRequestedDevice(device: USBDevice) {
      nextRequestedDevice = device;
    },
    rejectNextRequestedDeviceWith(err: unknown) {
      nextRequestedRejection = err;
    },
    get requestDeviceCallCount() {
      return requestDeviceCallCount;
    },
    dispatchConnect(device: USBDevice) {
      const event = new Event("connect") as USBConnectionEvent;
      Object.defineProperty(event, "device", { value: device });
      target.dispatchEvent(event);
    },
    dispatchDisconnect(device: USBDevice) {
      const event = new Event("disconnect") as USBConnectionEvent;
      Object.defineProperty(event, "device", { value: device });
      target.dispatchEvent(event);
    },
  };
}

/** Counts sessions whose phase is `ready` -- the invariant every concurrency test checks: exactly one live session, no matter how many opens raced. */
function readyCount(sessions: Session[]): number {
  return sessions.filter((s) => s.statusStore.getSnapshot().phase === "ready").length;
}

describe("WebUsbSessionManager", () => {
  let originalUsb: USB | undefined;

  beforeEach(() => {
    originalUsb = (navigator as Navigator & { usb?: USB }).usb;
  });

  afterEach(() => {
    Object.defineProperty(navigator, "usb", { value: originalUsb, configurable: true });
  });

  it("real navigator.usb 'disconnect' for the open device sets the session to lost and stops polling", async () => {
    const fake = fakeNavigatorUsb();
    const device = fakeUsbDevice();
    fake.authorize(device);
    Object.defineProperty(navigator, "usb", { value: fake.usb, configurable: true });

    const sessions: Session[] = [];
    const manager = new WebUsbSessionManager({ pollIntervalMs: 5, onSession: (s) => sessions.push(s) });
    await manager.start();
    await sleep(15);

    expect(sessions).toHaveLength(1);
    expect(sessions[0].statusStore.getSnapshot().phase).toBe("ready");

    fake.dispatchDisconnect(device);
    await sleep(5);

    expect(sessions[0].statusStore.getSnapshot().phase).toBe("lost");

    // The loop must actually be stopped, not just the status flipped:
    // give it plenty of time and confirm no further polling resurrects it.
    await sleep(30);
    expect(sessions[0].statusStore.getSnapshot().phase).toBe("lost");
  });

  it("a disconnect for an unrelated device does not affect the current session", async () => {
    const fake = fakeNavigatorUsb();
    const device = fakeUsbDevice();
    const unrelated = fakeUsbDevice();
    fake.authorize(device);
    Object.defineProperty(navigator, "usb", { value: fake.usb, configurable: true });

    const sessions: Session[] = [];
    const manager = new WebUsbSessionManager({ pollIntervalMs: 5, onSession: (s) => sessions.push(s) });
    await manager.start();
    await sleep(15);

    fake.dispatchDisconnect(unrelated);
    await sleep(5);

    expect(sessions).toHaveLength(1);
    expect(sessions[0].statusStore.getSnapshot().phase).toBe("ready");
  });

  it("replugging without reload (connect event) stops the stale session and yields exactly one live session", async () => {
    const fake = fakeNavigatorUsb();
    const device = fakeUsbDevice();
    fake.authorize(device);
    Object.defineProperty(navigator, "usb", { value: fake.usb, configurable: true });

    const sessions: Session[] = [];
    const manager = new WebUsbSessionManager({ pollIntervalMs: 5, onSession: (s) => sessions.push(s) });
    await manager.start();
    await sleep(15);
    expect(sessions).toHaveLength(1);
    const firstSession = sessions[0];
    expect(firstSession.statusStore.getSnapshot().phase).toBe("ready");

    fake.dispatchDisconnect(device);
    await sleep(5);
    expect(firstSession.statusStore.getSnapshot().phase).toBe("lost");

    // Replug: browser fires `connect` for a (possibly new) USBDevice object
    // naming our VID/PID, with no chooser.
    const replugged = fakeUsbDevice();
    fake.dispatchConnect(replugged);
    await sleep(15);

    expect(sessions).toHaveLength(2);
    const secondSession = sessions[1];
    expect(secondSession.statusStore.getSnapshot().phase).toBe("ready");

    // The stale session must be fully stopped: it should not still be
    // running (which would leave two loops driving the meter/status).
    expect(firstSession.statusStore.getSnapshot().phase).toBe("idle");
    expect(manager.session).toBe(secondSession);
  });

  // Review fix-first (pico-link-jyhk.11), second round: `openDevice`/
  // `requestDevice` had no mutual exclusion -- each begins with
  // `await this.stopCurrentSession()`, which always yields, so two calls
  // racing each read the same "previous" session and both go on to assign
  // `currentSession`/`currentTransport`, leaking the loser (open device,
  // live poll loop, disconnect listener). These three cases fire the races
  // back-to-back with NO sleep in between, so a passing run means the
  // serialization actually closed the gap rather than the fixture giving it
  // time to resolve on its own.

  it("two connect events for different devices, fired back-to-back, yield exactly one live session", async () => {
    const fake = fakeNavigatorUsb();
    const device = fakeUsbDevice();
    fake.authorize(device);
    Object.defineProperty(navigator, "usb", { value: fake.usb, configurable: true });

    const sessions: Session[] = [];
    const manager = new WebUsbSessionManager({ pollIntervalMs: 5, onSession: (s) => sessions.push(s) });
    await manager.start();
    await sleep(15);
    expect(sessions).toHaveLength(1);

    // Two more `connect` events, no await/sleep between them: both
    // `openDevice()` calls start before either has stopped anything.
    const second = fakeUsbDevice();
    const third = fakeUsbDevice();
    fake.dispatchConnect(second);
    fake.dispatchConnect(third);
    await sleep(20);

    expect(sessions).toHaveLength(3);
    expect(readyCount(sessions)).toBe(1);
    expect(sessions[2].statusStore.getSnapshot().phase).toBe("ready");
    expect(sessions[0].statusStore.getSnapshot().phase).toBe("idle");
    expect(sessions[1].statusStore.getSnapshot().phase).toBe("idle");
    expect(manager.session).toBe(sessions[2]);
  });

  it("a connect event racing start()'s own initial open yields exactly one live session", async () => {
    const fake = fakeNavigatorUsb();
    const device = fakeUsbDevice();
    const racer = fakeUsbDevice();
    fake.authorize(device);
    Object.defineProperty(navigator, "usb", { value: fake.usb, configurable: true });

    const sessions: Session[] = [];
    const manager = new WebUsbSessionManager({ pollIntervalMs: 5, onSession: (s) => sessions.push(s) });

    // `start()` awaits `listAuthorizedDevices()` before opening `device` --
    // fire a `connect` for a second device synchronously, before that await
    // (and therefore `start()`'s own `openDevice`) has resolved.
    const startPromise = manager.start();
    fake.dispatchConnect(racer);
    await startPromise;
    await sleep(20);

    expect(sessions).toHaveLength(2);
    expect(readyCount(sessions)).toBe(1);
    const readySession = sessions.find((s) => s.statusStore.getSnapshot().phase === "ready");
    expect(readySession).toBeDefined();
    expect(manager.session).toBe(readySession);
  });

  it("requestDevice racing a connect event yields exactly one live session", async () => {
    const fake = fakeNavigatorUsb();
    const chosen = fakeUsbDevice();
    const racer = fakeUsbDevice();
    fake.setNextRequestedDevice(chosen);
    Object.defineProperty(navigator, "usb", { value: fake.usb, configurable: true });

    const sessions: Session[] = [];
    const manager = new WebUsbSessionManager({ pollIntervalMs: 5, onSession: (s) => sessions.push(s) });
    await manager.start();

    // No await between the user-gesture chooser call and the browser's own
    // `connect` event (e.g. the device the user is about to pick re-enumerating).
    const requestPromise = manager.requestDevice();
    fake.dispatchConnect(racer);
    await requestPromise;
    await sleep(20);

    expect(sessions).toHaveLength(2);
    expect(readyCount(sessions)).toBe(1);
    const readySession = sessions.find((s) => s.statusStore.getSnapshot().phase === "ready");
    expect(readySession).toBeDefined();
    expect(manager.session).toBe(readySession);
  });

  // Review fix-first (pico-link-jyhk.11), third round: `requestDevice()` used
  // to enqueue `() => new WebUsbTransport()` -- a transport whose `open()`
  // calls `navigator.usb.requestDevice()` itself, inside `openChain`, so the
  // chooser call happened only after any in-flight open's
  // `stopCurrentSession()` + `session.start()` settled. That can outlive
  // Chrome's transient user activation and throws `SecurityError`. The fix
  // calls `navigator.usb.requestDevice()` synchronously first and enqueues
  // only the open of the already-chosen device.

  it("requestDevice calls navigator.usb.requestDevice synchronously, not deferred behind an in-flight open", async () => {
    const fake = fakeNavigatorUsb();
    const device = fakeUsbDevice();
    fake.authorize(device);
    Object.defineProperty(navigator, "usb", { value: fake.usb, configurable: true });

    const sessions: Session[] = [];
    const manager = new WebUsbSessionManager({ pollIntervalMs: 5, onSession: (s) => sessions.push(s) });
    await manager.start();
    await sleep(15);
    expect(sessions).toHaveLength(1);

    // Occupy `openChain` with a slow open (a `connect` event for a device
    // that takes 50ms to open) before the user clicks Connect.
    const slow = fakeUsbDevice({ openDelayMs: 50 });
    fake.dispatchConnect(slow);

    const chosen = fakeUsbDevice();
    fake.setNextRequestedDevice(chosen);

    expect(fake.requestDeviceCallCount).toBe(0);
    const requestPromise = manager.requestDevice();
    // Checked immediately, with NO await in between: if this were still
    // queued behind the slow open, the call would not have happened yet.
    expect(fake.requestDeviceCallCount).toBe(1);

    await requestPromise;
    await sleep(70);

    expect(readyCount(sessions)).toBe(1);
    expect(manager.session?.statusStore.getSnapshot().phase).toBe("ready");
  });

  it("a cancelled chooser (NotFoundError) leaves the current session untouched and does not enter the chain", async () => {
    const fake = fakeNavigatorUsb();
    const device = fakeUsbDevice();
    fake.authorize(device);
    Object.defineProperty(navigator, "usb", { value: fake.usb, configurable: true });

    const sessions: Session[] = [];
    const manager = new WebUsbSessionManager({ pollIntervalMs: 5, onSession: (s) => sessions.push(s) });
    await manager.start();
    await sleep(15);
    expect(sessions).toHaveLength(1);
    const firstSession = sessions[0];
    expect(firstSession.statusStore.getSnapshot().phase).toBe("ready");

    fake.rejectNextRequestedDeviceWith(new DOMException("The user cancelled the requestDevice() chooser.", "NotFoundError"));
    await expect(manager.requestDevice()).rejects.toMatchObject({ name: "NotFoundError" });

    // No new session was ever created, and the current one is unaffected.
    expect(sessions).toHaveLength(1);
    expect(manager.session).toBe(firstSession);
    expect(firstSession.statusStore.getSnapshot().phase).toBe("ready");

    // The chain must not be wedged by the cancellation: a later open still works.
    const replug = fakeUsbDevice();
    fake.dispatchConnect(replug);
    await sleep(15);

    expect(sessions).toHaveLength(2);
    expect(readyCount(sessions)).toBe(1);
    expect(manager.session).toBe(sessions[1]);
  });
});
