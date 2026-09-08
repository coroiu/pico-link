# The audio fault model: what can go wrong, what it is called, and how it is raised

Bead `pico-link-9eq2.1` (epic `pico-link-9eq2`). Author: Ada (architect), 2026-09-07.

> **Driving request, Andreas, 2026-09-07, verbatim:** *"I'm getting some stutters
> here and there and I have no idea if it's buffer over or underrun."*

Today every fault in the audio path is invisible on the device. The counters exist
— there are 30+ of them — but they reach only `pl_a2dp_report`'s CDC console lines,
and that console is unreliable (`pico-link-007`, `pico-link-5mb`) and usually not
attached. This document defines the **fault model**: the enumeration, the names, the
signal each fault is derived from, the severity classes, and the C/Rust seam.

**It does not design the UI.** Uma owns presentation on `pico-link-9eq2.2`; her
existing `.planning/design/2026-09-01-home-fault-strip.md` (bead `pico-link-h62`)
already settles geometry, stage tags, keying, freshness tiers and the display cap.
This document is the layer underneath it, and it explicitly answers the three items
that doc's section 11 handed to Ada.

---

## 0. Relationship to existing designs and beads

| Doc / bead | Relationship |
|---|---|
| `2026-09-07-home-fault-strip.md` (Uma, `pico-link-9eq2.2`) | **The binding presentation contract.** Landed in parallel with this doc; its §5 constrains this taxonomy (<=6 keys, <=16 chars, a declared glyph class, `wakes_display` separate from severity). This doc is built to it. Two deliberate deviations are named in §3.4. |
| `2026-09-01-home-fault-strip.md` (`pico-link-h62`) | Superseded by Uma's v2 above. Its `ENC OVERRUN` name is rejected here for the same reason her `OVERRUN`/`UNDERRUN` pair is (§4). |
| `pico-link-8jp` (USB supply health on screen) | Absorbed: Uma's v2 §8.3 makes its Home deliverable the `USB SUPPLY LOW` row itself and its numeric deliverable line 3 of the `why?` page. Its firmware half (`pl_usb_supply_q8()`) is **not built** — grep finds no such symbol. See §6.3. |
| `2026-09-07-ldac-abr-control-loop.md` | The signal-shaping idiom (EMA, dead band, settle, reseed-last) is reused verbatim here. An ABR rung step is **not a fault** — see §3.3. |
| `pico-link-fhf` (merged, `184a4eb`) | Its hysteresis band `PL_PCM_TRIM_BAND_BYTES` is reused as the `BUF HIGH` level fault's own band, deliberately — §5.2. |
| `pico-link-0gtk`, `pico-link-q4tq` | Coverage check in §8. Both are covered, and the model distinguishes them from each other. |
| `pico-link-6o2` | **Stale premise in the dispatch brief: this bug is CLOSED and fixed.** `bt.c`'s module doc (lines 60-92) records that the three `pl_bt_push_*` helpers no longer call Rust; `pl_bt_drain_events` from the superloop is the sole caller of `pl_ui_push_event` for BT events. The *rule* it established still binds this design (§7). |

---

## 1. The pipeline, walked from source

Read this session: `firmware/src/usb_audio.c`, `firmware/src/pcm_ring.{c,h}`,
`firmware/src/a2dp.c` (the `pl_a2dp_ctx_t` counter block at 443-668, `pl_a2dp_fill`
at 1384-1640, the media-timer handler at 2090-2200, `pl_a2dp_report` at 3430-3562,
`pl_a2dp_publish_counters` at 3571), `firmware/src/bt.c`, `ui-ffi/src/lib.rs`
(1405-1600), `core/src/power.rs`, `firmware/src/main.c` (552-880).

```
  host ──USB ISO OUT──▶ tud_audio_rx_done_pre_read_cb ──▶ pl_pcm_push
                                                              │
                                    [ PCM ring, 32KiB, 170ms cap, SPSC ]
                                                              │
   pl_usb_audio_feedback_task (1kHz) ──── fill EMA, ±500ppm feedback to host
                                                              │
                          pl_a2dp_fill ◀── credit clock (samples_owed) ── media tick
                                │
                          codec->encode  ──▶ tx slot ──seal──▶ tx ring (8 slots)
                                                                    │
                        CAN_SEND_MEDIA_PACKET_NOW grant ──▶ L2CAP ──▶ ACL ──▶ air
```

Five seams, each with its own failure modes. Below, **"visible?"** means *visible to
Andreas on the device*, not "does a counter exist".

### 1.1 Stage `IN` — host → dongle over USB ISO OUT

| # | Failure mode | Audible as | Counter today | Where | Visible? |
|---|---|---|---|---|---|
| 1 | Host stops sending while alt 1 is still selected | silence, then auto-pause | `packet_count` stall > 200 ms (`host_silent`, `a2dp.c:2130`) | a2dp.c | Not a fault (auto-pause is correct behaviour). Hero reads `idle`. |
| 2 | Host under-supplies (short or missing packets) | eventual gap/stutter | `s_rx_short_packets`, `s_rx_bytes_total` (`usb_audio.c:93-94`) | usb_audio.c | **NO** — counted, never surfaced, and there is no supply *ratio* |
| 3 | Host over-supplies / ignores our feedback | ring climbs → overflow or a resync cut | none directly; manifests downstream as #8/#11 | — | **NO** |
| 4 | ISO-OUT endpoint wedged, never re-armed (the historical TinyUSB defect) | total silence | indistinguishable from #1 | — | **NO — and structurally undetectable from USB alone.** See §6.4 |
| 5 | Non-frame-multiple ISO packet length | channel-phase desync (loud) | `pl_pcm_misaligned()` | pcm_ring.c | **NO** (should be permanently 0) |
| 6 | Alt setting drops mid-stream | silence | `streaming = false` (`usb_audio.c:256`) | usb_audio.c | Not a fault |
| 7 | Host clock drift beyond the ±500 ppm feedback authority | slow drift → #9 or #10 | none; the feedback rail is uncounted | usb_audio.c:471-478 | **NO.** This is `pico-link-0gtk`'s precursor and it is uninstrumented — §6.1 |

### 1.2 Stage `ENC` — the PCM ring

