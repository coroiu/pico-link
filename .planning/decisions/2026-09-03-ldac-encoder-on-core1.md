# Move the LDAC encoder to core1

- **Date:** 2026-09-03
- **Bead:** `pico-link-8b7` (the measurement), epic to be filed from §8 below
- **Status:** **Accepted — architecture delivered, performance acceptance MISSED
  (2026-09-03, after G3).** The architecture in §§1-7 stands unchanged and is
  built, stable and proven over a 44-minute soak. §9's success numbers were
  wrong and were replaced by §11.2a; §11.2a's numbers were **not met** and are
  **not re-scoped**. **Read §12 first — it is the current ruling, and it
  supersedes §11 where they disagree.** §11 remains the record of the post-G0
  re-scope; §9's table and §8's G0 entry are superseded by it. **Amends**
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
| **Lockout START times out** (core1 never entered the handler) | `multicore_lockout_start_timeout_us` returns false. | Count it, skip the write, keep the RAM-staged value. Recoverable: no SDK state is latched (`lockout_in_progress` stays false), core1 is untouched. Never hang. |
| **Lockout END times out** (core1 entered the handler and then died in it) | `multicore_lockout_end_timeout_us` returns false. | **FATAL. Panic into the recorder with a distinct reason, then reboot.** See §7.1 — this is not a failed save, it is a corrupted machine. |
| **Flash write while core1 runs from XIP** | Structurally prevented by the lockout. | — |
| **Lost update on `tx_count`** | Structurally prevented by deleting the field (§3.2). | — |
| **Core1 races a stream reset on core0** | Prevented by the quiesce handshake (§4.2); a timeout there is counted and logged by core0. | — |
| **Encoder outruns the sender** | `stop_queue_full` > 0 and `tx_depth_max` == SLOTS-1. Already instrumented. | Tune depth / add the doorbell (§5). |

### 7.1 Ruling: START and END timeouts are different failures (2026-09-03)

Raised by code review on `pico-link-nli.2`; verified against pico-sdk 2.1.1
`pico_multicore/multicore.c`. The row above originally conflated the two halves
of the handshake. They are not the same failure and must not share a response.

**START timeout is benign and recoverable.** `multicore_lockout_start_block_until`
sets `lockout_in_progress = rc`, so a failure latches nothing; core0 never
disabled interrupts (our helper returns before `save_and_disable_interrupts`);
core1 never entered the handler and is unaffected. The write is skipped, the
staged value survives in RAM, and the next attempt is clean. Count and continue.

**END timeout is fatal and unrecoverable.** `multicore_lockout_end_block_until`
only clears `lockout_in_progress` when the handshake succeeds, and there is no
public API to reset it. Two consequences, both permanent:

1. The next `multicore_lockout_start_*` anywhere in the firmware hits
   `hard_assert(!lockout_in_progress)` and panics — in release builds too.
   That includes BTstack's own link-key writes, which share the same funnel.
2. Core1 is still spinning in `multicore_lockout_handler` with interrupts
   disabled, inside an ISR, waiting for a `LOCKOUT_MAGIC_END` that will never
   arrive. The encoder core is dead.

Crucially, a successful START **proves core1 was alive and in the handler**. For
END to then time out, core1 must have faulted or lockedup while parked in a
four-instruction RAM loop. There is no benign reading of that event. Limping on
buys nothing: audio is already dead, and the *next* flash write is a guaranteed
`hard_assert` at an arbitrary later moment with no attribution.

**Response: panic into the recorder (`pico-link-gap`) with a distinct reason
code, then reboot.** Rationale specific to this project: a wedged board costs a
physical BOOTSEL hold (the press-free CDC path needs a live main loop), so a
controlled restart is strictly cheaper than a hang. Do **not** reboot silently —
the panic record is the only diagnostic that will ever exist for this event, and
it must name core1 as the suspect. Do **not** retry the END handshake: a second
`LOCKOUT_MAGIC_END` push can be left unconsumed in the FIFO and poison the next
START, trading a diagnosable fatal for an undiagnosable intermittent.

Data safety: `exit_safe_zone` runs *after* the flash mutation completed, so the
write itself has landed. A multi-operation TLV store can still be truncated at
an entry boundary; BTstack's log-structured flash bank loses that one entry, not
the bank. Accepted.

