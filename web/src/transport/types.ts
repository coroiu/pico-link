// The transport seam that makes hardware optional (FERN DESIGN section 3,
// pico-link-jyhk.8). Three implementations: `FakeTransport` (a scripted
// device model, this bead), `ReplayTransport` (plays a pyusb capture, this
// bead), `WebUsbTransport` (real hardware, stub only here -- jyhk.11).
//
// `bRequest` values (CLASS, recipient interface, iface 6):
//   0x01 IMPORT_PRESET (out), 0x02 GET_STATUS (in), 0x03 GET_TELEMETRY (in,
//   wValue = page id), 0x04 GET_INFO (in). See
//   firmware/src/usb_config_itf.h:97-100.
//
// 0x05 GET_LIBRARY (in), 0x06 HOST_OP (out), 0x07 GET_OP_STATUS (in) are
// NEW per ADA DESIGN on pico-link-jyhk.17 (`.planning/design/
// 2026-09-27-iface6-eq-management-protocol.md` section 2) -- firmware does
// not implement these yet (that's pico-link-jyhk.17's tasks 3-4); callers
// must gate on `DeviceInfo.opMask`/`libProto`/`opProto` (design section 8)
// before ever issuing them against real hardware. `FakeTransport` emulates
// them for tests and the dev page.
//
// 0x08 GET_RADIO (in) is section 13.4's device-management radio snapshot
// (bead pico-link-jyhk.24/.27/.30) -- same "gate on op_mask/radio_proto,
// FakeTransport emulates it" caveat as the ops above.

export const PL_CFG_REQ_IMPORT_PRESET = 0x01;
export const PL_CFG_REQ_GET_STATUS = 0x02;
export const PL_CFG_REQ_GET_TELEMETRY = 0x03;
export const PL_CFG_REQ_GET_INFO = 0x04;
export const PL_CFG_REQ_GET_LIBRARY = 0x05;
export const PL_CFG_REQ_HOST_OP = 0x06;
export const PL_CFG_REQ_GET_OP_STATUS = 0x07;
export const PL_CFG_REQ_GET_RADIO = 0x08;

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
