# Tolerating a host that halves ISO-OUT supply (pico-link-2ap.1)

**Author:** Ada (architect), 2026-09-01. Amended same day against the USBPods reading.
**Status:** design of record for `pico-link-2ap.1` and its four children.

## 0. Framing, and three quick-fixes to refuse

The host episodically supplies ~0.500 packets/SOF for 45-130 s. This is a **badly-behaved-host
contingency**, designed for generically; it is n=1 and E3 (second host / powered hub) is not done,
so nothing here may encode "macOS does X" as fact. Every mechanism below keys off a *measured*
supply rate, never off a host identity.

Three tempting fixes, all wrong, named so Ruby does not reach for them:

1. **Widen `PL_FB_MAX_PPM`.** A 0.500 packets/SOF shortfall is 500,000 ppm. No feedback authority
   reaches it — feedback asks the host to adjust, and a host that has stopped sending is not
   listening. Widening only degrades the healthy regime.
2. **Grow the ring to bridge the gap.** 100 s of 192 B/ms is 19 MB, and under `pico-link-8fi`
   a bridge *is* latency. The ring stays 170 ms. Concealment's job is not to bridge; it is to keep
   the pipeline real-time indefinitely.
3. **Suspend / re-prime on starvation.** Already proven wrong — `pico-link-pbv` round 1's remedy
   was a ~300 ms audible dropout that manufactured the oscillation it was meant to fix.
   `a2dp.c:1029-1046`'s auto-pause must stay gated on `host_silent` (real packet-count stasis);
   half-supply is **not** host-silent, packets still arrive every other frame.

The credit-paced clock in `pl_a2dp_fill` stays. `.planning/design/2026-08-29-a2dp-source-pipeline.md`
sec 1 forbids a data-driven drain, and pbv rounds 1-3 record what happens when that is relaxed.
Element (b) does **not** make the drain data-driven.

## 1. Element (a) — conceal-then-report

- **Act immediately, per PCM unit, with zero delay.** The encoder must never miss its real-time quota.
- **Report only after persistence.** UI enters "degraded" after ~300 ms, leaves after ~2 s clear.
  A 45-130 s episode trivially clears that; a one-frame blip never reaches the screen.
- **Concealment is loud, never silent.** A concealment path that hides a genuine firmware
  regression is exactly the load-bearing hack this project has been bitten by. The mute counters
  are first-class, printed at 1 Hz and published in `pl_prio` slot 0 alongside the a2dp counters
  (`a2dp.c:1876`). Any acceptance check reusing the pbv-era conservation identity **breaks** until
  mute-silence frames are added as an explicit term. Update it in the same bead, not later.

## 2. Element (b) — three regimes, split on supply health

**This element was corrected after reading USBPods.** The first draft prescribed concealment on
*every* ring-empty. USBPods' `audio_slot_pop` (`btstack_avdtp_source.c:948-1004`) skips and waits
instead, because silence-splicing was audibly worse — constant static — on identical hardware.
That finding is accepted.

**Why splicing produces static, precisely:** on a transient gap the data is not missing, it is
*late*. Inserting silence and then still playing the late data lengthens the timeline by one unit
per gap. Every gap costs an artifact and buys nothing. Skip-and-wait costs a sub-millisecond wobble
the sink's buffer absorbs, and no artifact. Framing size (LDAC vs AAC-ELD) changes the artifact's
pitch, not its existence.

USBPods is already a two-regime design (skip-and-wait vs silence-with-fade when the host is fully
idle). The error was collapsing regimes. The corrected design keeps their split and adds a third
state they never had to characterise, because they had no instrument that could see it.

### Regime A — transient gap (ring-empty, supply health OK)

**Skip and wait. No concealment. Element (b) makes NO CHANGE to this path.**
`a2dp.c:759-763` stays exactly as it stands: `starved = true; stop_ring_empty++; break;`.

The existing machinery is already correct: on a skipped tick `samples_owed` grows, and the
ring-keyed credit clamp (`a2dp.c:974-987`) immediately clamps it to ring-fill-plus-one-frame, so
there is no catch-up burst and no accumulated debt. Skip-and-wait plus that clamp is already
freshness-correct per `pico-link-8fi`. Today's code is right here; USBPods independently confirms it.

**Regime A must not acquire concealment.** That is the specific mistake the USBPods reading
corrected, and exactly the change a supervisor would "helpfully" make while implementing 1c.

### Regime B — sustained under-supply (`supply_q8` below threshold >200 ms while USB still streaming)

