# Separating the link axis from the discovery axis

**Bead:** `pico-link-88xs` (P0). Related: `pico-link-zl75` (P1, NOT closed by this).
**Author:** Ada (architect). **Status:** design of record, unbuilt.
**Base commit:** `6355970` (verified).
**Implementer:** Ruby. This document is a blueprint; no code was changed to produce it.

---

## 1. The defect, precisely

`LinkState` is used as the carrier for two independent facts:

1. the A2DP connection lifecycle of `BtModel::connected_addr`, and
2. whether the BR/EDR radio is currently running a GAP inquiry.

A BR/EDR inquiry does **not** tear down A2DP. The link survives, audio keeps
playing. But:

- `firmware/src/bt.c:537` (`pl_bt_start_scan`) and `firmware/src/bt.c:976`
  (`PL_COMMAND_TAG_START_SCAN`) push `PL_LINK_STATE_SCANNING`.
- `core/src/app.rs:2367` `set_link_state` clears `connected_codec`,
  `connected_addr`, `out_level` and `ldac_live_kbps` on **any** state that is
  not `Connected`.
- `firmware/src/bt.c:649` (`GAP_EVENT_INQUIRY_COMPLETE`) then pushes
  `PL_LINK_STATE_IDLE` — unconditionally, with no knowledge of whether a link
  is up.
- The only producer of `PL_LINK_STATE_CONNECTED` is `a2dp.c:3154`, on a fresh
  `STREAM_ESTABLISHED`. Nothing re-asserts it for a link that was already up.

So one accidental scan permanently converts a live, playing link into `NO LINK`
on Home, and simultaneously hides the device page's `QUALITY` row and the OUT
meter, until the user disconnects and reconnects.

### 1.1 The stronger motivating case: the pairing flow wipes its own result

Found by the detective on `pico-link-zl75` after this document's first draft;
it is a **second, independent path into the same wipe**, and it is almost
certainly the one Andreas actually hits.

The wizard never cancels its own inquiry when the user picks a device:

- `core/src/render/wizard.rs:309-317` queues `Command::Connect` on a device-row
  activation and does **not** queue `CancelScan`. `CancelScan` is wired only to
  the wizard's B/cancel path (`wizard.rs:379`).
- `firmware/src/a2dp.c`'s `pl_a2dp_establish_stream_now` (2472-2492) does not
  stop the inquiry either.

So on a fresh pair the inquiry is still running while the connection completes:

```
DiscoveryStart -> ... -> Connect -> CodecChanged -> Connected -> ConnectSucceeded
                                                                        |
                       GAP_EVENT_INQUIRY_COMPLETE (the ORIGINAL scan) ---+--> LinkState(IDLE) -> WIPE
```

The pairing flow destroys the model it just populated, milliseconds after
producing it. That reframes the bug: this is not primarily "the user
accidentally started a scan later", it is a race inherent to the happy path,
which is why it looks like "I have never been able to see device settings"
rather than "it broke that one time".

**This design fixes it by construction, with no extra work.** Inquiry-complete
stops touching the link axis entirely (§7); it pushes
`DiscoveryStateChanged(Idle)`, which writes one `bool` and cannot clear
anything. The race still exists in the sense that the two completions still
interleave arbitrarily — it simply stops mattering, because the two events no
longer write the same field. That is the test of whether an axis split is real:
the ordering question dissolves rather than needing a guard.

`on_scan_ended_if_applicable` is also safe under this race: it is guarded on
`matches!(*phase, WizardPhase::Scanning { .. })` and by then the wizard has moved
to `Connecting`/`Succeeded`, so the late inquiry-complete is a no-op.

**Separate follow-up worth a bead (NOT this one):** the wizard arguably *should*
queue `CancelScan` alongside `Connect`. Not for model correctness any more — this
design removes that need — but for **radio hygiene**: a BR/EDR inquiry running
concurrently with A2DP setup and early streaming contends for the same radio and
is a plausible contributor to first-connection flakiness and early dropouts.
That is a measurable audio-quality question, not a UI one; do not fold it into
this bead, and do not let it be the fix.

### 1.2 Why the two obvious fixes are wrong

- **Rust-side "don't clear on `Scanning`"** — inert. Inquiry-complete pushes
  `Idle`, which still clears.
