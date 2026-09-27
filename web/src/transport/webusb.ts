import { TransportError } from "./types";
import type { Transport, Unsubscribe } from "./types";

// Ambient WebUSB types (`USBDevice`, `navigator.usb`, ...) come from
// `./webusb-types.d.ts` -- a global declaration file with no imports/exports,
// picked up automatically by tsconfig's `"include": ["src"]`.

/**
 * VID/PID for the chooser filter (FERN DESIGN section 3). VID is pico-sdk's
 * shared "Raspberry Pi" allocation, PID identifies Pico Link specifically --
 * see `firmware/src/usb_descriptors.c:54-58`.
 */
export const PL_USB_VENDOR_ID = 0x2e8a;
export const PL_USB_PRODUCT_ID = 0x000c;

/**
 * `ITF_NUM_CONFIG` (`firmware/src/usb_descriptors.h:77`) -- the Pico Link
 * Config vendor interface. CLASS request type, not VENDOR: pico-sdk 2.1.1's
 * TinyUSB `usbd.c:732` routes `bmRequestType`-VENDOR control transfers to a
 * different (audio) handler on this device, so a real request MUST use
 * `requestType: "class"` or it silently lands on the wrong endpoint.
 */
export const PL_CONFIG_INTERFACE_NUMBER = 6;

/** True when the browser exposes `navigator.usb` (Chrome/Edge, secure context). */
export function isWebUsbSupported(): boolean {
  return typeof navigator !== "undefined" && "usb" in navigator;
}

/**
 * Devices the user has already granted this origin permission to talk to
 * (`navigator.usb.getDevices()`). No chooser prompt -- this is what makes
 * page-load auto-reconnect possible without a user gesture (design section
 * 4: "on load `navigator.usb.getDevices()` reopens a previously permitted
 * device with no chooser").
 */
export function listAuthorizedDevices(): Promise<USBDevice[]> {
  if (!isWebUsbSupported()) return Promise.resolve([]);
  return navigator.usb.getDevices();
}

/**
 * Subscribes to the browser-level `navigator.usb` `connect`/`disconnect`
 * events (distinct from `Transport.onDisconnect`, which fires only for the
 * device a specific `WebUsbTransport` instance has open). The session layer
 * uses `connect` to auto-reopen a device that came back after unplug or a
 * BOOTSEL reboot, per design section 4. Returns an unsubscribe.
 */
export function watchUsbConnectionEvents(onConnect: (device: USBDevice) => void, onDisconnect: (device: USBDevice) => void): Unsubscribe {
  if (!isWebUsbSupported()) return () => {};
  const handleConnect = (event: USBConnectionEvent) => onConnect(event.device);
  const handleDisconnect = (event: USBConnectionEvent) => onDisconnect(event.device);
  navigator.usb.addEventListener("connect", handleConnect);
  navigator.usb.addEventListener("disconnect", handleDisconnect);
  return () => {
    navigator.usb.removeEventListener("connect", handleConnect);
    navigator.usb.removeEventListener("disconnect", handleDisconnect);
  };
}

/** Thrown when claiming iface 6 fails -- another tab or a pyusb tool holds it (design: "busy-elsewhere"). */
export class WebUsbBusyError extends TransportError {}

/**
 * Real-hardware transport: WebUSB, CLASS request, recipient interface,
 * index `PL_CONFIG_INTERFACE_NUMBER` (FERN DESIGN section 3).
 *
 * Two ways to open: `open()` calls `navigator.usb.requestDevice` (shows the
 * chooser -- must be called synchronously from a user gesture, e.g. a click
 * handler with no `await` before this call) for the explicit-connect path,
 * or construct with an already-permitted `USBDevice` (from
 * `listAuthorizedDevices()`/`watchUsbConnectionEvents`'s `connect` event)
 * and call `open()` to reopen it with no chooser.
 *
 * Never calls `device.reset()` (design section 4: "NEVER device.reset()
 * from the page (drops audio)").
 */
