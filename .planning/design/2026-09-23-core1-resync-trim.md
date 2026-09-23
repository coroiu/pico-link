# Core1-safe resync trim

Ada, 2026-09-23, desk pass, no hardware. Base: main `fe8ce44` + `bd-pico-link-nli.9`
`11d6db7`. Respects ADR 2026-09-03 core1 invariants: no Rust, no alloc, no
interrupts-off, no `pl_log`, no `bt.c` push, no BTstack call.

## Problem

`pl_pcm_trim_to` writes pcm_ring `s_tail`. Under ON the sole `s_tail` writer is
core1 (`pl_pcm_read` inside `pl_a2dp_fill` from `pl_a2dp_core1_entry`). The fhf
trim block (a2dp.c ~2293-2339 on nli.9) runs in core0's media timer, so it is
ifndef'd out and ON has no surplus-side drift correction. The decision also
needs usb_audio state (`pl_usb_audio_fb_fill_ema`, `pl_usb_audio_fb_reset`)
that core1 must not touch.

## Design: DECIDE on core0, APPLY on the tail owner, COMPLETE on core0

Same split as the 7jol.3 LDAC ABR controller (DECIDE writes one request word
on core0; APPLY runs where the encoder lives). Reuse the pattern; do not
invent a second one. Rule: only the tail owner moves the tail; only core0
touches usb_audio state and `s_ctx` counters.

New statics in a2dp.c, both builds:

- `volatile uint32_t s_trim_req_seq` -- written only by core0
- `volatile uint32_t s_trim_ack_seq` -- written only by the applier
- `volatile uint32_t s_trim_dropped` -- written only by the applier, before
  the ack
- `s_ctx.trim_seen_ack` (uint32) -- core0-private, last ack COMPLETE
  consumed

Outstanding request = `(s_trim_req_seq != trim_seen_ack)`. No payload: the
applier reads `pl_pcm_target_fill_bytes()` itself (written only at
STREAM_ESTABLISHED, before RUNNING is published, so stable while core1 runs).

Three shared functions; the ifdef carve-out becomes a call-site choice:

1. `pl_a2dp_resync_decide(now_us, host_silent)` -- media timer, BOTH builds,
   same spot as today (after STREAMING guard + ABR block, before fill/send-
   kick). Return if `host_silent` or a request is outstanding. Else today's
   condition unchanged (`fill_ema > target + PL_PCM_TRIM_BAND_BYTES` and
   `now - last_resync_us > PL_PCM_TRIM_MIN_INTERVAL_US`). On trip:
   `last_resync_us = now; __dmb(); s_trim_req_seq++`.
2. `pl_a2dp_resync_apply()` -- the tail owner. `req = s_trim_req_seq`; if
   `req == s_trim_ack_seq` return (applier is the ack's only writer). Else
   `s_trim_dropped = pl_pcm_trim_to(pl_pcm_target_fill_bytes()); __dmb();
   s_trim_ack_seq = req`. ON: call in `pl_a2dp_core1_entry`'s RUNNING branch
   after `s_enc_quiesced = false`, before accrue_credit/`pl_a2dp_fill`. OFF:
   call inline right after decide.
3. `pl_a2dp_resync_complete()` -- media timer. `ack = s_trim_ack_seq`; if
   `ack == trim_seen_ack` return. Else `__dmb(); trim_seen_ack = ack;
   resync_drops += s_trim_dropped; resync_events++; pl_usb_audio_fb_reset()`
   LAST. ON: call at the top of the STREAMING part of the handler, before
   decide. OFF: inline after apply.

OFF runs decide/apply/complete back to back in one tick = exactly today's
fhf behaviour, no regression. ON: apply on core1's next iteration (at most
one 2ms fill cap + one encode, ~4ms), complete on the next media tick
(~11ms).

Remove the `ifndef PL_ENCODER_ON_CORE1` around the trim; its own comment's
condition ("do not remove without that design in place") is met. Rewrite
that comment to point here.

## Why it is race-free

- One `s_tail` writer at every instant. Under ON the trim runs on core1 in
  the same RUNNING context as `pl_pcm_read`. The producer (core0 0xC0
  worker) is untouched; `pl_pcm_trim_to` reads `s_head` once like
  `pl_pcm_read` (existing SPSC consumer contract). No ring data is read, so
  no data barrier needed.