- **C-side "push the true link state at inquiry complete"** — breaks the pairing
  wizard. `core/src/app.rs:2194` `on_scan_ended_if_applicable` detects scan-end
  by `LinkStateChanged(Idle)`; if inquiry-complete starts pushing `Connected`
  for a connected user, `WizardPhase::Scanning` never advances to
  `NothingFound` and the wizard hangs on a spinner forever.

Both are attempts to patch a *value* on a field whose *meaning* is wrong. The
axis has to split.

---

## 2. The seam decision

### 2.1 `LinkState` loses `Scanning`

```rust
// core/src/app.rs
pub enum LinkState {
    #[default]
    Idle,
    Connecting,
    Connected,
}
```

This is the load-bearing move, and it is deliberately a *narrowing*, not an
addition. The rule at `set_link_state` — "not `Connected` means the connected
model is gone" — is not relaxed. It is left **verbatim** and made *true* by
removing the one value that made it a lie. After this change every remaining
non-`Connected` value genuinely means the A2DP link is not up, so the clear is
correct by construction rather than by convention.

Contrast with the alternative of keeping `Scanning` in the enum and teaching
`set_link_state` to skip the clear for it: that leaves a value in the type whose
only correct handling is "ignore me", which is precisely the shape that invited
this bug and would invite the next one.

### 2.2 Discovery becomes a second, independent model field

```rust
// core/src/app.rs, on BtModel
/// Whether the radio is currently running a GAP inquiry. Deliberately a
/// SEPARATE axis from `link_state` (bead pico-link-88xs): an inquiry does
/// not disconnect A2DP, so scanning must never be expressible as a
/// `LinkState`. Written only by `App::set_discovering`.
pub discovering: bool,
```

**`bool`, not a `DiscoveryState` enum.** There are exactly two observable
states and no producer for a third — `gap_inquiry_stop()` synthesises a normal
`GAP_EVENT_INQUIRY_COMPLETE`, so there is no distinct "cancelling" phase to
model (`bt.c:547` documents this). An enum here would be gold-plating on a PoC.
This is a cheap, contained, easily-reversed choice: promoting `bool` to an enum
later is a mechanical change confined to one field and one setter.

Note the asymmetry with §2.3 below, which *does* use an enum across the FFI.
That is intentional and not an inconsistency: the wire needs room to grow
without an ABI event, the in-memory model does not.

### 2.3 What crosses the FFI

**A new, additive event tag.** Discovery does not ride on `LinkStateChanged`.

```c
/* firmware/include/pico_link_ui.h (cbindgen output; edit ui-ffi/src/lib.rs) */
typedef enum PlDiscoveryState {
    PL_DISCOVERY_STATE_IDLE     = 0,
    PL_DISCOVERY_STATE_SCANNING = 1,
} PlDiscoveryState;

typedef struct PlDiscoveryStateChangedPayload {
    uint32_t state;   /* a PlDiscoveryState value, carried as plain u32 */
} PlDiscoveryStateChangedPayload;

PL_EVENT_TAG_DISCOVERY_STATE_CHANGED = <next free>,
```

Shape copied exactly from `PlLinkStateChangedPayload`: a named C enum for
readability at the call sites, a plain `u32` on the wire so a garbage
discriminant is never UB to read, and a checked `TryFrom<u32>` in
`pl_ui_push_event` that increments `PlUi::malformed_tag_count` and returns on a
bad value (`ui-ffi/src/lib.rs:1644-1656` is the pattern to mirror).

An enum rather than a `uint8_t scanning` flag because the enum costs nothing
today and absorbs a future `PL_DISCOVERY_STATE_STARTING` (or an LE-scan variant)
without touching the payload struct — an additive value in an existing nested
enum, which this codebase has already established as free.

### 2.4 The ordinal, and the collision with `pico-link-9eq2.3.1`

**Read the merged header and take `max + 1`. Do not hardcode a number from this
document.**

Measured on `6355970`: `PL_EVENT_TAG_LDAC_BITRATE_CHANGED = 15` is the highest
tag, taken by `pico-link-7jol.5`.

`.planning/design/2026-09-07-audio-fault-model.md` and its INDEX row both say
`AudioFault` is "event tag 15". That document was written before `7jol.5` merged
tag 15, so 15 was already gone. **Confirmed by the orchestrator after this
document's first draft: the fault epic took 16, and `PL_EVENT_ABI_VERSION`
stayed at 5** — the additive-tag discipline held.

