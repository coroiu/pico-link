# Move the LDAC encoder to core1

- **Date:** 2026-09-03
- **Bead:** `pico-link-8b7` (the measurement), epic to be filed from §8 below
- **Status:** **Accepted — re-scoped 2026-09-03 after the G0 measurement.**
  The architecture in §§1-7 stands unchanged; §9's success numbers were wrong
  and are replaced. **Read §11 first — it is the ruling, and it supersedes §9's
  original table and §8's G0 entry.** **Amends**
  `.planning/decisions/2026-09-02-core1-allocation-and-the-repaint-ceiling.md`
  (see §1).
- **Author:** Ada (architect)

## 0. The finding this is designed against (do not re-derive)

Tex, `pico-link-8b7` rounds 3-4, measured on hardware and causally confirmed:

| | idle | LDAC 990 kbps | LDAC 330 kbps |
|---|---|---|---|
| superloop iters/s | 61-64 | **3** | 28-33 |
| `pl_ui_render` | 21.5-21.9 ms | **322-385 ms** | 68-79 ms |
| 1.77 ms register-only steal probe | 1841 us | 23-34 ms | 3.6-5.2 ms |
| steal factor | 1.0x | **16.3x** | 2.0-2.8x |
| dirty / forced | 0 / 1 | 3 / 0 | 4 / 0 |

While LDAC streams at 990 kbps **thread context on core 0 gets about 6% of the
core**. Two independent fixed workloads (a register-only probe that touches no
memory, and `pl_ui_render`) agree on the factor to within 1%: 16.3x vs 16.2x.
The confirmation experiment changed exactly one thing — `LDACBT_EQMID_HQ` ->
`LDACBT_EQMID_MQ`, bitrate verified on the board at 330 kbps, USB PCM identical
— and the loop went 3 -> ~31 iters/s.

Two qualifications from Tex that this design honours:

- **330 kbps is not a fix.** Render is still 76 ms against a 21.9 ms idle
  baseline; the encoder still costs ~60% of core 0 at MQ. Codec quality is a
  product property, not a tuning knob.
- At 330 kbps the two instruments diverge (2.3x vs 3.5x) because the probes
  themselves cost ~8.5 ms/iteration and perturb a fast loop more than a slow
  one. That row's direction and magnitude are solid; its absolute numbers are
  indicative.

**Ruled out with evidence, do not revisit:** the dirty gate, the input->dirty
link, the `LevelsChanged` path end to end, the `bt.c` ring, the meter's own
drawing cost, SRAM/DMA contention, flash/XIP contention.

## 1. Relation to the 2026-09-02 core1 ADR

That ADR **stands on its central decision and this design reinforces it.** It
rejected core1-for-*display* and wrote, in its own words, "Core1, if and when it
is started, runs the LDAC encoder — C, no Rust, no heap." This is that. The
seam argument it made is the reason this allocation is cheap and the other one
was not: libldac is C, allocates only at init, touches no Rust, no `PlUi`, no
allocator, no `spi1`. **The entire Rust surface stays single-core and
single-context.** `core/` is untouched by this epic and the FFI does not change.

Three things in that ADR need amending, and one of them is my own reasoning:

**(a) Its fact D was right and I weighted its consequence wrong.** Fact D says
LDAC and BTstack run at NVIC 0xFF and the USB pump at 0xC0, so audio already
preempts thread-context render by hardware, unconditionally. That is still true.
What I did not do was ask *how much* of the core that guarantee consumes. At
94% duty, "the NVIC already protects audio" and "the NVIC already destroys the
UI" are the same sentence. The guarantee is not the problem; the tenant's duty
cycle is. After this move, 0xFF on core 0 carries only BTstack and the packet
send — microseconds — so the identical NVIC guarantee stays in force and costs
the UI almost nothing.

**(b) Its deferral trigger is superseded.** It said start core1 for LDAC only
"if `pico-link-cz0.5.6` cannot bring HQ encode under ~1100 us on core0." That
trigger was sized against a per-encode budget. The real trigger turned out to be
a different quantity — **aggregate encoder duty on core 0** — and it has fired
hard. Per-encode optimisation cannot plausibly recover 16.3x.

**(c) Its "`pico-link-15n` must land first" is too strong, and I am narrowing
it.** See §6. The multicore-lockout half of 15n is a hard prerequisite. The
RAM-resident USB ISO ISR half is not, and remains deferred.

**Unchanged:** damage-rect render + partial blit is still worth building and is
still the only thing that moves the ~26 fps full-frame ceiling. But it is no
longer the critical path. With the encoder off core 0, render returns to ~22 ms
without it; damage-rect then takes 22 ms -> ~5 ms. Re-prioritise accordingly:
this epic first.

## 2. What runs where, after

