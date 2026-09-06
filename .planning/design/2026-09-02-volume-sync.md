# Volume sync: host <-> dongle <-> headphones

Bead: `pico-link-6mx` (epic). Ada, 2026-09-02. Base: `main` @ `9eaf306`.

## 1. What exists today (verified, not assumed)

- `firmware/src/usb_audio.c:129-171` implements the UAC2 feature unit. It
  **stores** `fu_volume[]` / `fu_mute[]` and **nothing reads them** (the only
  other references are the GET handler echoing them back). Moving the macOS
  slider today changes nothing, audibly or over the air.
- Declared volume RANGE is `bMin=-12800, bMax=0, bRes=256` -> 50 dB in 1 dB
  steps = **51 host-visible steps**. AVRCP absolute volume is **128 steps**.
  The domains do not line up. Section 4 says why that matters.
- No AVRCP volume handling anywhere. `a2dp.c:671-760` has target/controller
  packet handlers; neither touches volume. `a2dp.c:750` explicitly notes
  volume passthrough was out of scope for the media-keys bead.
- BTstack has both halves we need:
  `avrcp_controller_set_absolute_volume(cid, 0..127)` (`avrcp_controller.h:334`)
  and `AVRCP_SUBEVENT_NOTIFICATION_VOLUME_CHANGED` emitted at
  `avrcp_controller.c:263-269`, armed by `avrcp_controller_enable_notification`.
- TinyUSB has a UAC2 status interrupt endpoint (`tud_audio_int_write`,
  `CFG_TUD_AUDIO_ENABLE_INTERRUPT_EP`) but it is **off** in
  `firmware/src/tusb_config.h` and the descriptor macro we use
  (`TUD_AUDIO_SPEAKER_STEREO_FB_DESCRIPTOR`) does not include one.
- The HID consumer-control path **is** built and proven (`media_keys.c`,
  epic `pico-link-47z`, merged today). Usages 0xE9/0xEA (Volume
  Increment/Decrement) are one `#define` away from usable.

So: neither half exists. The bead's premise is correct.

## 2. Context discipline (the constraint that shapes everything)

Three execution contexts touch this feature, and none of them is the one that
may do the work:

| Edge | Arrives in | May it call Rust? | May it call BTstack? |
|---|---|---|---|
| Host FU SET/GET | `tud_task()` on the **1ms worker IRQ, 0xC0** (`main.c:413`) | No (`pico-link-6o2`) | No |
| AVRCP volume notification | cyw43/BTstack background **IRQ 0xFF** | No | Yes (already there) |
| Superloop | thread context | Yes | **No** (`pico-link-ouw`: BTstack calls from thread context are deferred onto `pl_bt_pending_service` in `bt.c`) |

Note 0xC0 is a *higher* priority than 0xFF: the USB worker preempts BTstack.
Any shared state written by the FU handler must be written under
`save_and_disable_interrupts()`, the same rule `bt.c` and `media_keys.c`
already follow.

**Conclusion: thread context (the superloop) owns the canonical volume.** It is
the only context that can talk to all three destinations (Rust directly,
BTstack via the pending queue, USB via a latch the 0xC0 handler reads). Both
inbound edges deposit into latches; the superloop resolves and fans out.

## 3. Module shape

New `firmware/src/volume.c` / `volume.h`. Single owner of the canonical state:

```
uint8_t  level;      // 0..127, the AVRCP absolute-volume domain -- canonical
bool     muted;
uint8_t  pre_mute;   // level to restore on unmute
```

Inbound latches (written from IRQ, cleared by the superloop under a critical
section):

```
volatile uint8_t host_pending;   volatile bool host_dirty;   // from 0xC0
volatile uint8_t sink_pending;   volatile bool sink_dirty;   // from 0xFF
```

Outbound latches (written by the superloop, read by their destination):