So: **`PL_EVENT_TAG_DISCOVERY_STATE_CHANGED = 17`.** Still re-check the merged
header before writing it; if `AudioFault` has somehow landed as 15 by
renumbering `LdacBitrateChanged`, that is a silent wire break on an existing
producer — stop and escalate rather than working around it.

### 2.5 `PL_EVENT_ABI_VERSION` does **not** move (stays 5)

Two changes, two arguments, same conclusion:

- **Adding `DiscoveryStateChanged`** is purely additive. Established precedent
  in the header itself: tags 13, 14 and 15 were each added without a bump
  (`pico_link_ui.h:278`, `:285`, `:294`). No bump.
- **Removing `PL_LINK_STATE_SCANNING = 1`** is a narrowing of a nested enum's
  accepted value space, not a layout change. The version guard is the *wrong*
  instrument for it, and actively worse than the alternative: on a version
  mismatch `pl_ui_push_event` returns silently and **every** event in the stream
  vanishes with no counter moved (`ui-ffi/src/lib.rs:1622`). Leaving the version
  alone means a stale producer of `state == 1` is rejected by
  `PlLinkState::try_from` and **counted** in `malformed_tag_count` — one bad
  event, observably. Given this project's history with silent event drops, the
  counted failure is strictly the better one.

Concretely: leave the discriminants punched — `Idle = 0`, **`1` reserved and
rejected**, `Connecting = 2`, `Connected = 3`. Do **not** renumber. Renumbering
buys nothing and turns a compile-checked change into a wire change. Document the
hole in `PlLinkState`'s doc comment with this bead ID.

### 2.6 What stays derived

Nothing new crosses the FFI beyond the one tag. Specifically:

- C does **not** push a composite "connected AND scanning" state. See §3.
- C does **not** learn about `connected_codec` / `out_level` / `ldac_live_kbps`
  lifecycle. Those stay Rust-side folds off their own events.
- The `Screen` chrome's glyph colour is **derived in `core`** from the two model
  fields at contribution time. See §5.

---

## 3. Who owns "the link is up"

**C stays authoritative for the link axis, exactly as today. C does not become
authoritative for a composite.**

Rejected: "C pushes the true link state at inquiry complete." It requires `bt.c`
to know a2dp.c's connection state at inquiry-complete time, which means a second
copy of connection truth living in the BT layer, kept in sync by hand — a
classic duplicated-state seam, and the kind of thing that stays subtly wrong for
months. It also does not fix the class of bug, only this instance: the next
non-link event that borrows `LinkState` re-opens it.

Rejected too: "Rust stops treating `!= Connected` as everything-is-gone." That
weakens a correct honesty rule to compensate for a lying input. The input is
what is wrong.

### 3.1 The invariant

> **INVARIANT L1.** `LinkState` describes exactly one thing: the A2DP connection
> lifecycle of `BtModel::connected_addr`. No producer may emit a `LinkState`
> for any other reason, and "the radio is scanning" is **not representable** as
> a `LinkState`.

Enforcement is structural, not documentary:

- `LinkState::Scanning` and `PlLinkState::Scanning` are **deleted**. A C site
  that tries to say "scanning" via this event does not compile (it has no
  `PL_LINK_STATE_SCANNING` constant); a stale prebuilt object that pushes the
  raw `1` is rejected and counted, and **cannot** wipe the model. The bad
  outcome is not "unlikely", it is unreachable through the type.

> **INVARIANT L2.** `BtModel::link_state` has exactly **one** writer:
> `App::set_link_state`. `BtModel::discovering` has exactly one writer:
> `App::set_discovering`.

This one is currently violated. `core/src/app.rs:2531`
(`App::record_connect_failure`) assigns `self.model.link_state = LinkState::Idle`
directly, bypassing the clear. It is benign today only because the preceding
`Connecting` push already cleared the four fields — i.e. it is correct by
accident. **Fix it as part of this bead:** call
`self.set_link_state(LinkState::Idle)` and drop the now-redundant second
`refresh_stack()` (or keep it; `refresh_stack` is idempotent, but one call is
the intent). With L2 held, "a field was cleared/not cleared" has exactly one
place to read.

Together L1 and L2 give the property asked for: after this change there is no
code path that can clear the connected model for a non-link reason, because
there is no non-link reason that can reach `set_link_state`.

