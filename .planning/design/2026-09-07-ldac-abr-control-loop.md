# LDAC adaptive bitrate: the control loop

**Bead:** `pico-link-7jol.1` (design), parent epic `pico-link-7jol`.
**Status:** design of record. Implementation is `pico-link-7jol.3`. UX is `pico-link-7jol.2` (Uma).
**Author:** Ada, 2026-09-07.
**Read first:** `.planning/design/2026-08-30-pcm-pacing.md` (credit pacing),
`.planning/design/2026-08-30-ldac.md` (the codec row),
`.planning/decisions/2026-09-03-ldac-encoder-on-core1.md` (the core1 seam),
bead `pico-link-fhf` (the hysteresis-banded resync trim this controller is shaped after).

---

## 0. Facts established by reading the vendored source this session

These are new relative to the epic's banked header facts. **Do not re-derive.**

### 0.1 The ladder is five rungs, not three

`ldacBT_alter_eqmid_priority` does **not** walk the public three-mode enum. It walks
`tbl_ldacbt_eqmid_property` (`firmware/vendor/libldac/src/ldacBT_internal.c:29-44`), a
13-entry table, and the descent is bounded by
`LDACBT_LIMIT_ALTER_EQMID_PRIORITY == LDACBT_EQMID_MQ`
(`ldacBT_internal.h:34`, enforced at `ldacBT_internal.c:474-479`). So the reachable ladder is:

| rung | EQMID | kbps @48k | bytes/frame | +hdr | frames/packet @MTU 679 | packets/s |
|---|---|---|---|---|---|---|
| 0 | `LDACBT_EQMID_HQ` | 990 | 330 | 333 | 2 | 187.5 |
| 1 | `LDACBT_EQMID_SQ` | 660 | 220 | 223 | 3 | 125 |
| 2 | `LDACBT_EQMID_Q0` | 492 | 164 | 167 | 4 | 93.75 |
| 3 | `LDACBT_EQMID_Q1` | 396 | 132 | 135 | 5 | 75 |
| 4 | `LDACBT_EQMID_MQ` | 330 | 110 | 113 | 6 | 62.5 |

(bytes/frame = bps/3000 at 128 samples/frame, 48 kHz; frames/packet =
`tx_size / frmlen_tx`, `ldacBT_internal.c:361`, floor, min 2 enforced at :365-371.)

Rungs 2 and 3 are **not nameable** — `LDACBT_EQMID_Q0`/`Q1` live only in the internal
header, and `ldacBT.h` exposes just HQ/SQ/MQ. That is fine: we never name a rung. We
step `±1` and keep our own rung *counter*, and use `ldacBT_get_bitrate()` as the
ground truth for what actually landed.

The finer ladder is free and strictly better than a three-step one: 990 -> 660 is a 33%
byte-rate cut in one move, which is a large audible quality jump. Having 492 and 396
between SQ and MQ lets the controller find a working point instead of overshooting.

### 0.2 `ldacBT_alter_eqmid_priority` hard-requires `pkt_type == _2_DH5`

`ldacBT_internal.c:455-460` returns `LDACBT_E_FAIL` for any other packet type.
`ldacBT_api.c:168` sets `pkt_type = _2_DH5` unconditionally in
`ldacBT_init_handle_encode`. So this is satisfied by construction for us — but it is a
silent total-failure mode if that ever changes, so the implementation must treat a
persistent alter failure as "controller disabled", logged once at thread context, not
as a per-tick no-op.

### 0.3 The change is applied by libldac at a packet boundary, internally

`ldacBT_set_eqmid_core` (`ldacBT_internal.c:206-217`) only writes `tgt_eqmid`,
`tgt_frmlen`, `tgt_nfrm_in_pkt`. The actual switch happens inside `ldacBT_encode`
(`ldacBT_api.c:429-520`):