**Timeout budget — a second defect the same review exposes.** The implementation
used 5 s per phase. The hardware watchdog is 2000 ms (`watchdog_sup.c:59`) and
`PL_WDT_USB_TASK`'s deadline is 250 ms; flash writes are performed from the
superloop. A 5 s handshake wait therefore cannot ever be observed — the watchdog
reboots first, uncontrolled and unattributed. **Both phases must use a budget
well under 250 ms; 20 ms is the recommendation** (a FIFO round-trip to a core
spinning in RAM is microseconds, so 20 ms is already ~1000x headroom).

**No pico-sdk fork.** The gap is real — `lockout_in_progress` is latched with no
reset API and `hard_assert` makes it terminal — but we do not need SDK surgery
to be correct: because we own `get_flash_safety_helper()`, treating END failure
as terminal means we never make the second call that would trip the assert. That
is containment at our own seam, not a special case buried in a vendored tree.
File a short upstream issue against `raspberrypi/pico-sdk` for the record; do not
diverge the SDK. (Contrast the TinyUSB ISO backport, where the defect was in the
vendored code path itself and there was no seam of ours to fix it from.)

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

---

# 12. POST-G3 RULING (2026-09-03, after Tess's acceptance measurement)

This section is the decision of record from here. Where it disagrees with §11,
it wins. §11's *architecture* rulings (11.1, 11.3's "measure first", 11.4) all
stand; §11.2a's numbers are **recorded as MISSED and are NOT re-scoped**.

## 12.0 What G3 returned

Tess, `pico-link-nli`, LDAC HQ 990 kbps verified on the board, active-paint
windows only, n=17 over 25 s. Stability: **zero reboots, zero panic records,
zero re-enumerations in a 2642 s (~44 min) monitored soak** — more than 2x
Tex's record — and G1 pairing-persistence confirmed across a real BOOTSEL
reboot, with the panic recorder proven live (`boot_seq=1`, no magic) on two
separate reflashes.

| Metric | idle | pre-epic | **G3 measured** | projection | target | fail |
|---|---|---|---|---|---|---|
| superloop iters/s | 61-64 | 3 | **20.5** | ~41 | >= 35 | < 25 |
| `pl_ui_render` | 21.9 ms | 322-385 ms | **133-141 ms** | ~33 ms | <= 40 ms | > 55 ms |
| steal factor | 1.0x | 16.3x | **~4.25x** | 1.75x | <= 1.75x | > 2.5x |
| non-thread share | — | 93.9% | **not measurable** (proxy ~76%) | ~34% | <= 40% | > 48% |

Two of the three measurable metrics fail **below the fail line**. This is a real
6.8x improvement on iters/s and 2.5x on render, and it is nowhere near its bar.

## 12.1 Where the projection went wrong: the model, not the subtraction

**The §11.5 reopen condition fired, and the inference attached to it is
refuted.** I wrote that a measured non-thread share above ~40% would mean the
~11% subtraction tail was wrong. It does not, and the tail is not where this
hid. G0's decomposition closes:

    56.2 (encode) + 22.1 (send) + 3.6 (ring/level/seal) + ~11 (tail) = 92.9%
    measured pre-epic non-thread share, both instruments             = 93.8%

0.9 points apart. There is no room in that sum for a 45-point error. **§11.5
stays closed; do not spend a flash splitting the tail.** The reopen condition
was a correctly-chosen *trigger* pointed at the wrong *suspect* — recorded in
§12.6.

What was actually wrong is the model underneath the whole projection:

> **I assumed core-0 time freed equals core-0 throughput gained. That
> assumption requires the two cores to be independent. They are not.**

The projection converted "59.8% of core 0's *busy time* moves to core1" into
"core 0's thread context gets ~66% of a *nominal-speed* core". The second half
does not follow. RP2350's two cores share one 16 KB XIP cache (confirmed:
`XIP_SRAM_BASE 0x13ffc000 .. XIP_END 0x14000000`), one QSPI flash interface,
the SRAM bank arbiter and the APB peripheral bridge. Moving a flash-resident,
cache-hostile workload off core 0 stops it *consuming* core-0 cycles but does
not stop it *degrading* them.