The state 2ap actually found, and the one USBPods never characterised — their split has no name for
"host is streaming, and has been delivering exactly half for ninety seconds."

Skip-and-wait does not extend here, for structural reasons rather than aesthetic ones. Over
45-130 s it means emitting at half real-time indefinitely: the sink's buffer drains in well under a
second and underruns continuously, and our RTP timeline ends up ~50 s behind wall clock,
recoverable only by a resync dropout. That is the worst outcome under `pico-link-8fi`.

**Policy: fade to silence over ~20 ms, hold silence for the duration, keep encoding against silence
so the stream stays real-time.** On recovery, `pl_pcm_trim_to(priming_target_bytes)` (adding the
returned frames to `s_ctx.resync_drops`, `a2dp.c:477`, currently always 0 — update its doc comment,
do not silently repurpose it) and fade back in over ~20 ms.

This is not splicing, and that distinction answers the counter-evidence. Static comes from
*alternation* — silence interleaved with real audio at frame rate — not from silence as such. A
sustained mute has exactly two artifacts, one fade out and one fade in; stream length stays equal
to wall clock, so nothing is lengthened and recovery is instant and fresh. Structurally it is the
same policy USBPods uses for host-idle, applied to a state they could not detect.

Real audio arriving during regime B is discarded. The user hears silence for up to ~130 s — **and
the screen says why.** That is the product argument, not a consolation: a blind dongle that mutes
for two minutes is broken; one that mutes and reports `HOST AUDIO LOW` is diagnosable. This is the
display earning its place.

**Ship the judgement call as a switch.** `PL_CONCEAL_SUSTAINED_POLICY` with `MUTE` (default) and
`PASSTHROUGH` (let the gated half-audio through, no concealment), so the A/B is one rebuild apart.
Empirical evidence exists on one side of the transient question and none on this one. Andreas's ear
settles it; the constant is trivially removable afterwards.

### Regime C — host idle

Unchanged: the existing `host_silent` auto-pause at `a2dp.c:1042-1046`. Do not touch.

### Consequence for the new module

`firmware/src/pcm_conceal.{c,h}` — a **mute state machine**: fade-out, hold-silence, fade-in, plus
regime-B hysteresis. Per-unit ramp/repeat/hold logic is deleted. Not inline in `a2dp.c`: that file
is explicitly forbidden a codec identity and should not grow a DSP responsibility; a separate module
is also the only version that is host-testable. Counters `mute_entries`, `mute_ms_total`,
`mute_max_ms`.

## 3. Element (c) — saturation detection in the PI controller

### 3.1 Supply-health signal (a first-class model input, not a debug counter)

**Site: `usb_pump.c` `pl_usb_sof_isr_sample` (148-168).** The one trustworthy instrument here — it
runs in true SOF ISR context via the vendored `usbd.c` patch, not the worker-dispatched
`tud_sof_cb` that poisoned `sof_phase_hist` / `hist2` / `ep_out_idle_ticks`.

Windowed ratio beside the existing cumulative counters (near line 102): `s_sof_w_streaming`,
`s_sof_w_got`, `s_supply_q8` (256 == 1.000 packets/SOF), `s_supply_valid`, `s_supply_seq`.

**Window = 256 SOFs (~256 ms)** — chosen so the latch is `s_supply_q8 = s_sof_w_got` with **no
division at all**, clamped to 256, and so onset latency is <=256 ms rather than a full second.
Increment both window counters inside the `pl_usb_audio_streaming()` branch; on reaching 256, latch,
bump `s_supply_seq`, zero the window. In the `else` branch zero the window and set
`s_supply_valid = false`, `s_supply_q8 = 256` — a stale window must never straddle a stream boundary
and read as a real measurement.

Accessors in `usb_pump.h`: `pl_usb_supply_q8()`, `pl_usb_supply_valid()`, `pl_usb_supply_seq()`.
Add `supply_q8` to the `usb-sof-2ap` report line (`usb_pump.c:439-444`).

**Caveat to write into the comment:** `s_sof_got` counts "packet_count advanced since the previous
SOF", so it saturates at 1 packet/SOF. Exactly right for ISO OUT at bInterval 1, cannot over-report,
can never read above 256.

### 3.2 Wiring without inverting the layering

