// Bit-exact TS port of core's Q16.16 peak-hold/release ballistics
// (`core/src/app/model.rs`'s `decay_peak`/`q16_pow`/`q16_mul`, and
// `core/src/app/fold.rs`'s `App::on_levels_changed`). FERN DESIGN section 6:
// "JS must match BIT-EXACTLY: the Q16 math ... is exact in doubles (every
// product <= 2^32)". Asserted against `fixtures/telemetry/meter-trace.json`
// in `ballistics.fixture.test.ts`.
import { OUT_LEVEL_HOLD_DURATION_MS, OUT_LEVEL_STALE_AFTER_MS, RELEASE_RATIO_PER_MS_Q16 } from "./constants";

/** `q16_mul` (model.rs): truncating Q16.16 multiply. Exact: both operands `<= 1<<16`, product `<= 2^32`, safely representable as a JS double. */
function q16Mul(a: number, b: number): number {
  return Math.floor((a * b) / 65536);
}

/** `q16_pow` (model.rs): exponentiation-by-squaring, no float/log/pow. */
function q16Pow(base: number, exp: number): number {
  let result = 1 << 16;
  let b = base;
  let e = exp;
  while (e > 0) {
    if (e & 1) result = q16Mul(result, b);
    b = q16Mul(b, b);
    e = Math.floor(e / 2);
  }
  return result;
}

/**
 * `decay_peak` (model.rs): decays `anchor` (a linear 0-255 peak) by
 * `elapsedMs` at `RELEASE_RATIO_PER_MS_Q16`'s ~20 dB/s release rate.
 * `elapsedMs` is clamped to `>= 0` (mirrors `saturating_duration_since`).
 */
export function decayPeak(anchor: number, elapsedMs: number): number {
  const elapsed = Math.max(0, Math.floor(elapsedMs));
  const ratio = q16Pow(RELEASE_RATIO_PER_MS_Q16, elapsed);
  return Math.floor((anchor * ratio) / 65536);
}

export interface LevelSample {
  /** Device clock (uptime ms) this sample was folded at -- both the hold/attack-anchor clock and the staleness `received_at`, per `App::on_levels_changed`. */
  atMs: number;
  peakL: number;
  peakR: number;
  rmsL: number;
  rmsR: number;
}

export interface LevelQuery {
  /** `(nowMs - receivedAt) >= OUT_LEVEL_STALE_AFTER_MS`, or `true` if nothing has ever been folded. */
  stale: boolean;
  displayedPeakL: number;
  displayedPeakR: number;
  /** Raw peak-hold cap -- never itself decayed (`OutLevelSample::hold_l`/`hold_r`). */
  holdL: number;
  holdR: number;
}

/**
 * Port of `BtModel::out_level` (an `Option<OutLevelSample>`) plus
 * `App::on_levels_changed`'s fold and `crate::render::hero`'s render-time
 * `decay_peak` query. One instance per meter (the device has exactly one,
 * for the stereo OUT bar).
 */
export class OutLevelBallistics {
  private holdL = 0;
  private holdLAt = 0;
  private holdR = 0;
  private holdRAt = 0;
  private attackL = 0;
  private attackLAt = 0;
  private attackR = 0;
  private attackRAt = 0;
  private receivedAt = 0;
  private hasSample = false;

  /**
   * Folds one sample, deduped/ordered by the caller on `received_ms`
   * (design section 6: "Fold on each new sample (dedupe on received_ms,
   * only when level_present)"). Not idempotent -- call at most once per
   * distinct `atMs`.
   */
  fold(sample: LevelSample): void {
    const now = sample.atMs;
    if (!this.hasSample) {
      // Mirrors `prev_out_level: None => (0, now, 0, now)` in fold.rs --
      // the "previous" anchor/hold defaults to zero anchored at *this*
      // fold's clock, not epoch zero.
      this.holdLAt = now;
      this.holdRAt = now;
      this.attackLAt = now;
      this.attackRAt = now;
    }

    if (sample.peakL >= this.holdL || now - this.holdLAt >= OUT_LEVEL_HOLD_DURATION_MS) {
      this.holdL = sample.peakL;
      this.holdLAt = now;
    }
    if (sample.peakR >= this.holdR || now - this.holdRAt >= OUT_LEVEL_HOLD_DURATION_MS) {
      this.holdR = sample.peakR;
      this.holdRAt = now;
    }

    const decayedL = decayPeak(this.attackL, now - this.attackLAt);
    if (sample.peakL >= decayedL) {
      this.attackL = sample.peakL;
      this.attackLAt = now;
    }
    const decayedR = decayPeak(this.attackR, now - this.attackRAt);
    if (sample.peakR >= decayedR) {
      this.attackR = sample.peakR;
      this.attackRAt = now;
    }

    this.receivedAt = now;
    this.hasSample = true;
  }

  /** Render-time query at device clock `nowMs` -- no mutation (matches `crate::render::hero`'s pure render-time decay). */
  query(nowMs: number): LevelQuery {
    return {
      stale: !this.hasSample || nowMs - this.receivedAt >= OUT_LEVEL_STALE_AFTER_MS,
      displayedPeakL: decayPeak(this.attackL, nowMs - this.attackLAt),
      displayedPeakR: decayPeak(this.attackR, nowMs - this.attackRAt),
      holdL: this.holdL,
      holdR: this.holdR,
    };
  }

  /** Reboot detection (design section 4): the device's own ballistics state is meaningless once uptime resets. */
  reset(): void {
    this.holdL = 0;
    this.holdLAt = 0;
    this.holdR = 0;
    this.holdRAt = 0;
    this.attackL = 0;
    this.attackLAt = 0;
    this.attackR = 0;
    this.attackRAt = 0;
    this.receivedAt = 0;
    this.hasSample = false;
  }
}
