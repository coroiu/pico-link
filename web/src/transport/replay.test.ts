import { describe, expect, it } from "vitest";
import { ReplayTransport } from "./replay";
import { TransportError } from "./types";

describe("ReplayTransport", () => {
  it("plays back entries in order and decodes their hex payloads", async () => {
    const transport = new ReplayTransport([
      { t_ms: 0, hex: "0102" },
      { t_ms: 33, hex: "0304" },
    ]);
    await transport.open();
    const first = await transport.controlIn(0, 0, 8);
    expect(Array.from(new Uint8Array(first.buffer, first.byteOffset, first.byteLength))).toEqual([0x01, 0x02]);
    const second = await transport.controlIn(0, 0, 8);
    expect(Array.from(new Uint8Array(second.buffer, second.byteOffset, second.byteLength))).toEqual([0x03, 0x04]);
  });

  it("throws once the capture is exhausted", async () => {
    const transport = new ReplayTransport([{ t_ms: 0, hex: "01" }]);
    await transport.open();
    await transport.controlIn(0, 0, 8);
    await expect(transport.controlIn(0, 0, 8)).rejects.toBeInstanceOf(TransportError);
  });

  it("caps returned bytes at the requested length", async () => {
    const transport = new ReplayTransport([{ t_ms: 0, hex: "0102030405" }]);
    await transport.open();
    const view = await transport.controlIn(0, 0, 3);
    expect(view.byteLength).toBe(3);
  });

  it("fires registered disconnect callbacks", async () => {
    const transport = new ReplayTransport([{ t_ms: 0, hex: "01" }]);
    let fired = false;
    transport.onDisconnect(() => {
      fired = true;
    });
    transport.simulateDisconnect();
    expect(fired).toBe(true);
  });
});