---

## 4. The four cleared fields: which honesty rules move?

**None of them move. All four stay keyed to the old rule, unchanged.**

| Field | Bead | Doc-comment guarantee | After this design |
|---|---|---|---|
| `connected_codec` | `pico-link-1v5` | a stale codec word surviving a disconnect is worse than `NO LINK` | unchanged — still cleared on any non-`Connected` |
| `connected_addr` | `pico-link-4vb.4` T5 | same lifecycle as the codec | unchanged |
| `out_level` | `pico-link-du0` | absent, never frozen | unchanged |
| `ldac_live_kbps` | `pico-link-7jol.5` | only meaningful for the codec it was measured on | unchanged |

This is the payoff of narrowing the type rather than special-casing the clear:
every one of these four guarantees keeps firing on a real disconnect and none of
them fires on a scan, and **not one line of the clear block changes**. Only the
doc comment on `set_link_state` needs an amendment (drop the parenthetical
"(or a scan/connect that reuses the link before a fresh `Event::CodecChanged`
arrives)", which describes the bug as if it were a feature, and add the
`pico-link-88xs` note that `LinkState` no longer carries a scan).

`set_connected_codec`'s own separate clear of `ldac_live_kbps` on a non-LDAC
renegotiation (`app.rs:2395-2399`) is untouched and still needed.

---

## 5. Rendering: the chrome glyph now has two inputs

`ChromeContribution::link: Option<LinkState>` (`core/src/render/widget.rs:181`)
is set at exactly **one** site, `home.rs:453`. `draw_link_glyph`
(`screen.rs:71-76`) maps it to three colours:

| Colour | Today |
|---|---|
| `BRAND_BRIGHT` | `Connected` |
| `STATUS_WARNING` | `Scanning` \| `Connecting` |
| `TEXT_SECONDARY` | `Idle` |

With `Scanning` gone the glyph needs both axes. **Recommended: map domain to
presentation at the contributing widget**, which is the render layer's existing
convention (`home.rs` already maps `connected_codec` → `CodecStatus` and
`out_level` → `OutLevelDisplay` rather than handing raw model types down).

```rust
// core/src/render/chrome.rs (or widget.rs, next to ChromeStatus)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkGlyph { Idle, Busy, Live }

// ChromeContribution
pub link: Option<LinkGlyph>,
```

`home.rs::chrome_contribution` computes it:

```rust
let glyph = match (self.link_state, self.discovering) {
    (LinkState::Connected, _)            => LinkGlyph::Live,
    (LinkState::Connecting, _) | (_, true) => LinkGlyph::Busy,
    (LinkState::Idle, false)             => LinkGlyph::Idle,
};
```

`HomeView` gains a `discovering: bool` snapshot field alongside its existing
`link_state` snapshot (`home.rs:159`), populated in `HomeView::new` from
`model.discovering` next to `home.rs:273`.

