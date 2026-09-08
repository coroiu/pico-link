# LDAC quality: the four-entry selector, and what Adaptive looks like while it runs

Bead `pico-link-7jol.2` (UX), under epic `pico-link-7jol`.
Author: Uma (UX designer), 2026-09-07.

**GOAL.** Andreas, 2026-09-07: he wants LDAC to adapt to the link, *and* to
override that from the device when he does not like what it picks. This
document settles how the choice is presented, chosen, confirmed, persisted and
— the part the bead really turns on — **what the user sees while Adaptive is
running and steps down mid-call.**

It does **not** design the controller. That is Ada, `pico-link-7jol.1`.

Documents this depends on or amends:

- `.planning/design/2026-09-02-device-page.md` — the device page and the picker
  mechanic. **This document amends its §3.2** (the `LDAC QUALITY` row) in four
  places, each marked **AMENDMENT** below. Everything else there stands.
- `.planning/design/2026-09-01-home-alignment-grid.md` — the Home grid and the
  bitrate band at `y 92..108`, `BITRATE_SLOT_WIDTH = 120` at `L = 12`.
- `.planning/design/2026-08-28-on-device-ui.md` — design of record; §15's
  no-faked-numbers rule is load-bearing here.
- `.planning/design/2026-09-02-field-list-widget-ruling.md` — Fern's row kinds.

**Settled by Andreas, not re-opened here:** a manual pick **pins** the EQMID and
ABR never overrides it. Adaptive is therefore a fourth peer entry, not a
modifier on the other three. Ceiling semantics were proposed and rejected.

---

## 1. The one-sentence shape

> **One row, `QUALITY`, on the device page, opening a four-entry single-select
> picker that applies live on `A` and stays open so you can audition. What you
> chose lives in the check gutter; what you are actually getting lives in the
> number, on Home, always, in both fixed and Adaptive modes.**

The whole design is that last split. **Check = intent. Number = reality.** It
is the same separation the `CODEC` row already makes between the pin and the
effective codec, and reusing it is why this feature needs no new vocabulary.

---

## 2. Where it lives, and how it is reached

Unchanged from the device-page design: row 2 of the device page, present only
when the effective-or-pinned codec is LDAC, `A` opens a depth-2 picker, `B`
returns.

| Gesture | From | Result |
|---|---|---|
| `X` | Home (menu face) | Device page for the connected device, depth 1 |
| `A` / `X` on a row | Devices | Device page, depth 1 |
| `A` on `QUALITY` | Device page | Quality picker, depth 2 |
| `A` on a picker row | Quality picker | Applies live; picker **stays open** |
| `B` | Quality picker | Device page |
| `B` | Device page | Back |

There is **no** Home-level shortcut to quality and there must not be one. Every
face button on Home is one press from a mid-call quality change, and a
mis-press that audibly degrades a call in progress is the worst accidental
activation this product can have. Two deliberate descents is correct friction.

### 2.1 AMENDMENT 1 — the label is `QUALITY`, not `LDAC QUALITY`

The row directly above says `CODEC … LDAC`, and the row only exists at all when
LDAC is in play. `LDAC` in the label is the same word twice within 28 vertical
pixels, and it costs ~35px of the 182px measure — the exact budget this row
needs back once its value grows a live number (§4.2).

---

## 3. Submenu, not a cycling field

The bead asks. It is a **submenu**, and one reason is decisive and specific to
this setting:

1. **Cycling applies the values you are passing through.** Quality applies live
   to a running stream (§5). A field that cycles `990 → 660 → 330 → Adaptive`
   makes reaching the fourth entry from the first *audibly* pass through two
   values you did not want, at real cost — an EQMID walk each time. A picker
   applies exactly one value: the one you pressed.
2. **`A` would mean two things on one page** — "descend" on `CODEC`, "change the
   value in place" on the row below it. Press-edge-only input has no second
   gesture to disambiguate with.
3. **Four options are not discoverable by cycling.** You learn there is a fourth
   only by pressing three times past the one you wanted.

The picker is identical in mechanics to the codec picker: leading check gutter,
label, trailing note, focus lands on every row.

---

## 4. The four entries, and what they read

### 4.1 The picker

