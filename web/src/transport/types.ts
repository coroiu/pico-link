// The transport seam that makes hardware optional (FERN DESIGN section 3,
// pico-link-jyhk.8). Three implementations: `FakeTransport` (a scripted
// device model, this bead), `ReplayTransport` (plays a pyusb capture, this
// bead), `WebUsbTransport` (real hardware, stub only here -- jyhk.11).
//
// `bRequest` values (CLASS, recipient interface, iface 6):
//   0x01 IMPORT_PRESET (out), 0x02 GET_STATUS (in), 0x03 GET_TELEMETRY (in,
//   wValue = page id), 0x04 GET_INFO (in). See
//   firmware/src/usb_config_itf.h:97-100.

export const PL_CFG_REQ_IMPORT_PRESET = 0x01;
export const PL_CFG_REQ_GET_STATUS = 0x02;
export const PL_CFG_REQ_GET_TELEMETRY = 0x03;
export const PL_CFG_REQ_GET_INFO = 0x04;

/** Unregisters a callback previously passed to `Transport.onDisconnect`. */
export type Unsubscribe = () => void;

export interface Transport {
  open(): Promise<void>;
  controlIn(bRequest: number, wValue: number, length: number): Promise<DataView>;
  controlOut(bRequest: number, wValue: number, bytes: Uint8Array): Promise<void>;
  close(): Promise<void>;
  /**
   * Registers `cb` to fire when the device disconnects. Returns an
   * unsubscribe function (review follow-up on pico-link-jyhk.10: "Transport.
   * onDisconnect has no unsubscribe") -- the session layer must be able to
   * detach its listener when it replaces or tears down a transport, or a
   * stale callback from a previous session fires alongside the new one.
   */
  onDisconnect(cb: () => void): Unsubscribe;
}

/** Thrown by a transport when a request has no answer configured/replayed. */
export class TransportError extends Error {}
