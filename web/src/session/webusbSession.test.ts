// Review fix-first (pico-link-jyhk.11): the reviewer proved the "unplug
// never reaches the Session" bug with a fake `navigator.usb` EventTarget
// dispatching a real `disconnect` event -- this is that regression test,
// plus coverage for the reconnect-leaks-a-stale-session half of the fix.
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { WebUsbSessionManager } from "./webusbSession";
import { Session } from "./session";
import { encodeDeviceInfoForTest } from "../proto/info";
import type { DeviceInfo } from "../proto/info";
import { encodeHomeSnapshotForTest, emptyHomeSnapshot, TELEMETRY_PAGE_HOME } from "../proto/telemetry";
import { PL_CFG_REQ_GET_INFO, PL_CFG_REQ_GET_TELEMETRY } from "../transport/types";
import { PL_USB_PRODUCT_ID, PL_USB_VENDOR_ID } from "../transport/webusb";

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

const DEFAULT_INFO: DeviceInfo = {
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
function fakeUsbDevice(): USBDevice {
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

/** A minimal `navigator.usb` fake: a real `EventTarget` for connect/disconnect, plus an authorized-devices list. */
function fakeNavigatorUsb() {
  const target = new EventTarget();
  const authorized: USBDevice[] = [];
  const usb = {
    addEventListener: (type: string, listener: EventListenerOrEventListenerObject) => target.addEventListener(type, listener),
    removeEventListener: (type: string, listener: EventListenerOrEventListenerObject) => target.removeEventListener(type, listener),
    getDevices: () => Promise.resolve([...authorized]),
    requestDevice: () => Promise.reject(new Error("not used in this test")),
  };
  return {
    usb: usb as unknown as USB,
    authorize(device: USBDevice) {
      authorized.push(device);
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
});
