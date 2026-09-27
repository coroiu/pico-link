import { describe, expect, it } from "vitest";
import { FakeTransport } from "./fake";
import { decodeDeviceInfo } from "../proto/info";
import { decodeHomeSnapshot, emptyHomeSnapshot, TELEMETRY_PAGE_HOME } from "../proto/telemetry";
import { PL_CFG_REQ_GET_INFO, PL_CFG_REQ_GET_TELEMETRY, TransportError } from "./types";

function toBytes(view: DataView): Uint8Array {
  return new Uint8Array(view.buffer, view.byteOffset, view.byteLength);
}

describe("FakeTransport", () => {
  it("answers GET_INFO with a decodable reply once opened", async () => {
    const transport = new FakeTransport();
    await transport.open();
    const view = await transport.controlIn(PL_CFG_REQ_GET_INFO, 0, 64);
    const info = decodeDeviceInfo(toBytes(view));
    expect(info).not.toBeNull();
    expect(info!.telemetryProto).toBe(1);
  });

  it("answers GET_TELEMETRY page 0 with the scripted snapshot", async () => {
    const snapshot = { ...emptyHomeSnapshot(), snapSeq: 7, linkConnected: true };
    const transport = new FakeTransport({ snapshot: () => snapshot });
    await transport.open();
    const view = await transport.controlIn(PL_CFG_REQ_GET_TELEMETRY, TELEMETRY_PAGE_HOME, 256);
    expect(decodeHomeSnapshot(toBytes(view))).toEqual(snapshot);
  });

  it("rejects any request before open()", async () => {
    const transport = new FakeTransport();
    await expect(transport.controlIn(PL_CFG_REQ_GET_INFO, 0, 64)).rejects.toBeInstanceOf(TransportError);
  });

  it("simulates a stall on every request when configured", async () => {
    const transport = new FakeTransport({ stalled: true });
    await transport.open();
    await expect(transport.controlIn(PL_CFG_REQ_GET_INFO, 0, 64)).rejects.toBeInstanceOf(TransportError);
    transport.setStalled(false);
    await expect(transport.controlIn(PL_CFG_REQ_GET_INFO, 0, 64)).resolves.toBeDefined();
  });

  it("fires registered disconnect callbacks on simulateDisconnect", async () => {
    const transport = new FakeTransport();
    await transport.open();
    let fired = false;
    transport.onDisconnect(() => {
      fired = true;
    });
    transport.simulateDisconnect();
    expect(fired).toBe(true);
    await expect(transport.controlIn(PL_CFG_REQ_GET_INFO, 0, 64)).rejects.toBeInstanceOf(TransportError);
  });

  it("throws for an unscripted bRequest", async () => {
    const transport = new FakeTransport();
    await transport.open();
    await expect(transport.controlIn(0x99, 0, 64)).rejects.toBeInstanceOf(TransportError);
  });
});