| # | Failure mode | Audible as | Counter today | Where | Visible? |
|---|---|---|---|---|---|
| 8 | **Ring overflow.** Producer outran consumer; the *newest* whole frames of the arriving batch are discarded | a click / a skipped instant | `pl_pcm_overrun_frames()` (`ovr_frames` in the report line) | pcm_ring.c:70 | **NO** |
| 9 | **Ring underrun.** The credit clock owed a frame and a slot was free, but the ring was empty | a gap / stutter, the classic dropout | `underrun_events`, `stop_ring_empty`, `silent_ticks` | a2dp.c:1543,1612 | **NO** |
| 10 | Fill drifts DOWN toward empty (free integrator, deficit side) | precursor to #9 | `pl_usb_audio_fb_fill_ema()`, `pl_usb_audio_fill_min()` | usb_audio.c:451,505 | **NO**, and **no controller acts on it** (`pico-link-0gtk`) |
| 11 | Fill drifts UP toward capacity (surplus side) | latency accumulation, then #8 | same EMA; `fhf`'s trim acts on it | a2dp.c:2178 | **NO** (the trim is silent) |
| 12 | Resync trim fired — we deliberately cut buffered audio to restore latency | a small deliberate skip | `resync_events`, `resync_drops` | a2dp.c:2182-2183 | **NO** |
| 13 | Stream-transition flush | expected silence | `flush_frames` | a2dp.c:631 | Not a fault |
| 14 | Ring returned less than its own `fill_bytes()` promised | shouldn't happen | `fill_short_read` | a2dp.c:1568 | **NO** (should be permanently 0) |
| 15 | Feedback loop saturated at the ±500 ppm rail / integral windup clamp | no authority left to correct drift | **none — uncounted** | usb_audio.c:466-478 | **NO.** The single best early warning we do not have — §6.1 |

### 1.3 Stage `ENC` — the encoder

| # | Failure mode | Audible as | Counter today | Where | Visible? |
|---|---|---|---|---|---|
| 16 | `codec->encode()` returned `!ok` | a dropped packet | `pkt_fail` | a2dp.c:1587 | **NO** |
| 17 | Encoder overran its dwell budget | pacing hiccup, starves core0 | `stop_dwell`, `dwell_max_us`, `stop_core1_budget` | a2dp.c:1444,1455 | **NO** |
| 18 | Credit clamp destroyed real samples (windup guard biting in band) | audio thrown away | `credit_clamped_samples`, `credit_clamp_events` | a2dp.c:1792-1793 | **NO** |
| 19 | Codec fallback (LDAC refused, negotiated SBC) | quality drop, not a fault | negotiated at SET_CONFIGURATION | a2dp.c:2565+ | Hero word + banner already cover it. **Not a fault row** (h62 §2.1) |
| 20 | Frame-shape contract violation (scratch too small) | defensive | `pkt_fail` | a2dp.c:1391 | **NO** (should be 0) |

### 1.4 Stage `AIR` — tx queue, L2CAP, ACL

| # | Failure mode | Audible as | Counter today | Where | Visible? |
|---|---|---|---|---|---|
| 21 | **Tx queue full** — fill could not seal because the send side is the limiter | latency, then #8, then dropouts | `stop_queue_full`, `tx_depth_max` | a2dp.c:1498,1531,1600 | **NO.** This is `pico-link-q4tq` |
| 22 | Grants arriving slower than we seal | same as #21 | `grants` vs `pkt_sent` vs `payloads_sealed` rates | a2dp.c:604-610 | **NO** |
| 23 | Grant arrived with an empty queue mid-stream (re-arm bug) | none directly | `spurious_grants` | a2dp.c:606 | **NO** (healthy only around SUSPEND) |
| 24 | A2DP stream suspended/released, or ACL dropped, **mid-stream** | silence, then reconnect | BTstack events; `pl_pcm_reset` at each transition | a2dp.c handlers | Partially — link state reaches the UI, but a mid-stream drop-and-recover is not distinctly named |
| 25 | Codec negotiation failure at connect | never plays | `ConnectFailureReason` over the FFI | ui-ffi | Already visible (wizard) |

### 1.5 Summary of the invisible half

