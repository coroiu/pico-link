# On-arm ring collapse (`PL_ENCODER_ON_CORE1=ON`)

**Author:** Ada (architect), desk review, no hardware round.
**Bead:** `pico-link-rzqd`, comment 2026-09-23 13:10.
**Status:** Live, implemented on `bd-pico-link-rzqd`.

This is a verbatim record of Ada's design comment on the bead -- it could not
be written directly to `.planning/design` at the time because the main-branch
edit guard blocked the write, so the bead comment was the record instead. It
is reproduced here now, unedited apart from formatting, per the bead's own
instruction ("This comment is the record").

## Finding

`und`/`stop_ring_empty` is NOT smooth. Flat reports 1-295 and 413-894; all
2.5M counts are in 5 ring-collapse episodes (fill -> 50-500B), each then
refilled by `fb` on the +500ppm rail (~96B/s, ~60s). `fb_rail` delta is 0 over
reports 520-894 (Tess's last-quarter 5.1% was mis-windowed). Settled ON ==
OFF.

Triggers: boot start; host pause/resume at report 296; 8x foreign ACL page
`CONNECTION_COMPLETE` 0x0d from `88:c9:e8:07:e5:fc` -> tx stall
(`stop_queue_full` jumps) -> resync trim at 373/385/395/408. OFF log had
neither trigger.

## BUG A (ON only)

`a2dp.c`'s `pl_a2dp_core1_entry` never refreshes `s_enc_last_tick_us` while in
WFE. `arm_running` zeroes `samples_owed`, but the first RUNNING iteration
accrues the whole idle gap; the clamp (`fill/4+128`) allows the whole primed
ring, and 7 tx slots swallow it. Priming is discarded on every start/resume.

**FIX A:** core1-local `was_running` edge; on entry to RUNNING set
`s_enc_last_tick_us=now` and skip that iteration's accrual.

## BUG B (BOTH builds, latent on OFF)

A tx stall grows the ring and `samples_owed` together, decide trims to
target, `pl_a2dp_resync_apply` leaves `owed` intact, and the stale credit then
drains the post-trim ring to ~0 (`credit_clamp_events` rises at each
episode).

**FIX B:** in `pl_a2dp_resync_apply`, `samples_owed -= min(samples_owed,
dropped)` after `pl_pcm_trim_to`. The applier owns `samples_owed` in both
builds, so no lock is needed. Freshness-consistent: dropped audio is never
also owed.

## FIX C (counter)

`stop_ring_empty` counts polls (core1 spins ~100k/s while starved).
`underrun_events` is the identical count and feeds `fault.c`'s `BUF_STARVED`.
Make `underrun_events` edge-counted (increment only when `silent_ticks==0`
before this starve), add `starved_us`, keep `stop_ring_empty` raw. Bar:
`underrun_events` delta 0 and `fb_rail` 0% after a 30s settle, both builds. No
separate audible-underrun counter exists today.

## Byte balance closed

512B/frame is correct. Same-block pairing (report 5 vs 885):
`rx 170,704,340 = enc 333,302x512 + resync_drops 12,363x4 + flush 36x4 + fill
delta 3,608` -> residual 512B (3ppm, one frame of skew). Tess omitted
`resync_drops`.

## Iters/s

Derivable now from lpf `n=` (superloop iterations; frame N prints every
60th). The old figures came from unmerged 8b7/1n4 instrument branches. OFF
45/s, ON 54/s. No firmware change needed.

## Before flip

1. A+B+C, one Ruby bead, `a2dp.c` only. (This bead, `pico-link-rzqd`.)
2. ON 10-min re-soak at 990k including a deliberate pause/resume and a tx
   stall (phone pairing attempt; SKIPTICKS does not model a tx stall under
   ON). Bars: `und` delta 0 post-settle, `fill_ema` back in band within 5s.
3. Render p50/p95 under UI activity with the screensaver defeated (Tess's
   were idle-dominated).
4. Andreas's ear across a pause.

## Implementation notes (Ruby, 2026-09-23)

- Fix A lives entirely inside the `PL_ENCODER_ON_CORE1` section of `a2dp.c`
  (`pl_a2dp_core1_entry`): a new core1-local `static bool s_enc_was_running`,
  cleared on every non-RUNNING iteration, used to detect the entry edge and
  skip that iteration's `pl_a2dp_accrue_credit` call while still resetting
  `s_enc_last_tick_us` and calling `pl_a2dp_fill()`.
- Fix B lives in `pl_a2dp_resync_apply` (plain, non-`ifdef`'d, compiled in
  both builds). `pl_pcm_trim_to` already returns a frame count (not bytes),
  matching `samples_owed`'s own units, so no conversion was needed beyond the
  floor-at-0 subtraction. Confirmed single-writer in both builds:
  `pl_a2dp_resync_apply` and `pl_a2dp_accrue_credit` run in the same context
  in each build -- core1's thread under ON, core0's media-timer IRQ under
  OFF -- so no lock was added.
- Fix C edge-counts `underrun_events` in `pl_a2dp_fill`'s existing
  `starved`/`silent_ticks` block, adds a `starved_us` field (cumulative
  duration of *completed* starvation episodes) with a getter
  (`pl_a2dp_starved_us`) and a `pl_a2dp_report` log line. `stop_ring_empty`
  is untouched -- still the raw per-poll counter. `fault.c`'s `BUF_STARVED`
  raise condition (`d_underrun >= 1`) is unaffected: an ongoing starvation
  episode still produces a nonzero delta, just a smaller (correct) count.
  No threshold or test changes were needed -- `test_fault_evaluator.c` drives
  `underrun_events` through a synthetic mock model, not through `a2dp.c`
  itself.
- Verified: `cargo build`/`cargo test` at the repo root; cross-compiled all
  three configurations (default, `PL_ENCODER_ON_CORE1=ON`, and a plain
  Release build without `PL_DEBUG_REMOTE`) with
  `/Applications/ArmGNUToolchain/15.2.rel1/arm-none-eabi/bin` and
  `PICO_SDK_PATH=/Users/andreas/.pico-sdk/sdk/2.1.1`; `check_ldac_not_in_flash`
  passed in all three. `PL_ENCODER_ON_CORE1`'s default was **not** flipped --
  step 2 of "Before flip" (the 10-min hardware re-soak) is still open.