- One writer per shared word. Core0 writes `req`; the applier writes
  `dropped` + `ack`. Core1 never touches usb_audio or `s_ctx` counters, so
  the core1-does-not-call-modules-it-does-not-own invariant holds exactly
  as for `s_enc_heartbeat`.
- Reseed sees the post-trim tail: core1 stores tail, dmb, ack (release);
  core0 loads ack, dmb, fb_reset which reads `pl_pcm_fill_bytes` (acquire).
  Today's in-IRQ "reseed LAST" ordering becomes a cross-core
  release/acquire pair. All words aligned 32-bit, no tear.
- No stacking: decide refuses while a request is outstanding, so a stalled
  core1 cannot bank multiple cuts and fire them in a burst later. The 2s
  lockout runs from decide time, far above any apply/complete latency.
- EMA gap is harmless: between apply and complete the 0xC0 EMA folds in
  ~12 post-trim samples, moving toward the new value, then the reseed
  overwrites it. Better than today's one stale sample.
- Core0 single writer context: `pico_cyw43_arch_threadsafe_background`
  (CMakeLists.txt:532) runs BTstack timers and packet handlers in the same
  `async_context`, so the media timer (decide/complete) and
  STREAM_STARTED (cancel, below) are serialized. No lock. State this in
  the code comment. Do NOT add a critical section (invariant 3).

## Cancel at stream (re)arm -- REQUIRED

A request outstanding across SUSPEND/RELEASE would be applied on the next
stream's first RUNNING iteration and trim the priming cushion (fhf's rule:
no trim at STREAM_STARTED; the C2-3 deleted trim). In
`pl_a2dp_core1_arm_running`, before the `__dmb` that publishes RUNNING:
`s_trim_req_seq = s_trim_ack_seq; s_ctx.trim_seen_ack = s_trim_ack_seq`.
Safe because core1 is quiesced (it only applies while RUNNING, and every
path into STREAM_STARTED passes a quiesce or boot IDLE). A quiesce timeout
is the existing counted risk (`s_enc_quiesce_timeouts`), not a new one.
OFF: same two assignments next to `resync_events = 0` at STREAM_STARTED.

## Debug injection under ON (test A needs it)

SKIPTICKS (`s_debug_skip_media_ticks`, a2dp.c:749/2358) only gates the OFF-
path `pl_a2dp_fill`; under ON it is a no-op, i.e. a test that cannot fail.
Add, `PL_DEBUG_REMOTE` only: `pl_a2dp_debug_skip_media_ticks(K)` under ON
writes `s_dbg_core1_skip_us = K * PL_A2DP_AUDIO_TIMEOUT_MS * 1000`,
`__dmb()`, `s_dbg_core1_skip_seq++`. Core1 keeps a private last-seen seq;
on change it latches `skip_until = now + us`; while `now < skip_until` it
skips accrue AND fill, sets `s_enc_last_tick_us = now`, bumps heartbeat,
continues (no WFE -- nobody would SEV). Discarding credit is deliberate:
otherwise core1 (about 3.3x real time at 809us/encode) drains the injected
surplus itself and the trim may never be needed; discarding simulates a
drain-rate deficit, which is what the trim exists for. Quiesce still works
(state re-read every iteration).

## Verification (Tess, hardware, after nli.9 + this merge)

Both arms from one commit, `PL_DEBUG_REMOTE=ON`, LDAC HQ pinned,
bitrate=990000 verified on console, same RF environment, minutes apart.
PRECONDITION or the run is void: host USB throughput steady 189.7-192.2 kB/s
before injecting (fhf test A failed on host supply, not the mechanism).

- Test A-ON (inject K=10, 100ms): ring gains 19200 B, fill 5760 -> ~24960
  (76%). Pass ALL: resync_drops +4800 +/-400 frames within 1s;
  resync_events + exactly 1, none more for 10s; fill_ema back to 5760
  +/-600 within 2s; ovr_frames flat. Zero on any = FAIL. Repeat 5x, 3s+
  apart: 5 events, 5 matching drops.