- if the transport-frame buffer is empty (`nfrm_in == 0`), apply immediately;
- if we are moving toward MORE frames per packet (i.e. **downward** in bitrate — "for
  better connectivity, apply ASAP"), it shrinks `frmlen` mid-packet to a fitting
  intermediate value, or flags `LDACBT_ALTER_OP__FLASH` to flush the partial packet early;
- if we are moving **upward** in bitrate, it sets `LDACBT_ALTER_OP__STANDBY` and waits
  for a packet boundary.

**Consequence:** down-steps are fast and self-prioritising, up-steps are deferred. That
asymmetry is already the right one and we get it for free. Latency of a step is at most
one packet, i.e. 5.3 ms (HQ) to 16 ms (MQ).

### 0.4 `ldacBT_get_bitrate()` reports the APPLIED rate; `ldacBT_get_eqmid()` reports the REQUESTED one

`ldacBT_api.c:125` returns `hLdacBT->bitrate`, written only inside
`ldacBT_update_frmlen` (`ldacBT_internal.c:374`) — i.e. after the switch has physically
taken effect. `ldacBT_api.c:317` returns `tgt_eqmid` — the request.

This is exactly the distinction the panel needs. **The readout must use
`ldacBT_get_bitrate()`**, so the screen never claims a rate that is not yet on the air.
This also transitively confirms the existing `codec_ldac.c:196` readout is correct and
will keep being correct across ABR steps with no change to its logic.

### 0.5 `ldacBT_update_frmlen` is already on the IRQ hot path

It is called from inside `ldacBT_encode`, which a2dp.c already calls from IRQ context.
It is table lookups plus `ldaclib_set_encode_info` — no allocation, no logging, no
blocking. So a rung change costs nothing new in terms of context safety; the encode
call it rides on already had to be safe.

---

## 1. Question 4 (answered first, because it constrains everything else): no AVDTP reconfigure

**Purely encoder-side. No AVDTP RECONFIGURE, no SUSPEND, no re-negotiation, no audible gap.**

From the source, not assumption:

1. The media codec information we negotiate is the fixed 8-byte blob in
   `codec_ldac.c:69-74`: vendor ID, vendor codec ID, a sampling-frequency **bitmap**, a
   channel-mode **bitmap**. There is **no bitrate or EQMID field** anywhere in it. The
   value we send is identical at every rung, so there is nothing to renegotiate.
2. The sink learns the frame length from the LDAC frame header
   (`LDACBT_FRMHDRBYTES`, written per transport frame by `ldacBT_encode`), not from
   AVDTP. That is the whole reason the ladder is walkable mid-stream at all.
3. `ldacBT_alter_eqmid_priority` touches only handle-local state (0.3 above). It makes
   no transport call, and libldac has no notion of AVDTP.

This is the single most load-bearing fact in the design: it makes ABR a **local encoder
policy** rather than a signalling protocol, so it cannot desync the A2DP state machine,
cannot race `a2dp_source_*`, and does not need to be serialised against BTstack at all.

Corollary for the AVDTP layer: **a rung change must never touch
`s_ctx.max_media_payload_size`, `s_ctx.frames_per_packet`, `s_ctx.priming_target_bytes`,
`pl_pcm_set_target_fill_bytes`, `rtp_next`, or the tx flush.** Any implementation that
recomputes stream geometry on a rung change has misunderstood this section.

---

## 2. Question 1: the control input

### 2.1 What is wrong with `stop_queue_full` as-is

`stop_queue_full` (`a2dp.c:1498,1531,1600`) increments when the fill loop breaks because
`pl_a2dp_tx_count() >= PL_A2DP_TX_QUEUE_SLOTS - 1`. It is:

- **cumulative**, so a controller must difference it;
- **saturating** — it can trip at most a couple of times per media tick, so its rate is
  bounded by the tick rate. It is really a *duty fraction* ("what fraction of ticks ended
  queue-full"), not a magnitude;
- **late** — it only fires once the queue is already at the rail, i.e. once we are
  already dropping the audio on the floor. A controller keyed on it alone can only react
  after the damage.

It is a good *tripwire* and a bad *regulator input*.

### 2.2 The signal: EMA of tx-queue occupancy, sampled once per media tick

**Primary input:** `pl_a2dp_tx_count()`, sampled exactly once per
`pl_a2dp_media_timer_handler` call, low-pass filtered.

```
// Q8 fixed point, units of 1/256 of a slot. Shift 4 => tau ~= 16 ticks ~= 160 ms.
s_abr_q_ema += ((int32_t)pl_a2dp_tx_count() * 256 - s_abr_q_ema) >> 4;
```

Rationale, directly transferred from `pico-link-fhf`:

- **EMA, never raw.** Raw `tx_count` sawtooths by a full packet between the seal and the
  `can_send_now` grant. Its excursion (1-2 slots) is comparable to any usable band, so a
  raw comparison would trip constantly. This is precisely the argument fhf makes for
  `fb_fill_ema` versus raw fill, and it applies here for the same structural reason.
- **Occupancy, not edge count.** Depth rises *before* the rail, so the controller acts
  before `stop_queue_full` would ever have fired. This is also what Android's ABR
  actually watches (A2DP transmit queue depth), so we are not inventing a signal.
- **Once per tick, not once per seal.** Sampling inside the fill loop would weight ticks
  by how many packets they happened to produce, which is itself bitrate-dependent — the
  signal would move when we changed the rung even if the link did not. Sampling on the
  fixed 10 ms tick keeps the signal's meaning invariant across rungs. **This matters and
  is easy to get wrong.**

**Where it is read:** `pl_a2dp_media_timer_handler`, in the `PL_A2DP_MEDIA_STREAMING`
branch, immediately after the `host_silent` computation and *before* `pl_a2dp_fill()` —
the same place and for the same reason the fhf trim sits there. Note `pl_a2dp_tx_count()`
is already read from this handler under the core1 build (`a2dp.c:2245`), so the *signal*
side is core1-safe with no new hazard.

### 2.3 The secondary input: `stop_queue_full` delta, as a veto only

Keep a per-window snapshot of `s_ctx.stop_queue_full`. Its delta is used for exactly one
thing: **a step UP is forbidden unless the delta over the entire up-dwell window is zero.**

This earns its place because it needs no tuning — "did we hit the rail even once in the
last minute" is a binary fact, and it is a much stronger recovery gate than any threshold
on `q_ema`. It is not used for the down trigger (2.2's occupancy already fires earlier).

### 2.4 Gating

The controller evaluates only when **all** of:

- `s_ctx.state == PL_A2DP_MEDIA_STREAMING` (never during PRIMING — the tx queue is empty
  and the signal is meaningless);
- `!host_silent` (a paused host must not be stepped down toward MQ on its way to
  auto-pause — same reasoning as fhf's `host_silent` skip);
- the active codec row is LDAC and the mode is Adaptive (see sec 5);
- we are past the settle window (sec 3.3).

---

## 3. Question 2: the step policy

### 3.1 Bands

Seven usable slots (`PL_A2DP_TX_QUEUE_SLOTS - 1`).

| constant | value | meaning |
|---|---|---|
| `PL_LDAC_ABR_Q_HI` | `4 * 256` | q_ema at/above 4.0 slots -> step down |
| `PL_LDAC_ABR_Q_LO` | `1 * 256` | q_ema at/below 1.0 slots -> step-up candidate |
| `PL_LDAC_ABR_SETTLE_US` | `1000000` | after ANY step, ignore the signal for 1 s |
| `PL_LDAC_ABR_UP_DWELL_US` | `60000000` | minimum time at a rung before stepping up |

The dead band 1.0..4.0 slots is deliberately enormous — three of seven usable slots. A
narrow band buys nothing here: we are choosing between five discrete operating points,
not tracking a continuous setpoint.

### 3.2 Triggers

**Step down** (toward robustness, `LDACBT_EQMID_INC_CONNECTION`):
`q_ema >= Q_HI` and `now - last_step_us > SETTLE_US` and `rung < 4`.

**Step up** (toward quality, `LDACBT_EQMID_INC_QUALITY`):
`q_ema <= Q_LO` and `now - last_step_us > UP_DWELL_US` and
`stop_queue_full` delta over that whole window `== 0` and `rung > 0`.

**Otherwise:** hold.

### 3.3 Settle: reseed the EMA after every step, last

After a successful step, in this order: apply, then `s_abr_last_step_us = now`, then
**reseed** `s_abr_q_ema = pl_a2dp_tx_count() * 256`, then snapshot `stop_queue_full`.

This is fhf's ordering lesson applied verbatim: the EMA holds a stale pre-step value for
roughly 5 tau (~800 ms) after the operating point moves, and an ungated second evaluation
would step again on a reading that no longer exists. `SETTLE_US` = 1 s is chosen as
`> 5 tau`, so the lockout is *sufficient* rather than merely helpful.

Worst-case full descent HQ -> MQ is therefore 4 s. The first step (990 -> 660, a 33% cut
in byte rate and packet rate) lands within ~1 s of the onset, which is the one that
matters for audibility.

### 3.4 Why this cannot oscillate — and where it still can

The dead band alone does **not** prove it. Consider a link that is ~20% over capacity at
990: the controller steps to 660, `q_ema` collapses well below `Q_LO`, and after the
up-dwell it steps back to 990 and re-congests. That is a limit cycle whose period is
`UP_DWELL_US`, and no width of dead band removes it, because the plant genuinely has two
states either side of the boundary.

The defence is **asymmetric dwell**: 1 s down, 60 s up. The cycle therefore costs at most
one brief quality wobble per minute in a genuinely marginal environment, and zero in a
good one. Every step down is fast; every step up is slow and requires a provably clean
minute.

**Documented upgrade, deliberately NOT built now:** per-rung exponential up-backoff — if a
step up from rung N is followed by a step down within 30 s, double that rung's personal
up-dwell (cap ~5 minutes), reset on `STREAM_STARTED`. This is roughly ten lines and it
converts the once-a-minute wobble into a one-time settle. **Build it if, and only if,
listening reveals a repeating wobble.** Building it speculatively is over-engineering
against a plant we have not measured; leaving it undesigned would be worse, so it is
specced here with a named trigger condition.

### 3.5 Rails

`ldacBT_alter_eqmid_priority` returns `LDACBT_E_FAIL` with
`LDACBT_ERR_ALTER_EQMID_LIMITED` at either end of the ladder. Treat as "already at the
rail": do **not** advance the rung counter, increment `abr_rail_hits`, and log nothing
(IRQ context). `abr_rail_hits` climbing steadily at rung 4 is a legitimate readout —
"the link is worse than our floor" — and is a useful thing to see in `pl_a2dp_report`.

---

## 4. Question 3: interaction with the PCM pacing loop

This was flagged as the risky part. **It is less risky than feared in one dimension and
more risky than feared in another.** Both answers come from the code.

### 4.1 The credit clock is bitrate-invariant. A rung change cannot desync it.

`pl_a2dp_accrue_credit` (`a2dp.c:1725-1745`) accrues `samples_owed` in **PCM
sample-frames** from wall-clock `elapsed_us * sample_rate`. `pl_a2dp_fill` decrements it
by exactly `pcm_frames_per_encoded_frame` (128, `LDACBT_ENC_LSU`) per `encode()` call.
Neither term contains an encoded-byte quantity. LDAC consumes 128 PCM frames per call at
**every** rung.

Therefore: **the credit clock does not know the bitrate exists.** Changing the rung
changes how many bytes come *out*, never how much PCM goes *in* per unit time. There is
no arithmetic to re-derive, nothing to re-seed, and no desync hazard. This is the good news
and it is structural, not incidental.

The R3-1 credit clamp (`a2dp.c:1783-1791`) is keyed to `pl_pcm_fill_bytes()`, i.e. to the
ring, not to packet size — also bitrate-invariant. (Its `frames_per_packet > 0` guard is
satisfied trivially; see 4.3.)

RTP is likewise safe: `pl_a2dp_seal_head` advances `rtp_next` by
`head->frames * pcm_frames_per_encoded_frame`, and `head->frames` accumulates
`result.frames_emitted` — the count of transport frames libldac actually put in the
payload. When the frames-per-packet changes, that count changes with it, so the timestamp
stays exact even across a mid-packet `frmlen_adj` transition (0.3).

### 4.2 What DOES change: the drain's burst granularity — and this makes `pico-link-0gtk` worse if unaddressed

A rung change changes `nfrm_in_pkt` from 2 (HQ) to 6 (MQ). The PCM time carried by one
AVDTP packet therefore goes from **5.33 ms (1024 B) to 16 ms (3072 B)** — a 3x growth in
the consumer's single-excursion size, in the direction ABR steps *toward*.

The priming/setpoint derivation at `a2dp.c:2915-2917` sizes the cushion from exactly that
excursion:

```
one_packet_bytes = frames_per_packet * pcm_frames_per_encoded_frame * PL_PCM_FRAME_BYTES;
derived_cushion  = one_packet_bytes + 192 + jitter_bytes;
priming_target   = min(max(derived_cushion, PL_PCM_TARGET_FILL_BYTES /*5760*/),
                       PL_PCM_RING_CAPACITY/4 /*8192*/);
pl_pcm_set_target_fill_bytes(priming_target);   // fhf: the ONE runtime setpoint
```

But `frames_per_packet` is **forced to 1 for any self-packetising codec**
(`a2dp.c:2866-2871`: `encoded_frame_bytes > 0 ? ... : 1u`). So LDAC's `one_packet_bytes`
is computed as **512 B today, against a real HQ excursion of 1024 B and a real MQ
excursion of 3072 B** — understated 2x now and 6x at the ABR floor.

This is a **pre-existing bug, not one ABR introduces** — but ABR is the thing that makes
it bite, because ABR is what drives the system toward the 6-frame packet. And its
signature is exactly `pico-link-0gtk`: a cushion too small for the consumer's excursion
drifts toward empty and underruns.

**Ruling: `pico-link-7jol.3` must not ship without this.** Two options:

- **Quick:** ignore it; the 5760 B floor happens to exceed even the 3072 B MQ excursion,
  so with small jitter the floor covers it anyway. *Future cost:* it only holds while
  `jitter_bytes` is small; the moment a tick stalls, the derivation is sized off a number
  that is wrong by 6x, and the failure is a silent underrun. It also leaves a known-false
  quantity in the one derivation three separate mechanisms now agree on (fhf's unification).
- **Sustainable (recommended):** make `one_packet_bytes` honest for self-packetising rows
  and size it for the **floor rung the controller may reach**, not the current one.
  For LDAC the excursion is computable from the public API alone —
  `bytes_per_frame = ldacBT_get_bitrate()*1000/3000`, `frmlen_tx = that + 3`,
  `frames_per_packet = clamp(MTU / frmlen_tx, 2, 15)` — evaluated once at
  `STREAM_ESTABLISHED` **for the lowest permitted rung** (MQ if Adaptive, the pinned rung
  if pinned). *Present cost:* about 15 extra lines, and roughly +15 ms of buffered latency
  at HQ (cushion ~7.1 KB vs today's 5.76 KB floor) because the cushion is now sized for the
  worst rung.

Sizing for the worst rung is what makes the cushion **rung-invariant**, and rung-invariance
is the entire point: a mid-stream step must never invalidate a setpoint that three
mechanisms (PRIMING's exit condition, the USB feedback loop, and fhf's trim) all regulate
against. **Recomputing `pl_pcm_set_target_fill_bytes` on a rung change is forbidden** — it
would move the setpoint out from under a live fhf trim and strand the ring, which is the
precise failure fhf was created to fix.

**Effect on `pico-link-0gtk`: strictly better, with the sustainable option.** 0gtk is
deficit-side drift toward empty; a cushion sized for the true worst-case excursion gives
it more margin, not less. **With the quick option, ABR makes 0gtk worse**, because every
step down triples the excursion the (unchanged, understated) cushion has to absorb. Say
this plainly to whoever implements: *do not land ABR ahead of the honest excursion.*

### 4.3 Things a rung change must NOT touch

`max_media_payload_size`, `frames_per_packet`, `priming_target_bytes`,
`pl_pcm_set_target_fill_bytes`, `pl_pcm_trim_to`, `rtp_next`, `samples_owed`,
`samples_owed_rem_us`, the tx flush, the codec row's `init()`. A rung change is a single
libldac call plus two of our own counters. Anything more is a bug.

### 4.4 The unrelated lever worth naming

`PL_LDAC_INIT_MTU` is 679 (`codec_ldac.c:88`), libldac's documented minimum, while the
real negotiated AVDTP payload is larger — a known, already-flagged gap. Raising it is the
*other* congestion lever (fewer, larger packets at the same byte rate) and is orthogonal
to ABR. Out of scope here; noting it so ABR is not mistaken for the only available fix.

---

## 5. Question 5: pinned versus adaptive — SETTLED by Andreas, 2026-09-07

**Ruling: a manual quality choice PINS the EQMID. ABR never overrides a manual pick.**
Ceiling semantics were considered and rejected.

So `persist.c`'s already-reserved encoding is used exactly as written, per-device:

| `ldac_quality` | meaning | init EQMID | controller |
|---|---|---|---|
| 0 | unset (legacy / never chosen) | HQ | inert (treat as pinned HQ, today's behaviour) |
| 1 | 990 kbps | `LDACBT_EQMID_HQ` | inert |
| 2 | 660 kbps | `LDACBT_EQMID_SQ` | inert |
| 3 | 330 kbps | `LDACBT_EQMID_MQ` | inert |
| 4 | Adaptive | `LDACBT_EQMID_HQ` (rung 0) | **active**, ladder 0..4 |

**Documentation correction owed:** `persist.h:289-290` and `persist.c:85-86` both say
"4 = Adaptive (reserved -- not implementable with the vendored libldac, design sec 5)".
That is now false — sec 0.1 shows it is implementable. `pico-link-7jol.3` must fix both
comments; leaving them would actively mislead the next reader.

### 5.1 Inertness is structural, not a runtime branch

The three pinned values are all public `LDACBT_EQMID_*` constants, so a pin is applied by
passing the mapped value straight into `ldacBT_init_handle_encode`
(`codec_ldac.c:141`, replacing the hardcoded `LDACBT_EQMID_HQ`). **The controller is then
never constructed at all** for a pinned device — not "constructed and told to do nothing".
That is what makes "fully inert" provable rather than asserted, and it costs one
substituted argument.

### 5.2 Controller state across a mode change

`s_abr_rung`, `s_abr_q_ema`, `s_abr_last_step_us`, `s_abr_qfull_snapshot` are **reset
whenever any of these happens**: `STREAM_STARTED`, `STREAM_ESTABLISHED`, a change to the
active device's `ldac_quality`, and codec re-negotiation. There is no carried-over
controller state, ever. Rationale: every one of those events changes either the plant or
the operating point, and stale controller state is exactly the class of bug fhf's
EMA-reseed rule exists to prevent. A device switched pinned -> Adaptive starts a fresh
controller at rung 0 with a cold EMA and a full settle window.

### 5.3 Changing the setting mid-stream

A pin change while streaming does **not** require a re-init or a reconnect: walk the
ladder to the target with the same one-rung-per-fill mechanism (sec 6.2), converging in
~40 ms, gaplessly. This falls out of sec 1 (no AVDTP involvement) and costs nothing extra.

### 5.4 The line to change if the ruling is ever revisited

Model the controller with `(ceiling_rung, floor_rung)`. A pin is `ceiling == floor`;
ceiling semantics would be `ceiling = pinned, floor = 4`. Both cost the same. **All of
the pin-versus-ceiling policy therefore lives in the single function that maps
`ldac_quality` to that pair** — one `switch`, one file. If Andreas ever reverses the
ruling it is a one-line change, not a redesign. Do not scatter the policy anywhere else.

---

## 6. Placement, and the `PL_ENCODER_ON_CORE1` question

### 6.1 Is this design safe under `PL_ENCODER_ON_CORE1`? YES — if built as specified

The naive implementation is **not** safe. Calling `ldacBT_alter_eqmid_priority` directly
from the media-timer IRQ writes `tgt_eqmid`, `tgt_frmlen` and `tgt_nfrm_in_pkt` — three
separate, non-atomic words — while under core1 mode `ldacBT_encode` reads all three on
core1 (`ldacBT_api.c:430-469`). A torn triple would pair a new `frmlen` with an old
`nfrm_in_pkt` and mis-size a packet. That implementation inherits fhf's
`#ifndef PL_ENCODER_ON_CORE1` gate exactly.

**This design does not inherit the gate**, because it splits decide from apply.

### 6.2 The split: decide on core0, apply at the encoder's own context

- **Decide (core0, media-timer IRQ, both build modes).** Sample `pl_a2dp_tx_count()`,
  update `q_ema`, evaluate sec 3, and on a decision write a single `volatile int32_t`
  **target rung**, not a delta. Both inputs are already read from this handler under core1
  mode; nothing new is exposed.
- **Apply (whichever context owns the encoder: core0 IRQ legacy, core1 under the flag).**
  At the top of `pl_a2dp_fill`, before the loop, once per call:
  compare the requested rung with the applied rung, and if they differ take **one** step
  toward it via `ldacBT_alter_eqmid_priority`, then update the applied rung.

Why this is sound with no lock: one aligned 32-bit word, exactly one writer (core0) and
exactly one reader (the encoder context); a *target* rather than a delta is idempotent, so
a missed or duplicated observation converges rather than accumulating error. Stepping one
rung per `fill` call (100/s legacy, faster on core1) bounds the per-call cost and makes a
4-rung pin change converge in ~40 ms. This is the same discipline `pl_a2dp_seal_head`
already uses for `tx_head`.

**Present cost of the sustainable path: roughly 15 lines** versus the direct IRQ call.
**Future cost of the quick path:** a *second* mechanism dark under `PL_ENCODER_ON_CORE1`,
compounding `pico-link-quzf` (which already owes a core1-safe resync) and undermining the
P0 mitigation for the encoder starving core0. That is load-bearing and hard to undo later,
because by then the controller will be tuned against the legacy build only. **Take the
sustainable path.**

Publishing the new bitrate is likewise split: the encoder context stores the applied rung
and `ldacBT_get_bitrate()` into plain volatiles; **core0's media-timer handler** notices
the change and calls `pl_bt_push_codec_changed`. This keeps the core1 invariant "no bt.c
ring push from core1" intact by construction, and it keeps the UI notification on the one
context that is already allowed to make it.

### 6.3 Module boundaries

- `a2dp.c` owns the **controller**: `q_ema`, the bands, the dwell timers, the trigger
  logic, the counters. It knows about tx-queue depth, which is its own concept.
- `codec_ldac.c` owns the **ladder**: the `ldac_quality` -> EQMID mapping, the rung
  counter, every `ldacBT_*` call, and the applied-bitrate readback. This preserves
  `persist.h`'s own stated rule that the EQMID mapping lives in exactly one place.
- The seam between them is **one optional vtable slot**,
  `void (*apply_pending_tuning)(void *state)` on `pl_codec_t`, `NULL` for SBC, called by
  `pl_a2dp_fill` when non-NULL; plus `pl_codec_ldac_request_rung(int rung)` and
  `pl_codec_ldac_set_quality(uint8_t ldac_quality_1based)` for the core0 side.

Use the vtable slot rather than an `if (codec_id == PL_CODEC_ID_LDAC)` in `a2dp.c`.
`codec_table.h`'s established convention is to branch on **declared shape**, never codec
identity (the `encoded_frame_bytes == 0` convention is the precedent). A codec-identity
branch in the pacing loop is the exact kind of special-case that later has to be unpicked
when a third codec arrives.

---

## 7. Observability (non-optional)

Add to `pl_a2dp_report` (thread context, where logging is legal):
`abr_rung`, `abr_bitrate_bps` (the applied readback), `abr_steps_down`, `abr_steps_up`,
`abr_rail_hits`, `q_ema` (in slots, one decimal), and the existing
`stop_queue_full`/`stop_credit`/`stop_ring_empty` triple alongside them so the diagnosis
is readable in one line.

**No `pl_log` anywhere in the controller or the apply path.** This file's module doc
forbids it in the hot path and it killed the ISO-OUT endpoint once already
(`pico-link-0d2`).

The `abr_rung` readout is also the acceptance instrument: a controller that never steps
and a controller that is not wired look identical from the outside, and this project has
been burned by exactly that reading (`zero readings are not passes`).

---

## 8. Verification plan (for `pico-link-7jol.3` / Tess)

1. **Host:** no new host tests are possible — none of this exists in `core/`. Do not
   manufacture one; say so rather than reporting a green suite that proves nothing.
2. **Bench, on target:** run `ldac_bench` (it already sweeps HQ/SQ/MQ with a
   continuous-sweep input, `ldac_bench.c:43-45,195`). **The epic's "stepping down buys CPU
   headroom" claim is UNMEASURED and I am sceptical of it:** the encode *call rate* is
   375/s at every rung (128 samples per call regardless), and the MDCT dominates; only the
   quantisation loop shrinks. Expect a modest saving, not one proportional to bitrate.
   Get the number before anyone leans on that motive.
3. **On target, streaming:** confirm from `pl_a2dp_report` that `abr_rung` is 0 and
   `abr_steps_*` are 0 in a good RF environment over several minutes. A controller that
   fidgets when nothing is wrong is the primary failure mode.
4. **Provoked congestion:** walk out of range / obstruct, and confirm the sequence
   `q_ema` rises -> one step down within ~1 s -> `q_ema` falls -> no second step inside
   the settle window. Then confirm recovery takes >= 60 s and does not immediately
   re-descend.
5. **A/B in the same RF environment**, per the standing project rule — one arm cannot
   distinguish "we caused it" from "already true".
6. **Sink compatibility:** a mid-stream frames-per-packet change is legal LDAC and is what
   every Android phone does, but we have exactly one test sink (`94:DB:56:54:7C:F2`).
   Listen for artefacts *at the step*, not just after it.

---

## 9. Summary of decisions

| # | Question | Decision |
|---|---|---|
| 1 | Control input | EMA (Q8, shift 4, tau ~160 ms) of `pl_a2dp_tx_count()` sampled once per media tick in `pl_a2dp_media_timer_handler`; `stop_queue_full` delta used only as a binary veto on stepping up |
| 2 | Step policy | 5-rung ladder; down at q_ema >= 4.0 slots after a 1 s settle; up at q_ema <= 1.0 slots after 60 s clean; EMA reseeded after every step, last; asymmetric dwell is the anti-oscillation mechanism; per-rung exponential backoff specced but deliberately not built |
| 3 | Pacing interaction | Credit clock is bitrate-invariant — no desync possible. But `frames_per_packet` is hardcoded 1 for LDAC, understating the cushion 2-6x; must be made honest and sized for the FLOOR rung before ABR lands, or ABR makes `pico-link-0gtk` worse. The setpoint must never be recomputed on a rung change |
| 4 | AVDTP | Purely encoder-side. No reconfigure, no suspend, no gap. Proven from the negotiated blob and libldac's internals |
| 5 | Pin vs ceiling | **PINS** (Andreas, 2026-09-07). Adaptive is a fourth mode. Inertness is structural (pin applied at `init_handle_encode`). Policy confined to one mapping function so the ruling is reversible in one line |
| 6 | core1 | **Safe under `PL_ENCODER_ON_CORE1`, does not inherit fhf's gate** — provided decide (core0) and apply (encoder context) are split via a single volatile target-rung word. The naive direct-IRQ call is NOT safe |

## 10. Open risks

1. **The `frames_per_packet == 1` cushion bug (sec 4.2) is the real hazard in this epic**,
   and it is pre-existing. If `7jol.3` lands ABR without it, expect more underruns, and
   expect them to be blamed on ABR.
2. **The oscillation argument rests on a plant we have not measured.** Sec 3.4's asymmetric
   dwell bounds the damage to one wobble per minute; it does not eliminate the cycle. The
   backoff upgrade is the answer if listening shows it.
3. **The CPU-headroom motive is unverified** (sec 8.2) and I expect it to be weaker than
   the epic assumes.
4. **Single test sink.** Mid-stream rung changes are standard LDAC behaviour, but "standard"
   and "verified on our one headset" are different claims.
5. **`PL_LDAC_INIT_MTU` at 679** means we are congesting the link with more, smaller packets
   than necessary at every rung. ABR will mask that rather than fix it.

---

## 11. Addendum, 2026-09-07: two questions from Uma's picker (`pico-link-7jol.2`)

Uma's design (`.planning/design/2026-09-07-ldac-quality-selector.md`) applies a quality
choice **live, mid-stream, on one button press with no confirm**, allows picking **while
disconnected**, draws a **checkmark from the stored echo**, and shows the **live rate** on
Home. Two consequences for this control loop.

### 11.1 Can the `alter_eqmid_priority` walk fail to reach a requested rung?

**Between rungs 0..4 the walk is guaranteed, and it converges even if applies lag behind
requests. Uma's checkmark is not a lie — provided it stays bound to the stored echo and
never to `ldacBT_get_bitrate()`.** The detail matters, so here is the full failure surface.

**(a) The walk chains off the REQUEST, not the APPLY.** `ldacBT_get_altered_eqmid`
searches the ladder for `hLdacBT->tgt_eqmid` (`ldacBT_internal.c:464`) — the *requested*
rung — not `hLdacBT->eqmid`, the applied one. So two `alter` calls issued back-to-back
before any `ldacBT_encode` runs step 0 -> 1 -> 2 correctly. A naive reading fears the walk
stalls when the encoder has not caught up; it does not. This is what makes "one step per
`pl_a2dp_fill` call" (sec 6.2) provably convergent rather than merely usually convergent.

**(b) A single step can fail for exactly three reasons, all observable:**

| cause | source | our reading |
|---|---|---|
| already at a rail (rung 0 going up, rung 4 going down) | `ldacBT_internal.c:470,479` | expected; `abr_rail_hits++`, rung counter unchanged |
| `pkt_type != _2_DH5` | `ldacBT_internal.c:455-460` | impossible by construction (sec 0.2), but if it ever happens **both directions fail forever** |
| handle NULL / not in encode mode | `ldacBT_api.c:324-328` | fatal; the stream is already broken |

The caller distinguishes them with information it already has: a failure **while our own
rung counter is strictly inside 0..4** is not a rail and is therefore a fault. That is the
whole test, and it needs no new libldac call.

**(c) A step can succeed at the API level and still fail to APPLY — and libldac retries it
automatically.** `ldacBT_encode` ignores the return value of its
`ldacBT_update_frmlen(tgt_frmlen)` calls (`ldacBT_api.c:432,471,520,619`). But
`hLdacBT->eqmid` is only advanced *inside* a successful `update_frmlen`
(`ldacBT_internal.c:375`), so after a failed apply `eqmid != tgt_eqmid` still holds and the
next `ldacBT_encode` re-enters the same branch and tries again. A failed apply is retried
every encode call, not silently abandoned. It cannot wedge.

**(d) The one genuine observability trap: the live rate transiently shows a NON-LADDER
value.** On a fast down-step libldac may install an *intermediate* `frmlen_adj` to make the
in-flight packet fit (`ldacBT_api.c:441-455`), and `ldacBT_update_frmlen` sets
`hLdacBT->bitrate = ldacBT_frmlen_to_bitrate(frmlen_adj, ...)`
(`ldacBT_internal.c:374`) from that intermediate length. So `ldacBT_get_bitrate()` can
briefly report something like 700 kbps — a real, honest, on-the-air rate that is on no rung
of the ladder. It lasts at most one packet (5-16 ms), but the UI samples at ~20 Hz and
**will** catch it occasionally.

**Ruling for Uma:** this is fine and requires no change, because her two readouts are
already correctly separated:

- **Home's live bitrate** is *supposed* to show the real rate, including the transient. It
  is not lying; it is being precise. Do not snap it to the nearest ladder value — that
  would reintroduce the exact defect `pico-link-qx8` fixed (a panel restating a table
  instead of asking the library).
- **The picker checkmark** must be driven by the **stored `ldac_quality` echo** and never
  by the live rate. It answers "what did you choose", not "what is on the air right now".
  Uma already specifies it this way. Binding it to the live rate would make it flicker
  during transients and, under Adaptive, make it wrong permanently.

**Design addition, so a stuck walk is never silent:** keep `quality_requested` and
`quality_applied` as distinct values, plus an `abr_apply_fail` counter, and surface all
three in `pl_a2dp_report` (sec 7). If (b)'s impossible-by-construction case ever becomes
possible, it shows up as a divergence in a report line rather than as a checkmark that
quietly means nothing.

### 11.2 Does a manual pin survive a reconnect?

**It survives — but only because it is re-applied at codec negotiation on every
connection. And the seam that does that DOES NOT EXIST YET, so this is a requirement on
`pico-link-7jol.3`, not a property of today's code.**

Measured state of the tree: `pl_persist_get_device_settings` and
`pl_persist_write_device_settings` (`persist.h:318,334`) are **defined and called by
nothing outside `persist.c`.** `ldac_quality` is stored, mirrored and echoed, and never
read by anybody. The pin is currently inert in both directions.

**The correct re-apply point is `pl_a2dp_finish_codec_negotiation` (`a2dp.c:2276`), in the
`row->init()` call.** Everything needed is already in place there:

- `s_ctx.connect_addr` is populated well before it (`a2dp.c:2363`, in
  `pl_a2dp_establish_stream_now`), so the per-device lookup key is available.
- `pl_persist_get_device_settings` is *explicitly documented* as safe from this exact
  context and as reading a **live in-RAM mirror**, with its own doc comment naming the goal
  "a pin set now is visible to a2dp.c's next connection attempt immediately, not only after
  a power cycle" (`persist.h:322-333`). The seam was designed for this and then left
  unwired.

So: read `ldac_quality` for `connect_addr` immediately before `row->init()`, hand it to
`pl_codec_ldac_set_quality()`, and let `init()` pass the mapped EQMID to
`ldacBT_init_handle_encode` (sec 5.1). This runs on **every** connection, so a pin survives
a link drop, a power cycle, and a device switch, and each device gets its own.

**Can the controller start stepping before the mode is applied? No, and it is structurally
impossible, not merely unlikely.** Three independent barriers, in order:

1. `row->init()` runs during **codec negotiation**, which is strictly before
   `STREAM_ESTABLISHED`, which is strictly before `PRIMING` (`a2dp.c:3038`), which is
   strictly before `STREAMING` (`a2dp.c:3054`).
2. The controller's gate (sec 2.4) requires `s_ctx.state == PL_A2DP_MEDIA_STREAMING`. The
   media timer returns early in every other state (`a2dp.c:2133-2146`).
3. For a **pinned** device the controller is never constructed at all (sec 5.1) — the pin
   is the `eqmid` argument to `init_handle_encode`, so there is nothing that could step.

Barrier 3 is the one that makes this a guarantee rather than an ordering argument. The
window Uma is worried about — "ABR silently resumes after the link comes back" — would
require the controller to exist for a pinned device, and it does not.

**Two adjacent cases worth stating, because they are cheap to get wrong:**

- **Auto-resume after a host pause** takes the `SUSPENDED -> PRIMING` path
  (`a2dp.c:3159`) and does **not** re-run `init()`. That is correct: the libldac handle
  and its `tgt_eqmid` persist across a suspend, so the pin persists too. What must reset is
  the *controller* state (`q_ema`, dwell timers, rung), and sec 5.2 already resets it on
  `STREAM_STARTED`, which fires on resume.
- **Picking while disconnected** writes the record and the echo; the pin lands at the next
  `init()`. But `pl_persist_write_device_settings` **returns false and writes nothing if
  the address is not currently remembered** (`persist.h:300-305` — there is deliberately no
  "pin a device we have never stored" path). The UI must not draw the checkmark on a
  `false` return, or the picker will show a selection that no reconnect will ever honour.
  Likewise, if the live stream negotiated **SBC** rather than LDAC, the setting is stored
  and takes effect at the next LDAC negotiation; `pl_codec_ldac_set_quality` must be a
  safe no-op on the live stream in that case rather than touching a handle that is not the
  active codec.

### 11.3 Additions to sec 9 and sec 10

Add to the decision table: **the pin is re-applied at every codec negotiation, in
`pl_a2dp_finish_codec_negotiation` via `pl_persist_get_device_settings`** — the persistence
read seam exists and is documented for this call site, but is currently wired to nothing.

Add to the risk list: **`ldac_quality` is read by nobody today.** A `7jol.3` that wires the
picker to the *live encoder* without also wiring the *connect-time re-apply* would produce
a setting that works until the first reconnect and then silently reverts to Adaptive HQ —
which would look exactly like a flaky control loop and would be debugged as one.