### The evidence that this is stall, not hidden preemption

**(a) The two instruments have come apart, in the direction contention
predicts.** Pre-epic, the register-only steal probe and `pl_ui_render` — two
completely different workloads — agreed on the factor to within 1% (16.3x vs
16.2x). That is the signature of pure time-slicing: preemption is
instrument-independent. Post-G3 they disagree by 43% (render 133/21.9 = **6.1x**;
probe 11.9/2.8 = **4.25x**), and the heavier, more flash- and memory-resident
instrument is the one that suffers more. Preemption cannot produce that.
Cache/fetch stalls produce exactly that.

**(b) libldac's hot working set is 1.8x the entire shared cache.** From
`build_main/pico_link.elf.map`:

    ldacBT.c.o   .text 0x10dc (4316)   .rodata 0x178  (376)
    ldaclib.c.o  .text 0x2fa4 (12196)  .rodata 0x2d90 (11664)
                 -------------------------------------------
                 text 16.1 KB + rodata 11.8 KB = ~28.4 KB, all XIP-resident

The XIP cache is 16 KB. At HQ the encoder runs ~375 times a second
(56.2% of a core at 1495-1517 us in situ). So ~28.4 KB is streamed through a
16 KB cache **375 times a second**, from core 1, continuously. Whatever core 0
had resident is evicted on essentially every frame. Core 0 then re-fetches
every line it touches over QSPI. Pre-epic this cost was hidden inside "the
encoder is 56.2% of core 0"; post-G3 it is exposed as core 0 running at a
fraction of its nominal IPC while looking idle.

**(c) Core 1 never sleeps.** `a2dp.c:1690-1711`: the non-RUNNING branch is
`s_enc_quiesced = true; s_enc_heartbeat++; continue;` — a bare spin — and the
RUNNING branch calls `time_us_64()` **twice per iteration** (an APB peripheral
read) plus `pl_a2dp_fill()`, with no pacing and no `__wfe`. Between encodes
(~44% of wall time at HQ) core 1 issues a maximal-rate stream of APB reads and
SRAM ring reads that core 0 must arbitrate against, and when no stream is
running at all core 1 still spins flat-out from boot. This is a defect in its
own right — it costs power and bus bandwidth for nothing — independent of
whether it is the dominant term here.

**Arithmetic of the residual, under this model.** If core 0's true busy stays
at the predicted 33.1%, thread context gets 66.9% of core 0's *cycles*; render
at 133 ms then implies core 0 is executing at 21.9 / (133 x 0.669) = **~25% of
its uncontended rate**. A ~4x stall factor is large, and is what a 16 KB cache
being swept 375 times a second by a 28 KB working set looks like.

**This is a hypothesis with a decisive test, not a conclusion.** See §12.5.

## 12.2 Is the send path the whole story? No, and it cannot be.

`pico-link-lyv` is real — 22.1% of core 0, ~1190 us of CPU per media packet at
~190 packets/s — and §11.3's ruling to measure before touching stands. But it
cannot close this gap, under either reading of the data:

- **Under the (now falsified) pure-time-slice model**: removing it takes core-0
  busy from 33.1% to ~11%, render to ~24.6 ms, iters to ~55/s. That model
  already failed to predict the current measurement, so this number is not
  credible.
- **Under the measurement we actually have**: removing 22.1 points from a
  measured ~76% non-thread share leaves ~54%, i.e. render ~48 ms and iters
  ~28/s. **Still fails both 11.2a targets, and is a whisker off the fail
  lines.**

So: worth doing, and **not sufficient**, and — see §12.4 — no longer the next
lever. It also remains the most expensive option on the board (a fork of a
vendored driver with a hardware-only test loop) for a term that is at most a
third of the remaining problem.

## 12.3 Ruling on the epic: it closes on architecture and stability. The numbers stay missed.

**The epic does NOT get re-scoped a second time.** §11.2a's targets stay on the
record as **MISSED**. Moving a bar after measuring against it, twice, converts
the bar into a description of whatever happened, and Andreas is right that the
justification bar for doing it again is higher than the first time. It is not
met and I am not going to meet it by editing the table.