- Test C-ON (cancel path): inject K=10, pause the host within ~50ms,
  resume. Pass: first report after "stream started" shows resync_events=0
  and fill >= priming target minus one tick. Only this exercises the
  arm-time cancel.
- Test B-ON (15 min soak, 1.5x the fhf bar because this gates the flip; no
  injection; 1s report to file). Pass: ovr_frames delta 0 after the first
  30s (PRIMARY); every resync event >= 720 frames (EMA-keyed, not raw);
  stop_core1_budget < 1/s; quiesce_timeouts and flash lockout timeouts 0 AS
  PRINTED (prereq 1); zero reboots/panics/bootloader drops. Drift:
  least-squares slope of fill_ema over minutes 1-15; ON slope within
  +/-20% of the OFF arm's, or both under 2 B/s. Deficit drift is
  pico-link-0gtk, present in both modes, scored against the OFF arm, not
  this bead. OFF arm resync behaviour must be identical to pre-refactor
  fhf.

## Before CMakeLists.txt:322 flips OFF -> ON

1. HARD, cheap, rides on quzf: `pl_a2dp_encoder_quiesce_timeouts`
   (a2dp.c:2163) and `pl_flash_lockout_timeout_count` /
   `_active_count` (flash_lockout.c:205) have NO call site in
   `pl_a2dp_report`. nli.9's "both 0" acceptance criterion is unreadable
   today; an unprinted counter reads as zero. Add them to the report line
   (quiesce under ifdef ON; lockout in both builds).
2. quzf tests A/B/C pass + the nli.9 10-min acceptance.
3. gmy (P0 inreview): evidence is 9 min / 0 reboots after the 0zr fix,
   with Tex's own shorter-than-20-min caveat. Close on combined evidence:
   that + B-ON + one 30-min ON soak with the gmy nav/render harness. That
   soak must re-check gmy's unexplained iters/s 61 -> 12-28 after ~2 min.
4. 4ju (watchdog observe-only): ACCEPT CONSCIOUSLY, do not block.
   Observe-only is global and applies to OFF too, so the flip does not
   create the gap. But after the flip a core1 stall is a silent hang on
   the shipping path; Andreas should say so explicitly. Keep 4ju P1.
5. nli.6 (closed): one check only. libldac now executes from SRAM, so any
   panic symbolisation assuming libldac at 0x10xxxxxx is wrong. Grep the
   panic tooling for an address filter; confirm a core1 fault still
   yields a record.
6. 0gtk (deficit drift): NOT a blocker, equal in both modes. Note it on
   the flip commit so it is not rediscovered as a core1 regression.

## OFF after the flip: keep, demote, schedule deletion

Do not delete OFF at the flip: it is the A/B control every core1 diagnosis
relied on (nli.7, nli.9, gmy, 0gtk), and deleting it exactly as ON starts
accruing mileage removes the control group. Present cost: the ifdefs stay.
Do not keep it forever unbuilt either: there are NO `.github/workflows` and
nothing builds OFF automatically, so it will rot, and a legacy flag that no
longer compiles is worse than none. This design already shrinks the
carve-outs (shared functions, call sites differ). Concretely: (a) the flip
commit changes the option help text to "OFF = legacy core0 encoder,
fallback/A-B only"; (b) cross-compile both configs on every firmware
change -- a two-config build script under `tools/` or an explicit Tess
checklist item; (c) file a delete-OFF bead triggered by 4 weeks as default
+ no ON-attributed regression + 4ju decided. Quick path (delete now): saves
~10 ifdefs, loses the only control. Do nothing: free now, a trap later.
Recommended: one script and one bead.

## Touch points (Ruby)

firmware/src/a2dp.c only: the 3 statics + trim_seen_ack + 3 functions;
media-timer call sites for both builds; the core1_entry apply call and the
ON skip hook; the cancel in arm_running and in the OFF STREAM_STARTED path;
the prereq-1 report additions. No change to pcm_ring.c (`pl_pcm_trim_to` is
correct as written) or usb_audio.c. Do not touch the a2dp.c:893 credit
clamp or the deleted STREAM_STARTED trim. Cross-compile both configs; `nm`
on release must show no skip hook symbols.
