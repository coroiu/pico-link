// Bit-exact ballistics cross-check against fixtures/telemetry/meter-trace.json
// (FERN DESIGN section 5/6, pico-link-jyhk.8). samples[] are folded in order
// as each query's at_ms is reached; queries[] assert the render-time output
// -- see meter-trace.json's own `_comment` for the exact contract.
import { describe, expect, it } from "vitest";
import { OutLevelBallistics } from "./ballistics";
import { fixtureJson } from "../test/fixtures";

interface TraceSample {
  at_ms: number;
  peak_l: number;
  peak_r: number;
  rms_l: number;
  rms_r: number;
}

interface TraceQuery {
  at_ms: number;
  received_ms: number;
  stale: boolean;
  displayed_peak_l: number;
  displayed_peak_r: number;
  hold_l: number;
  hold_r: number;
}

interface MeterTrace {
  samples: TraceSample[];
  queries: TraceQuery[];
}

describe("OutLevelBallistics vs fixtures/telemetry/meter-trace.json", () => {
  const trace = fixtureJson<MeterTrace>("meter-trace.json");

  it("has a non-trivial trace (guards against a silently-empty fixture)", () => {
    expect(trace.samples.length).toBeGreaterThan(0);
    expect(trace.queries.length).toBeGreaterThan(0);
  });

  it("matches every query bit-exactly", () => {
    const ballistics = new OutLevelBallistics();
    let nextSample = 0;

    for (const query of trace.queries) {
      while (nextSample < trace.samples.length && trace.samples[nextSample].at_ms <= query.at_ms) {
        const s = trace.samples[nextSample];
        ballistics.fold({ atMs: s.at_ms, peakL: s.peak_l, peakR: s.peak_r, rmsL: s.rms_l, rmsR: s.rms_r });
        nextSample += 1;
      }

      const result = ballistics.query(query.at_ms);
      expect({ atMs: query.at_ms, ...result }).toEqual({
        atMs: query.at_ms,
        stale: query.stale,
        displayedPeakL: query.displayed_peak_l,
        displayedPeakR: query.displayed_peak_r,
        holdL: query.hold_l,
        holdR: query.hold_r,
      });
    }
  });
});