Change the signature to `pl_usb_audio_feedback_task(uint32_t supply_q8, bool supply_valid)` and have
`usb_pump.c:293` — the only caller — pass its own state. **Do not** add `#include "usb_pump.h"` to
`usb_audio.c`. The dependency runs pump -> audio in one direction; an include the other way makes it
mutual and blurs a seam for no benefit. Small but load-bearing: free to get right now, annoying to
unwind later.

### 3.3 The controller change (`usb_audio.c:400-428`)

The accumulator clamp (413-419) bounds the integral's magnitude but leaves the controller *pinned*
at the clamp for the whole episode, integrating against a physically unreachable target. Add
**conditional integration**: compute `authority_meaningful = supply_valid && supply_q8 >=
PL_FB_SUPPLY_OK_Q8` (230, ~0.90) and `pushing_into_clamp` from the unclamped ppm against the error
sign; integrate only when `authority_meaningful && !pushing_into_clamp`. The existing clamp stays as
a backstop. The P term keeps acting throughout — memoryless and harmless.

**Preserve the integrator across the episode; do not zero it.** The pre-episode integral is the best
estimate of the real crystal offset (~200-270 ppm per `nxf`). Zeroing means re-converging over ~11 s
after every episode, straight into the dry-ring regime `nxf` just fixed. Freezing is the point.

Counters `s_fb_degraded_ticks`, `s_fb_sat_ticks`, printed at 1 Hz so "the gate fired" is observable
and falsifiable.

## 4. Element (d) — surfacing it on the display

**Constraint: no new IRQ-context Rust call.** `pico-link-6o2` is an existing latent bug of exactly
that shape (`bt.c:102`); do not add a second. The `bt.c` MPSC ring is the correct vehicle:
`pl_bt_ring_push` (`bt.c:159-182`) runs its whole body inside `save_and_disable_interrupts`, is
already used from IRQ 0xFF by the a2dp packet handler, and only the superloop's `pl_bt_drain_events`
ever calls `pl_ui_push_event`.

**Producer: `a2dp.c` `pl_a2dp_media_timer_handler`, after `pl_a2dp_fill()` at line 1027.**
Deliberately *not* the superloop: at 6 Hz (`pico-link-p1r`) it is a poor debounce clock, and audio
health must not depend on render health. The media timer is ~100 Hz and runs only while a stream
exists — exactly when audio health is meaningful.

Edge-triggered only (a few events per minute): degraded-in when `supply_q8 < 192` (0.75) or mute
engaged, sustained ~300 ms; degraded-out after ~2 s of neither; clear on `STREAM_SUSPENDED` /
`STREAM_RELEASED`.

New helper matching the existing family: `void pl_bt_push_audio_health(uint32_t state, uint16_t
supply_q8);` — builds a `PlEvent`, calls `pl_bt_ring_push(event, NULL, 0)`, nothing else.

**FFI (`ui-ffi/src/lib.rs`):** `PlEventTag::AudioHealthChanged = 9`; `PlAudioHealthPayload { state:
u32, supply_q8: u16 }` (plain `Copy`, no pointers — same safety story as
`PlLinkStateChangedPayload`); union member; decode arm; `try_from` arm. **The out-of-range-tag test
at `ui-ffi/src/lib.rs:1301` hardcodes 9 as "one past the highest legal tag" — it must move to 10.**
Regenerate `firmware/include/pico_link_ui.h` via cbindgen; if the generated header does not change,
the change did not land.

**Core (`core/src/app.rs`):** `Event::AudioHealthChanged(AudioHealth)`; `BtModel::audio_health`
(`enum AudioHealth { Ok, Degraded { supply_q8: u16 } }`, `Default = Ok`) added **as a field**, per
`BtModel`'s stated "grows by adding fields, not FFI setters" rule at `app.rs:372-375`; `handle_event`
arm near line 727; cleared to `Ok` in `App::set_link_state` on any non-`Connected` state, exactly as
`connected_codec` is (`app.rs:~852`).

**Render (`core/src/render/home.rs:164-175`):** a distinct amber sub-line on the hero, **not**
reusing `CodecStatus::Connected { fallback }`. `fallback` means "the codec fell back and here is
why"; "the host is not delivering audio" is a different fact, and conflating them is the kind of
semantic special-case that looks cheap and later forces a branch at every read site. Wording and
visual treatment are Uma's call; the design commitment is the data and the slot. Placeholder:
`HOST AUDIO LOW`.

## 5. Sequencing — does any of this need `pico-link-p1r` first?

**No. Every element lands independently of p1r, by construction.**

