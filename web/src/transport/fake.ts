import { encodeHomeSnapshotForTest, emptyHomeSnapshot, TELEMETRY_PAGE_HOME } from "../proto/telemetry";
import type { HomeSnapshot } from "../proto/telemetry";
import { encodeDeviceInfoForTest } from "../proto/info";
import type { DeviceInfo } from "../proto/info";
import { PL_CFG_REQ_GET_INFO, PL_CFG_REQ_GET_TELEMETRY, TransportError } from "./types";
import type { Transport } from "./types";

const DEFAULT_INFO: DeviceInfo = {
  infoVer: 1,
  importProto: 1,
  statusVer: 1,
  telemetryProto: 1,
  telemetryPageMask: 1 << TELEMETRY_PAGE_HOME,
  version: "dev",
};

export interface FakeTransportOptions {
  /** Called each time GET_TELEMETRY page 0 is polled; return the live snapshot. */
  snapshot?: () => HomeSnapshot;
  info?: DeviceInfo;
  /** Injected reply latency in ms (design section 3: "injectable latency"). */
  latencyMs?: number;
  /** If true, every controlIn/controlOut rejects with a stall (design: "stalls"). */
  stalled?: boolean;
}

/**
 * A scripted device model answering 0x02/0x03/0x04, per FERN DESIGN
 * section 3. Backs the dev page's default run mode -- no hardware, no
 * capture file, just a live-generated snapshot (or a fixed one).
 */
export class FakeTransport implements Transport {
  private disconnectCbs: Array<() => void> = [];
  private opened = false;

  private options: FakeTransportOptions;

  constructor(options: FakeTransportOptions = {}) {
    this.options = options;
  }

  async open(): Promise<void> {
    await this.delay();
    this.opened = true;
  }

  async close(): Promise<void> {
    this.opened = false;
  }

  onDisconnect(cb: () => void): void {
    this.disconnectCbs.push(cb);
  }

  /** Test/dev-page hook: simulates the device unplugging. */
  simulateDisconnect(): void {
    this.opened = false;
    for (const cb of this.disconnectCbs) cb();
  }

  setStalled(stalled: boolean): void {
    this.options.stalled = stalled;
  }

  async controlIn(bRequest: number, wValue: number, length: number): Promise<DataView> {
    await this.delay();
    if (!this.opened) {
      throw new TransportError("fake device not open");
    }
    if (this.options.stalled) {
      throw new TransportError("stall (fake)");
    }

    if (bRequest === PL_CFG_REQ_GET_INFO) {
      const bytes = encodeDeviceInfoForTest(this.options.info ?? DEFAULT_INFO);
      return sliceView(bytes, length);
    }

    if (bRequest === PL_CFG_REQ_GET_TELEMETRY && wValue === TELEMETRY_PAGE_HOME) {
      const snapshot = this.options.snapshot ? this.options.snapshot() : emptyHomeSnapshot();
      const bytes = encodeHomeSnapshotForTest(snapshot);
      return sliceView(bytes, length);
    }

    throw new TransportError(`FakeTransport: no scripted answer for bRequest 0x${bRequest.toString(16)} wValue ${wValue}`);
  }

  async controlOut(_bRequest: number, _wValue: number, _bytes: Uint8Array): Promise<void> {
    await this.delay();
    if (!this.opened) {
      throw new TransportError("fake device not open");
    }
    if (this.options.stalled) {
      throw new TransportError("stall (fake)");
    }
    // No IMPORT_PRESET/EQ-editor scripting yet -- jyhk.11+ territory.
  }

  private delay(): Promise<void> {
    const ms = this.options.latencyMs ?? 0;
    if (ms <= 0) return Promise.resolve();
    return new Promise((resolve) => setTimeout(resolve, ms));
  }
}

function sliceView(bytes: Uint8Array, length: number): DataView {
  const capped = bytes.subarray(0, Math.min(length, bytes.length));
  return new DataView(capped.buffer, capped.byteOffset, capped.byteLength);
}