```
+----------------------------------------+-----+
| Quality                                |     |   y 0..15
+----------------------------------------+  A  | pick
| v 990 kbps                  best audio |     |   y 28..55
|   660 kbps                    balanced |     |   y 56..83
|                                        +-----+
|   330 kbps               most reliable |  B  | back
|   Adaptive                     660 now |     |   y 112..139
|                                        +-----+
|                                        |     |
|                                        |     |
|                                        +-----+
|                                        |     |
|                                        |     |
+----------------------------------------+-----+
 ^L=12                                R=194^
```

| Order | Label | Trailing note | `ldac_quality` |
|---|---|---|---|
| 1 | `990 kbps` | `best audio` | 1 |
| 2 | `660 kbps` | `balanced` | 2 |
| 3 | `330 kbps` | `most reliable` | 3 |
| 4 | `Adaptive` | `660 now` while streaming, `varies` otherwise | 4 |

**Numbers lead, Sony's mode names are absent.** `HQ` / `SQ` / `MQ` are the
vendor's internal names; on our panel they are three two-letter codes a user
must learn a mapping for, to reach a number that is already the thing they
compare. Andreas's ruling names the entries "HQ 990 / SQ 660 / MQ 330" as
*identification*, not as label copy. If the mode names are ever wanted they
belong in the trailing note, not the label — but `best audio` / `balanced` /
`most reliable` say the consequence, and the consequence is what a user picking
under a bad link actually needs.

**Descending order, highest first.** It matches the mental model (the top of the
list is the top of the range), it puts the default under the initial focus, and
Adaptive is last because it is the odd one out — the only entry that is not a
number.

**Adaptive's trailing note is live and is the second-best answer to the bead's
central question.** `660 now` mirrors the codec picker's existing `SBC now`
verbatim, so the vocabulary is already taught. It updates as ABR steps.

### 4.2 The row on the device page

**AMENDMENT 2 — the row's trailing value carries the live rate under Adaptive.**
The device-page design has it render the flat word `Adaptive`. That is no longer
enough: Andreas's stated need is to see what ABR picked so he can decide whether
to override it, and the device page is where the override lives. Making him
back out to Home to read the number and descend again to act on it is a
four-press round trip through the exact screen the decision is made on.

| State | Trailing value |
|---|---|
| Fixed pick, any link state | `990 kbps` / `660 kbps` / `330 kbps` |
| Adaptive, streaming LDAC | `Adaptive · 660` |
| Adaptive, not streaming | `Adaptive` |
| Never chosen (`ldac_quality == 0`) | the effective default, as a fixed value — see §7 |

`Adaptive · 660` is 14 characters ≈ 98px in `font::value`, against a 182px
measure shared with a `QUALITY` label of ~35px. Comfortable, and comfortable
*because* of AMENDMENT 1. **Ruby must measure it**; the fallback if it overruns
is `Adaptive 660`, and the fallback is never a smaller font — the number is the
payload of the row.

No `kbps` unit inside the row's Adaptive form: the unit is stated on all three
fixed entries, in the picker, and on Home, and a fourth statement of it inside a
compound value costs 35px to teach nothing.

---

## 5. Commit semantics: live on press, no confirm, picker stays open

`ldacBT_alter_eqmid_priority` is callable at any time post-init (epic bead), so
unlike a **codec** pin — which needs an A2DP renegotiation and therefore a
reconnect confirm — a quality change **takes effect on the running stream**.
The UX must spend that.

> **RULING. `A` applies the pick immediately. There is no confirm, no reconnect
> prompt, and the picker does not pop. `B` is the only way out.**

**Why it stays open when the codec picker pops.** The rule is not "pickers pop"
or "pickers stay"; it is:

> **A picker pops when its effect cannot be evaluated in place, and stays when it
> can.**

A codec pin changes nothing you can hear until the next connect, so there is
nothing to stay for. A quality change is audible in about a second. Popping
would make each A/B comparison a four-press round trip (`B`, focus, `A`, focus)
instead of one press. Auditioning two adjacent rates by ear is the entire
reason a human opens this screen rather than leaving it on Adaptive.

**Reversibility is what makes "no confirm" safe.** Every entry is one press from
every other entry, nothing is destroyed, and the worst outcome of a mis-press is
a few seconds of the wrong bitrate. `ConfirmView` is reserved for the
irreversible (`Forget`), and spending it here would devalue it there.

### 5.1 What confirms the change took effect

Three signals, in the order they arrive, and **none of them is a toast, a
flash, an animation or a timer.** `redraw_after` stays `None` on both screens.