- (a)/(b) live in the media timer, IRQ 0xFF. (c) lives in the 1 ms worker, IRQ 0xC0. Both preempt
  the superloop; a 6 Hz superloop is irrelevant to them.
- (d) touches the superloop only at the *drain* end. The producer is edge-triggered in the media
  timer and the `bt.c` ring has 31 usable slots, so a 170 ms iteration costs at most 170 ms of badge
  latency and cannot drop events.
- The one choice that *would* create a p1r dependency — putting the debounce/detector in the
  superloop — is explicitly ruled out. If a supervisor moves it there "for simplicity", the feature
  becomes hostage to p1r.

## 6. Child beads

| Bead | Scope | Deps |
|---|---|---|
| **2ap.1a — supply-health signal** | Windowed packets/SOF in `pl_usb_sof_isr_sample`, accessors in `usb_pump.h`, `supply_q8` on the `usb-sof-2ap` report line. Pure instrumentation, zero behaviour change. | none |
| **2ap.1b — PI saturation gating** | Signature change to `pl_usb_audio_feedback_task`, conditional integration, `fb_degraded_ticks` / `fb_sat_ticks`. | 2ap.1a |
| **2ap.1d — UI surface** | FFI tag 9 + payload + tests, `BtModel::audio_health`, `set_link_state` clearing, Home sub-line, `pl_bt_push_audio_health`, media-timer edge detector. | 2ap.1a |
| **2ap.1c — sustained-undersupply mute** | Regime B mute state machine, hysteresis, freshness trim on exit, `PL_CONCEAL_SUSTAINED_POLICY` A/B switch, mute counters. **Regime A is explicitly NO CHANGE.** | 2ap.1a |

**Order:** 1a alone first — small, risk-free, and it confirms the detector actually fires during a
real episode *before* any behaviour changes on the audio path. Then 1b and 1d in parallel (they
share only the accessor). Then 1c last: largest, and the only one touching the proven-audible LDAC
path.

**Risks**

- 1c is the real risk. A/B listen-tested by Andreas, with a hardware capture showing the mute
  counters and `supply_q8` across a full episode. "Tests pass and it builds" is not evidence here.
- Concealment can mask a genuine regression. The counters are the mitigation and are non-optional.
- Any acceptance check reusing the pbv-era conservation identity breaks until mute-silence frames
  are an explicit term.

## 7. What USBPods settled, and what it cannot

**Section 10 of the original 2ap design is closed with a null result.** USBPods implements no UAC2
feedback endpoint at all (`CFG_TUD_AUDIO_ENABLE_FEEDBACK_EP` undefined, speaker-only, no EP_IN);
`resamp44.c` is a fixed-ratio Bluetooth-side resampler, not host-clock adaptation. The section 5
list-scheduling hypothesis is **unreviewable** against this reference, not unreviewed — precise
wording matters so nobody reopens it as an outstanding task.

Their 13-line `rp2040_usb.c` patch only comments out the re-prime panic; they do **not** patch
`audio_device.c:759-762` or `usbd.c:356-360`. No relief from that direction; our `sdk-patches/`
remain ours alone.

**Caveat on transfer:** with no feedback endpoint, USBPods must tolerate permanent host-vs-device
clock drift, whereas we correct it. Their consumer is tuned for a plant that always drifts. That
weakens the transfer in principle — not enough to override a direct empirical listening result on
the concealment question, which is why their policy is adopted for regime A rather than argued around.

## 8. Live experiment: full-speed behind a TT hub

A note, not a design input. Because we are full-speed, a USB 2.0 high-speed hub interposes a
Transaction Translator and the host schedules split transactions on a structurally different
cadence — an independent mechanism by which **E3 could change the outcome rather than confirm it**.
That promotes E3 from control run to potential discriminator, and it is the cheapest outstanding
experiment on this bead.

Nothing here is designed around an assumed hub result; the regime split keys off measured supply
health and is correct for any host that under-delivers, for any reason. But if the hub *does* fix
it, this work stays needed as generic defensive handling while its priority drops below
`pico-link-p1r` — worth knowing before spending 1c's listening-test budget.

## 9. Closed hypotheses

**STM32 `SEVNFRM` / `SODDFRM` odd/even frame-parity lock** (raised 2026-09-01 from a search result):
refuted. Wrong silicon (RP2350 has no such registers), wrong direction (IN vs our OUT), and it
predicts a constant 50% from frame one with no recovery, whereas ours ramps in over ~30 s and fully
heals. Already killed independently via `rp2040_usb.c:169-191`. Do not re-raise.