**Ruling:** `pico-link-nli` is **delivered on its architecture and its
stability, and failed on its performance acceptance.** It stays open for
exactly two things and nothing else:

1. `pico-link-nli.6` (G5, core1 panic observability) — the last open child.
2. The two human checks only Andreas can do: the 5-minute ear test, and
   eyeballing the VU meter on the panel.

When those land, **close it, with the 11.2a result recorded as missed.**

**The performance gap does not keep this epic open.** The gap is caused by a
mechanism the epic did not create, is not scoped to fix, and cannot fix inside
its own one-variable-at-a-time discipline. Keeping the epic open around it turns
a finished, stable, 6.8x improvement into an open-ended container for a
different problem — which is how a clean epic rots into a tracking bug. It
moves to its own bead (§12.4).

What the epic actually bought, stated plainly and without inflation: press-to-
pixels goes from ~680 ms (unusable) to ~195-210 ms measured (poll ~49 ms +
render ~133 ms + blit ~13-27 ms). That is not the ~57 ms projected and it is not
"drivable". It is "usable under protest". The device is better and the job is
not done.

## 12.4 The next lever is the contention, not the send path — and it is the cheapest thing on the board

**New P1 bead (recommended title): "Diagnose and fix cross-core memory
contention: libldac's 28 KB working set thrashes the shared 16 KB XIP cache".**
Sequenced **before** `pico-link-lyv`, and before `pico-link-7h5` (damage-rect).

That reorders §11.3 and §11.4, and the reason is cost-of-change, the same
criterion §11.4 used:

| lever | change | cost | reversible | predicted render |
|---|---|---|---|---|
| **F1 libldac to SRAM** | ~28.4 KB of 520 KB SRAM, one linker fragment | build-system only, no fork | build flag | **~33 ms** (if H1 holds) |
| **F2 park core1** | `__wfe` + `__sev` doorbell instead of a spin | ~20 lines in `a2dp.c` | trivially | improves F1's floor |
| F3 `lyv` send path | fork of vendored cyw43 driver | high, hardware-only test loop | poorly | ~48 ms |
| F4 `7h5` damage-rect | Rust in `core/`, host-testable | medium | yes | reduces the 21.9 ms base |

**F1 is the falsifiable one.** If the contention hypothesis holds, moving
libldac's `.text` and `.rodata` into SRAM restores core 0's IPC to near nominal
and render lands at 21.9 / 0.669 = **~33 ms with ~41 iters/s — precisely the
§11.2a projection.** That is the claim to test: the projection's *arithmetic*
was right and its *memory model* was missing one term. If F1 lands and the
numbers still sit at ~130 ms, H1 is dead and §12.5's core-0 busy measurement is
the arbiter.

F1 also speeds up the encoder itself — core 1's 1495-1517 us in-situ encode
against the bench's 1069-1159 us is *itself* partly XIP-miss cost — which
returns headroom on core 1 as a side effect.

**Sustainability note on F1.** Do it as a supplementary linker fragment
(`INSERT AFTER`-style placement of `*libpl_ldac_enc.a:(.text* .rodata*)` into
the RAM-resident section), **not** by forking `memmap_default.ld`, and **not**
by decorating vendored sources with `__not_in_flash_func`. The first keeps
pico-sdk's linker script upgradable; the other two are exactly the kind of
load-bearing, hard-to-undo edit this ADR exists to prevent. Gate it behind a
CMake option so the A/B is one flag. If placement by archive name proves
awkward, the acceptable fallback is a dedicated `.ldac_ram` section attribute
applied at the *build-system* level (`-ffunction-sections` plus placement), not
edits inside `vendor/`.

**F2 (park core1) should land regardless of what F1 measures.** A core that
free-spins from boot, calling `time_us_64()` twice per iteration forever, is a
defect on power and bus grounds alone, and it is 20 lines. It should have been
in §4.1 of this ADR and was not — my omission.

`pico-link-lyv` and `pico-link-7h5` both stay open and both stay P1. Re-evaluate
their ordering after F1 reports.

## 12.5 Fixing the instrument: measure the specified quantity, and make it arbitrate

