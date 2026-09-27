// Wires the browser-level WebUSB connect/disconnect lifecycle (design
// section 4: "on load navigator.usb.getDevices() reopens a previously
// permitted device with no chooser; the navigator.usb connect event
// auto-reopens after unplug or a BOOTSEL reboot") to a fresh `Session` +
// `WebUsbTransport` pair. Kept separate from `session.ts` so `Session`
// itself stays transport-agnostic and unit-testable without touching
// `navigator.usb`.
import { Session } from "./session";
import type { SessionOptions } from "./session";
import { isWebUsbSupported, listAuthorizedDevices, PL_USB_PRODUCT_ID, PL_USB_VENDOR_ID, watchUsbConnectionEvents, WebUsbTransport } from "../transport/webusb";
import type { Unsubscribe } from "../transport/types";

export interface WebUsbSessionManagerOptions extends SessionOptions {
  onSession: (session: Session) => void;
}

/**
 * Manages the WebUSB device lifecycle for one page: auto-reopens a
 * previously authorized device on construction, and whenever the browser's
 * `connect` event names our device again (post-unplug or post-BOOTSEL-
 * reboot), starts a fresh `Session` and hands it to `onSession`. Never
 * calls `device.reset()`.
 */
export class WebUsbSessionManager {
  private readonly options: WebUsbSessionManagerOptions;
  private unwatch: Unsubscribe | undefined;
  private currentSession: Session | undefined;

  constructor(options: WebUsbSessionManagerOptions) {
    this.options = options;
  }

  /** Starts watching for `connect` events and attempts to reopen any already-authorized device. Call once. */
  async start(): Promise<void> {
    if (!isWebUsbSupported()) return;

    this.unwatch = watchUsbConnectionEvents(
      (device) => {
        if (device.vendorId === PL_USB_VENDOR_ID && device.productId === PL_USB_PRODUCT_ID) {
          void this.openDevice(device);
        }
      },
      () => {
        // The `Transport`'s own `onDisconnect` (wired inside `Session`)
        // handles the session-level "lost" transition; nothing to do here.
      },
    );

    const authorized = await listAuthorizedDevices();
    const ours = authorized.find((d) => d.vendorId === PL_USB_VENDOR_ID && d.productId === PL_USB_PRODUCT_ID);
    if (ours) {
      await this.openDevice(ours);
    }
  }

  /** Shows the chooser (must be called synchronously from a user gesture) and opens whatever the user picks. */
  async requestDevice(): Promise<void> {
    const transport = new WebUsbTransport();
    const session = new Session(transport, this.options);
    this.currentSession = session;
    this.options.onSession(session);
    await session.start();
  }

  stop(): void {
    this.unwatch?.();
    this.unwatch = undefined;
  }

  private async openDevice(device: USBDevice): Promise<void> {
    const transport = new WebUsbTransport(device);
    const session = new Session(transport, this.options);
    this.currentSession = session;
    this.options.onSession(session);
    await session.start();
  }

  /** Test/debug hook. */
  get session(): Session | undefined {
    return this.currentSession;
  }
}