```
volatile int16_t fu_report;      // read by usb_audio.c's GET handler on 0xC0
volatile uint8_t avrcp_desired;  volatile bool avrcp_dirty;  // read by bt.c's heartbeat on 0xFF
```

### 3.1 Which ring? (the question the dispatch asked)

**None of the three, for the volume level itself.** Volume is a *level*, not an
event: only the newest value has meaning. A ring preserves every entry, so a
slider drag at 30-60 SET/s would fill any ring we point it at and then start
dropping — dropping the *newest*, which for a level is exactly the wrong end.
A latch coalesces for free and is one word. This is the same reasoning that put
`out_level` behind a 4 Hz sample rather than a per-buffer event.

The three rings still each get their correct, narrow job:

- **`bt.c`'s UI event ring** carries the new `PL_EVENT_TAG_VOLUME_CHANGED` to
  Rust. Right ring: it is the "a fact reached `core`" channel, it already
  serialises IRQ producers with a critical section, and it is already drained
  once per frame immediately before `pl_ui_tick`. Reuse, do not extend.
- **`media_keys.c`'s ring** carries the outbound HID Volume Increment/Decrement
  reports in direction B. Right ring: there it genuinely *is* "a key to press",
  which is that ring's exact payload semantics, and `media_keys.c` owns every
  `tud_hid_*` call by contract.
- **`bt.c`'s pending-action queue** is *not* used for volume. It is a queue of
  discrete one-shot BTstack calls; a coalescing latch read by the same
  `pl_bt_wdt_heartbeat_handler` consumer is the correct shape. Add the latch
  read to that handler, next to `pl_bt_pending_service()`.

No new ring. No new timer. No new IRQ.

## 4. The loop-breaking rule

> **A value arriving from either side is applied to the canonical level and
> propagated outward if and only if, after quantisation into the canonical
> 0..127 domain, it differs from the current canonical level. If it is equal,
> it is absorbed silently and nothing is emitted.**

That is the whole rule. Three things about it are load-bearing:

**(a) Comparison happens in the canonical domain, after quantisation.** Never
compare raw host dB values or raw sink values. Both sides are projected into
0..127 first.

**(b) We never echo a peer's value; we always emit our own canonical value.**
The FU GET handler reports `fu_report`, derived from `level`, not the last raw
`bCur` the host sent. The AVRCP SET sends `level`, not the sink's last report.
This is what makes the fixed point stable when a peer re-quantises.

**(c) Origin tagging is deliberately NOT the primary mechanism, and a
suppression window is NOT used at all.** Both are the obvious design and both
are worse here. An AVRCP `VOLUME_CHANGED` notification carries no origin, and a
sink that snaps to its own step grid (many headphones have 16 or 32 steps)
echoes a *different* number than we sent. Origin-plus-time-window cannot
distinguish that re-quantised echo from a real user turn, so it either
suppresses a genuine change or admits an echo, depending on the window. Value
equality gets it right: the re-quantised echo is a genuine new truth, it is
accepted, propagated once, and the next round is equal and dies.

### 4.1 Termination proof sketch

Let `q_sink` and `q_host` be each peer's snap-to-nearest-step function. Both
are idempotent (`q(q(x)) = q(x)`) — true of any snap-to-grid. A change from the
host at value `v`:

1. canonical := `q_c(v)`, emit to sink.
2. sink applies `q_sink(level)`, may notify it back.
3. If `q_sink(level) == level`, rule (a) absorbs it. **Done, 1 round.**
4. Otherwise canonical := `q_sink(level)`, emitted to both sink and host.
   Sink: `q_sink` is idempotent, so its echo now equals canonical -> absorbed.
   Host: we report canonical; if the host writes it back it is equal ->
   absorbed. **Done, 2 rounds.**

Bounded at two rounds. The system has no self-sustaining oscillation because
every propagation is gated on inequality with canonical, and canonical is only
ever advanced by an external actor (a human moving a slider or a dial). Nothing
in the loop generates a new value on its own.

