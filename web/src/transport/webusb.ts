import type { Transport } from "./types";

/**
 * Real-hardware transport: WebUSB, CLASS request, recipient interface,
 * index 6 (FERN DESIGN section 3). STUB ONLY -- the session layer (device
 * chooser/reopen, claim-failure handling, single-flight queue, pacing,
 * reconnect) is `pico-link-jyhk.11`. This file exists so the `Transport`
 * union is complete and importable; every method throws.
 */
export class WebUsbTransport implements Transport {
  async open(): Promise<void> {
    throw new Error("WebUsbTransport is not implemented yet (pico-link-jyhk.11)");
  }

  async controlIn(): Promise<DataView> {
    throw new Error("WebUsbTransport is not implemented yet (pico-link-jyhk.11)");
  }

  async controlOut(): Promise<void> {
    throw new Error("WebUsbTransport is not implemented yet (pico-link-jyhk.11)");
  }

  async close(): Promise<void> {
    throw new Error("WebUsbTransport is not implemented yet (pico-link-jyhk.11)");
  }

  onDisconnect(): void {
    throw new Error("WebUsbTransport is not implemented yet (pico-link-jyhk.11)");
  }
}

/** True when the browser exposes `navigator.usb` (Chrome/Edge, secure context). */
export function isWebUsbSupported(): boolean {
  return typeof navigator !== "undefined" && "usb" in navigator;
}
