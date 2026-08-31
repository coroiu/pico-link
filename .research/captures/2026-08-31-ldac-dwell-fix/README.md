# 2026-08-31 — LDAC dwell-cap fix: the three captures that proved it

Real-target captures from the Pico Plus 2 W streaming LDAC to a Sony WH-1000XM3,
taken with `tools/usb-console/cdc_reader.py` (direct USB bulk, no tty node — see
CLAUDE.md on why the tty path is forbidden on this Mac).

All three were read with the **priority counter channel** from `pico-link-auh`,
which is what made them possible: before it, the `a2dp` counter lines vanished
from the console the moment streaming started, and the diagnosis had been blocked
on that for the whole of 2026-08-31.

## Parsing these files — read this first

**The CDC console is not line-atomic.** A line-anchored regex (`^ctr `) finds
roughly a sixth of the `ctr` lines and reports hundreds of missing sequence
numbers, which looks exactly like a catastrophic firmware failure. It is a
measurement artifact. **Always match unanchored.** This trap cost a wrong
conclusion during this very session before it was caught.

Slot line formats (fixed width, `pl_prio.h`):

- `ctr s= u= t= e= p= d= c= o=` — seq, uptime_ms, tick_count, enc_frames_total,
  pkt_sent, stop_dwell, stop_credit, ovr_frames
- `atr pc0= b0= pc1= b1= pc2= b2=` — top-3 log producers by bytes, resolve with
  `arm-none-eabi-addr2line -e firmware/build/pico_link.elf`
- `drn c= s= z= bd= bp= dr=` — drain_calls, drain_skips, room_zero,
  bytes_drained, bytes_pushed, bytes_dropped

Only **cumulative counter deltas** are trustworthy on this firmware. Instantaneous
rates read from a single window are not — two windows in one session once read
168 and 396 packets/s.

## `capture_zmg.log` — 673 s, BEFORE the fix (bead `pico-link-zmg`)

The A1-A8 hardware verification of the priority channel, and the first capture in
which the LDAC dwell falsifier could actually be evaluated.

- Δ`stop_dwell`/Δ`tick_count` = **0.1851** — 18.5% of ticks hit the 6000 µs
  backstop. `stop_dwell` is a per-tick occurrence counter (`a2dp.c:610-682`), not
  microseconds. This **supported** Ada's dwell-cap model.
- `enc_frames/s` = 338.27 against 375.0 needed — a **9.8% shortfall**.
- `ctr` sequences 147-161 are missing, then 633 consecutive publishes with zero
  gaps. That gap was a real firmware bug (`pico-link-idy`, fixed): the
  `available == 0` early return in `pl_log_ring_drain` preceded the priority-slot
  check, so a slot was silently dropped whenever the log ring happened to be empty.
- `drain_calls/s` = **6.14** against ~28 fps idle — the superloop collapse now
  tracked as `pico-link-p1r`. This refuted the 7.7 KB/s drain-ceiling estimate the
  `auh` design was written against; the real ceiling is ~1.57 KB/s.
- Top-3 `atr` PCs all resolve to `pl_usb_pump_report`. `main.c:528` is **not**
  among them, refuting `pico-link-wbq`'s attribution.

## `residual.log` — 94 s, AFTER the fix

`PL_A2DP_MAX_ENCODE_DWELL_US` raised 6000 → 10000 µs (merged at `0c92dd3`).
Andreas, listening: *"muuuuuch better ... I can actually listen to music like
this"*, with a teeny-tiny residual crackle.

- `enc_frames/s` = **374.76** against 375.00 — shortfall **0.064%**, down from 9.8%.
- Δ`stop_dwell` = 65 over 94.2 s = 0.69/s = **0.78% of ticks**, down from 18.5%.
- Δ`stop_credit` = 8233 (87.6/s) — essentially every tick now stops on credit,
  which is the healthy reason.
- Δ`ovr_frames` = 0, `ctr` sequence gaps = 0.
- Per-second encode rate: min 370.7, p05 372.8, p50 374.9, p95 375.6, max 378.9.

This is the evidence behind `pico-link-fn9` (residual crackle).

**`tick/s` reads 88.93 against a nominal 100 and is NOT a defect.** `a2dp.c:65-86`
documents it: pico-sdk 2.1.1's `btstack_run_loop_async_context.c` adds
`timeout_in_ms + 1` and re-derives from an already-truncated ms value, giving a
real cadence of ~11.3 ms. Credit-pacing accrual is immune because it uses a real
`time_us_64()` delta every tick. This was nearly filed as a bug before that
comment was read.

## `badstate.log` — 30 s, the episode that hit right after

Audio degraded badly while Andreas was listening. **Not a regression from the
dwell fix** — the first capture of this failure taken with working instruments:

- `enc_frames/s` = **187.82**, exactly 375/2.
- `pkt_sent/s` = **93.89**, exactly half of the healthy 187.38.
- Δ`stop_dwell` = **0** — not encode-limited at all.
- `stop_ring_empty` climbing 1076 → 2059; `rx_short_packets` climbing 3026 → 4492.

An exact factor of two with the input ring running empty is a host that halved its
packet rate. This is the `pico-link-2ap` signature: episodic halving of ISO-OUT
delivery to an exact 0.500 packets/SOF floor, lasting ~50-130 s, roughly every
400 s, already proven host-side with our feedback pinned flat. macOS, not us.

## Why these are in the repo

The beads board lives in a gitignored Dolt DB and a session's `/private/tmp` does
not survive. These three files are the primary evidence for `pico-link-zmg`,
`pico-link-idy`, `pico-link-p1r`, `pico-link-fn9` and `pico-link-2ap`, and
re-taking them costs hardware time plus a human with headphones on.