Note this deliberately overturns `ChromeContribution::link`'s doc comment
("Reuses `LinkState` ... the chrome doesn't need a fifth concept of link
state") — but it does so *on that comment's own stated principle*, which is that
independent axes of state must be able to read differently on screen at once.
There are now two domain axes feeding one glyph; the chrome should receive the
resolved answer, not both inputs. Secondary benefit: `screen.rs` stops importing
`crate::app::LinkState`, removing a domain dependency from the render layer.

**Fallback if this ripples further than expected:** add
`ChromeContribution::discovering: bool` beside the existing `link` field and
fold both. Strictly worse (it re-conflates at the presentation layer and doubles
the paint-key fold), but it is a one-file change and severable. Do not silently
take this path — say so in the bead if you do.

### 5.1 Paint key (do not skip this)

`title_paint_key` (`screen.rs:174-200`) folds `link` as a 0..4 integer. It must
fold the **`LinkGlyph`**, i.e. exactly the value that decides the drawn colour —
not `LinkState` and `discovering` separately, and not `discovering` alone. This
is the "damage keys must fold only what is drawn" rule; folding both inputs
would reinstate a repaint every time `discovering` flips without changing a
pixel (e.g. a scan while already `Connected`, where the glyph is `Live` either
way and must **not** repaint).

New fold: `None => 0, Some(Idle) => 1, Some(Busy) => 2, Some(Live) => 3`.

The four existing tests at `screen.rs:933-963` need renaming/retargeting to the
glyph vocabulary; `scanning_link_state_paints_the_glyph_in_the_warning_color`
becomes a `LinkGlyph::Busy` test.

---

## 6. The pairing wizard, rewired

`App::handle_event` (`app.rs:2136-2140`) currently does:

```rust
Event::LinkStateChanged(state) => {
    self.set_link_state(state);
    self.on_scan_ended_if_applicable(state);
}
```

becomes:

```rust
Event::LinkStateChanged(state) => self.set_link_state(state),
Event::DiscoveryStateChanged { scanning } => self.set_discovering(scanning),
```

with

```rust
/// Records whether the radio is running an inquiry. The SECOND, independent
/// axis (bead pico-link-88xs) -- deliberately does NOT touch `link_state`
/// and does NOT clear any connected-model field: an inquiry does not
/// disconnect A2DP.
pub fn set_discovering(&mut self, scanning: bool) {
    self.model.discovering = scanning;
    if !scanning {
        self.on_scan_ended_if_applicable();
    }
    self.refresh_stack();
}
```

and `on_scan_ended_if_applicable` losing its `state: LinkState` parameter and
its `if state != LinkState::Idle { return; }` guard (`app.rs:2194-2197`) — the
caller now owns that condition, and it is the *right* condition.

**This is strictly more correct than what it replaces.** Today a connect
failure, or a disconnect that happens to land while the wizard is on
`WizardPhase::Scanning`, pushes `Idle` and spuriously flips the wizard to
`NothingFound`. After the change only a genuine `GAP_EVENT_INQUIRY_COMPLETE`
can.

### 6.1 The two things that must not regress

- **`WizardPhase::Scanning`'s pending-timestamp backfill.**
  `stamp_pending_wizard_timestamp` (`app.rs:2173`) is called *unconditionally at
  the end of* `handle_event`, after the `match`. The new arm is inside that
  match, so it participates automatically. **Do not add an early `return` in the
  `DiscoveryStateChanged` arm** — that is the only way to break this, and it
  would break it silently (the phase would render `PENDING_TIMESTAMP` as its
  start time).
- **The `DevicesCleared` pairing.** `bt.c` pushes `DevicesCleared` immediately
  before the scan-start push, at both producer sites (`bt.c:536-537` and
  `bt.c:975-976`). Keep that order and that adjacency: `DevicesCleared` clears
  `wizard_devices`, and `on_scan_ended_if_applicable`'s
  `wizard_devices.borrow().is_empty()` check is what distinguishes "found
  nothing" from "found something". Swapping the order would make a stale
  previous-scan list survive into the new scan's emptiness test.

---

## 7. Firmware changes (`firmware/src/bt.c`)

Five call sites, all mechanical. **No new IRQ-context calls into Rust** — every
one of these goes through `pl_bt_ring_push`, exactly as the existing
`pl_bt_push_link_state` does, and `pl_bt_drain_events` from the superloop
remains the sole caller of `pl_ui_push_event` (`pico-link-6o2` stays fixed).

Add a helper next to `pl_bt_push_link_state` (`bt.c:234`):

```c
static void pl_bt_push_discovery_state(enum PlDiscoveryState state) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_DISCOVERY_STATE_CHANGED,
        .payload = {.discovery_state_changed = {.state = state}},
    };
    pl_bt_ring_push(event, NULL, 0);
}
```

| Site | Today | After |
|---|---|---|
| `bt.c:537` (`pl_bt_start_scan`, IRQ ctx) | `pl_bt_push_link_state(SCANNING)` | `pl_bt_push_discovery_state(SCANNING)` |
| `bt.c:976` (`PL_COMMAND_TAG_START_SCAN`, thread ctx) | `pl_bt_push_link_state(SCANNING)` | `pl_bt_push_discovery_state(SCANNING)` |
| `bt.c:649` (`GAP_EVENT_INQUIRY_COMPLETE`) | `pl_bt_push_link_state(IDLE)` | `pl_bt_push_discovery_state(IDLE)` |
| `bt.c:333` (`pl_bt_push_link_state_disconnected`) | `pl_bt_push_link_state(IDLE)` | **unchanged** — a real disconnect |
| `bt.c:1005`, `bt.c:1145` (connect / debug-connect) | `pl_bt_push_link_state(CONNECTING)` | **unchanged** |

After this, `bt.c:649` no longer touches the link axis at all — which is the
whole point: inquiry-complete has no opinion about the A2DP link, and now has no
way to express one.

Also update `bt.c:547`'s comment block about `pl_bt_cancel_scan_radio` relying on
`GAP_EVENT_INQUIRY_COMPLETE` — the mechanism survives, the pushed event changes.

### 7.1 Ordering note

The two axes travel in the same MPSC ring and are drained in order, so a
`DiscoveryStateChanged` can never overtake a `LinkStateChanged` from the same
producer. Because they now write disjoint model fields, interleaving across
producers is harmless — that is the structural benefit of the split.

---

## 8. Blast radius

Every reader, from `grep` over `core/src`, `ui-ffi/src`, `emulator/src` at
`6355970`.

### 8.1 `link_state` — 6 non-test sites

| Site | Change |
|---|---|
| `core/src/app.rs:126-132` `enum LinkState` | drop `Scanning` |
| `core/src/app.rs:2367` `set_link_state` | body unchanged; doc comment amended |
| `core/src/app.rs:2531` `record_connect_failure` | route through `set_link_state` (invariant L2) |
| `core/src/render/home.rs:159, 273, 313, 453` `HomeView::link_state` | keep, add `discovering`, emit `LinkGlyph` |
| `core/src/render/screen.rs:71-76, 174-200, 765` glyph + paint key | take `LinkGlyph`; drop the `LinkState` import |
| `core/src/render/widget.rs:169-181` `ChromeContribution::link` | type + doc comment |

### 8.2 The four cleared fields — **zero semantic changes**

All readers below keep working unchanged; they simply stop being lied to.

- `connected_codec`: `home.rs:215`; `app.rs:1493` (`device_page_quality_present`),
  `1533` (`device_page_quality_row_value`), `1577` (`device_page_rows` CODEC),
  `1873` (picker's Adaptive note).
- `connected_addr`: `app.rs:1149, 1151, 1163` (devices list),
  `1180, 1203` (device rows), `1492, 1571, 1631, 1872`; `home.rs:166, 274, 314, 410`.
- `out_level`: `app.rs:2443, 2469, 2479` (ballistics); `home.rs:251`;
  `hero.rs:339-391, 867, 976, 1096`.
- `ldac_live_kbps`: `home.rs:240`; `app.rs:1534, 1888, 2423`.

### 8.3 FFI — `ui-ffi/src/lib.rs`

`PlLinkState` enum + `TryFrom` (`:905-929`), `From<PlLinkState> for LinkState`
(`:931-940`), new `PlDiscoveryState` + payload + union member + `PlEventTag`
variant + `TryFrom` arm + the `pl_ui_push_event` match arm (`:1642` region).
`firmware/include/pico_link_ui.h` regenerated by cbindgen.

### 8.4 Tests that will fail and must be updated, not deleted

`app.rs:3764, 3770, 4049`; `screen.rs:933-963` (four glyph tests);
`ui-ffi/src/lib.rs:2408-2464, 2517-2660, 2963, 3086-3110`. Several of these
push `PlLinkState::Scanning`; retarget them at `PlDiscoveryState`.

### 8.5 New tests this bead owes

1. **The regression itself.** `Connected` + codec + level + kbps, then
   `DiscoveryStateChanged(scanning)`, then `DiscoveryStateChanged(idle)` —
   assert all four fields survive and `link_state` is still `Connected`. This is
   the test whose absence let the bug ship.
2. A real disconnect still clears all four (guard against over-correcting).
3. Wizard: `WizardPhase::Scanning` + empty devices + `DiscoveryStateChanged(idle)`
   → `NothingFound`; and `LinkStateChanged(Idle)` alone **no longer** triggers it.
4. Pending-timestamp backfill fires on a `DiscoveryStateChanged` event.
5. Paint key: glyph key is **unchanged** when `discovering` flips while
   `Connected` (§5.1), and changes when it flips while `Idle`.
6. FFI: raw `PlLinkStateChangedPayload { state: 1 }` is rejected and bumps
   `malformed_tag_count`.

### 8.6 Emulator

No `link_state` / `discovering` references in `emulator/src` — the emulator
drives the model through `App`, so it needs no change beyond whatever fixture
code constructs a `BtModel`.

---

## 9. Does this close `pico-link-zl75`? **No.**

It closes **one confirmed cause** of zl75's connected branch: a scan wiping
`connected_codec` while connected, after which
`device_page_quality_present` (`app.rs:1491`) returns `false` and the `QUALITY`
row vanishes from a device that is streaming LDAC.

It does **not** close zl75, for two reasons:

1. **The disconnected branch is untouched.** `ldac_quality != 0` is still a
   chicken-and-egg: the only way to make the row appear is to have used the row.
   That needs a UX ruling against
   `.planning/design/2026-09-07-ldac-quality-selector.md` §§2/7/8's
   absent-not-dim and no-first-run-prompt decisions. Not this bead. Not designed
   here, deliberately.

2. **Measured finding that narrows zl75, banked here so it is not re-derived:**
   zl75's stated open question — "whether A2DP `SET_CONFIGURATION` (and
   therefore `CodecChanged`) fires at CONNECT time or is deferred until audio
   actually starts" — is **answered, and the deferred hypothesis is refuted**.
   `pl_bt_push_codec_changed` is called at `a2dp.c:2419`, the tail of
   `pl_a2dp_finish_codec_negotiation`, reached from the
   `SIGNALING_MEDIA_CODEC_SBC/OTHER_CONFIGURATION` subevents (`a2dp.c:2859`,
   `a2dp.c:2898`) — pure AVDTP signaling, strictly **before** `AVDTP_OPEN`.
   `Event::ConnectSucceeded`, which writes `connected_addr`, is pushed at
   `A2DP_SUBEVENT_STREAM_ESTABLISHED` (`a2dp.c:3152-3155`), and `a2dp.c:3135-3142`
   records that it was deliberately moved **off** `STREAM_STARTED` precisely
   because that one needs the host to begin streaming, which is outside our
   control (bead `pico-link-4vb.2`). Neither push requires any PCM to flow.
   So on the happy path the drain order is
   `Connecting` (clears) → `ConnectStepChanged` → `CodecChanged` (sets codec) →
   `Connected` (does not clear) → `ConnectSucceeded` (sets addr). **Streaming
   audio is not required for `connected_codec` to be populated.** Whatever else
   zl75 is, it is not a deferred-`CodecChanged` problem.

Recommendation: after this bead merges, re-test zl75 on hardware. Note that
§1.1's pairing-flow race means "without scanning first" was **never actually
achievable** on a fresh pair — the wizard's own inquiry was always still in
flight — so the connected branch has plausibly *never* worked for a freshly
paired device. Expect this fix to close most of zl75's observed symptom. What
survives is the disconnected branch, ruled on in §12.

---

## 10. Conflicts with `pico-link-9eq2.3.1` (concurrent)

That bead is additive on the same three files. Expected conflicts, all
mechanical:

- **`ui-ffi/src/lib.rs`** — both add a `PlEventTag` variant, a union member, a
  `TryFrom` arm and a `pl_ui_push_event` match arm. Textually adjacent, not
  semantically conflicting. **Land `9eq2.3.1` first**, then take `max + 1`
  (§2.4). Neither bumps `PL_EVENT_ABI_VERSION`, so there is no version conflict.
- **`core/src/app.rs`** — `9eq2.3.1` adds a `FaultLog` field to `App` and a
  `handle_event` arm; this bead adds a `BtModel` field and a `handle_event` arm.
  Different structs (`App` vs `BtModel`), adjacent lines in the same `match`.
- **`core/src/run.rs`** — this design does not touch it. No conflict.
- **Home's fault strip** reads `FaultLog`, not `link_state`. The
  `ChromeContribution::link` type change (§5) is in `widget.rs`/`screen.rs`,
  which `9eq2.3.1` should not be editing — **verify before merging**; if it is,
  the `LinkGlyph` change is the severable part (§5 fallback).

---

## 11. Sustainability verdict

This is the cheap-and-right path, not a shortcut. The expensive alternative —
folding `connected_addr`/`connected_codec`/`out_level`/`ldac_live_kbps` into a
single `Option<ConnectedLink>` so that connected data is *unrepresentable*
without a connection — is the strictly stronger design, and I am explicitly
**not** recommending it for this bead:

- It touches ~30 read sites and every test that hand-builds a `BtModel`, in a
  file another agent is editing right now, to fix a P0.
- Invariants L1 + L2 already make the specific bug class unreachable; the
  refactor would buy defence against a *different* class (a future field that
  forgets to join the clear block).
- Crucially, **the refactor is a pure superset of this design** — it would
  consume `set_link_state`'s clear block, which this design leaves untouched. No
  rework is created by deferring it.

File it as a follow-up (`Fold the connected-link fields into
Option<ConnectedLink>`, related to `pico-link-88xs`) and do it the next time a
fifth field wants to join that clear list. That is the trigger.

---

## 12. Ruling requested: seeding `ldac_quality` on first LDAC negotiation

**Asked of me after the first draft. Adjacent, genuinely separate — it belongs
in its own bead, and part of it is Uma's to rule on, not mine. Below is the
split.**

The proposal (from the `zl75` detective): make
`device_page_quality_present`'s disconnected branch reachable by seeding
`PairedDevice::ldac_quality` on the first successful LDAC negotiation, rather
than only on manual picker use.

### 12.1 The architectural half — **my ruling, and it is a no as stated**

`ldac_quality` is a **user-intent** field. Its doc comment
(`core/src/app.rs:998-1005`) and its wire contract are explicit: `0` = *never
chosen*, `1`/`2`/`3` = *pinned* 990/660/330, `4` = *Adaptive chosen*. The
`Command` that writes it is documented "User-initiated" (`app.rs:277`).

Writing a value into it because the *radio negotiated LDAC* overloads a
user-intent field with a device-capability observation. **That is the same
defect this entire document exists to fix**, one field over: two independent
facts sharing one carrier, where the reader can no longer tell which one it is
looking at. I will not sign off on introducing it while removing it elsewhere.

Two concrete costs, not just a principle:

1. **It is not observably inert at the encoder seam.** `a2dp.c:2396` re-reads
   the stored `ldac_quality` on *every* connection and calls
   `pl_codec_ldac_set_quality(...)` with it (bead `pico-link-7jol.3`). Seeding
   would change that call from `0` (unset/default) to `4` (Adaptive) on every
   subsequent connect. Those two are *plausibly* equivalent — and that is
   exactly the shape of an assumption this project has been burned by.
   `pl_ldac_quality_to_initial_state`'s `0` vs `4` behaviour is **unverified**;
   it would have to be measured, not asserted, before anyone claims the seed is
   free.
2. **It corrupts the picker's own check mark.** `ldac_quality_checked_key`
   (`app.rs:1473-1480`) renders the stored echo as the selected radio row. After
   a seed, a device the user has never configured would open its picker showing
   `Adaptive` **checked**, indistinguishable from a device where the user
   deliberately chose Adaptive. `.planning/design/2026-09-07-ldac-quality-selector.md`
   §5.1's "stored echo, never the local press" rule makes that check mark a
   claim about the user, and it would become false.

**If the UX ruling (§12.2) is that the row should appear, the sustainable
mechanism is a separate bit, not a seeded pin.** A `PairedDevice::ldac_seen:
bool` (or a flags byte in `persist.c`'s device record, which is where a second
one-bit fact should go) records the capability observation on its own axis;
`device_page_quality_present`'s disconnected branch becomes
`ldac_seen || ldac_quality != 0`; `ldac_quality` keeps meaning exactly what it
means today; the encoder seam and the check mark are both untouched. That costs
one persisted bit and one store-schema revision — real, but small and
non-load-bearing, and it is the version that does not have to be unpicked later.

### 12.2 The product half — **Uma's, and I am stopping here**

Whether a paired-but-disconnected device the user has never configured *should*
show a `QUALITY` row at all is a UX decision, and it is already a decided one:
`.planning/design/2026-09-07-ldac-quality-selector.md` §§2/7/8 rule
absent-not-dim and explicitly no-first-run-prompt, and `app.rs:1487-1490`
implements that ruling faithfully. The chicken-and-egg is not an implementation
slip — it is the designed behaviour, and it may still be the right one now that
§1.1 explains why the *connected* branch was the real failure.

I am not proposing an amendment to it, because I do not have the input that
would justify one: whether Andreas actually wants to configure a headset that is
not currently connected. **Take that question to Uma with §1.1 attached** — the
honest framing is "the connected path was broken all along; now that it works,
is the disconnected path still a problem worth changing the design for?" If Uma
rules yes, §12.1's `ldac_seen` bit is the mechanism. If she rules no, `zl75`
closes on this bead's fix alone.

**Do not implement past this section in either direction.**
