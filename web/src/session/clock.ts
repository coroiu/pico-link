// Device-clock offset estimation (FERN DESIGN section 6, pico-link-jyhk.8):
// "use the MINIMUM of (host_receive - uptime_ms) over a sliding ~2s window,
// not the latest. Snapshot can be 20ms old plus RTT; the minimum is the
// least-delayed estimate and the window follows drift. Without this, one
// slow reply shifts the offset and fires false staleness."

const WINDOW_MS = 2_000;

interface OffsetSample {
  /** Host wall-clock time (e.g. `performance.now()`) this reading was taken at -- the window key. */
  hostAtMs: number;
  /** `hostReceiveMs - uptimeMs` for that reading. */
  offsetMs: number;
}

/**
 * Tracks `hostReceive - uptimeMs` samples over a rolling window and reports
 * the minimum -- an estimate of `deviceUptimeMs = hostNowMs - offset` that a
 * single slow reply can't drag upward (offset is bounded below by the
 * least-delayed sample, matching design section 6).
 */
export class ClockOffsetEstimator {
  private samples: OffsetSample[] = [];
  private readonly windowMs: number;

  constructor(windowMs: number = WINDOW_MS) {
    this.windowMs = windowMs;
  }

  /** Records one `(hostReceiveMs, uptimeMs)` pair from a fresh telemetry reply. */
  record(hostReceiveMs: number, uptimeMs: number): void {
    this.samples.push({ hostAtMs: hostReceiveMs, offsetMs: hostReceiveMs - uptimeMs });
    const cutoff = hostReceiveMs - this.windowMs;
    this.samples = this.samples.filter((s) => s.hostAtMs >= cutoff);
  }

  /** `null` until at least one sample has been recorded. */
  offsetMs(): number | null {
    if (this.samples.length === 0) return null;
    return this.samples.reduce((min, s) => Math.min(min, s.offsetMs), Number.POSITIVE_INFINITY);
  }

  /** Converts a host wall-clock reading (e.g. `performance.now()`) to the estimated device uptime, or `null` if no sample has been recorded yet. */
  toDeviceMs(hostNowMs: number): number | null {
    const offset = this.offsetMs();
    return offset === null ? null : hostNowMs - offset;
  }

  /** Reboot detection (design section 4): drop every sample, e.g. when uptime_ms goes backwards or snap_seq restarts. */
  reset(): void {
    this.samples = [];
  }
}