**Core 0 keeps everything it has today except the encode call.** USB (TinyUSB +
the 0xC0 pump worker), the cyw43 driver, **BTstack in its entirety**, the A2DP
and AVRCP packet handlers, the media timer, `persist.c`, `st7789`/SPI1, the Rust
UI and the superloop.

**Core 1 owns exactly one thing: the PCM-to-encoded-payload conversion.** It
runs a bare C loop in thread context with no BTstack, no Rust, no allocator, no
`pl_log`, no `save_and_disable_interrupts`, and no SDK call that can block.

```
  USB ISO OUT  --(0xC0 IRQ, core0)-->  pcm_ring (SRAM SPSC, 32 KB)
                                          |
                                          |  consumer moves core0-0xFF -> core1-thread
                                          v
                                    [ CORE 1 ]  credit clock + ldacBT_encode + level accum
                                          |
                                          |  producer: seals slots
                                          v
                                     tx ring (existing pl_a2dp_slot_t tx[5])
                                          |
                                          |  consumer stays core0-0xFF (BTstack)
                                          v
                        pl_a2dp_send_media_packet -> a2dp_source_stream_send_media_payload_rtp
```

The two seams the PCM crosses are **the same two rings that already exist**. No
third ring, no copy of the ~679-byte payloads. What changes is which execution
context sits on each end, and therefore what synchronisation each ring owes.

### Why not `multicore_fifo` / SIO FIFO for the handoff

Ruled out, for a structural reason worth writing down so nobody re-proposes it:
`multicore_lockout_victim_init()` installs an **exclusive** handler on core1's
SIO FIFO IRQ (`multicore.c:243`), and the lockout handshake uses the FIFO in
both directions. Since the lockout is a hard prerequisite (§6), the FIFO is
spoken for. Any encoder handoff over it would either be stolen by the lockout
handler or steal the lockout's ack. Use shared SRAM rings; if a wakeup IRQ is
ever needed, use the **RP2350 doorbells** (`multicore_doorbell_*`, present in
pico-sdk 2.1.1, a separate mechanism from the FIFO) — see §5.

## 3. The two rings

### 3.1 `pcm_ring` — producer core0 (0xC0 IRQ), consumer core1 (thread)

