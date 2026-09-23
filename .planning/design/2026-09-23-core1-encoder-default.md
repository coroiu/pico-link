# Core1 encoder default: root-causing why core1 fill saturates its 2ms budget

**Author:** Ada (architect), desk pass, no hardware round.
**Date:** 2026-09-23
**Status:** Accepted. Implemented by Ruby on pico-link-nli.9 (P1 instrumentation
+ P2 objcopy-into-SRAM fix), measured by hardware A/B. Does NOT flip
`PL_ENCODER_ON_CORE1`'s default — that step waits on pico-link-quzf.
**Beads:** pico-link-nli.8 (this design), pico-link-nli.9 (implementation),
pico-link-quzf (blocking dependency for the flip), pico-link-nli (epic).

This is the design of record. Writing it directly to `.planning/design/` was
blocked on `main` at design time, so it lived as a bead comment on
pico-link-nli.8 until committed here verbatim.

## Context

Andreas wants `PL_ENCODER_ON_CORE1` as the default mode. pico-link-nli.7
measured (main `fe8ce44`, LDAC 660kbps, Andreas's music): ON crackles
continuously; `stop_core1_budget` ~490/s (`a2dp.c:1524`, cap
`PL_A2DP_CORE1_FILL_BUDGET_US=2ms` at `a2dp.c:257`), `ovr_frames` ~183/s, fill
32636 vs 5760 ring. OFF ran clean for 9.7min. ~490 caps/s x 2ms is ~98 percent
of core1 — i.e. core1 appears saturated and still falling behind, while the
same encode on core0 cost ~56 percent (G0, pico-link-nli.1).

The open question: is per-frame LDAC encode time actually slower on core1
than core0 (and if so why — bus/SRAM/XIP port bandwidth, flash lockout,
clocks, core1 loop cadence), or is the cap/credit accounting in the core1
fill path wrong (e.g. cap trips re-counting, `samples_owed` runaway, ring
clamp)?

Eliminated for render already (do not re-test): XIP miss rate, seqlock, ABR
thrash, libldac-in-SRAM, core1 stack. Raw logs:
`/tmp/nli7_logs/armON/cycle_crackle.log` and `armOFF/`.

## Verdict

**The accounting is CORRECT.** The encode really is about 2-2.8x slower on
core1. The 2ms cap is a symptom counter; it costs no throughput. Lead cause:
libldac runs from flash (28.4KB in XIP, 1.8x the 16KB cache that both cores
share). On core1 it stalls on the XIP cache and QMI while core0 runs its
stream-time code at the same moment. Confidence: high that the encode is
slow, moderate that XIP is why. The fix and the decisive test are the same
change (P2), so one Ruby+Tess round both proves and fixes it.

## Evidence (nli.7 armON, consecutive 1.008s windows, fe8ce44, 660kbps)

- `enc_frames_total` +338 per window = 335 encodes/s. Real time needs
  48000/128 = 375/s.
- `stop_core1_budget` +294 = fill calls/s, so EVERY call exits on the cap.
  `stop_credit`, `stop_queue_full` and `stop_ring_empty` are all 0 in
  absolute terms.
- `ovr_frames` +5051 = about 5010 PCM frames/s dropped = 39 encodes/s short
  = 375-335. The numbers close. `pkt_sent` +113 x `frames_per_packet` 3 =
  339, which also closes.
- The cap costs nothing. `core1_entry` (`a2dp.c:1966-1990`) calls fill again
  immediately, and the only work per call outside the encode is
  `accrue_credit` and the level publish. Raising or deleting
  `PL_A2DP_CORE1_FILL_BUDGET_US` (`a2dp.c:257`) cannot help, and it would
  break the `static_assert` against the 5ms quiesce timeout.
- The credit clamp works as designed. The ring is full (32504-32764 B), and
  `a2dp.c:1848-1857` caps `samples_owed` at ring fill + 128, so
  `stop_credit` can never fire. No windup, no double counting.
- Per-encode cost is DERIVED from the counters. 294 calls carry 338 encodes,
  1.15 per call. The cap tests only the accumulated encode dt
  (`a2dp.c:1629-1634`), so at least 85% of encodes take 2000us or more each,
  and the mean is at most 1e6/335 = 2985us.
- `enc_max_us` is 5917 on core1 against 2066 on core0 (OFF arm). Core0's
  figure INCLUDES 0xC0 preemption, and nothing preempts core1.
- Reference encode costs:
  - uncontended bench: avg 1069us (`codec_ldac.c:432-436`)
  - core0 in situ: about 1500us (G0)
  - core1 with core0 rendering and NO stream: about 1620us (F0: 61.6% at
    380/s)
  - core1 with the full stream, now: 2000us or more per encode, mean about
    2500-2985us
- F0 and now run the same code on the same core. The only difference is
  that core0 now also runs TinyUSB ISO, the pump, BTstack, cyw43 and the
  send path.

### Why XIP

- libldac's `.text` (0x1004af1c/0x1004bff8) and `.rodata`
  (0x100a0eb0/0x100a1028) live in XIP.
- `nm -u` shows its only external calls are memcpy/memset/calloc/free, and
  newlib's mem* functions are already in RAM (`memmap_default.ld:125`). So
  libldac IS the encode's entire flash dependency.
- F3 measured a chip-wide miss rate of 2.1-2.3% with a real stream, against
  0.4% in F0 without one. That is about 1.2M misses/s at CLKDIV 2, each
  line fill about 0.3-0.4us.
- OFF runs the encode and core0's other work in turn: core1 waits in WFE
  while libldac runs on core0. ON runs them at the same time, so core0's
  code keeps evicting libldac's cache lines. Every eviction puts a QMI
  round trip on core1's critical path.
- This is NOT a re-test of an eliminated hypothesis. XIP and
  libldac-in-SRAM were eliminated as causes of core0's RENDER cost, and F1
  (1n4.2) was never built. Nobody has measured core1's ENCODE throughput
  with libldac in SRAM.

### Not yet excluded, lower prior

- Stolen cycles on core1. Only the lockout FIFO IRQ is installed there
  (`flash_lockout.c`, `multicore.c:243`), and it fires only on flash
  writes.
- SRAM or bus-fabric contention. No BUSCTRL priority is set anywhere, and
  the SRAM banks are word-striped.

## Implementation (Ruby, pico-link-nli.9)

One worktree from local `main`. Scope: `firmware/CMakeLists.txt` and
`firmware/src/a2dp.c` only. NO edits under `firmware/vendor`.

**P1, instrument (both builds):** in `pl_a2dp_fill` next to `enc_max_us`
(`a2dp.c` ~1627), accumulate `enc_sum_us` and `enc_count` over each report
window. `pl_a2dp_report` prints `enc_mean_us=sum/count` and
`enc_win_max_us`, then resets both. Same racy diagnostic-only producer
rules as `enc_max_us`; document that.

**P2, the fix, UNCONDITIONAL rather than behind the flag:** it helps OFF
too and keeps one memory layout.

- Add a POST_BUILD step on the `pl_ldac_enc` target that runs
  `CMAKE_OBJCOPY --rename-section .text=.time_critical.pl_ldac_text
  --rename-section .rodata=.time_critical.pl_ldac_rodata` on that target's
  output file. `memmap_default.ld:168` places `*(.time_critical*)` in RAM
  `.data`.
- This relies on libldac being built WITHOUT `-ffunction-sections` (the map
  shows one plain `.text` and `.rodata` per object). Add a post-link check
  that FAILS the build if any `ldacBT.c.o` or `ldaclib.c.o` section lands
  at `0x10xxxxxx`.
- Cost: about 28.4KB of SRAM, against about 295KB used of 512KB (`size -A`:
  bss 274892, data 11128, uninit 8548). Put the before/after `size -A` on
  the bead.
- Do NOT fork `memmap_default.ld`, and do NOT add `__not_in_flash_func`
  inside vendor. pico-sdk 2.1.1 has no linker-fragment hook, so objcopy on
  our own build output is the least invasive seam.

**P3, ONLY if the result lands in the grey zone below:**
`__not_in_flash_func` on `core1_entry`, `pl_a2dp_fill`, `accrue_credit`,
`accumulate_levels`, `publish_levels`, `seal_head`,
`pl_pcm_read`/`fill_bytes` and `pl_codec_ldac_encode`. Not pre-emptively:
doing it now would mix two variables into one test.

**NOT the fix:**

- raising or removing the 2ms cap: zero effect
- dropping to MQ/330kbps: a product regression (ADR sec 0)
- a deeper ring: the 39 encodes/s shortfall is structural, so it only
  delays the overflow

## Measurement (Tess, after P1+P2)

ON and OFF builds from the same commit, both `PL_DEBUG_REMOTE=ON`. Pin HQ
with the QUALITY picker and VERIFY `bitrate=990000` on the console (nli.7
ran at 660k). Andreas plays his own music, at least 10 min continuous per
arm, same RF environment.

Rule fixed in advance on core1's `enc_mean_us`:

- **1400us or below:** XIP confirmed; go to acceptance.
- **1400-2000us:** do P3, re-measure once.
- **2000us or above:** XIP REFUTED; stop. The next measurement is then a
  RAM-resident fixed register loop timed on core1 once per window, plus
  logging core1's NVIC ISER (expected: only SIO_FIFO).

### Acceptance for the flip (ON, 990kbps, 10 min)

- `ovr_frames` delta 0 after the first 30s
- `stop_core1_budget` under 1/s
- loop iters/s and `pl_ui_render` p50/p95 no worse than OFF in the same
  session
- zero reboots, panics and bootloader drops
- `quiesce_timeouts` and `flash_lockout` timeouts both 0
- Andreas hears no crackle
- the OFF arm with P2 shows a lower `enc_mean_us` than today and no worse
  render

## Dependencies before `CMakeLists.txt:280` flips OFF to ON

- pico-link-quzf is HARD. The resync trim is compiled out (`#ifndef`) under
  core1, so ON has no drift correction at all, while OFF did use it during
  nli.7 (`resync_drops` 864 to 1680). Credit only accrues at real time, so
  a drift backlog never drains, and the 10-min zero-ovr bar can fail on
  drift alone.
- Order: measure P1+P2 (this answers the root cause), then quzf, then the
  10-min acceptance, then the flip.
- Confirm gmy (P0 bootloader drops, inreview) and 4ju/nli.6 (watchdog
  observe-only) are closed or consciously accepted, because the flip makes
  core1 the shipping path.

## Why no hardware round for this design pass

The only experiment that separates the hypotheses is P2 itself, a firmware
change. Instrumentation alone would only re-confirm the per-encode cost the
counters already give.