**Every single row marked "NO" above is currently invisible on the device.** That is
18 of 25 failure modes, including *both halves of the question Andreas actually
asked* (#8 overflow and #9 underrun). Three of them (#7, #15, #22) have no counter
at all.

---

## 2. Doctrine: faults are a *view*, never a second source of truth

The tempting cheap path is a bespoke `volatile bool s_fault_underrun` set from the
media-timer IRQ wherever the condition is noticed. **Reject it.** It creates a
second state machine sitting next to the counters, with no conservation property,
no way to verify it against the report line, and — decisively — it *can miss edges*,
because the superloop that would read it stalls for hundreds of milliseconds
(`pico-link-ka3` measured `PL_LOOP_PHASE_TOTAL` at 576 ms; `pico-link-p1r` describes
a ~6 Hz loop). A flag set and cleared between two superloop passes is simply lost.

**Rule 1 — every fault is derived from a monotonic counter, or from a sticky
witness written by its own context.**

- A **cumulative counter cannot miss an edge by construction**: a delta over a
  longer window still counts every event that occurred inside it. A stalled
  superloop delays the report; it never loses it.
- A **level** signal (a fill EMA, a supply ratio, a queue depth) *can* be missed by
  instantaneous sampling. Every level fault must therefore have a **sticky witness**
  written in the producing context — a windowed min/max or a saturating tick count,
  cleared by the evaluator on read. The idiom already exists in the tree:
  `pl_usb_audio_fill_min()` (usb_audio.c:505) is exactly this shape. Copy it; do not
  invent a second one.

**Rule 2 — no new counter unless it is justified on its own terms.** The fault
layer's job is to *read* the instrumentation that pacing and correctness already
required. §6 lists the four counters this design does ask for, each with its
independent justification.

**Rule 3 — names, strings and colours live in Rust.** C sends a key *ordinal*. A
rename is then a Rust-only change and never a firmware reflash. This is also what
keeps `core/` platform-free: the fault vocabulary is application state, not a
platform detail.

---

## 3. The catalogue

Built to Uma's §5 contract: **at most 6 keys, ever**; names <= 16 characters,
uppercase, ASCII; every key declares a **glyph class** (`Filled` / `Starved` /
`Neutral`) and a **severity** (`Audible` / `Concealed`); `wakes_display` is a
separate field which is `false` for every `Concealed` key.

### 3.1 The six keys

| Ord | Name | Len | Glyph | Severity | Wakes | Signal shape | Raised on |
|---|---|---|---|---|---|---|---|
| 0 | `BUF STARVED` | 11 | **v** `Starved` | Audible | **yes** | rate | `d underrun_events >= 1` in a window |
| 1 | `BUF OVERFLOW` | 12 | **^** `Filled` | Audible | **yes** | rate | `d pl_pcm_overrun_frames() >= 1` |
| 2 | `USB SUPPLY LOW` | 14 | **v** `Starved` | Concealed | no | rate + level | `d s_rx_short_packets >= 1`, or supply ratio below `PL_FAULT_SUPPLY_LO` for `ENTER_WINDOWS` |
| 3 | `AIR CONGESTED` | 13 | **#** `Neutral` | Concealed | no | rate | `d stop_queue_full >= PL_FAULT_CONGEST_MIN` |
| 4 | `AIR LINK LOST` | 13 | **#** `Neutral` | Audible | **yes** | state -> rate | unexpected stream suspend/release while `STREAMING && !host_silent` |
| 5 | `ENC RESYNC` | 10 | **#** `Neutral` | Concealed | no | rate | `d resync_events >= 1` |

Every name is at or under 16. Every `Concealed` key has `wakes_display = false`.
Exactly three glyph classes are used, and the two directional ones are used only by
the two ring keys — which is what makes the glyph column mean something.

**Value slot** (Uma struck it from Home; it now feeds line 3 of her `why?` page):

| Ord | Value kind | Reads |
|---|---|---|
| 0 | `Millis` | windowed minimum ring fill, in ms — **`0ms` whenever the fault is real** |
| 1 | `Count` | frames dropped this window |
| 2 | `Ratio` (q8) | supply ratio; absent (`None`) until `pl_usb_supply_q8()` exists — see §6.3 |
| 3 | `Count` | `d stop_queue_full` |
| 4 | `Count` | occurrences |
| 5 | `Count` | frames dropped by the trim |

### 3.2 What was collapsed to reach six, and why

Uma's rule is *merge by user-facing consequence, not by mechanism*. The raw
enumeration in §1 produced twelve candidates. The six above survive because each is
a distinct thing Andreas experiences; the rest are causes or precursors of one of
them.

| Collapsed | Into | Reason |
|---|---|---|
| `ENC SLOW` (`stop_dwell`, `stop_core1_budget`, `credit_clamp_events`, `dwell_max_us`) | **demoted to quiet** | A slow encoder is a *cause*. Its audible consequence is always `BUF STARVED` — the drain falls behind, the ring drains, the gap is heard there. A row for the cause and a row for the consequence reports one stutter twice, and the cause is what the `why?` page is for. |
| `BUF HIGH` / `BUF LOW` (fill-EMA drift level faults) | **demoted to quiet** | Precursors. `BUF LOW` always resolves into `BUF STARVED`; `BUF HIGH` always resolves into `ENC RESYNC` (we trimmed) or `BUF OVERFLOW` (we did not in time). Reporting the precursor *and* the outcome is the same double-count. |
| `USB GAP` (short packets) + `HOST SLOW` (supply ratio) | `USB SUPPLY LOW` | Two mechanisms, one fact: the computer is not feeding us enough. |
| L2CAP send stalls + dropped media frames | `AIR CONGESTED` | h62's own merge, retained: same user-facing fact, "the radio couldn't keep up". |
| ISO-OUT endpoint wedged | *nothing* | Structurally indistinguishable from a paused host. It presents as `BUF STARVED` within a second, which is the honest statement. See §6.4. |

### 3.3 Quiet faults — `why?` page and console only, never a Home row

Invariant tripwires: each should read exactly zero in a healthy run, and each has a
firmware-bug rather than a user-experience meaning. A Home row for any of them would
train Andreas to ignore the strip.

| Name | Source | Meaning if nonzero |
|---|---|---|
| `ENC SLOW` | `stop_dwell` + `stop_core1_budget` + `credit_clamp_events` | the encoder, not the clock, is the limiter |
| `BUF HIGH` / `BUF LOW` | fill EMA vs `pl_pcm_target_fill_bytes()` | the ring is drifting; the free-integrator precursor |
| `FB RAIL` | new, §6.1 | the feedback loop has no authority left to correct drift |
| `MISALIGNED` | `pl_pcm_misaligned()` | TinyUSB produced an impossible ISO length |
| `SHORT READ` | `fill_short_read` | the ring contradicted its own `fill_bytes()` |
| `ENC FAIL` | `pkt_fail` | `codec->encode()` returned `!ok` |
| `SPUR GRANT` | `spurious_grants` mid-stream | a `can_send_now` re-arm bug |
| `HOST SILENT` | `host_silent` latch | the host paused, or our endpoint died (§6.4) |

**Deliberately no promotion mechanism.** If one of these ever fires in practice,
promote it with a bead and a measurement. A generic "quiet counter becomes a row
above N" escalator would be gold-plating a PoC against a case that has never
occurred — and the key cap is 6, so a promotion would have to evict something.

### 3.4 Two deliberate deviations from Uma's offer — hers to overrule

1. **`BUF` rather than `USB` as the ring pair's first word.** Her offer was
   `USB OVERFLOW` / `USB STARVED`. I use `BUF STARVED` / `BUF OVERFLOW` for two
   reasons. (a) **`USB` would misattribute.** In `pico-link-q4tq` the ring
   overflowed because the *radio* stopped taking packets while USB throughput was
   measured healthy at 189-192 kB/s; a row reading `USB OVERFLOW` would have pointed
   at the innocent stage. The ring is ours, and the co-live `AIR CONGESTED` row is
   what assigns blame. (b) **`BUF` is Andreas's own word** — "buffer over or
   underrun" — so the row answers his question in his vocabulary.
   The pair still satisfies her legibility rule exactly as her own offer does:
   different discriminating word (`STARVED` / `OVERFLOW`), different first letter of
   that word (S / O), different length (11 / 12).
2. **The stage word is `BUF`, which is not one of `IN`/`ENC`/`AIR`.** Her §5.4 makes
   the stage the first word of the name. Four stage words rather than three is a
   small cost; the alternative is naming the ring after a stage that is often not to
   blame. `USB` and `AIR` still head the two rows that *do* assign blame.

If she prefers her originals, the change is two strings on the Rust side and nothing
else moves (Rule 3, §2).

### 3.5 What is deliberately *not* a fault

Restating h62 §2.1 with two additions, because this list is what stops the catalogue
proliferating past six:

- **Codec fallback to SBC** — the hero word and banner already say it.
- **Muted volume** — the `MUTED` banner.
- **No link at all** — the hero word reads `NO LINK`.
- **Host idle / auto-pause** — a normal state; the bitrate line reads `idle`.
- **Anything measured while no stream exists** — meaningless.
- **NEW: an LDAC ABR rung step down.** When `pico-link-7jol.3` lands, the controller
  steps 990 -> 660 on exactly the congestion signal `AIR CONGESTED` watches. A rung
  step is the *successful mitigation*, not the fault; it belongs on the hero, where
  the live bitrate number changes. If congestion persists *after* the ladder bottoms
  out, `AIR CONGESTED` fires — the correct division of labour, and it costs no key.
- **NEW: the drift that provoked a resync.** `ENC RESYNC` reports our response; the
  drift is `BUF HIGH`, which is quiet (§3.2).

---

## 4. Over-run and under-run must be unmistakable

This is the entire request, so it gets a section and a ruling.

**Ruling 1: the words `OVERRUN` and `UNDERRUN` are rejected as a pair.** Uma's
measurement is decisive — at `helvB08` they are the same length and first differ at
character 6 of 8, so at 8px on a screen glanced at for a second and a half they are
undiscriminable. That defeats the entire request. h62's `ENC OVERRUN` is rejected
with them, doubly so: in this codebase "overrun" is already the established word for
the *ring overflowing* (`pl_pcm_overrun_frames`, `ovr_frames`), so the name collided
head-on with the one distinction we were asked to make sharp. It becomes the quiet
key `ENC SLOW` (§3.2), and the word "overrun" is retired from the display vocabulary
entirely.

**Ruling 2: the distinction is carried by four independent channels, and the word is
only one of them.**

| | `BUF OVERFLOW` (ord 1) | `BUF STARVED` (ord 0) |
|---|---|---|
| **Glyph** | **^** `Filled` — the level went up past the top | **v** `Starved` — the level went down past the bottom |
| Physical cause | too much audio: supply outran drain | too little audio: drain outran supply |
| Where the loss happens | producer side, in `pl_pcm_push` | consumer side, in `pl_a2dp_fill` |
| What is lost | the newest arriving frames | nothing is lost — a *gap* is emitted |
| Ring at the moment | at capacity (~32 KB / 170 ms) | at zero |
| `why?` line 3 reads | frames dropped — a large count | **`0ms`** — the fill minimum |
| Usual blame | the radio isn't taking packets, or the host over-supplies | the host under-supplies, or the ring drifted down |

1. **Glyph, resolved without decoding.** Up vs down triangle, matching the vertical
   OUT meter 6px to its right where up already means more. This is the primary
   channel and it is Uma's design, not mine.
2. **Word shape.** `OVERFLOW` vs `STARVED`: different first letter, different length.
3. **Direction of the numbers on the `why?` page.** Starved's value is a fill
   *minimum in milliseconds*, at or near `0` whenever the fault is real; overflow's
   is a frame count, always large. Even a misread word is corrected by the number.
4. **Co-occurrence.** They are almost never both live — except in the congestion
   signature (§8.2), where the pair being live together *is* the diagnosis.

**Constraint back to Uma:** what she may not do is give the two keys the same glyph
class, or an ordering rule under which only one of them is ever visible.

---

## 5. The trigger: turning cumulative counters into edges

Nearly every existing counter is cumulative since boot (a few reset at
`STREAM_STARTED`). A cumulative counter is a poor edge on its own. This section
defines the derived signal, the clear, and the storm control, reusing the two
controllers we already shipped rather than inventing a third idiom.

### 5.1 The evaluation window

**One evaluator, one cadence.** `pl_fault_evaluate()` runs from the superloop in
**thread context**, on a fixed `PL_FAULT_WINDOW_US = 1_000_000` boundary — the same
place and cadence `pl_a2dp_report()` and `pl_a2dp_publish_counters()` already run
(`main.c:854-877`). It holds a snapshot array of the previous window's counter
values and computes deltas.

Consequences that make this the right shape:

- **At most one decision per key per second.** One bad second therefore produces at
  most one raise per key. Storm control is a property of the cadence, not a patch on
  top of it.
- **A superloop stall delays the report, never loses it** (Rule 1). If the loop
  stalls 600 ms, the window is 1.6 s and the delta still counts everything.
- **No IRQ-context work is added anywhere**, which is the §7 seam requirement.

### 5.2 Three signal shapes, and each fault declares one

**(a) Rate faults** — derived from a cumulative counter.

```
delta = counter_now - snapshot;  snapshot = counter_now;
if (delta > threshold) raise(key, value);
```

Raise on the **first** window that exceeds the threshold — a dropout is audible
immediately and delaying it three seconds would defeat the instrument.

**(b) Level faults** — derived from a continuous quantity, with hysteresis. This is
`pico-link-fhf`'s and the ABR loop's shape, transferred without modification:

- evaluate the **EMA, never the raw value** (raw fill sawtooths by a full tick's
  drain, ~3840 B, comparable to any usable band);
- **enter** above/below the outer threshold, sustained for
  `PL_FAULT_LEVEL_ENTER_WINDOWS = 3` consecutive windows;
- **exit** at a strictly inner threshold (recommend half the band), so a signal
  sitting on the boundary cannot chatter;
- after any state change, **reseed last** — the fhf ordering lesson.

`BUF HIGH`'s entry threshold (quiet, §3.3) is `target + PL_PCM_TRIM_BAND_BYTES`, **the exact
condition the fhf trim itself trips on** (a2dp.c:2178). One constant, two consumers:
the display and the controller agree by construction rather than by coincidence, and
a future retune moves both together. `BUF LOW`'s is `target - PL_PCM_TRIM_BAND_BYTES`.
(Both are quiet per §3.3, but the mechanism is specified here because
`USB SUPPLY LOW`'s supply-ratio half is a level fault and uses it.)

**(c) State faults** — a discrete condition, edge-detected in the context that owns
the state, latched into a saturating counter, then read as a rate fault. `AIR LINK LOST`
is the only one: the A2DP event handlers increment `s_ctx.link_lost_events`, and the
evaluator treats it identically to (a). This keeps the evaluator uniform and means
no state fault can be missed by a slow superloop.

### 5.3 Clearing, and why the FFI carries raises only

**A fault is never explicitly cleared across the seam.**

C keeps a per-key `active` flag purely to gate re-raising. It clears after
`PL_FAULT_CLEAR_WINDOWS = 3` consecutive quiet windows (3 s). This asymmetry — raise
on 1, clear on 3 — is deliberate and is the same asymmetry the ABR loop uses (fast
down, slow up): react immediately to something audible, be slow to declare it over.

The **UI's** notion of "gone" is entirely h62's render-time retirement
(`now - last_seen > FAULT_RETIRE`, 120 s in Uma's v2). There is no clear event, no `active` field in the
payload, and therefore **no two-sided state machine to desync**. That is worth more
than the fidelity a clear event would buy: a dropped clear event would leave a
permanent phantom row, and the bt-event ring can drop (`s_bt_ring_drop_count`).

**Refresh:** while a fault remains active in C, it re-emits once every
`PL_FAULT_REFRESH_US = 10_000_000` so a genuinely persistent condition does not
retire out from under itself at 120 s. (12x margin; if Uma retunes `FAULT_RETIRE`
down, this stays valid until it reaches 10 s.)

### 5.4 The payload carries an absolute count, not an increment

`count` in the payload is **C's authoritative running count for that key**, and Rust
*assigns* it rather than adding to it. A dropped event then costs one refresh cycle
of staleness instead of permanently desynchronising the number. Idempotent by
construction; this is the same reasoning that made `pl_a2dp_poll_levels` a seqlock
snapshot rather than a queue.

### 5.5 Storm control, stated as a bound

Worst case: 6 keys, each raising once in the same window, then refreshing at 0.1 Hz.
That is **≤ 7 events in one second, then < 1 per second sustained**, against a
31-slot ring drained ~20×/s. The event path cannot be saturated by faults. No
rate-limiter beyond the window cadence is needed, and adding one would be
unjustified complexity.

### 5.6 Gating and reset — the traps

1. **Evaluate only while `s_ctx.state == PL_A2DP_MEDIA_STREAMING`.** Faults measured
   with no stream are meaningless (h62 §2.1).
2. **Skip while `host_silent`** — same reason fhf's trim skips: a paused host must
   not be reported as under-supplying on its way to auto-pause.
3. **Re-snapshot and clear all fault state at every stream transition**
   (`STREAM_ESTABLISHED` / `STARTED` / `SUSPENDED` / `RELEASED`,
   `SIGNALING_CONNECTION_RELEASED`). The `pl_pcm_reset()` flush at those sites would
   otherwise read as a giant fault.
4. **A NEGATIVE delta means the counter was reset, not that a fault occurred.**
   `resync_events` and `tx_depth_max` are reset at `STREAM_STARTED` (a2dp.c:3111,
   594-598) while `resync_drops` and `ovr_frames` are not. Treat `now < snapshot` as
   "re-snapshot, no fault this window". This is a concrete false-positive generator;
   it must be in the implementation and in a unit test.
5. **`pl_usb_audio_fill_min()` is DESTRUCTIVE ON READ** (usb_audio.c:505-509 — it
   resets the window). It is already consumed by `pl_a2dp_report`. If the fault
   evaluator also calls it, the two readers steal each other's windows and both
   produce garbage — the exact "two readers on one channel" failure mode that cost
   this project a debugging session on the CDC tty. **Ruling: the fault evaluator
   becomes the sole caller**, caches the value, and exposes
   `pl_fault_last_fill_min()` for `pl_a2dp_report` to print. Any future
   destructive-read accessor inherits this rule.

### 5.7 Constants

| Constant | Value | Rationale |
|---|---|---|
| `PL_FAULT_WINDOW_US` | `1_000_000` | matches the existing report cadence; one decision per key per second |
| `PL_FAULT_CLEAR_WINDOWS` | `3` | slow to declare over; asymmetric with the 1-window raise |
| `PL_FAULT_LEVEL_ENTER_WINDOWS` | `3` | level faults are precursors, not events; no need to be fast |
| `PL_FAULT_REFRESH_US` | `10_000_000` | 6× margin under h62's 60 s retirement |
| `PL_FAULT_CONGEST_MIN` | `8` | `stop_queue_full` reads small nonzero values in healthy LDAC runs; q4tq's episode was **+598/85 s**. Tune from a healthy baseline capture before shipping — flagged as the one number in this design with no measurement behind it |
| `PL_FAULT_SUPPLY_LO` / `_HI` | q8 `250` / `254` | ~0.977 / ~0.992 of nominal; hysteresis band |
| *(wake limiters)* | — | **Uma owns these** (`FAULT_WAKE_HOLD` 20 s, `FAULT_WAKE_COOLDOWN` 5 min, `FAULT_WAKE_SESSION_CAP` 6). They live in Rust beside `IdlePolicy`, not in C — see §7.5. |

---

## 6. New instrumentation this design requires

Four counters. Each is justified independently of the fault strip.

### 6.1 `fb_rail_ticks` (usb_audio.c) — REQUIRED

Count ticks on which `pl_usb_audio_feedback_task`'s output was clamped at
`PL_FB_MAX_PPM`, or `s_fb_i_accum` was clamped at `PL_FB_I_ACCUM_MAX`
(usb_audio.c:466-478). Two lines.

This is the **single most valuable missing counter in the firmware**. It answers
"does the feedback loop still have authority?" — and `pico-link-0gtk` is precisely
the case where the answer is no and nothing says so. Its value is independent of
this bead: it belongs in `pl_a2dp_report` regardless. Class quiet (`FB RAIL`, §3.3).

### 6.2 `link_lost_events` (a2dp.c) — REQUIRED

A saturating counter incremented in the `STREAM_SUSPENDED` / `STREAM_RELEASED` /
`SIGNALING_CONNECTION_RELEASED` handlers **only when** `state == STREAMING` and
`!host_silent` (i.e. the drop was not our own auto-pause). Backs `AIR LINK LOST` (ord 4),
which is otherwise the one catalogue row with no counter at all. §5.2(c).

### 6.3 `pl_usb_supply_q8()` (usb_audio.c) — REQUIRED for the `IN` row only

`pico-link-8jp`'s windowed supply ratio: bytes actually received over the window
divided by 192 B/ms × window ms, in q8. **Not built** — no such symbol exists.

**If it is not built, `USB SUPPLY LOW` (ord 2) ships with only its short-packet half**, and
its value slot is absent rather than faked (h62 §2.1's rule, and the design of
record's section-15 "never fake a value"). Do not block the epic on it.

### 6.4 What this design refuses to build

A detector that distinguishes **"the host paused"** from **"our ISO-OUT endpoint
wedged"** (§1.1 #4). From the USB device side these are the same observation: packets
stop arriving while alt 1 is selected. Any detector would be an inference dressed as
a measurement, and h62 §2.2 rule 1 forbids exactly that.

**It costs us nothing.** A wedged endpoint starves the ring and raises
`BUF UNDERRUN` within a second, which is the user-facing truth. Say "audio stopped",
not a guess about why.

---

## 7. The FFI seam

### 7.1 The ruling: thread context, direct push, no new ring

`pl_fault_evaluate()` runs from the **superloop, thread context**, alongside the
existing 1 Hz reporting block in `main.c`. It therefore calls `pl_ui_push_event()`
**directly**. No new IRQ→Rust call is added anywhere, so the `pico-link-5am` /
`pico-link-6o2` rule is satisfied structurally rather than by a deferral mechanism.

h62 §11 asked Ada for "edge-triggered and debounced in C … so a 6 Hz superloop
cannot miss edges", and recommended routing through bt.c's MPSC ring. **The debounce
is C-side as asked; the ring is not needed.** §5.2's counter-delta discipline is what
makes edges unmissable — a ring would only have preserved edges that a flag-based
design could lose, and this design has no flags to lose. Routing through bt.c's ring
would also point the module dependency the wrong way: a2dp/usb faults have no
business travelling on the Bluetooth-domain ring.

The evaluator reads `volatile uint32_t` counters written from IRQ context. Every one
is a single aligned 32-bit word, so a read concurrent with a producer's increment is
benign (it returns the old or the new value, never a tear) — **the identical
convention `pl_a2dp_report` has used since `pico-link-pbv`.** No new hazard, no
critical section.

### 7.2 Module ownership

New `firmware/src/fault.c` / `fault.h`. Owns the key enum, per-key state, the
snapshot array, and `pl_fault_evaluate()`.

- May include `a2dp.h`, `usb_audio.h`, `pcm_ring.h`, `pico_link_ui.h`.
- **Nothing includes `fault.h` except `main.c`.** In particular `pcm_ring.c` must not
  — it is owned by neither side of the seam it crosses and must stay that way.
- Needs a small number of new getters on `a2dp.h` (`pl_a2dp_underrun_events()`,
  `pl_a2dp_resync_events()`, `pl_a2dp_stop_queue_full()`, `pl_a2dp_dwell_max_us()`,
  `pl_a2dp_link_lost_events()`, `pl_a2dp_stop_dwell()`, `pl_a2dp_credit_clamp_events()`).
  Plain accessors over existing `s_ctx` fields — a2dp.h already has this pattern.

### 7.3 What crosses

One purely additive event tag, same discipline as tags 13 (`LevelsChanged`) and 14
(`VolumeChanged`), so `PL_EVENT_ABI_VERSION` is **unchanged**:

```rust
// ui-ffi/src/lib.rs
pub enum PlEventTag { /* ... */ AudioFault = 15 }

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlAudioFaultPayload {
    pub key: u8,         // PlFaultKey ordinal; append-only wire value
    pub severity: u8,    // 0 = Concealed (amber), 1 = Audible (red)
    pub glyph: u8,       // 0 = Neutral, 1 = Filled (^), 2 = Starved (v)
    pub value_kind: u8,  // 0 = None, 1 = Ratio(q8), 2 = Count, 3 = Millis
    pub value: u16,      // feeds line 3 of the `why?` page, never Home
    pub count: u16,      // ABSOLUTE, assigned not added -- see 5.4
}
```

Eight bytes, POD, `Copy`, no `Drop`, **no pointers** — unlike `DeviceDiscovered` it
borrows nothing, so it has no lifetime contract to violate and nothing to memcpy.

**`glyph` and `severity` are on the wire, not derived in Rust**, which is Uma's §5.2
requirement ("declared, not inferred at render time") satisfied with **one** table
rather than two. C is authoritative for both, including `AIR CONGESTED`'s escalation
from `Concealed` to `Audible` on co-occurrence with `BUF OVERFLOW` — a dynamic
severity that Rust cannot compute, because it depends on the window's counter deltas
which never cross the seam. Rust's only per-key table is the **name string** (Rule 3,
§2), so a rename stays a Rust-only change.

**`wakes_display` is deliberately NOT on the wire.** It is a static per-key property
consulted at the C raise site to decide whether to request a wake, and Rust never
needs it. Uma's rule that it must be a separate field from severity is honoured — it
is a separate column in C's static key table — without spending a wire byte on a
constant.

`key`, `severity`, `glyph` and `value_kind` are all **checked** on the Rust side with
the existing `TryFrom` idiom; an unknown ordinal increments
`PlUi::malformed_tag_count` and is dropped rather than being matched on as a
discriminant (which would be UB). Ordinals are **append-only forever** — reordering
the catalogue is a wire break.

**Not routed through `bt.c`'s MPSC ring** — a direct disagreement with Uma's §12
item 5, stated rather than silently bent. Her reason for the ring is
`pico-link-6o2`, i.e. never call Rust from IRQ context. This design never does: the
evaluator is thread-context by construction (§7.1), so there is nothing to defer.
Routing audio faults through the *Bluetooth-domain* ring would also point the module
dependency the wrong way, and would add a queue that can drop (`s_bt_ring_drop_count`)
in front of a payload whose whole design assumes lossless delivery is unnecessary
(§5.4). The rule she is protecting is fully honoured; the mechanism is not needed.

### 7.4 The Rust side

- `core::app::FaultLog`: a **fixed array of `PL_FAULT_KEY_COUNT` entries**, no `Vec`,
  no allocation per fault (h62 §11's Ruby item 1). Entry:
  `{ count: u16, first_seen: Instant, last_seen: Instant, value: Option<FaultValue> }`.
- `FaultValue`: `Ratio(u16 /*q8*/) | Count(u16) | Millis(u16)` — h62's enum, unchanged.
- `Event::FaultRaised { key, class, value, count }` → `App::handle_event` → assigns
  into the log and marks the hero composite dirty.
- **Retirement, freshness tiers and the 4-of-7 selection are computed at render
  time** from `now - last_seen`. No timer, no background task, no retirement event.
- The damage key must fold **only what is actually drawn**
  (`.planning/decisions/…damage-keys…`, and the 2026-09-06 damage-rect design):
  the fault strip's key folds the visible rows' `(key, count, value, tier)` and
  nothing else. Folding `last_seen` raw would reinstate a full repaint every frame —
  a fault display that causes the SPI contention it is reporting, which h62 §8 calls
  out and which this project has already shipped once.

### 7.5 Wake-on-fault

**Correction to a first draft of this section, made after reading Uma's v2:** do
**not** reuse `PlUi::volume_wake_since_last_tick`. That flag ORs into `had_input`
inside `pl_ui_tick`, which resets `last_input` — and a fault must not count as
input. Doing so would re-arm the full 60 s screensaver on every fault, and would
also mean an already-blanked screen wakes and then re-blanks on the very next tick,
a one-frame flash.

**The mechanism is an expiring display-power floor on `IdlePolicy`**, exactly
parallel to the existing `mute_or_zero` floor and differing only in that it expires:

- `IdlePolicy::on_fault_wake(now)` sets `fault_hold_until = Some(now + FAULT_WAKE_HOLD)`.
- `IdlePolicy::tick` checks `fault_hold_until` alongside `mute_or_zero` when deciding
  `PowerState`, and clears it when it expires.
- **Any real press cancels the floor** and hands control back to the ordinary
  screensaver — otherwise the hold would fight the user's own input.
- `last_input` is **not** touched.

`IdlePolicy` lives in `core/src/run.rs` but is a **shared struct, not part of the run
loop** — `PlUi` owns an instance directly (`core/src/run.rs:501-502`, "shared with
`ui-ffi`") and `pl_ui_tick` drives it. This matters: **the firmware does not run
`core/src/run.rs`'s loop** (`.planning/design/2026-09-01-idle-policy-across-the-ffi-seam.md`).
A wake implemented inside `run::run` would work in the emulator and do nothing on the
device — this project has shipped that exact mistake once already
(`firmware-does-not-run-cores-run-loop`). The power level then reaches GP13 through
the already-wired pull-based `pl_ui_display_power` call at `main.c:651-660`; no new
firmware plumbing is needed.

**Only `Audible` keys with `wakes_display = true` request a wake** — three of six
(ords 0, 1, 4). `Concealed` never wakes, by Uma's rule 4 and for a good reason: a
fault we concealed is by definition one Andreas did not hear, and waking for it
trains him to ignore the wake.

**`AIR CONGESTED` keeps `wakes_display = false` even when it escalates to
`Audible`.** The escalation only fires in a window where `BUF OVERFLOW` is *also*
raised, and that key wakes — so the screen lights either way, without giving the
storm-prone key its own wake authority. This is exactly what Uma's rule 4 (a separate
field, so a key can be demoted without lying about its severity) exists to express.

**The storm limiters — 20 s hold, 5 min cooldown, 6-per-session cap — live in Rust
beside `IdlePolicy`, not in C.** They are display policy, they need the same clock
`IdlePolicy` already has, and putting them in C would split one policy across the
seam. C's only wake responsibility is the static `wakes_display` bit.

## 8. Coverage check against the two live bugs

### 8.1 `pico-link-0gtk` — deficit drift with audible underruns

Measured: ring 8564 -> 2160 B against a 4608 B target; `underrun_events` reached 13
over 60 s; `credit_clamp_events` moved; host USB throughput healthy.

| What the strip would have shown | From |
|---|---|
| **v** `BUF STARVED  x13` — Audible, red, **wakes the screen** | `d underrun_events`; `why?` line 3 reads `0ms` |
| **no `USB SUPPLY LOW` row at all** | supply healthy — and the absence *is* the finding |
| `why?` page: `ENC SLOW`, `BUF LOW` live for the whole run, `FB RAIL` climbing | quiet keys, §3.3 + §6.1 |

**Covered, and the diagnosis is legible without a console.** A `v` glyph with no
`USB` row above it says "starved, and not the computer's fault" in one glance —
which is precisely the conclusion the A/B took a matched hardware run to establish.
`FB RAIL` would additionally have said the feedback loop had no authority left,
which is 0gtk's actual mechanism and which nothing in the firmware can say today.

### 8.2 `pico-link-q4tq` — the unreproduced congestion episode

Measured over 85 s: `stop_queue_full` 16 -> 614; `stop_credit` and `stop_ring_empty`
**flat**; `ovr_frames` +41683 with no injection; `underrun_events` 0 -> 16; fill
oscillating 32636 <-> 1568.

| What the strip would have shown | From |
|---|---|
| **#** `AIR CONGESTED  x85` — escalated to `Audible`/red by co-occurrence | `d stop_queue_full` >> threshold, with `BUF OVERFLOW` live |
| **^** `BUF OVERFLOW  x85` — Audible, **wakes the screen** | `d ovr_frames` |
| **v** `BUF STARVED  x16` — Audible | `d underrun_events` |

**Covered — and the co-occurrence *is* the diagnosis.** A `^` and a `v` live at the
same time, with an `AIR` row above them, is the unmistakable signature of a radio
withholding grants: the ring oscillates between capacity and empty because the drain
is gated by the air side, not by our clock. The flat `stop_credit` that excluded our
own pacing maps to the *absence* of the quiet `ENC SLOW` key.

This is exactly why §3.1 keeps `AIR CONGESTED` and `BUF OVERFLOW` as separate keys
rather than merging them as h62 merged L2CAP stalls with dropped media frames. That
merge was right for two *air-side* mechanisms; merging across the air/ring boundary
would destroy the co-occurrence signature that names this bug. It is also why the two
directional glyphs must be able to appear simultaneously — a single row with a
flipping glyph, which Uma already declined in her §13, would have shown one of these
three facts and hidden the other two.

### 8.3 The return on this bead

Both bugs took matched hardware A/B runs with console capture to characterise, and
q4tq was **never reproduced**. With this model the next occurrence is
self-diagnosing at a glance, and for the two Audible keys the screen is already
awake when he looks up.

---

## 9. Sustainability: the quick path versus this one

**The quick path** (~1 day): one `volatile bool` per fault set from the media-timer
IRQ, one `PlEvent` per flag, six hardcoded strings in C.

- *Present cost:* small.
- *Future cost:* a second state machine with no conservation property, unverifiable
  against the report line, that **silently loses edges under a superloop stall**
  (measured at 576 ms, `pico-link-ka3`), and that puts display strings in firmware
  so every rename is a reflash. It is load-bearing — every future fault inherits the
  shape — and it is hard to undo.

**This path** (~2-3 days for `9eq2.3`): one `fault.c` module, one snapshot array, a
1 Hz thread-context evaluator, seven accessors, one additive event tag, four new
counters.

- *Present cost:* one new firmware module and ~7 trivial getters.
- *Future cost:* adding a fault is one enum ordinal, one delta line, one Rust string.

**My recommendation is the sustainable path**, because the difference is one to two
days on a mechanism every future audio diagnostic will sit on, and because the quick
version's specific failure — losing edges during exactly the stalls that cause the
faults — makes it worse than nothing (a strip that stays empty during a bad episode
manufactures the false confidence that nothing went wrong).

**Where I am deliberately *not* gold-plating:** no clear events, no promotion
mechanism for quiet counters, no generic rate-limiter beyond the window cadence, no
new ring, no fault persistence across reboot, no Advanced page in this epic.

---

## 10. Open items and handoff

**For Andreas — one decision:**
`PL_FAULT_CONGEST_MIN` is the only constant here with no measurement behind it.
`stop_queue_full` reads small nonzero values in healthy LDAC runs and reached 614 in
q4tq's episode. **Recommendation: start at 8 per second, and have Tess capture a
healthy 60 s LDAC baseline before `9eq2.3` merges** so the number is set from data
rather than from my estimate. Everything else in §5.7 is derived or inherited.

**For Uma (`pico-link-9eq2.2`) — your contract, met, with two flags:**
1. **Six keys exactly** (§3.1), all names <= 16 chars, each declaring one of your
   three glyph classes, `Audible`/`Concealed`, and a separate `wakes_display`. §3.2
   states what was collapsed and why, per your "merge by consequence" rule.
2. **Deviation, yours to overrule (§3.4):** `BUF STARVED` / `BUF OVERFLOW` rather
   than your `USB STARVED` / `USB OVERFLOW`. Reason: `USB` misattributes q4tq, where
   the ring overflowed with USB throughput measured healthy. Same legibility
   properties as your offer. Costs a fourth stage word (`BUF` alongside
   `IN`/`ENC`/`AIR`).
3. **`AIR CONGESTED` has dynamic severity** (`Concealed` -> `Audible` on
   co-occurrence with `BUF OVERFLOW`) but keeps `wakes_display = false` — the
   co-live `BUF OVERFLOW` provides the wake. This is your rule 4 doing exactly the
   job you designed it for.
4. **The value slot is gone from Home as you specified.** The value semantics survive
   as the payload's `value`/`value_kind`, feeding line 3 of your `why?` page — see
   the table in §3.1.
5. The two directional keys **must be able to be live simultaneously** (§8.2). Your
   §13 already declined the flipping-glyph single row; that call is load-bearing and
   this is the evidence for it.
6. Quiet keys (§3.3) never render on Home, but they **do belong on the `why?` page**
   as history entries if they fired — `ENC SLOW` and `FB RAIL` are what turn
   "starved" into "and here is why". Your §8.2 block shape accommodates them; tell me
   if you would rather they stayed off that page too.

**For Ruby (`pico-link-9eq2.3`), in order:**
1. `firmware/src/fault.{c,h}` + the a2dp getters (§7.2). **No IRQ-context work.**
2. The two required new counters (§6.1 `fb_rail_ticks`, §6.2 `link_lost_events`);
   `pl_usb_supply_q8()` (§6.3) is optional and its absence degrades one key's `why?`
   value only.
3. Event tag 15 + `PlAudioFaultPayload` (§7.3). `PL_EVENT_ABI_VERSION` **unchanged**.
4. `core::app::FaultLog` (§7.4) — fixed array of 6, no allocation, render-time
   retirement.
5. `IdlePolicy::on_fault_wake` + `fault_hold_until` (§7.5). **Not** the
   `volume_wake_since_last_tick` path, and **not** inside `run::run`.
6. The traps that need explicit unit tests: negative delta on counter reset
   (§5.6.4); the destructive `pl_usb_audio_fill_min()` reader move (§5.6.5); the
   damage key folding only drawn values (§7.4); a fault wake that does not extend
   `last_input`.

**For Tess:** a healthy 60 s LDAC baseline for §5.7's congestion threshold; a
fixture-driven headless pass over the strip states; then a hardware run with
`SKIPTICKS` injection, which should produce `BUF OVERFLOW` + `ENC RESYNC` and
nothing else. Uma's §12 lists the six wake behaviours to prove on the panel.