### 4.2 Simultaneous change on both sides

The two latches are read in one place, in one context, in a fixed order
(host then sink) once per superloop frame. So simultaneous changes are
**totally ordered by the drain, last one wins**, and both propagate — the sink
briefly receives two values in sequence. This is correct and imperceptible.

The genuine bad case: a user drags the macOS slider *while* spinning the
headphone dial. Then both sides keep producing new values and the level visibly
fights. This terminates the moment either user stops, and it is the same
behaviour every USB headset on the market has. **Do not build arbitration for
it.** Building a tie-breaker here would be the over-engineering failure mode.

### 4.3 Failure modes to accept, and to watch for

| Mode | Behaviour | Verdict |
|---|---|---|
| Sink re-quantises our value | one extra round, then stable | accept |
| Sink never notifies at all (no `VOLUME_CHANGED` support) | direction A works, B silently dead | accept; detect at T4 via the notification-registration response and set a capability flag |
| Both humans move at once | brief fight, terminates | accept |
| Host ignores `bRes` and sends off-grid values | we round; we report on-grid; still terminates by (b) | accept, but **T1 must measure it** |
| Sink echoes with a 1-LSB wobble that is *not* idempotent | genuine oscillation | the one real hazard. Mitigation: a change-emission counter and a circuit breaker — after N>8 emissions within 1 s with no human input, stop propagating for 2 s and log. Cheap insurance, ~15 lines. **Build it in T2.** |

## 5. Domain mapping (must be finalised by T1's measurements, not before)

Current declared range gives 51 host steps against 128 AVRCP steps. A coarse
domain mapped onto a fine one and back is **not** idempotent, and by section
4.1 non-idempotence is the one thing that breaks termination. Fix by making the
grids line up:

**Proposal (pending T1): change the declared RANGE to `bMin=-12700, bMax=0,
bRes=100`** — 127 steps of 0.39 dB, exactly bijective with 0..127:

```
level = (cur + 12700) / 100          cur = level * 100 - 12700
```

If T1 shows macOS ignores `bRes` and sends arbitrary `bCur`, we clamp and round
to the nearest grid point on ingest and always report the grid point back;
rule (b) keeps that terminating. **This is why T1 comes first: do not write
these constants before seeing what macOS actually sends.**

Two decisions I am settling now:

- **Linear in dB, not in perceived loudness.** AVRCP 0..127 is conventionally
  a linear percentage and UAC2 is dB; a strictly correct `20*log10` map is not
  bijective at the low end and reintroduces the rounding hazard. Linear-in-dB
  may feel slightly wrong at the extremes. Ship it; revisit only if Andreas
  notices by ear.
- **Do not attenuate PCM locally.** Volume is delegated entirely to the sink.
  Local attenuation would double-attenuate *and* throw away bits before LDAC
  sees them.

Mute: AVRCP has no mute. `mute -> send level 0, remember pre_mute; unmute ->
send pre_mute`. An inbound sink level of 0 does **not** set `muted`.

## 6. Direction B: how the host learns (the real risk)

**SUPERSEDED 2026-09-06 (VT4a, `pico-link-2ue`, and its 2026-09-06 code
review; measurement confirmed by VT4a.1, `pico-link-rmp`): the
recommendation below is OUT OF DATE. VT4a measured that macOS's
`AppleUSBAudio` DOES act on the UAC2 status interrupt endpoint -- it
re-GETs the feature unit and its slider follows the reported value exactly,
non-linear curve and all (section 5 below now carries the verified
bijective RANGE). VT4 (`pico-link-4v2.4`) therefore implements M1, not M2.
The two mechanisms below and the "let T1 decide" framing are kept for
historical record; do not build M2's HID tap-burst path.**

Two candidate mechanisms.

