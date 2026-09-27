// Named constants a host-side ballistics/decode port must use verbatim
// rather than retype (FERN DESIGN section 5, pico-link-jyhk.8: "nothing
// numeric is retyped from reading Rust ... a stale fixture fails cargo
// test"). These literals are asserted against
// `fixtures/telemetry/constants.json` in `constants.fixture.test.ts` --
// that test, not this file, is what catches drift from core.

/** `core/src/app/model.rs`'s `RELEASE_RATIO_PER_MS_Q16`. */
export const RELEASE_RATIO_PER_MS_Q16 = 65384;

/** `core/src/app/model.rs`'s `OUT_LEVEL_HOLD_DURATION` (ms). */
export const OUT_LEVEL_HOLD_DURATION_MS = 1500;

/** `core/src/render/hero.rs`'s staleness threshold (ms). */
export const OUT_LEVEL_STALE_AFTER_MS = 200;

/** `core/src/app/fault.rs`'s `FaultLog` tiers (ms). */
export const FAULT_LIVE_WINDOW_MS = 20_000;
export const FAULT_RETIRE_MS = 120_000;