The 4th metric is unmeasurable today because `a2dp.c:1393`'s `duty:`/`enc_us`
accumulator sits outside the `#ifndef PL_ENCODER_ON_CORE1` guard, so under the
flag one name reports two different quantities: core 0's non-thread share when
OFF, core 1's own busy percent when ON. **A metric whose meaning changes with a
build flag is not a metric.** Fix it as follows.

**1. Split the accumulator into two names that can never be confused.**

- `s_core1_busy_us` — core 1's own encode busy time. What the current
  accumulator reports under the flag. Keep it; label it `core1_duty:`.
- `s_core0_nonthread_us` — **new, core 0 only.** Accumulate `timer_hw->timerawl`
  deltas across entry/exit of *every* core-0 exception handler that matters:
  the 0xFF BTstack/cyw43 HCI path (including the grant handler already
  instrumented for G0), the 0xC0 USB pump, and the alarm/run-loop timer.
  Report `s_core0_nonthread_us / wall_us` as `core0_nonthread:`.

This is the same busy-microseconds technique G0 already used and validated —
applied on core 0, summed over all handlers, rather than on one handler.

**2. Report both, always, in the same line, on both sides of the flag.** The
whole defect was a single label doing double duty; the fix is not a better
`#ifdef`, it is two unambiguous labels.

**3. Add the arbiter: `XIP_CTR_HIT` / `XIP_CTR_ACC`.** RP2350's XIP block has
free hardware hit and access counters
(`hardware/regs/xip.h`: `XIP_CTR_HIT_OFFSET 0x0c`, `XIP_CTR_ACC` adjacent).
Sample and reset them over each reporting window and print
`xip_acc`/`xip_hit`/miss-rate. Zero CPU cost, no cores involved, and it settles
§12.1 outright.

**4. The decision table this produces.** One flash answers everything:

| `core0_nonthread` | XIP miss rate vs core1-halted | verdict |
|---|---|---|
| **~33-40%** | **much higher** | H1 confirmed: stall, not preemption. Do F1. |
| ~33-40% | unchanged | contention is real but not XIP — look at SRAM banks / APB. Do F2 first. |
| **~76%** | either | H1 dead. G0's decomposition has a 45-point hole; re-derive it before touching anything else. |

**5. The clean control, worth one flash on its own:** build with core 1 launched
but the encoder left on core 0 (core 1 running only its idle spin), no audio,
and measure iters/s and `pl_ui_render` against the 61-64 / 21.9 ms idle
baseline. Any degradation at all is pure contention with **zero** audio work in
the picture, and it quantifies F2's ceiling before F2 is written.

## 12.6 What I got wrong this round, recorded so it is not repeated

1. **I sized a multicore offload with single-core arithmetic.** Every number in
   §11.2a follows from "time freed = throughput gained", which silently assumes
   core independence. On a chip with one shared 16 KB cache and one flash
   interface, that assumption is the whole ballgame, and I never wrote it down —
   which means I never checked it. **When moving a workload between cores,
   state the shared-resource assumption explicitly and size the working set
   against the shared cache before projecting anything.** libldac's 28.4 KB
   against a 16 KB cache was computable from the map file at design time, at
   zero cost, and I did not compute it.
2. **My reopen condition named the wrong suspect.** §11.5's trigger ("non-thread
   share materially above 40%") fired correctly and its attached inference ("the
   ~11% tail was wrong") is refuted by G0's own arithmetic closing to within
   0.9 points. A reopen condition should name the *trigger* and demand a fresh
   diagnosis — it should not pre-commit to a cause, because pre-committing sends
   the next round to spend a flash on the one place the answer provably is not.
3. **§4.1's "move the code, do not redesign it" was right, and I should still
   have specified core 1's idle behaviour.** "Do not redesign" is not "do not
   specify". A busy-spin fell out of the port by default, and defaults in a
   design document are decisions whether or not anyone made them.
4. This is the second time on this project that a fixed-input bench under-
   predicted an in-situ cost (§11.0(a)), and it is now clear both instances have
   the same root: **the bench had the cache to itself.** The lesson is stronger
   than "measure in situ" — it is *benches do not model the memory system you
   will actually run in*.
