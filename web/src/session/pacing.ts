// Pure pacing math, pulled out of `Session` so it's directly unit-testable
// (design section 4: "Pacing: 33ms start-to-start ... Median RTT over 30
// polls; if > 20ms drop to 50ms").

/** Median of a small RTT sample window. Returns 0 for an empty window (no backoff yet). */
export function medianRtt(samples: readonly number[]): number {
  if (samples.length === 0) return 0;
  const sorted = [...samples].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 1 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
}

export interface PacingOptions {
  pollIntervalMs: number;
  backoffIntervalMs: number;
  rttBackoffThresholdMs: number;
}

/** `pollIntervalMs` at a healthy median RTT, `backoffIntervalMs` once it exceeds `rttBackoffThresholdMs`. */
export function nextPollIntervalMs(rtts: readonly number[], opts: PacingOptions): number {
  return medianRtt(rtts) > opts.rttBackoffThresholdMs ? opts.backoffIntervalMs : opts.pollIntervalMs;
}

/** Bounded RTT sample window (design: "Median RTT over 30 polls"). Mutates `samples` in place, returning it for convenience. */
export function pushRttSample(samples: number[], rtt: number, windowSize: number): number[] {
  samples.push(rtt);
  if (samples.length > windowSize) samples.shift();
  return samples;
}