1. **The check glyph moves** to the pressed row. It follows the **stored**
   setting as echoed back by the model, not the local press — an optimistic
   check that later has to move back is a worse lie than a half-second delay.
   If the echo is slower than ~1 frame in practice, Ruby should say so and we
   will revisit; do not pre-emptively make it optimistic.
2. **Adaptive's trailing note / the row's live number** changes to the new rate
   as the encoder actually reaches it.
3. **Home's bitrate line** changes (§6).

Because the check follows the stored value while the number follows the
encoder, a pick of `330` from `Adaptive @ 990` shows the check move at once and
the number walk down over the following moment. That is not a glitch, it is the
truthful rendering of a stepped primitive, and it is the same intent/reality
split as `CODEC`'s pin vs effective value.

### 5.2 Persistence, and the note row we already have

The flash write is gated while the host is streaming (device-page §3.2's
finding, from Ada: three ~3ms interrupts-off blackouts against a 2ms ISO-OUT
re-arm bar where one miss is permanent). So the common case — changing quality
*while listening*, which is the only time anyone will do it — is a **staged**
write.

**AMENDMENT 3 — the existing `Saves when playback stops` note row now covers
this row too, not only `CODEC`.** No new state, no new row, no new colour. Its
copy is already correct for the split: the change **applies** now and **saves**
later, and the note says "saves".

`Save failed` / `retry` behaves identically. If power is lost while staged, the
setting reverts on next boot and the page truthfully shows the old value — the
same residual window the codec pin already has.

---

## 6. Home: what Adaptive looks like while it is running

This is the bead's central question. The device-page design's answer — Home's
bitrate slot renders the word `Adaptive` instead of a number — is **superseded.**

> **AMENDMENT 4 (supersedes device-page §3.2). Home's bitrate line ALWAYS shows
> the live rate as a number, in every mode. Under Adaptive it additionally
> carries a dim `ADAPTIVE` tag to the right of the number.**

```
+----------------------------------------+-----+
| Pico Link                    68%  (bt) |     |   y 0..15
+----------------------------------------+  A  | devs
| Sony WH-1000XM5                        |     |   y 28..43
|                                        |     |
|                                        +-----+
| LDAC                                   |  B  |   y 56..87  font::hero
|                                        |     |
| 660 kbps  ADAPTIVE                     +-----+   y 92..108
|                                        |  X  | device
|                                        |     |
|                                        +-----+
| LINK GOOD   48 kHz                     |  Y  |   y 144..154
|                                        |     |
+----------------------------------------+-----+
 ^L=12
```

Fixed pick, for contrast — identical line, tag absent, number in the same
place:

```
| 990 kbps                               |         y 92..108
```

**Why the number, not the word.** §15 of the design of record forbids faking a
number; it does not forbid printing a true one. Under ABR the instantaneous
rate is a *fact we have* — it is the EQMID the encoder is running right now.
Replacing a fact with the word "Adaptive" throws away exactly the information
Andreas opened this bead to get, and it would make Home the one screen that
knows less while more is happening. The word belongs on the *setting*; the
number belongs on the *readout*.

**Why a tag and not colour.** The tag says something structural — "this number
is not pinned and may move" — which is not a warning and must never be painted
like one. `STATUS_WARNING` on Home already means "you did not get what you asked
for"; under Adaptive at 330 the user got precisely what they asked for. So:
`TEXT_SECONDARY`, `font::label`, uppercase, matching the stat strip's existing
face, 8px after the number's ink.

Measurement: `660 kbps` ≈ 56px + 8px gap + `ADAPTIVE` at ≈4.5px/char ≈ 36px =
**≈100px**, inside the existing `BITRATE_SLOT_WIDTH = 120` at `L = 12`. It fits
the slot we already clear, so it costs no layout change and no new damage
region. Fallback if measurement disagrees: `AUTO` (≈18px). Do not shrink the
number.

**The number is anchored at `L` and does not move between modes.** The tag is a
suffix precisely so the glance target — the digits — is at the same x whether
the mode is fixed or Adaptive. This is the same argument that moved the hero
word to `L` in the alignment grid: a readout that relocates when its state
changes has to be re-found at the moment you most need it.

### 6.1 What the user sees when Adaptive steps down mid-call

**`990 kbps  ADAPTIVE` becomes `660 kbps  ADAPTIVE`. The digits change. Nothing
else happens.** No banner, no flash, no colour change, no sound, no timer.

This is a deliberate ruling and it is the one most likely to be argued with, so
the reasoning in full:

