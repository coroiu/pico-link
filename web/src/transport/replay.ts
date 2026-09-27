import { TransportError } from "./types";
import type { Transport } from "./types";

/**
 * One recorded reply from `tools/usb-console`'s pyusb poller
 * (`--record out.jsonl`, FERN DESIGN section 3, B6/jyhk.5). `hex` is the
 * raw reply bytes for a `controlIn`, hex-encoded; `t_ms` is capture-relative
 * time, used to reproduce the real cadence when replaying.
 */
export interface ReplayEntry {
  t_ms: number;
  hex: string;
}

function hexToBytes(hex: string): Uint8Array {
  const clean = hex.trim();
  const out = new Uint8Array(clean.length / 2);
  for (let i = 0; i < out.length; i++) {
    out[i] = Number.parseInt(clean.substring(i * 2, i * 2 + 2), 16);
  }
  return out;
}

/**
 * Plays a captured real session back with no board attached (design
 * section 3). Sequential: each `controlIn` call consumes the next entry,
 * after waiting out the recorded inter-reply gap (scaled by `speed`).
 * `controlOut` is a no-op (the capture only records inbound replies).
 */
export class ReplayTransport implements Transport {
  private index = 0;
  private lastTMs = 0;
  private disconnectCbs: Array<() => void> = [];
  private entries: ReplayEntry[];
  private speed: number;

  constructor(entries: ReplayEntry[], speed = 1) {
    this.entries = entries;
    this.speed = speed;
  }

  async open(): Promise<void> {
    this.index = 0;
    this.lastTMs = this.entries[0]?.t_ms ?? 0;
  }

  async close(): Promise<void> {
    // Nothing to release; the capture is just an in-memory array.
  }

  onDisconnect(cb: () => void): void {
    this.disconnectCbs.push(cb);
  }

  /** Test/dev-page hook: fires the registered disconnect callbacks. */
  simulateDisconnect(): void {
    for (const cb of this.disconnectCbs) cb();
  }

  async controlIn(_bRequest: number, _wValue: number, length: number): Promise<DataView> {
    if (this.index >= this.entries.length) {
      throw new TransportError("ReplayTransport: capture exhausted");
    }
    const entry = this.entries[this.index];
    const gapMs = Math.max(0, entry.t_ms - this.lastTMs) / this.speed;
    if (gapMs > 0) {
      await new Promise((resolve) => setTimeout(resolve, gapMs));
    }
    this.lastTMs = entry.t_ms;
    this.index += 1;

    const bytes = hexToBytes(entry.hex).subarray(0, length);
    return new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  }

  async controlOut(): Promise<void> {
    // Captures only record inbound replies (FERN DESIGN section 3) --
    // outbound commands during replay are accepted and ignored.
  }
}