**M1 — UAC2 status interrupt endpoint.** The specification-correct answer.
Costs: `CFG_TUD_AUDIO_ENABLE_INTERRUPT_EP 1`, a hand-rolled audio-control
descriptor (the stock macro has no interrupt EP), a changed
`CFG_TUD_AUDIO_FUNC_1_DESC_LEN`, and one more endpoint. Risks: this project has
a long history of macOS refusing this device's alt settings over descriptor
details, and **whether macOS's `AppleUSBAudio` acts on a status interrupt for
the feature unit is unverified.** A descriptor change is the highest-blast-
radius edit available in this firmware.

**M2 — HID Consumer Volume Increment/Decrement (0xE9/0xEA).** Push N up/down
taps through the already-built, already-proven `media_keys.c` ring. macOS
certainly honours these — it is how every USB headset's volume buttons work.
Costs: relative not absolute, so a large jump is a burst of taps; and macOS
responds by changing *its* output volume, which round-trips back to us as an FU
SET, so the post-change canonical value is the host's grid point rather than
exactly the headphone's. Section 4 handles that correctly; the user-visible
consequence is that the number can land a step off what they set on the
headphone. Every USB headset behaves this way.

**Recommendation: M2 for the MVP, M1 as a later optional task, and let T1
decide.** M2 is cheap *and* contained — it reuses a module that already owns
that hardware, adds two `#define`s and a tap-burst loop, touches no descriptor,
and is trivially reversible. Its wrongness is a small fidelity loss, not an
architectural corner, so by this project's own standard it is not debt. M1 is
cleaner on paper but spends the enumeration risk budget on an unverified
assumption. If T1 shows M2's round-trip is ugly, M1 becomes a real bead with
real evidence behind it.

## 7. The Rust seam

New event, **tag 14**, additive, `PL_EVENT_ABI_VERSION` stays **4** — same
discipline as `LEVELS_CHANGED` (tag 13):

```c
PL_EVENT_TAG_VOLUME_CHANGED = 14
struct { uint8_t level; uint8_t muted; uint8_t source; } volume_changed;
```

`source` is `0=host, 1=sink, 2=device` — carried for display/diagnostics only,
**not** used by the loop-breaking rule (section 4c). Pushed by a new
`pl_bt_push_volume_changed()` in `bt.c`, called from the superloop's volume
service (thread context) — so unlike `pl_bt_push_levels_changed` it has no IRQ
producer, but it uses the same ring anyway rather than calling `pl_ui_push_event`
directly, for uniformity and because that ring's drain point is where `core`
expects state to land relative to `pl_ui_tick`.

`core` side: `BtModel::volume: Option<VolumeState { level: u8, muted: bool }>`,
alongside `out_level`. `core/` stays platform-free; nothing here needs a
platform crate. `run.rs` is untouched (emulator-only).

## 8. Out of scope, deliberately

**Device-side volume (d-pad Up/Down on Home).** `core/src/render/hero.rs:228`
already anticipates it as Tier 2. It is a *third* writer, and my rule handles it
with no change — it is just another external actor advancing canonical, and it
propagates to both peers. But it needs the M1/M2 question answered first and it
doubles the hardware test matrix. **Separate bead, after this epic.** Design
`volume.c` so `pl_volume_set_from_device(level)` is the one-line addition it
should be.

## 9. Task breakdown (epic children)

**T1 — Risk gate: does the host drive our feature unit, and can we move its
slider?** *(C only; no Bluetooth, no Rust, no descriptor change)*
Debug-console commands behind `PL_DEBUG_REMOTE`:
- `VOL GET` -> print `fu_volume[0..2]`, `fu_mute[0..2]`, `fu_set_calls`,
  `fu_get_calls`.
- `VOL WATCH` -> log every FU SET with its raw `bCur`, channel and selector.
- `VOL HOSTUP` / `VOL HOSTDOWN [n]` -> push n HID 0xE9/0xEA taps via
  `media_keys.c`.