- **The user is in a meeting.** That is the stated use case for these headphones
  on this project. A visual event that demands attention during a call, to
  report that the device is *successfully doing its job*, is a notification for
  the device's benefit and not the user's.
- **A step-down is a success, not a fault.** ABR stepping to 660 under
  congestion is the feature working. The fault vocabulary — amber, banners, the
  fault strip — exists for things that are wrong, and diluting it here is how a
  status colour dies.
- **The screen is usually asleep anyway.** The idle policy blanks it; a
  transient signal would be seen only by someone already looking, who can read
  the number they are already looking at.
- **A changing number is a sufficient signal for the person who cares.** Andreas
  glancing at the dongle after hearing something he does not like reads the
  current rate directly, and one `X` reaches the row where he pins it.

**What legitimately does deserve a signal, and does not get one here:** ABR
sitting at its floor (330) and still congested is a genuine "we cannot deliver"
state. That belongs to the existing **fault strip**, as a link-quality fault
owned by Ada's controller, and not to the bitrate line. Out of scope for this
bead; noted so nobody invents a second warning channel for it later.

### 6.2 The Home line in every state

| State | Bitrate line |
|---|---|
| Fixed pick, streaming | `990 kbps` |
| Adaptive, streaming | `660 kbps  ADAPTIVE` |
| Adaptive selected, host silent | `idle  ADAPTIVE` |
| Connected, host silent | `idle` (unchanged) |
| Not LDAC | whatever that codec's line already is; no tag, ever |
| Not connected | absent (unchanged — never `0 kbps`, never frozen) |

Under `idle  ADAPTIVE` the tag is still correct and still useful: it tells you
what will happen when sound starts. `idle` remains the existing anti-faking
rule.

---

## 7. First run: `ldac_quality == 0`

> **RULING. `0` is a storage state, never a display state. The UI never shows
> "unset", never shows a blank check gutter, and never asks the user to choose
> before they have a reason to.**

On a device with `ldac_quality == 0`, the row and the picker render **the
effective built-in default** — whatever `codec_ldac.c` actually configures —
exactly as if it had been chosen. The check sits on it. Storage stays `0` until
the user presses `A`.

Two consequences, both wanted:

- **No first-run prompt, no wizard step, no badge.** The pairing wizard does not
  gain a quality question. A user who never opens this screen gets a working
  device, which is the correct outcome for a setting whose default is fine.
- **Un-chosen devices follow the firmware default forward.** If the shipped
  default later changes, every device the user never touched moves with it, and
  every device they explicitly pinned does not. That is precisely the meaning of
  "never chosen", and it is why 1-based encoding was worth the byte.

**Ruby must read the default from `codec_ldac.c`, not from this document.**
Today it is hardcoded HQ, so the fresh-device row reads `990 kbps` with the
check on row 1.

**Recommendation, not a ruling (Andreas/Ada own it):** once ABR is trusted,
change the *firmware* default to Adaptive. The failure mode of a fixed-990
default is audible dropouts for the user who never opens a menu; Adaptive's
failure mode is a quieter, lower bitrate. Nothing in this document changes if
that happens — the check simply starts on row 4.

---

## 8. What the user must not be able to do

1. **Reach the picker when LDAC is not effective-or-pinned.** The row is
   **absent**, not dim. (Existing device-page rule, unchanged; the reasoning
   there — a dim row repeating its neighbour is noise — still holds.)
2. **Change quality from Home.** No face button on any Home face maps to it.
   §2's two-descent friction is the safety property.
3. **Reach an invalid or empty state.** Exactly four entries. No "Off", no "None",
   no numeric entry, no fifth row. One is always checked (§7).
4. **Be shown a number that is not the link's.** LDAC's rates are sample-rate
   dependent: 990/660/330 at 48kHz, **909/606/303 at 44.1kHz**. The picker
   labels, the row value and Home's line must all be derived from the negotiated
   sample rate through **one** mapping function, not hardcoded three times. When
   the rate is unknown or the device is disconnected, use the 48kHz set (our USB
   chain is 48k-only today). This is the cheapest bug in this feature to ship and
   the most embarrassing to be shown.

**Explicitly ALLOWED, and worth stating because it looks like it should not be:
picking a quality while disconnected.** It is a stored preference, not a live
command; the device-page rule "a stored setting never dashes, because it is
still true when nothing is connected" applies unchanged. The pick is saved and
honoured on the next connect. What must be true in that state is only that no
label lies: Adaptive's trailing note reads `varies`, not `660 now`, and the row
reads `Adaptive`, not `Adaptive · 660`.

