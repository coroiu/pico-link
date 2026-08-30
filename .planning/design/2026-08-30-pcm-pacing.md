# PCM pacing: what pbv actually measured, and the two live defects behind it

**Bead:** `pico-link-pbv` · **Author:** Ada (architect) · **Date:** 2026-08-30
**Status:** design of record for the PCM ring / A2DP drain pacing

Source-derived from `firmware/src/a2dp.c`, `pcm_ring.c/.h`, `usb_audio.c`. No
hardware was available this session; every claim below is marked source-derived
or needs-measurement.

## Context

`pico-link-pbv` recorded, on hardware 2026-08-29 with SBC and a real headset:
`ovr_frames` climbing linearly (11300 → 293543 over ~20-25 s, never plateauing)
and `fill` steady at 31000-32700 against `PL_PCM_TARGET_FILL_BYTES` of 4608 —
recorded in the bead as "roughly 7x target". USB ingestion measured correct at
191600-192040 B/s.

The orchestrator posed the contradiction that motivated this pass: at a nominal
88.5 packets/s x 896 sample-frames/packet the drain should consume ~317 kB/s of
PCM against 192 kB/s ingested — comfortably more than supply — yet the ring
overflowed continuously. Both cannot be true.

## Decision

### The wrong premise: both factors, plus a misread of `fill`

**`fill` was never "7x target". It was the ring completely full.**
`PL_PCM_RING_CAPACITY` is 32768 (`pcm_ring.h:32`; verified). A reading of
31000-32700 is 95-99.8% of capacity — saturation, not a loop settling at the
wrong setpoint. "7x target" invites a controller-tuning reading; the correct
reading is "the drain could not keep up at all".

**The packet rate was ~44/s, not 88.5.** With the old
`PL_A2DP_MAX_FRAMES_PER_TICK` of 5 against a `frames_per_packet` of 6-7, and
fill-and-arm happening only once per tick (`a2dp.c:673-685`), a single packet
takes *two* ticks to fill. Throughput averaged 3-3.5 frames/tick, i.e. 265-310
SBC frames/s = **136-159 kB/s of PCM against 192 kB/s ingested**.

The resulting deficit of 8300-14000 sample-frames/s brackets the measured
~12500/s overflow rate. Capacity was never 317 kB/s; **drain never exceeded
supply.** This is the same mechanism `a2dp.c:35-44` already names in its own
module doc — the pbv measurement is *pre-fix* and fully consistent with it. The
contradiction dissolves; there is no third unknown bug behind the original
numbers.

### Finding 1 — `priming_target_bytes` can exceed the ring and deadlock PRIMING

`a2dp.c:887` computes `jitter_bytes = (worst_tick_interval_us / 1000) * 192`.
`worst_tick_interval_us` is a monotone maximum with **no reset anywhere**: the
only writer is `a2dp.c:573-574`. Verified — and note `a2dp.c:83` and
`a2dp.c:612` *explicitly cite its non-resetting* as the reason not to use it in
the credit clamp, and then `a2dp.c:887` uses it anyway.

One prior stall over ~171 ms therefore makes `priming_target_bytes` exceed
`PL_PCM_RING_CAPACITY`, a level `pl_pcm_fill_bytes()` can never reach. The
PRIMING state never satisfies its exit condition, `a2dp_source_start_stream`
(`a2dp.c:664`) is never called, and **the stream silently never starts** — no
error, no counter, no log line saying why.

Given that superloop stalls over **2 seconds** are already on record
(`pico-link-okx`), this is a live reconnect hang, not a theoretical one. It is
also a plausible contributor to `pico-link-dgx` (navigating Home drops the link).

**Fix:** reset `worst_tick_interval_us` at STREAM_STARTED, and clamp
`priming_target_bytes` to at most `PL_PCM_RING_CAPACITY / 4`.

### Finding 2 — the credit clamp is a one-way ratchet keyed to the wrong quantity

`a2dp.c:627` bounds `samples_owed` at
`frames_per_packet * 128 + accrued + 128` (~1400 samples) and *destroys* the
excess. But credit is only fictitious when the **ring** is dry. The real windup
source is that accrual at `a2dp.c:591` is unconditional while the
`stop_ring_empty` path consumes none.