Proves, in one hardware session: (i) that macOS writes our feature unit at all
when the slider moves; (ii) the **actual raw values and step grid** macOS uses —
the numbers section 5's constants must be written against; (iii) whether M2
moves the macOS slider; (iv) whether macOS then writes our FU in response,
i.e. whether the round trip that section 4 has to survive actually exists.
**Nothing downstream may start before T1's numbers are in a bead comment.**
This is the T1-of-media-keys pattern: cheapest possible thing that can
invalidate the design.

**T2 — `volume.c`: canonical state, mapping, loop rule, circuit breaker.**
*(C only; console-driven, no AVRCP, no Rust)*
Implements section 3 and 4 with the constants T1 measured. Console `VOL SET n`
sets canonical and **logs what it would emit to each peer** without emitting.
Also lands the RANGE descriptor change from section 5 if T1 says it is needed
(and re-verifies enumeration and audio still work — that is a real risk on this
device). Proves the rule in isolation, where a bug is readable in a log rather
than inferred from a wobbling slider.

**T3 — Direction A: host -> headphones.** *(C; USB + AVRCP)*
FU SET (0xC0) -> `host_pending` latch -> superloop service -> `avrcp_desired`
latch -> `pl_bt_wdt_heartbeat_handler` (0xFF) -> `avrcp_controller_set_absolute_volume`.
One SET in flight at a time, gated on `AVRCP_SUBEVENT_SET_ABSOLUTE_VOLUME_RESPONSE`
with a timeout — AVCTP shares the link with A2DP media and this project has a
crackle history; do not flood it. Proves: **macOS slider changes headphone
volume, by ear.**

**T4 — Direction B: headphones -> host.** *(C; AVRCP + HID)*
`avrcp_controller_enable_notification(cid, AVRCP_NOTIFICATION_EVENT_VOLUME_CHANGED)`
on AVRCP connect; handle the subevent on 0xFF -> `sink_pending` latch ->
superloop -> HID tap burst via `media_keys.c`. Re-register after each
notification (AVRCP notifications are one-shot). If registration is rejected,
set a capability flag and log it — a sink that does not support the notification
is a supported outcome, not a bug. **This is where the loop first becomes
possible; acceptance is "change volume on the headphones, the macOS slider
moves, and then it STOPS moving" plus the circuit breaker never trips.**

**T5 — The Rust seam.** *(C + Rust)* Section 7. Event tag 14, `BtModel::volume`,
ABI version unchanged. No screen changes. Proves: `cargo test` green, firmware
cross-compiles, volume observable in `BtModel`.

**T6 — Display.** *(Rust)* Uma's design, rendered as **one surface with the VU
meter**, per the same instruction `pico-link-h62` carries. Blocked on T5 and on
Uma.

Order: T1 -> T2 -> {T3, T4 (T4 depends on T3 landing the shared service loop)}
-> T5 -> T6. T5 may run in parallel with T3/T4 once T2 fixes the event shape.

## 10. UX split

**Settled here, no Uma needed:** canonical domain and mapping, the loop rule,
event shape and ABI, mute semantics, rate limits, that volume is delegated to
the sink and not applied to local PCM, that device-side volume is out of scope.

**Needs Uma (`pico-link-6mx` child, before T6):**
- How volume reads on Home *next to* the VU meter — a number, a bar sharing the
  meter's axis, or a transient overlay on change. It must be one coherent
  surface, not a second competing overlay.
- Whether a volume change wakes or extends the idle screensaver. My instinct:
  a change from the **headphones** should (the user is at the device), a change
  from the **host** should not (the user is at the laptop and does not need the
  screen). That asymmetry is exactly the kind of call Uma should make, and it
  is the only reason `source` is carried in the event.
- Whether mute has a distinct visual, and whether the source is ever shown.
- What is shown before the first volume is known (`Option::None`) and when the
  sink does not support the notification at all.