**Also allowed:** picking while the pin is LDAC but the link fell back to SBC.
The row is present (pin is LDAC), the values render, no live number appears,
and Home's hero shows SBC with its own line and no tag.

---

## 9. Handoff

### For Fern

Nothing new is asked of the widget layer. Specifically:

- The picker is the codec picker's shape: leading check gutter, label, trailing
  note, focus on every row. The gutter was already requested by the device-page
  design (§8 item 5) and this is its second consumer.
- The `QUALITY` row is a plain `Action` row.
- The note row is the existing conditional `Readonly` label-only row.
- **One genuinely new thing, and it is in `hero.rs`, not a widget:** the bitrate
  line draws a second, optional string in `font::label` / `TEXT_SECONDARY`, 8px
  after the number's ink, inside the existing 120px slot. `hero.rs` already
  composes multiple elements by hand; this is not a widget capability.
- `redraw_after` stays `None` everywhere. Nothing here animates.

### For Ruby

1. **`BitrateStatus` gains an adaptive flag** (`core/src/render/hero.rs:207`).
   `BitrateStatus::Kbps(u16)` becomes rate + `adaptive: bool`, and `Idle` gains
   the same flag so `idle  ADAPTIVE` is expressible. **The damage key at
   `hero.rs:436` must fold the flag** — folding an undrawn value is one bug, but
   *not* folding a drawn one silently freezes the tag on screen. That failure
   mode has already bitten this project once.
2. **The `QUALITY` row and its picker**, per §4. Label is `QUALITY`.
3. **One sample-rate → {990,660,330} mapping function**, per §8.4. One place.
4. **The set-quality command** rides the seam alongside the codec pin
   (`2026-09-02-device-page-seam.md`), payload `{addr, ldac_quality: u8}`,
   1-based. The check follows the `PairedDeviceUpserted` echo, not the press.
5. **Mark dirty on the local `A` press.** Applying a quality is a local action,
   not an inbound event; the firmware gates the blit on `pl_ui_dirty()`, and a
   state that changes without dirtying freezes.
6. **Measure two strings before trusting them:** `Adaptive · 660` in the row
   (fallback `Adaptive 660`) and `660 kbps  ADAPTIVE` in the 120px Home slot
   (fallback tag `AUTO`).

### For Tess

Screenshots that pin the states this design turns on, at zoom:

1. Home, Adaptive, streaming — number + tag, tag dim, number at `x = 12`.
2. Home, fixed 990 — number in the **same** x position, no tag.
3. Home, Adaptive, host idle — `idle  ADAPTIVE`.
4. Device page, Adaptive streaming — `QUALITY   Adaptive · 660`.
5. Device page, disconnected, Adaptive pinned — `QUALITY   Adaptive`.
6. Picker, check on `Adaptive`, trailing `660 now`.
7. Picker, check on `990 kbps`, Adaptive trailing `varies` (disconnected).
8. Fresh device, `ldac_quality == 0` — check on the firmware default, not blank.

**One non-screenshot check that matters:** at 990 kbps while streaming, the
encoder starves core 0 (measured: 62 → 3 superloop iters/s). The picker is the
screen most likely to be used *during* streaming at 990, so verify that
navigating it and pressing `A` still feels responsive in that exact condition.
If it does not, that is a real finding about the render/audio contention, not a
reason to change this design.

### Open questions for Ada (`pico-link-7jol.1`)

1. **How fast does the applied EQMID reach the target?** The primitive steps one
   level at a time. If a 990 → 330 pick takes seconds to land, §5.1's
   number-walks-down rendering is right; if it lands immediately, nothing
   changes but the sketch is less interesting. Either way the UI does not need to
   know — but tell me if it can *fail* to reach the target, because "the check
   says 330 and the number never gets there" would need a state this design does
   not have.
2. **Does a manual pin need to survive an ABR-relevant event** (reconnect, codec
   renegotiation) without ABR silently resuming? The product ruling says a pin
   pins. The UI shows a check on a fixed row; if ABR ever moved the rate under
   that check, the check would be a lie.
3. **Does the controller want a "floor and still congested" signal** for the
   fault strip (§6.1)? Not this bead, but if it exists it should be one fault
   row, not a second warning channel.