Keying the bound to packet size means it can also bind while the ring is deep —
where the credit is genuine and destroying it is simply lost drain. Every such
event is a permanent step up in ring fill, removable only by the +/-500 ppm USB
feedback loop (`usb_audio.c:306-314,328,346`) at 96 B/s: roughly **48 seconds to
work off each 4.6 kB step**.

**Fix:** clamp `samples_owed` to `pl_pcm_fill_bytes() / 4 + pcm_frame_count`.
That never binds when the ring is full, and is exactly the deficit-side resync
the clamp was meant to be.

### The structural consequence: ring fill is a free integrator

With credit pacing, drain rate is defined by our crystal, which is also what
supply is regulated against. Ring fill therefore has **no restoring force** — it
integrates any offset forever. The only regulator is the 500 ppm USB feedback
loop, which is about **100x too weak** to remove an offset in useful time.

Offsets must be removed **discretely**: a hysteresis-banded one-shot
`pl_pcm_trim_to(target)`, counted in `resync_drops`. Both the function
(`pcm_ring.h:98`) and the counter (`a2dp.c:322`) **already exist and are both
currently dead** — `pico-link-ye0` files the function as unused code. It should
be wired up, not deleted.

This is the missing third leg. Without it, pbv can pass a single 30 s run and
still drift out over ten minutes — which is precisely the shape of the
"~5.4 min ingestion collapse" noted separately in the 2026-08-30 handoff.

## Alternatives considered

1. **Tune the feedback controller harder.** Rejected: the loop is capped at
   +/-500 ppm by the USB audio class, and the sign/EMA/clamp were already
   verified correct in review. No amount of tuning gives a 100x.
2. **Grow the ring.** Rejected: a free integrator overflows any finite buffer;
   a larger ring only moves the failure later and adds latency.
3. **Treat pbv as a controller-tuning problem** (the "7x target" reading).
   Rejected on the capacity arithmetic above — the ring was saturated, not
   mis-set.

## Consequences

- The original pbv measurement is **explained and superseded**; it should not be
  re-measured as stated. Two new, distinct source-derived defects replace it.
- `pl_pcm_trim_to` and `resync_drops` graduate from dead code to required
  mechanism. `pico-link-ye0` (delete the dead function) must NOT be actioned.
- `pico-link-85v` should be rescoped — see below.

## `pico-link-85v`: sequencing and scope

**The dependency on pbv is correct, but not for the reason it was filed.** 85v
is not *blocked* by pbv on throughput grounds: today's ceiling is
`frames_per_packet` x ~88.5 = ~620 SBC frames/s against a demand of 375 — 1.65x
headroom — so the packet ceiling does not bind for SBC at all, and
`stop_packet_full_hot` should read 0. The real coupling is that both edit the
same fill/arm loop, and 85v's acceptance criterion is stated in packets/s, which
is meaningless while the pacer is unsettled. Keep the dependency; the *design*
half of 85v can proceed in parallel.

**Scope change:** frame 85v as **"decouple fill from send"** — a small queue of
ready payloads, fill on credit, re-arm `request_can_send_now` while the queue is
non-empty — not as "send N packets per tick". Today a tick with
`sbc_ready_to_send` still true does **no filling at all** (`a2dp.c:683-685`,
verified: the `else` branch only increments `ticks_send_pending`). That is the
actual ceiling mechanism. Fixing it is the same work, and it additionally
removes a jitter source that LDAC will amplify.

## What still needs hardware — ONE capture, not a hunt

Connect, play 60 s, disconnect, reconnect, play 30 s. From `pl_a2dp_report`:

| # | Read | Confirms |
|---|---|---|
| a | `enc_frames_total` delta / `report_dt_us` | should be 375 +/-2 /s |
| b | `credit_clamp_events`, `credit_clamped_samples` | nonzero while `fill` is high confirms Finding 2 |
| c | `fill` / `fill_ema` trend across the full 60 s, not one sample | monotone rise with `ovr_frames`=0 is the free-integrator drift |
| d | `stop_dwell`, `stop_packet_full_hot`, `stop_ring_empty` | all expected 0 |
| e | `worst_tick_interval_us`; on the **second** stream's establish line, `priming_target_bytes=` and `jitter_bytes=` | `jitter_bytes` > 32768 confirms Finding 1; a stream that never logs "stream started" is Finding 1 firing |