export class WebUsbTransport implements Transport {
  private device: USBDevice | null;
  private disconnectCbs: Array<() => void> = [];
  private readonly presetDevice: USBDevice | null;

  constructor(presetDevice: USBDevice | null = null) {
    this.device = null;
    this.presetDevice = presetDevice;
  }

  async open(): Promise<void> {
    if (!isWebUsbSupported()) {
      throw new TransportError("WebUSB is not supported in this browser");
    }

    const device = this.presetDevice ?? (await navigator.usb.requestDevice({ filters: [{ vendorId: PL_USB_VENDOR_ID, productId: PL_USB_PRODUCT_ID }] }));

    await device.open();
    if (device.configuration === null) {
      await device.selectConfiguration(1);
    }

    try {
      await device.claimInterface(PL_CONFIG_INTERFACE_NUMBER);
    } catch (err) {
      // Leave the device open but unclaimed; close() below still releases it.
      await device.close().catch(() => {});
      throw new WebUsbBusyError(`could not claim interface ${PL_CONFIG_INTERFACE_NUMBER}: ${String(err)}`);
    }

    this.device = device;
  }

  async controlIn(bRequest: number, wValue: number, length: number): Promise<DataView> {
    const device = this.requireDevice();
    const result = await device.controlTransferIn(
      { requestType: "class", recipient: "interface", request: bRequest, value: wValue, index: PL_CONFIG_INTERFACE_NUMBER },
      length,
    );
    if (result.status !== "ok" || !result.data) {
      throw new TransportError(`controlIn 0x${bRequest.toString(16)} wValue ${wValue} failed: status=${result.status}`);
    }
    return result.data;
  }

  async controlOut(bRequest: number, wValue: number, bytes: Uint8Array): Promise<void> {
    const device = this.requireDevice();
    // `Uint8Array<ArrayBufferLike>` isn't assignable to the DOM `BufferSource`
    // union (which pins `ArrayBuffer`, excluding `SharedArrayBuffer`) --
    // copy into a plain `ArrayBuffer`-backed view, cheap at this transfer size.
    const copy = new Uint8Array(bytes.length);
    copy.set(bytes);
    const result = await device.controlTransferOut(
      { requestType: "class", recipient: "interface", request: bRequest, value: wValue, index: PL_CONFIG_INTERFACE_NUMBER },
      copy,
    );
    if (result.status !== "ok") {
      throw new TransportError(`controlOut 0x${bRequest.toString(16)} wValue ${wValue} failed: status=${result.status}`);
    }
  }

  async close(): Promise<void> {
    const device = this.device;
    this.device = null;
    if (!device) return;
    try {
      await device.releaseInterface(PL_CONFIG_INTERFACE_NUMBER);
    } catch {
      // Already released (e.g. the device disconnected first) -- fine.
    }
    try {
      await device.close();
    } catch {
      // Already closed/disconnected -- fine.
    }
  }

  /** The underlying `USBDevice`, once open -- for identity comparisons against `connect`/`disconnect` events. */
  get underlyingDevice(): USBDevice | null {
    return this.device;
  }

  onDisconnect(cb: () => void): Unsubscribe {
    this.disconnectCbs.push(cb);
    return () => {
      this.disconnectCbs = this.disconnectCbs.filter((registered) => registered !== cb);
    };
  }

  /**
   * Called by the session layer when the browser's global
   * `navigator.usb` `disconnect` event names this transport's device. Not
   * wired internally (a `WebUsbTransport` doesn't self-subscribe to the
   * global event -- the session owns that, since it also needs the *other*
   * direction, `connect`, to auto-reopen).
   */
  notifyDisconnected(): void {
    this.device = null;
    for (const cb of this.disconnectCbs) cb();
  }

  private requireDevice(): USBDevice {
    if (!this.device) {
      throw new TransportError("WebUsbTransport: not open");
    }
    return this.device;
  }
}