Already SPSC with each side writing only its own index. Its header's safety
argument is explicitly an **IRQ-nesting** argument ("a 0xC0 preemption of the
0xFF consumer mid-read is safe"). **That argument does not carry to two cores
and must be rewritten, not merely re-read.** RP2350's SRAM is coherent across
the two M33s through the bus fabric (there is no data cache; the XIP cache is
flash-only), so no cache maintenance is required — but store *ordering* is not
free. Required change, and it is small:

- `__dmb()` on the producer between "write the data" and "publish `head`".
- `__dmb()` on the consumer between "read `tail`/`head`" and "read the data".
- Indices stay single aligned 32-bit words. No locks, no atomics beyond this.

That is the whole change to `pcm_ring.c`. The drop-newest overflow policy and
the "producer never writes `tail`" rule are unchanged and now matter more.

### 3.2 The tx ring — producer core1 (thread), consumer core0 (0xFF IRQ)

This is the invariant flip, and it must be done deliberately because
`a2dp.c:335-350` currently documents the *opposite* and tells future readers not
to add barriers:

> "CONCURRENCY: none. ... `tx_head`/`tx_tail`/`tx_count` are therefore PLAIN,
> NON-VOLATILE fields ... Do NOT add atomics, memory barriers, or volatile here"

After this change there genuinely is a second context, and:

- **`tx_count` must be deleted as a stored field.** It is a read-modify-write
  performed by both the fill side (increments at seal) and the send side
  (decrements at send). Cross-core RMW on a plain word is a lost update.
  Replace it with a derived accessor: `(tx_head - tx_tail) & (SLOTS-1)`, with
  `PL_A2DP_TX_QUEUE_SLOTS` changed from 5 to a **power of two (8)** so the
  subtraction is exact without a modulo. Each core then writes only its own
  index, which is the same discipline `pcm_ring` already has.
- `tx_head`/`tx_tail` become `volatile uint32_t` with `__dmb()` at publish and
  consume, exactly as §3.1.
- `slot->rtp_ts` and `s_ctx.rtp_next` are written only at seal, so they move to
  core1 with the fill loop. Their reset points (`STREAM_ESTABLISHED` /
  `RELEASED`) are core0 and run only while core1 is quiesced (§4.2).
- The comment block above must be rewritten to say why it changed. A stale
  "there is no second context" comment sitting above genuinely concurrent code
  is worse than no comment.

**Counters** (`stop_*`, `payloads_sealed`, `enc_max_us`, `dwell_max_us`,
`fill_short_read`, `underrun_events`, ...) are written by core1 and read by
core0's reporter. They are already `volatile uint32_t`; monotonic single-writer
counters read for diagnostics are fine as-is. Do not add locking to them; do
note in the report line that they are sampled, not snapshotted atomically.

## 4. Core1's loop

### 4.1 Behaviour: move the code, do not redesign it

**Port `pl_a2dp_fill()` to core1 essentially verbatim.** Its credit clock
(`samples_owed`, `samples_owed_rem_us`, `PL_A2DP_CATCHUP_K`), its stop-reason
instrumentation, and its seal logic are the product of three measurement rounds
(`pbv`, `85v`, `okx`) and are not to be re-derived while also changing cores.
One variable at a time is the entire lesson of the investigation that produced
this document.

Two changes only:

1. **The credit clock's tick source.** Today accrual happens in the media timer
   handler from `elapsed_us` between ticks. On core1 the loop reads
   `time_us_64()` itself each iteration and accrues from the delta. Same
   arithmetic, same remainder handling, no timer.
2. **The dwell budget is retired.** `PL_A2DP_MAX_ENCODE_DWELL_US`,
   `work_bound_us`, `duty_bound_us` and `stop_dwell` exist solely to bound how
   long an encode burst may hold an IRQ. Core1 has no other tenant, so there is
   nothing to yield to. **Keep `stop_dwell` as a counter wired to nothing and
   assert it reads 0** for one release rather than deleting it silently — a
   nonzero reading would mean the port kept a bound it should not have.

Backpressure is then the natural pacer: core1 stops when `pcm_ring` is empty
(the existing `stop_ring_empty`/starved path) or when the tx ring is full
(`stop_queue_full`). Latency stays bounded by tx depth: 8 slots at ~7 ms/packet
is ~56 ms worst case, and `tx_depth_max` already measures it.

Simplifying the credit clock away entirely, to pure two-ring backpressure, is
**a plausible follow-up and explicitly not part of this epic.** It is the
tempting quick win that would make a regression here indistinguishable from a
regression there.

### 4.2 Lifecycle: core1 is started once and never stopped

Core1 is launched at boot (after cyw43/BTstack init) and runs forever. It is
**never** `multicore_reset_core1()`d on stream start/stop — restarting a core
mid-session is a much larger failure surface than a state flag.

State is a single `volatile uint32_t s_enc_state` written by core0 and read by
core1: `IDLE -> RUNNING -> DRAINING -> IDLE`. Core0 sets `DRAINING` at
`STREAM_SUSPENDED`/`RELEASED`, then **spins on a `volatile bool s_enc_quiesced`
that core1 sets when it has left the fill body**, with a bounded timeout, before
touching any shared field (`rtp_next`, `tx_head`, codec re-init, `pl_pcm_reset`).
That quiesce handshake is the only place core0 waits on core1, it is bounded,
and it is what makes every existing core0-side reset path safe unchanged.

`ldacBT_init_handle_encode` / `ldacBT_close_handle` keep running on **core0** in
thread/async context (they allocate; they must not move), always while core1 is
quiesced.

### 4.3 Invariants core1 must hold (these are the review checklist)

1. **No Rust.** Not from core1, not ever. This is the seam the project exists to
   protect and it is the entire reason the codec is the right tenant.
2. **No allocation.** `ldacBT_get_handle()` is init-only and stays on core0.
3. **Never `save_and_disable_interrupts()`.** Core1 must service its own SIO
   FIFO IRQ promptly or `multicore_lockout_start_blocking()` on core0 hangs
   forever. Any interrupts-off window on core1 is a potential system deadlock.
4. **Never call `pl_log`.** `pl_log_ring.c:199` takes a
   `save_and_disable_interrupts()` critical section — violating (3) — and that
   critical section would not serialise against core0's writers anyway, which is
   the same corruption class `bt.c` documents. Core1 diagnostics are **counters
   only**, printed by core0's existing reporter.
5. **Never call BTstack, cyw43, `persist.c`, or anything that touches flash.**
6. **Never push into the `bt.c` event ring.** `pl_bt_ring_push`'s
   `save_and_disable_interrupts()` serialises core0's two producers only; a
   core1 producer would silently corrupt it. See §5.
7. Core1's code may live in flash (XIP) — the lockout parks it in the
   RAM-resident handler during a write. It must not *require* being in RAM.

## 5. Levels — and why this fixes the original `8b7` symptom by construction

`s_level_accum` lives inside the fill loop, so it moves to core1. It must not
reach the UI the way it does today (invariant 6).

**Design:** core1 publishes a **seqlock snapshot** into shared SRAM —
`{volatile uint32_t seq; uint16_t peak_l, peak_r, rms_l, rms_r;}` — writing
`seq++` (odd) / `__dmb()` / fields / `__dmb()` / `seq++` (even). Core0's
superloop reads it once per iteration with the standard even-seq retry, and
issues `LevelsChanged` through the normal thread-context `pl_ui_push_event`
path, rate-limited by core0's own clock.

This deletes `pl_a2dp_maybe_push_levels`, its 250 ms gate, its three
instrumentation counters, and one `bt.c` ring producer. It also removes the
staleness-at-birth bug Tex identified: the sample is now read and timestamped in
**the same superloop iteration that renders it**, instead of being stamped with
the previous iteration's `now_us`. Against `hero.rs`'s 600 ms
`OUT_LEVEL_STALE_AFTER`, at 50+ iters/s, the margin stops being marginal.

**Send-latency kick, deliberately deferred.** Core1 seals payloads but cannot
call `a2dp_source_stream_endpoint_request_can_send_now` (BTstack, core0 only).
v1 does the simple thing: core0's **existing media timer stays**, its body
shrinks to "if slots pending and not `send_requested`, request now" plus stats,
and the grant chain re-arms itself while the queue is non-empty
(`a2dp.c:1140`). With 8 slots of depth this should cover the tick period.
**Only if G3 measures packet jitter or `stop_queue_full` > 0** add an RP2350
doorbell from core1 to a core0 handler installed at the *same* NVIC priority as
the cyw43 background IRQ (equal priority does not preempt, which is the
serialisation BTstack needs). That is a subtle invariant and it should be bought
with a measurement, not assumed.

## 6. Flash, and the narrowing of `pico-link-15n`

`persist.c` already refuses to write while `pl_usb_audio_streaming() ||
pl_a2dp_streaming()` (`persist.c:706`, `persist.c:852`) and stages the write for
later. So flash writes only ever happen when core1 is **idle**. That is the
fact that lets me narrow my own ADR.

**Hard prerequisite (small, must land before core1 is ever started):**

- `multicore_lockout_victim_init()` on core1.
- Every flash write path bracketed by `multicore_lockout_start_blocking()` /
  `multicore_lockout_end_blocking()`.
- Core1's idle wait keeps interrupts enabled (invariant 3), so the handshake
  completes. The SDK's handler is already `__not_in_flash_func`
  (`multicore.c:213`), so core1 is parked in RAM for the whole write.
- Use the **timeout** variants and count failures rather than
  `_blocking` in the shipping path, so a wedged core1 fails a save instead of
  hanging the device.

**Not a prerequisite, still deferred:** the RAM-resident USB ISO ISR half of
15n. That addresses the ~3 ms interrupts-off blackouts against the 2 ms ISO-OUT
re-arm bar — a **core0** problem, unchanged by this work, and still gated by the
no-write-while-streaming rule. Split the bead: `15n` keeps the ISO-ISR half,
a new child carries the lockout half into this epic.

Note the cost the earlier ADR flagged and that this narrowing does **not**
eliminate: with core1 running, a flash write now stalls core1 for the full write
duration. Because writes only happen when not streaming, that stall lands on an
idle encoder. If the streaming gate is ever relaxed, this becomes a hard
real-time problem again and 15n's other half becomes a prerequisite after all.
Write that dependency into the bead so a future "just let it write while
streaming" change trips over it.

## 7. Failure modes

| Failure | Detection | Response |
|---|---|---|
| **Core1 hangs / faults** — audio stops, UI stays perfectly healthy and lies | Core1 bumps a `volatile uint32_t s_enc_heartbeat` each loop iteration. Core0 gets a new `PL_WDT_ENCODER` subsystem, **enabled only while streaming**, fed from the heartbeat advancing — never fed by core1 itself. | Existing watchdog path: breadcrumb + reboot. |
| **Core1 panics** | `pl_panic_c_hook` writes `__uninitialized_ram` and calls `watchdog_reboot`, which works from either core. **Add `get_core_num()` into the record's `diag` field** — otherwise a post-mortem cannot tell which core died, and that is the first question. | Record identifies the core; `pico-link-gap`'s reader prints it. |
| **Recursive/racing panic, both cores** | The panic-in-progress flag is an RMW that can now race. Accept the race but make it visible: if the record's core number and the reporting core disagree, say so. | Do not add a lock in the panic path. |
| **Lockout handshake never completes** (core1 wedged with IRQs off) | `multicore_lockout_start_timeout_us` returns false. | Count it, skip the write, keep the RAM-staged value. Never hang. |
| **Flash write while core1 runs from XIP** | Structurally prevented by the lockout. | — |
| **Lost update on `tx_count`** | Structurally prevented by deleting the field (§3.2). | — |
| **Core1 races a stream reset on core0** | Prevented by the quiesce handshake (§4.2); a timeout there is counted and logged by core0. | — |
| **Encoder outruns the sender** | `stop_queue_full` > 0 and `tx_depth_max` == SLOTS-1. Already instrumented. | Tune depth / add the doorbell (§5). |

## 8. Bead breakdown

Epic: **"LDAC encoder on core1"**. Children in order; the first is the risk gate
and **no code moves until it reports**.

**G0 — RISK GATE: how much of the 94% is actually the encoder?**
The arithmetic does not yet close. `enc_max_us` is 1848 us, the L0 bench put HQ
mean at ~1069-1159 us, and at ~385 encoded frames/s that is ~410 ms/s = **~41%
of the core** — against a measured **94%** steal. The missing ~53% is either (a)
encode being far more expensive in situ than on the bench (see the standing
lesson that fixed-input benchmarks measure a fixed point), or (b) BTstack, L2CAP
and the cyw43 SPI transport moving ~124 KB/s, which does not move to core1 at
all. **If it is mostly (b), this epic buys less than half of what it looks like
it buys and must be re-scoped before it is built.**
*Do:* add `enc_us_total` (a cumulative sum of the `dt` already computed at
`a2dp.c:1027`) and a cumulative 0xFF-IRQ-residency counter; report both as a
percentage of wall clock over a 990 kbps stream. Instrumentation only, one
flash, no architecture. Branch `bd-pico-link-8b7-gate` already carries most of
the harness — reuse it, do not throw it away.
*Proves:* the ceiling on everything below.
*GO if `enc_us_total` >= 70% of wall clock. If 40-70%: build it, but restate the
success numbers in §9 against the measured ceiling. If < 40%: STOP and re-open
the design — the answer is elsewhere.*

**G1 — multicore lockout (the narrowed half of `pico-link-15n`).**
Link `pico_multicore`; launch core1 into an empty loop that does nothing but
`multicore_lockout_victim_init()` and idle with interrupts enabled; set
`PICO_CORE1_STACK_SIZE` explicitly (currently SDK default, and
`firmware/CMakeLists.txt:316` says so because core1 was never launched); bracket
every flash write with the timeout lockout variants.
*Proves:* core1 can be started and a real pairing save still lands. No audio
involvement, independently mergeable, **can run concurrently with G2.**

**G2 — cross-core-ready rings, still single-core.**
Delete stored `tx_count` for a derived accessor; `PL_A2DP_TX_QUEUE_SLOTS` 5 -> 8;
`volatile` + `__dmb()` on both rings' index publish/consume; rewrite both
"CONCURRENCY: none" / IRQ-nesting comment blocks to state the new contract.
Everything still executes on core0.
*Proves:* the ring refactor is **behaviour-neutral** — all `pico-link-85v`
acceptance counters unchanged at 990 kbps, audio unchanged by ear. This is the
one-variable-at-a-time bead; without it a regression in G3 is unattributable.

**G3 — move the fill loop to core1.** Depends on G0, G1, G2.
Behind a compile-time `PL_ENCODER_ON_CORE1` so a bad build is one flag from
reverting. Port `pl_a2dp_fill` verbatim per §4.1; core1 loop + state machine +
quiesce handshake; `s_enc_heartbeat` + `PL_WDT_ENCODER`; media timer body
shrinks to the send kick; enforce §4.3's seven invariants.
*Proves:* §9's numbers. This is the bead the epic exists for.

**G4 — level snapshot.** Depends on G3.
Seqlock publish from core1, read + `LevelsChanged` synthesis in core0's
superloop; delete `pl_a2dp_maybe_push_levels` and its counters.
*Proves:* the meter is steady on hardware for 5 minutes — the original
`pico-link-8b7` symptom, closed at its root.

**G5 — core1 observability.** Depends on G1 (not G3); can run alongside G4.
`get_core_num()` into the panic record; a deliberately-faulted core1 in a debug
build produces a readable record and a `PL_WDT_ENCODER` trip.
*Proves:* a dead core1 is diagnosable rather than silent.

**G6 — doorbell send kick. OPTIONAL, gated.** Only if G3 measures
`stop_queue_full` > 0 or packet-interval jitter that the media timer cannot
cover. Do not build it speculatively.

## 9. Success, as numbers — SUPERSEDED BY §11.2

Same instruments, same build shape as `bd-pico-link-8b7-gate` (gate line, the
1.77 ms register-only steal probe), streaming **LDAC HQ at 990 kbps** to a real
headset, verified on the board via `ldacBT_get_bitrate` — not assumed.

**The table below was written before G0 and is NOT REACHABLE from the encoder
offload alone. It is kept only as the record of what I predicted. The binding
numbers are in §11.2.**

| Metric | idle (baseline) | today at 990 kbps | ~~target~~ | ~~fail~~ |
|---|---|---|---|---|
| superloop iters/s | 61-64 | 3 | ~~>= 50~~ | ~~< 40~~ |
| `pl_ui_render` | 21.5-21.9 ms | 322-385 ms | ~~<= 30 ms~~ | ~~> 40 ms~~ |
| 1.77 ms steal probe | 1841 us | 23-34 ms | ~~<= 2.6 ms~~ | ~~> 3.7 ms~~ |

The target band is deliberately not "back to idle": core 0 still carries
BTstack, the USB pump and the send path, and the probes themselves cost real
time. **>= 50 iters/s and <= 30 ms render is a device that feels immediate**
(press-to-pixels ~20 ms poll + ~30 ms paint = ~50 ms, against today's 350-700 ms).

Audio must not regress, and these are pass/fail:

- `stop_dwell` == 0 (it is now wired to nothing; nonzero means the port kept a
  bound it should not have)
- `underrun_events` == 0, `pl_pcm_overrun_frames` == 0, `pkt_fail` == 0
- `stop_queue_full` == 0, `tx_depth_max` <= 3 of 8
- encoded frames/s ~= 375; `grants/s` ~= `pkt_sent/s` ~= 140
- `s_enc_heartbeat` advances every window; `PL_WDT_ENCODER` never trips
- **Andreas's ear over a 5-minute continuous stream: no crackle, no dropout.**
  The bench has been wrong about this before and the ear has not.

And the reason this matters beyond the meter: at 3 iters/s the d-pad is polled
three times a second. **The MVP is "drive the whole device from the buttons
while it streams."** Right now, while it streams, it is barely drivable.

## 10. What this deliberately does not do

- Does not touch `core/`, `ui-ffi`, or the FFI. No Rust moves. No allocator
  change. `pico-link-a67`'s surface is untouched.
- Does not lower the codec quality. 330 kbps was an experiment, not a plan.
- Does not build damage-rect rendering. Still wanted, now second.
- Does not simplify the credit clock to pure backpressure. Tempting, and
  exactly the change that would make a regression unattributable.
- Does not move BTstack, USB, `persist`, or the display off core 0.

---

# 11. RE-SCOPE RULING (2026-09-03, after G0)

This section is the decision of record. Where it disagrees with §§8-9, it wins.

## 11.0 What G0 returned

Tex, `pico-link-nli.1`, n=126 consecutive streaming windows across three signal
types, bitrate read from `ldacBT_get_bitrate` **on the board** every window,
display blanked so render was not running.

| item | share of core 0 | moves to core1? |
|---|---|---|
| `ldacBT_encode` | **56.2%** | **yes** |
| pcm ring read + level accum + seal | 3.6% | **yes** |
| L2CAP/BTstack/cyw43 send path (the grant handler) | **22.1%** | no |
| USB 0xC0 pump + cyw43 bg/RX + BTstack run loop (**by subtraction**) | ~11% | no |
| leftover thread context | ~1% | — |

**59.8% moves. ~33.8% stays.**

My G0 hypothesis offered two explanations for the unclosed arithmetic and asked
which. The answer is **both, simultaneously**, which is exactly why the sum did
not close:

- (a) In-situ encode is **1495-1517 us** against the L0 bench's 1069-1159 us —
  29-42% dearer. The bench predicted 41% duty for something that is 56%. This is
  the standing "fixed-input benchmarks measure a fixed point" lesson landing on
  my own arithmetic. **Do not size a duty cycle from a bench again; measure it
  in situ.**
- (b) There *is* a large transport cost that does not move: 22.1% in the grant
  handler alone.

Signal content is falsified as a factor (pink noise 56.50 / music-like 56.24 /
pure sine 55.51 — 1.0 point across the whole spectral range a transform codec
can see). Nesting was proved, not assumed (`mt% + send% = 105.2%`; two disjoint
busy intervals on one core cannot exceed wall clock). Probe overhead was
separated **structurally** — `enc_us/wall` is measured busy-microseconds over
elapsed time, and the encoder runs at 0xFF so it preempts the probes rather than
competing with them.

## 11.1 Ruling: the epic PROCEEDS AS DESIGNED, with revised numbers

56.2% is comfortably inside my own 40-70% "build it, but restate the success
numbers" band, and near its top. **Nothing in §§1-7 changes.** The allocation
(§2), the two rings (§3), core1's loop and invariants (§4), the level seqlock
(§5), the narrowed flash lockout (§6) and the failure modes (§7) were all
designed against "the encoder is the tenant", not against a specific percentage.
The children `nli.2` (G1), `nli.3` (G2), `nli.4` (G3), `nli.5` (G4), `nli.6`
(G5) are unchanged in content and stay as written. G6 stays gated on a
measurement.

**What the epic buys, stated honestly:** thread availability on core 0 goes from
**6.1% to ~66%** — an ~11x improvement. That is the difference between a device
that polls the d-pad 3 times a second and one that polls it ~40 times a second.
It does not get us back to idle and was never going to.

**Why it is still worth building even though a third of the steal stays:** there
is no other lever that returns 60% of a core. The remaining 34% is spread across
four tenants that must be on core 0 for structural reasons (BTstack owns the
send path; TinyUSB owns the ISO endpoint; both are single-context). Attacking
the 22% send path is a *fork of a vendored driver*; the offload is our own code
moving to a core we own. Cheapest sustainable win first.

## 11.2 Revised success numbers — REPLACES §9's table

Two bars, because they answer different questions and conflating them is what
produced the unreachable §9.

### 11.2a — G3 acceptance (what the offload alone must deliver)

Derived from the measured ~33.8% residual steal, i.e. ~66% thread availability.
Same instruments and build shape as `bd-pico-link-nli.1`, LDAC HQ at 990 kbps
verified on the board.

| Metric | idle | today | projection | **target** | **fail** |
|---|---|---|---|---|---|
| superloop iters/s | 61-64 | 3 | ~41 | **>= 35** | **< 25** |
| `pl_ui_render` | 21.5-21.9 ms | 322-385 ms | ~33 ms | **<= 40 ms** | **> 55 ms** |
| 1.77 ms steal probe | 1841 us | 23-34 ms | ~2.8 ms | **<= 3.2 ms (<= 1.75x)** | **> 4.6 ms (> 2.5x)** |
| measured non-thread share | — | 93.9% | ~34% | **<= 40%** | **> 48%** |

The projections are Tex's arithmetic, not measurements, and are recorded so a
result that lands far off them is itself a finding. **A measured non-thread share
materially above 40% means the ~11% subtraction tail was wrong** — that, and
only that, is when §11.5 reopens.

The audio no-regression list in §9 is **unchanged and still pass/fail in full**,
including `stop_dwell == 0`, `underrun_events == 0`, `stop_queue_full == 0`,
`tx_depth_max <= 3 of 8`, and Andreas's ear over a continuous 5-minute stream.

### 11.2b — Epic / product exit (what "drivable while streaming" requires)

`>= 50 iters/s` and `<= 30 ms` render — the original §9 targets — are **retained
as the product bar and explicitly moved off G3.** They are reachable only with a
second lever (§11.3 or §11.4), and G3 is not permitted to be judged against
them.

The bar exists because the MVP is *drive the whole device from the buttons while
it streams*. Press-to-pixels is poll period + paint:

- today: ~330 ms poll + ~350 ms paint = **~680 ms** (unusable)
- after G3 alone (projected): ~24 ms + ~33 ms = **~57 ms** (usable)
- after G3 + one more lever: ~12 ms + ~10 ms = **~22 ms** (immediate)

**G3 alone crosses from unusable to usable.** That is why it ships on 11.2a.

## 11.3 Ruling on the send path: its own bead, sequenced after G3, measure before touching

22.1% of core 0 — **~1190 us of CPU per media packet at 186-192 packets/s, to
push a ~679-byte payload.** After G3 lands this is *two-thirds of all remaining
steal on core 0* and the largest single item in the system. It is worth
attacking.

**Not part of this epic.** Two reasons, and the first is the one that matters:
this epic's entire discipline is one variable at a time (§8, G2's whole purpose),
and folding a cyw43-transport change into the core1 move makes any regression
unattributable between them. Second, it is a different subsystem, a different
failure mode, and — see below — probably a *vendored-driver fork*, which is a
sustainability decision Andreas should make deliberately rather than inherit.

**It is a new P1 bead, related to this epic, sequenced immediately after G3.**

### What must be measured (do not act on the suspicion below without this)

1. **Split the 1190 us into phases**, timestamped in the same busy-microseconds
   style G0 used: (i) BTstack framing — `a2dp_source_stream_send_media_payload_rtp`
   through `l2cap`/`hci` down to `hci_transport_cyw43_send_packet`, including the
   payload memcpy; (ii) `cybt_bus_request()`, which is a **polling loop**
   (`cybt_wait_bt_awake`, retry count 300); (iii) `cybt_get_bt_buf_index()`;
   (iv) `cybt_mem_write()`, the bulk payload write; (v) `cybt_reg_write_idx()` +
   `cybt_toggle_bt_intr()`.
2. **Count bus transactions per media packet** — specifically the number of
   `cyw43_set_backplane_window` calls and `cyw43_write_bytes` calls. This is the
   number that decides whether the cost is per-transaction or per-byte.
3. **Measure the PIO SPI clock actually in use** at 150 MHz sysclk, and the
   measured wire time of one 64-byte backplane write. Everything else is
   overhead by subtraction.

### The suspicion, stated as code references so it can be checked or killed

Not a guess about "SPI being slow" — a specific structural cost read out of the
SDK sources:

- `CYW43_BUS_MAX_BLOCK_SIZE` is **64** on the SPI bus
  (`lib/cyw43-driver/src/cyw43_ll.h:187`; it is 16384 on SDIO).
- `cybt_mem_write()`
  (`src/rp2_common/pico_cyw43_driver/cybt_shared_bus/cybt_shared_bus_driver.c:590-603`)
  therefore chops a ~683-byte HCI ACL write into **~11 chunks**.
- Each chunk goes through `cyw43_ll_write_backplane_mem()`
  (`lib/cyw43-driver/src/cyw43_ll.c`), which calls
  `cyw43_set_backplane_window(addr)` before the write **and
  `cyw43_set_backplane_window(CHIPCOMMON_BASE_ADDRESS)` after it** — the window
  is set up and torn down *per 64-byte chunk*, and each window change is itself
  up to three register writes over the same bus.
- Plus, once per packet, the `cybt_wait_bt_awake` poll and
  `cybt_toggle_bt_intr()`'s read-modify-write of `HOST_CTRL_REG`.

Order-of-magnitude sanity check, which is what makes this worth a flash: 683
bytes on the wire is roughly one-seventh of 1190 us at plausible PIO SPI clocks.
**Most of the 1190 us is transaction setup, not payload.** ~1190 us / ~11 chunks
is ~108 us per 64 bytes.

**Do not raise the AVDTP payload size as the fix.** I checked: the media payload
size is `btstack_min(a2dp_max_media_payload_size(...), 1029)`
(`firmware/src/a2dp.c:1837-1838`) — it is bounded by the *peer's* negotiated
L2CAP MTU, not by us. 679 bytes is the headset's number. Fewer, larger packets
is not a lever we hold.

### The sustainability flag on this one

If the measurement confirms the chunked-window hypothesis, the fix lives in
**pico-sdk's vendored cyw43 driver, not in our code.** That is a fork, and forks
of a driver we do not control are exactly the kind of load-bearing debt this
project should take deliberately. Preference order, and it is a real ordering,
not a hedge:

1. An **upstreamable** change (hoist the window restore out of the chunk loop;
   keep the window across chunks of one contiguous write) — file it upstream,
   carry the patch meanwhile.
2. A **contained local override** with a one-file patch and a comment naming the
   upstream issue.
3. Anything that special-cases the A2DP path inside the driver — **reject**.

## 11.4 Ruling on damage-rect (`pico-link-7h5`): stays deprioritised, becomes the designated follow-on

**It does not come back now.** Andreas deprioritised it today, and the argument
that would override him — "this is the difference between the UI working and not
working" — **is not true after G3.** G3 alone takes press-to-pixels from ~680 ms
to ~57 ms. That is a usable device. Damage-rect takes it to ~22 ms. That is a
nicer device. His call stands.

**But it is the designated next lever if §11.2b is missed**, and it should be
preferred over the send path if only one gets built. The reasoning is a
cost-of-change argument, not a performance one:

- Damage-rect is **pure Rust in `core/`**, host-testable in the emulator, no
  hardware in the loop, no vendored fork, and it is reversible.
- The send path is a **fork of a third-party driver** with a hardware-only test
  loop.
- They are comparable in effect on the superloop, because the superloop's
  per-iteration cost is render-dominated: cutting render ~33 ms -> ~10 ms lifts
  iters/s and render simultaneously, which is both of §11.2b's numbers.

So: **re-evaluate `pico-link-7h5` when G3 reports measured numbers, with
damage-rect as the default choice.** Do not schedule it before then.

## 11.5 Ruling on the ~11% unattributed tail: NOT worth a flash

No. Three reasons:

1. **No decision depends on it.** Every candidate in it — the USB 0xC0 ISO-OUT
   pump, cyw43 background/RX, the BTstack run loop — stays on core 0 regardless
   of the split. Knowing the proportions changes nothing in this epic.
2. **It is bounded and small.** At most ~11%, against a 22.1% item that is
   already localised and actionable. The flash budget belongs to §11.3.
3. It is the residual of an arithmetic that closed well elsewhere
   (`fill - enc - send = 3.60%`, matching the expected ring-read + level-accum +
   seal cost almost exactly). That agreement is decent evidence the subtraction
   is sound.

**Reopen condition, and it is the only one:** if G3's measured non-thread share
exceeds ~40% (§11.2a's target), the subtraction was wrong somewhere and
splitting the tail becomes diagnostic rather than curiosity. Then, and only
then, spend the flash.

## 11.6 What I got wrong, recorded so it is not repeated

I sized the encoder's duty cycle from the L0 microbenchmark and predicted 41%.
It is 56%. The bench was not wrong about what it measured; I was wrong to treat
a per-call mean from a fixed-input harness as an aggregate duty cycle in a
system with a different cache, interrupt and memory profile. **The G0 gate that
caught this was the most valuable thing in the original design** — the epic
would otherwise have been built against targets it could not hit, and the miss
would have read as a failed implementation rather than a mis-set bar. Keep
writing the risk gate first.
