# Home fault strip, v2: over-run vs under-run at a glance, and wake-on-fault

**Bead:** `pico-link-9eq2.2` (epic `pico-link-9eq2`). Author: Uma (UX designer), 2026-09-07.
**Status:** design of record, unbuilt. Implementation is `pico-link-9eq2.3`.
**Supersedes:** `.planning/design/2026-09-01-home-fault-strip.md` — that doc's
*concept* survives intact (bottom-anchored red rows, keyed by fault kind, one row
per key, absence as the healthy state, brightness for recency). Its **row anatomy,
width budget, value slot, freshness windows and the "no X binding" ruling are all
replaced here.** See §1 for why: the screen changed under it.
**Amends:** `.planning/design/2026-08-28-on-device-ui.md` §6 (Home) and §4's X
binding; `.planning/design/2026-09-01-idle-policy-across-the-ffi-seam.md`
(`IdlePolicy` gains a third floor); `.planning/design/2026-09-07-volume-on-display.md`
§5 (the wake-policy table gains a fourth source).
**Depends on:** `pico-link-9eq2.1` (Ada) for the fault taxonomy. This doc designs
against **name slots and severity classes**, not against her final names. §5 states
the contract she must satisfy.

Andreas, 2026-09-07, verbatim and the whole brief:

> *"I'm getting some stutters here and there and I have no idea if it's buffer
> over or underrun. The screen should also turn on whenever an error message is
> pushed to the homepage."*

Two sentences, two requirements: **tell me which**, and **be on when I look**.

---

## 1. What changed since the 2026-09-01 design — the width finding

The old doc budgeted a **206px** fault row and drew a row anatomy with a 22px
stage tag, a 118px name, a 26px count and a 34px value slot. **That width no
longer exists.** The vertical OUT meter
(`.planning/design/2026-09-03-vertical-out-meter.md`, shipped) carves
`METER_STRIP_WIDTH = 48` off the button edge of everything below the name band,
and its columns are bottom-pinned to `METER_BLOCK_BOTTOM_INSET = 10` above the
content bottom — i.e. the meter occupies the full vertical range the fault strip
wanted, on the right.

Measured from `core/src/render/hero.rs`:

| Quantity | Value |
|---|---|
| Panel | 240 x 240 |
| Title bar | 16px, abs y0..16 |
| Button rail | 34px on `PANEL.button_edge()` (right), abs x206..240 |
| Content area | 206 x 224, abs origin (0, 16) |
| `hero_body` (content minus the 48px meter strip) | **158px wide**, abs x0..158 |
| `LEFT_MARGIN` / `RIGHT_MARGIN` | 12 / 12 |
| **Usable fault-row ink width** | **145px** (abs x12..157) |

**145px, not 206px. A 30% cut.** Everything in §3 follows from that number, and
the single most important consequence is: **there is no room on Home for both a
repeat count and a live value.** The old doc's absorption of `pico-link-8jp`'s
supply ratio into a right-aligned value slot is therefore struck — see §8, which
gives that number a better home that did not exist when the old doc was written.

I am not proposing narrowing the meter. It is shipped, it is Live in the index,
and it is the one instrument on Home that is continuously honest. The strip
fits in 145px; it just has to be designed for 145px instead of retrofitted.

---

## 2. The one question, and why the obvious answer fails

Andreas does not want an error log. He wants one bit, in under a second, while
someone is talking to him: **is the pipe backing up, or is it running dry?**

The obvious implementation — two rows reading `RING OVERRUN` and `RING UNDERRUN`
— **is the trap**, and it is worth naming explicitly so nobody rebuilds it:

- At `helvB08` (8px), those two strings differ at **character 6 of 12**. They have
  the same length, the same first word, the same silhouette, and the same colour.
- He is glancing, at 30-50cm, mid-sentence, at a row that may be one of four.
- Distinguishing them requires *reading*, and reading is exactly the operation
  the glance budget does not have.

Colour cannot rescue it either. This theme's `STATUS_ERROR` is `Rgb565(31,22,12)`
(orange-red) and `STATUS_WARNING` is `Rgb565(31,44,4)` (amber-yellow) — already
hue-adjacent, already spent on *severity*, and re-spending them on *direction*
would collide. Both over-run and under-run are audible, so both are red anyway.

**So direction is encoded in shape, pre-attentively, and the word is redundant
confirmation rather than the carrier.**

---

## 3. The glyph column — the actual answer to "which"

Every fault row opens with a **9px glyph column at the left rule (abs x12..21)**,
drawn as filled/outlined primitives, not font characters (`helvB08_tf` has no
reliable arrow glyphs, and a primitive is cheaper than a second font anyway).

Only **three** shapes exist, ever:

| Glyph | 7x7 shape | Meaning | Mnemonic |
|---|---|---|---|
| **▲** | up triangle | **Buffer filled / we had too much** — over-run, overflow, drops from a full queue | the level went **up** past the top |
| **▼** | down triangle | **Buffer starved / we had too little** — under-run, ring-empty, host under-supply | the level went **down** past the bottom |
| **■** | square (5x5, vertically centred) | **Not a level fault** — link lost, congestion, resync, anything non-directional | neutral: "something else" |

Three shapes is the cap. **A fourth shape is a design regression**, because the
whole value of the column is that the eye resolves it without decoding.

The direction mapping is not arbitrary: it matches the vertical OUT meter sitting
6px to its right, where bars growing **up** already means *more*. Up is more on
this screen, on both instruments, in the same glance.

**Freshness is encoded in the same glyph, as fill:**

| Fill | Freshness | Means |
|---|---|---|
| **Filled** | Live (0-20 s since last occurrence) | *You are hearing this right now, or just did.* |
| **Outline** (1px stroke) | Recent (20-120 s) | *This happened during this session.* |

This replaces the old design's separate 4px freshness tick — one object now
carries both direction and recency, which is 4px of width recovered and one less
thing on the row. Shape-plus-fill is also strictly more robust than the old
bright/dim colour pair on a screen whose two status hues are already close.

---

## 4. Row anatomy at 145px

```
 abs x:  12    21  24                               133 145
          |    |   |                                  |   |
          [glyph]  [ NAME, uppercase, <= 16 chars ]  [ xNN ]
            9px  3   ~105px                          4  24px
                gap                                 gap  right-aligned
```

- **Glyph** — 9px column, 7x7 ink, vertically centred in the row slot.
- **Name** — `font::label()` (`helvB08`), uppercase, `STATUS_ERROR` or
  `STATUS_WARNING`, dimmed variant when Recent. Left-aligned at abs x24.
  **Hard cap 16 characters** (see §5).
- **Count** — right-aligned to abs x145, same face and colour as the name.
  Renders `x2`..`x99`, saturating at `x99+`. **`x1` renders as nothing** — a
  count of one is the default and printing it is noise.
- **No value slot on Home.** Struck from the 2026-09-01 design; see §8.

**Row slot: 12px** (8px ink + 4px leading). **CONFIRMED 2026-09-08** against
`helvB08`'s real line-height of 10px — the 12px slot holds with 2px spare. No
change.

**Vertical geometry** (absolute panel y, all fixed slots — nothing cursor-derived,
per `.planning/design/2026-09-01-home-alignment-grid.md`):

| Element | abs y band |
|---|---|
| stat strip (`USB 48K 24-BIT`) ends | 154 |
| **1px `DIVIDER` rule** | **174** |
| air | 175..179 |
| row 4 (newest key) | 180..192 |
| row 3 | 192..204 |
| row 2 | 204..216 |
| **row 1 (first-seen key)** | **216..228** |
| bottom padding | 228..240 |

Bottom padding of 12 is symmetric with `TOP_PADDING`, and clearance from the stat
strip to the divider is a **constant 20px in every state** — banner or no banner,
stream or no stream. Nothing on Home ever moves because a fault appeared.

The strip lives entirely inside `hero_body`, so it never overlaps the OUT meter's
x-range and no arbitration between them is needed.

---

## 5. The contract for Ada's taxonomy

I am designing against slots, not names. Ada owns `pico-link-9eq2.1`; this is
what the presentation layer can accept. **Anything outside this does not fit and
must come back to me rather than being silently truncated.**

1. **Name: <= 16 characters, uppercase, ASCII.** **CONFIRMED 2026-09-08 by
   measurement, not estimate:** the longest name in Ada's catalogue,
   `USB SUPPLY LOW`, renders at **94px** against the 105px name field. The 16-char
   cap holds with room to spare and no name needs re-cutting. 17+ characters
   truncates, and a truncated fault name is worse than no fault name.
2. **Every fault declares exactly one glyph class: `Filled` / `Starved` /
   `Neutral`.** Not derived, not inferred at render time — a field on the fault
   kind. If a fault genuinely has both directions it is two faults with two keys.
3. **Every fault declares one severity: `Audible` (red) or `Concealed` (amber).**
   The definition is the old doc's and it stands: red means *you heard this*,
   amber means *we measured degradation and hid it*. This split is load-bearing
   twice over — it drives colour **and** it gates wake-on-fault (§7).
4. **Every fault declares `wakes_display: bool`,** which must be `false` for every
   `Concealed` fault. It exists as a separate field, not `severity == Audible`,
   so an audible-but-storm-prone fault can be demoted without lying about its
   severity.
5. **At most 6 fault keys, total, ever.** One row per key; 6 keys and a 4-row cap
   makes overflow bounded and scrolling structurally impossible.
6. **The over/under pair must differ in first letter AND length.** Even though the
   glyph carries the meaning, the word must not actively fight it. `OVERRUN` /
   `UNDERRUN` fails both tests. My preferred pair, offered not imposed:
   **`USB OVERFLOW` (12) / `USB STARVED` (11)** and **`AIR OVERFLOW` / `AIR
   STARVED`** — different first letters after the stage word, different lengths,
   and the stage word survives as the row's first token so the old design's
   "whose fault, roughly" column scan still works within 16 characters.

**Flagged now, before it costs a round:** the 2026-09-01 design's fixed-width
`IN`/`ENC`/`AIR` **tag column does not fit at 145px** alongside a glyph and a
count. The stage must be the first word of the name, not a separate column. It
still scans vertically, it just is not padded to a fixed width.

---

## 6. Several at once, ordering, and dwell

### 6.1 One row per key, forever

A repeat increments the count and refreshes `last_seen`. It never appends a row.
With <= 6 keys and one row each, the strip cannot scroll and cannot evict a useful
row. This is unchanged from 2026-09-01 and it is the right answer.

### 6.2 Position is first-seen, stable, growing upward

The first fault of the session sits on the bottom row (abs y216) and stays there.
The next new key appears above it. **Rows are never re-ordered relative to each
other.** A row that moves between two glances is worse than a row you have to
scan for, on a screen read for 1.5 seconds at a time by someone who was doing
something else.

Recency is carried by glyph fill and colour brightness, not by position (§3).

**Scope of this rule, made explicit (ruling, 2026-09-08).** §6.2 governs
**where a displayed key sits**. It does not govern **which keys are displayed**
when there are more of them than there are slots — that is §6.4, and the two
rules are orthogonal by construction. Below the cap (<= 4 non-retired keys) there
is no interaction at all: every key is shown, in first-seen order, and nothing
ever moves.

### 6.3 Dwell — the numbers, and why

Andreas's stated pattern is the whole spec: *the stutter lasts a moment, he looks
up a few seconds later.* A 2-second live window (the 2026-09-01 value) is
therefore **wrong** — by the time he looks, the thing he heard has already
demoted itself and he cannot tell it from something that happened four minutes
ago.

| Tier | Window since `last_seen` | Rendering |
|---|---|---|
| **Live** | **0-20 s** | Filled glyph, full `STATUS_ERROR` / `STATUS_WARNING` |
| **Recent** | 20-120 s | Outline glyph, `*_DIM` colour |
| **Retired** | > 120 s | Row is gone |

- **20 s Live** is chosen to be *exactly* `FAULT_WAKE_HOLD` (§7). One constant,
  two uses: the screen is lit for precisely as long as the row is bright. If he
  looks up while the screen is on because of a fault, the row that caused it is
  guaranteed to be the bright one.
- **120 s retirement**, up from 60. A meeting-length device is glanced at every
  few minutes; a 60-second window retires faults he never got the chance to see.
  Two minutes is long enough to survive a glance-cycle and short enough that a
  cleared strip still means "it stopped."
- **Clearing is information.** A fault that stops firing visibly disappears, which
  answers "did that fix it?" without a single button press. A strip that never
  cleared would be a permanent accusation.
- **Nothing is lost on retirement.** The session tally is intact in the detail view
  (§8) with counts and relative times.

Retirement and tier are **computed at render time** from `now - last_seen`. No
timer, no retirement event, no background task.

### 6.4 Overflow — recency selects, first-seen presents

**RULING, 2026-09-08 (Uma).** The v2 draft said "nothing is ever re-sorted" here
and "show the 3 most-recently-active keys" three paragraphs later, which are not
the same instruction once five keys are live. The implementer caught it rather
than picking one silently. This section is the resolution and it is now the only
statement of the overflow rule.

**Neither draft rule wins outright, because they were answering different
questions.** Ranking by recency and *drawing in that order* is a moving target
and is rejected. But so is pure first-seen selection: in a storm it shows the
three keys that appeared **earliest in the session**, which may all be Recent —
dim, outlined, calm — while the keys firing right now sit behind `+N MORE`. That
inverts the entire brief ("tell me what is going wrong, at the moment it goes
wrong") and produces a strip that looks most placid exactly when things are
worst. That is the worse failure of the two, and it is not a close call.

So: **recency governs *selection*; first-seen governs *presentation*.**

Cap stays at 4 rows. It is a legibility cap, not a space cap: once more than four
*kinds* of thing are wrong simultaneously, individual identity has stopped being
the useful information and "a lot is wrong" is.

**Selection — which 3 keys occupy the content rows.** Let *eligible* = every
non-retired key (§6.3). If eligible <= 4, all of them are shown and none of this
applies. Otherwise the displayed set **S** (|S| = 3) is maintained
**incrementally**, never recomputed by a sort:

1. **Seeding.** The first three eligible keys of the session enter S in the order
   they are first seen.
2. **Promotion.** A key that becomes **Live** and is not in S displaces the member
   of S with the **oldest `last_seen`** — but **only if that member is currently
   `Recent`**. A Live member is never displaced.
3. **No Live-on-Live churn.** If every member of S is Live, nothing is displaced.
   When everything on screen is already firing, *which* three are shown carries
   no information and stability wins outright. This clause is what makes the rule
   thrash-free: two keys alternating at 10 Hz cannot swap slots.
4. **Recent never displaces.** Only a Live key can take a slot. A key going from
   Retired back to Recent does not evict anything.
5. **Retirement.** When a member retires (> 120 s), it leaves S and the slot goes
   to the highest-priority non-member: **Live before Recent, then greater
   `last_seen`** (i.e. most recently active), ties broken by earlier `first_seen`.
6. Evaluation happens on the strip's ordinary repaint cadence (§6.5, at most
   1 Hz), never per fault event.

"Most-recently-active" therefore means **`last_seen` descending, with Live
outranking Recent**, and it is used **only to choose members** — never to order
them on screen.

**Presentation — where those 3 keys sit.** The members of S are drawn bottom-up
in **`first_seen` ascending order among themselves**, exactly per §6.2. The
oldest-established member of S is on the bottom row; the newest is directly under
the `+N MORE` row. §6.2's spatial rule is untouched.

**What this costs and why it is the right cost.** A row can still change under a
glance — but only in one situation: a **stale** row (dim, outlined) is replaced by
a key that is **firing right now**. That is not cosmetic motion, it is the strip
delivering its one job. Every other kind of movement — recency reshuffles, sorts,
ties, repeats — is structurally impossible. The user never sees two bright rows
trade places.

**The fourth row** is always the top slot (abs y180):

```
  +2 MORE - PRESS X
```

`N` = eligible keys not in S (with 6 keys max and 3 shown, N is 1..3). 16
characters, no glyph, `TEXT_SECONDARY`. Nothing is lost: the `why?` page (§8)
lists **every** key including retired ones, ordered most-recently-active first.

**Not chosen, and why:** a fifth row (the cap is legibility, not pixels); a
"worst severity wins" selector (severity is already spent on colour, and an amber
key firing now is more informative than a red key that stopped four minutes ago);
freezing S for a dwell period after each change (adds a timer, and clause 3
already removes the churn the dwell would have absorbed).

### 6.5 Repaint discipline

- The strip's count field updates at **most 1 Hz**, no matter how fast the
  underlying fault fires. **A diagnostic that repaints per fault event would cause
  the very contention it is reporting.** With the damage-rect work
  (`2026-09-06-damage-rect-render-and-partial-blit.md`) a repaint is ~0.5ms, which
  makes this cheap — but the rule stands, because a fault storm is exactly when
  the loop is least able to absorb extra frames.
- Tier boundaries (20 s, 120 s) are scheduled via `redraw_after`, never polled:
  `redraw_after = min(1 s, next tier boundary)` while the strip is non-empty,
  and **nothing scheduled at all when it is empty.**
- **A row appearing is a single-frame change. No slide-in, no fade, no blink.**
  A blinking red row is a strobe, and this document spends its whole §7 budget
  arguing against strobing the display.
- The strip's contribution to the hero's `PaintKey` must fold **only what is
  drawn**: glyph class, fill state, name, colour tier, displayed count. Folding
  `last_seen` directly would change the key every microsecond and silently
  reinstate full repaints (project note: *damage keys must fold only what is
  drawn*). Fold the **derived tier**, not the timestamp.

---

## 7. Wake-on-fault

> *"The screen should also turn on whenever an error message is pushed to the
> homepage."*

This is a requirement, not an option. Here is the whole policy.

### 7.1 The screensaver as it actually is today

Read, not assumed. `core/src/run.rs` is emulator-only and the firmware does not
run it — but the *policy* is shared, which is what makes this designable:

- `core/src/run.rs` defines **`IdlePolicy`**, deliberately `Platform`-free, with
  `on_input()` (the clockless Asleep -> Active flip) and
  `tick(now, had_input, at_home_root, on_external_power, mute_or_zero)`.
- `ui-ffi/src/lib.rs`'s `PlUi` owns an `IdlePolicy` built with
  `Some(DEFAULT_IDLE_TIMEOUT)` = **60 s** and `deep_sleep_timeout: None`.
  `firmware/src/main.c` pulls the level every superloop iteration via
  `pl_ui_display_power()` and applies it to GP13 through
  `st7789_set_backlight()`. Backlight only — no `DISPOFF`/`SLPIN`, so a wake is
  a single GPIO write with no panel re-init and no SPI race.
- The screensaver **only arms `at_home_root`**. Anywhere else (Devices, Settings,
  the wizard) it never blanks.
- There is already a **non-input wake precedent**, and it is the exact shape this
  design needs: `pl_ui_push_event`'s volume handler calls `ui.idle.on_input()`
  when `VolumeState::wakes_idle()`, and separately holds the screen up via
  `IdlePolicy::tick`'s `mute_or_zero` floor
  (`.planning/design/2026-09-07-volume-on-display.md` §5.3/5.4).

**Two consequences worth stating plainly.** First: because the screensaver only
arms at Home root, *if the screen is asleep, the user is already on Home.* A
fault wake can never deliver him to a screen that does not show the strip — that
whole class of bug does not exist here. Second: `on_input()` flips power but never
touches `last_input`, which is precisely the primitive a fault wake needs.

### 7.2 What wakes

**Only faults whose kind declares `wakes_display: true` (§5.4), and only on a
key that is not already Live.**

- A **`Concealed` (amber)** fault never wakes. He did not hear it; lighting the
  room for it is a false alarm, and false alarms are how a wake feature gets
  turned off.
- A **repeat of a key already showing Live** never wakes. The screen is already on
  for that fault, or was very recently; the count incrementing is not new
  information.
- A **new key**, or a key currently Recent/Retired, wakes — subject to §7.4.

This adds a fourth row to the volume doc's §5 wake-policy table:

| Source | Effect on a blanked screen | Extends the idle timer? |
|---|---|---|
| `Host` (macOS volume) | nothing | no |
| `Sink` (headphones) | wake to full | yes |
| `Device` (d-pad) | wake to full | yes |
| **Audible fault** | **wake to full, held 20 s** | **no** |

### 7.3 The hold, and re-arming

`on_input()` alone is not sufficient and this is the subtle part. If the last real
button press was ten minutes ago, then `now - last_input >= 60 s` is *already*
true, and the very next `IdlePolicy::tick` re-blanks — a **one-frame flash** that
is worse than not waking at all.

So a fault wake arms a time-boxed floor:

- **`FAULT_WAKE_HOLD = 20 s`.** While the hold is active, `IdlePolicy::tick`
  refuses to transition to `Off`. Structurally this is a third floor alongside
  `at_home_root` and `mute_or_zero`, differing only in that it expires.
- **A fault wake never touches `last_input`.** It is not evidence of a human at
  the device. Consequence: when the hold expires, the 60 s screensaver clock is
  already long past, so the screen blanks **immediately** at hold expiry. That is
  correct — 20 s is the whole grant.
- **Any real button press during the hold cancels it** and hands control back to
  the ordinary 60 s screensaver, because now there *is* a human at the device.
- The wake is applied at the event site (in `pl_ui_push_event`'s fault handler),
  not deferred to the next tick — the same reasoning the volume path already
  records.

### 7.4 What stops a fault storm strobing the screen all night

Four independent limiters, because a single one is a single point of failure and
the failure mode is *a device that flashes in a dark bedroom for eight hours*:

1. **New keys only.** A repeating fault cannot wake twice while it stays Live.
2. **Global cooldown: `FAULT_WAKE_COOLDOWN = 5 min`.** At most one fault wake in
   any 5-minute window, regardless of key. Worst case: 12 wakes/hour x 20 s =
   4 minutes of light per hour.
3. **Session cap: `FAULT_WAKE_SESSION_CAP = 6`.** After 6 fault wakes in one
   continuous stream session, wake-on-fault **stops entirely** for that session.
   A pathological night therefore lights the panel 6 times, totalling 2 minutes,
   and is then dark. The cap resets on a stream stop -> start transition, or on
   any real button press.
4. **No stream, no faults, no wakes.** The strip does not exist when no stream
   exists (§9), so a disconnected dongle on a desk can never wake at all.

**Deliberately not used: time of day.** This board has no RTC (a prior design
round was specced against one that does not exist), so any "quiet hours" rule
would be built on a fiction. The caps above are RTC-free by construction.

### 7.5 Does the screensaver setting still win?

Yes. If `IdlePowerSetting::enabled` is false the screensaver is off and the screen
is always on, so wake-on-fault is a no-op. There is no case where a fault
overrides a user's explicit power preference — it only ever moves the screen
towards *on*, never towards *off*.

---

## 8. Is the live strip enough? No — one detail view, on X

A 16-character name and a count answer *which kind*. They do not answer *how bad,
how often, since when,* or *what is the actual number* — and Andreas's brief is a
troubleshooting brief. So: **one detail view, and exactly one.**

### 8.1 The binding: X, labelled `why?`, live only when the strip is non-empty

The 2026-09-01 doc recommended **against** binding X, on the grounds that
Device detail (E11) would claim it and a binding that appears for one release and
vanishes is worse than none. **That objection is now answerable rather than
merely overruled**, and I am reversing my own recommendation on this basis:

- `core/src/render/home.rs` confirms X is still inert and unlabelled on both
  faces, and that the parent design's §4 table already gives Home's status face
  **`X: link` / `why?`**. The label I want is the label the design already
  reserved.
- So the fault detail **is** the `why?` page. When E11 lands, `why?` becomes a
  section of Device detail rather than a separate destination. **The label never
  changes and the muscle memory survives.** That is the difference between a
  temporary binding and an early one.
- **X is labelled and live only while the strip is non-empty.** Empty strip -> X
  is unlabelled and inert, satisfying design §4 rule 2 ("an unlabelled X or Y does
  nothing", implying a labelled one must do something) with no dead press ever
  possible. Dynamic slot liveness already has precedent in
  `.planning/design/2026-09-02-a-button-label-rule.md`.
- **B is Back, universally, unchanged.** One press in, one press out, press-edge
  only, no long-press anywhere. This is the whole reason X is the right button:
  the escape route is already free.

### 8.2 What the detail page shows

Full 206px content width (no meter on this screen), the existing field-list
mechanic (`.planning/design/2026-09-02-field-list-widget-ruling.md`), vertical
scroll with the joystick, B to leave. Header: `WHY?`.

One two-line block per key, **including retired keys** — this is the session
history, and it is the reason retirement on Home costs nothing:

```
 ▲ USB OVERFLOW                  x14
    last 8s ago . first 4m ago
    ring hit full, 14 frames dropped
```

Line 1: glyph, name, total count (no saturation here — show the real number).
Line 2: `last <rel> ago . first <rel> ago`, `TEXT_SECONDARY`.
Line 3: one plain-language consequence sentence plus **the raw number**, which is
where `pico-link-8jp`'s supply ratio and every other counter value now lives.

**Relative times only. Never absolute timestamps** — no RTC.

Order: most-recently-active first. Ordering may change between visits here,
because unlike Home this is a page you *read*, not a page you *glance at*.

**Empty state:** the page is unreachable when empty (X is inert), so there is no
empty state to design. That is the point of the conditional binding.

### 8.3 Consequence for `pico-link-8jp`

Its Home deliverable was already reduced to a value slot by the old doc. That slot
is now gone (§4), so its Home deliverable becomes **the row itself** — a
`▼ USB SUPPLY LOW xN` row — and its numeric deliverable is line 3 of the `why?`
page. Its firmware half (`pl_usb_supply_q8()`) is unchanged and is the data source
for both. This is strictly simpler than the old absorption ruling.

`Settings > About > Advanced` remains the right home for a continuous numeric HUD
(ring depth, per-counter breakdowns, tx queue depth) and is still unbuilt. The
`why?` page does **not** replace it; `why?` is fault-scoped and event-driven,
Advanced is continuous and exhaustive.

---

## 9. What must NOT be shown

Naming the exclusions is as load-bearing as the design, because otherwise every
future counter argues its way onto Home.

**Never on Home:**

- **A "no faults" / "OK" / "healthy" row, or a persistent empty strip frame.** The
  healthy state occupies **zero pixels**. A permanent all-clear is a reminder of a
  problem you no longer have, and it costs the hero its air.
- **`x1`.** A count of one is the default; printing it is noise.
- **Raw counter identifiers** — `stop_queue_full`, `ovr_frames`, `resync_drops`,
  `stop_credit`. Those are our vocabulary, not his. They may appear on the `why?`
  page as dim secondary text; they may never appear on Home.
- **Numbers with no defined window.** A ratio or rate whose averaging window
  nobody specified is a number that cannot be acted on.
- **Anything measured while no stream exists.** The strip does not exist before a
  stream is up. Faults measured against silence are meaningless and would train
  him to ignore the strip — which is the only failure mode that kills this feature
  permanently.
- **Normal states as faults.** Host idle / auto-pause already reads `idle` on the
  bitrate line. Codec fallback is the amber hero word plus its banner. Mute is the
  `MUTED` banner. No link is the `NO LINK` hero word. **None of these get a row**;
  a row would be a second link in a chain that already works.
- **An ABR step-down.** Per `.planning/design/2026-09-07-ldac-abr-control-loop.md`
  and the quality-selector doc: stepping 990 -> 660 under congestion is *the
  feature working*. Only **"at the 330 floor and still congested"** is a fault,
  and it is one `■` row.
- **A title-bar fault dot on every screen.** Declined, unchanged from 2026-09-01:
  the bar already carries the Bluetooth and USB glyphs and the volume percent, and
  a fourth indicator on every screen is permanent clutter for a rare event that
  `B, B` reaches in two presses.
- **Blink, flash, fade, slide, spinner, or any animation.** Cost, plus §7's entire
  argument against strobing.
- **Stack traces, hex, error codes, panic text.** That is `pico-link-gap`'s panic
  recorder, a different feature with a different audience.

**Never anywhere:**

- **Absolute timestamps.** No RTC.
- **A fault the firmware inferred rather than measured.** If a counter for it does
  not exist, ship that row **absent, never faked** (parent design §15).

---

## 10. ASCII sketches

True 240x240 proportions. Grid: **1 column = 6px, 1 row = 12px** -> 40 x 20 cells.
`|` marks the content/rail boundary at x206.

### 10.1 Healthy — the strip costs zero pixels (the common case)

```
      0                              206    240
   0  +----------------------------------+-------+
      | (bt)  PICO LINK      82%   (usb) |       |   y0..16   title
  16  +----------------------------------+   A   |   devs
      |                                  |       |
      |  Sony WH-1000XM5           OUT   +-------+
      |                            # #   |       |
      |                            # #   |   B   |
  72  |        L D A C             # #   |       |
      |                            # #   +-------+
      |                            # #   |       |
 108  |        909 kbps            # #   |   X   |   (inert)
      |                            # #   |       |
      |                            # #   +-------+
 144  |  USB 48K 24-BIT            # #   |       |
      |                            # #   |   Y   |   set
      |                            # #   |       |
      |                            # #   +-------+
      |                            # #   |
      |                            # #   |
      |                            # #   |
 240  +----------------------------------+
```

### 10.2 One live fault — the whole point of the feature

The user just heard a click. He looks up. One bright row, one filled triangle
pointing **up**: the buffer filled. He did not read a word to know that.

```
      0                              206    240
   0  +----------------------------------+-------+
      | (bt)  PICO LINK      82%   (usb) |       |
  16  +----------------------------------+   A   |   devs
      |  Sony WH-1000XM5           OUT   |       |
      |                            # #   +-------+
  72  |        L D A C             # #   |   B   |
      |                            # #   +-------+
 108  |        909 kbps            # #   |   X   |   why?  <- now live
      |                            # #   +-------+
 144  |  USB 48K 24-BIT            # #   |   Y   |   set
      |                            # #   +-------+
 174  |  ------------------------  # #   |
      |                            # #   |
      |                            # #   |
 216  |  # USB OVERFLOW      x14   # #   |   <- filled ▲, red, LIVE
 228  |                            # #   |
 240  +----------------------------------+
```

### 10.3 The distinction, side by side

This is the comparison the design has to win. Left: the pipe is backing up.
Right: the pipe is running dry. **One glance, no reading, no colour dependence.**

```
   ------------------------            ------------------------
                                   
   /\  USB OVERFLOW    x14          \/  USB STARVED     x9
   --                                --
   filled up-triangle                filled down-triangle
   "too much arrived"                "not enough arrived"
```

### 10.4 Three keys, mixed direction and freshness

Bottom row is oldest (first seen) and never moves. The one **filled** glyph is
what is happening *now*; the two outlines are session history. Note that the eye
resolves "one up, one down, one neutral" before it resolves any word — which is
itself the finding: both ends of the ring have been hit, so this is drift, not a
one-sided overflow.

```
 174  |  ------------------------  # #   |
 180  |  o AIR CONGESTED      x3   # #   |   <- outline ■, red DIM,  recent
 192  |  v USB STARVED        x9   # #   |   <- outline ▼, red DIM,  recent
 204  |  # USB OVERFLOW      x14   # #   |   <- filled  ▲, red,      LIVE
 216  |  o RESYNC TRIM       x22   # #   |   <- outline ■, amber DIM, first seen
 228  |                            # #   |
 240  +----------------------------------+
```

### 10.5 Overflow (5+ non-retired keys)

Five keys are eligible; three slots. `USB OVERFLOW` is Live and displaced the
stalest Recent member (§6.4 clause 2). The three shown are then drawn in
first-seen order among themselves — recency picked them, position did not move
for them.

```
 174  |  ------------------------  # #   |
 180  |    +2 MORE - PRESS X       # #   |   <- TEXT_SECONDARY, no glyph
 192  |  o AIR CONGESTED      x3   # #   |
 204  |  # USB OVERFLOW      x14   # #   |
 216  |  o RESYNC TRIM       x22   # #   |
 228  |                            # #   |
 240  +----------------------------------+
```

### 10.6 Banner plus a full strip — no collision, nothing dropped

The fixed-slot grid means the fallback banner, the stat line and four fault rows
all coexist. The 2026-09-01 doc's "drop the stat line" rule is unreachable and is
formally struck.

```
  16  +----------------------------------+-------+
      |  Sony WH-1000XM5           OUT   |   A   |
  72  |        S B C               # #   +-------+   <- amber hero
 108  |        328 kbps            # #   |   B   |
 116  |  ################################ #   |
      |  # HEADPHONES DON'T SUPPORT LDAC # X   |   why?
 136  |  ################################ #   |
 144  |  USB 48K 24-BIT            # #   |   Y   |
 174  |  ------------------------  # #   +-------+
 180  |  o AIR CONGESTED      x3   # #   |
 192  |  v USB STARVED        x9   # #   |
 204  |  # USB OVERFLOW      x14   # #   |
 216  |  o RESYNC TRIM       x22   # #   |
 240  +----------------------------------+
```

### 10.7 The `why?` page (X from Home)

```
      0                              206    240
   0  +----------------------------------+-------+
      | (bt)  WHY?                 (usb) |       |
  16  +----------------------------------+   A   |
      |                                  |       |
      |  # USB OVERFLOW            x14   +-------+
      |     last 8s ago . first 4m ago   |   B   |   back
      |     ring hit full, 14 frames     |       |
      |     dropped                      +-------+
      |                                  |       |
      |  v USB STARVED              x9   |   X   |
      |     last 47s ago . first 4m ago  |       |
      |     ring ran empty, host under-  +-------+
      |     supplied 0.62 of nominal     |   Y   |
      |                                  |       |
      |  o RESYNC TRIM             x22   +-------+
      |     last 2s ago . first 6m ago   |
      |     freshness trim dropped 22    |
      |     frames to hold latency       |
      |                              v   |   <- more below
 240  +----------------------------------+
```

---

## 11. Rationale

- **Shape beats text at a glance, and this is the one screen element where that is
  decisive.** Andreas's brief is a *discrimination* task under time pressure, not
  a reading task. Two words differing at character six cannot be discriminated in
  a glance; two triangles pointing opposite ways can be discriminated
  pre-attentively, without foveating the row at all.
- **Reusing up=more from the OUT meter** means the two instruments on Home teach
  each other instead of competing. There is one spatial metaphor on this screen
  and both users of it agree.
- **Fill-for-freshness collapses two encodings into one object**, which is how
  a 30% width cut got absorbed without losing information. It also removes the
  design's dependence on distinguishing two hue-adjacent status colours at 8px.
- **20 s Live == 20 s wake hold** is the single most important number here. It
  makes "the screen is on" and "this row is the reason" the same fact, so the
  wake and the strip cannot disagree about what he is looking at.
- **Not extending the idle timer on a fault wake** is what keeps the screensaver
  trustworthy. A fault is not a person. The screen going back off after 20 s is
  the feature, not a bug.
- **Four independent storm limiters** because the failure mode is nocturnal,
  unattended, and would destroy trust in one night. Bounding the worst case at
  6 wakes and 2 minutes of light per stream session is a number I can defend to
  someone woken up by it.
- **Absence as the healthy state** is what keeps the strip from converting Home
  from "everything is fine" into "here is a dashboard". The hero word is a
  reassurance mechanic; a permanent diagnostic band under it would be a different
  product.
- **Reversing my own X ruling** is not inconsistency — the objection was "the
  binding will be reclaimed and vanish", and the answer is that the label `why?`
  is the one the parent design already reserved for the reclaiming feature. The
  binding does not vanish; it gets a bigger page behind it.

---

## 12. Handoff

**Ada (`pico-link-9eq2.1`) — the taxonomy contract:**
1. Every fault kind carries: `name` (<= 16 chars uppercase), `glyph:
   Filled|Starved|Neutral`, `severity: Audible|Concealed`, `wakes_display: bool`
   (must be `false` for every `Concealed`).
2. **<= 6 keys total.** If the counter inventory suggests more, merge by
   user-facing consequence, not by mechanism — several counters may feed one row.
3. The over/under name pair must differ in first letter and length. `OVERRUN` /
   `UNDERRUN` is rejected on legibility grounds; `USB OVERFLOW` / `USB STARVED`
   is my offer.
4. The stage (`IN`/`ENC`/`AIR` in the old design) is the **first word of the
   name**, not a separate fixed-width column — that column does not fit at 145px.
5. Faults are edge-triggered and **debounced C-side**, delivered over the existing
   `bt.c` MPSC event ring as a `PlEvent` variant. Never a Rust call from IRQ
   context (`pico-link-6o2` is exactly that bug).
6. Where a counter does not exist yet, ship that row **absent, never faked**.

**Fern (fe-architect) — what must be expressible:**
1. The strip is **part of the hero composite**, not a sibling on the `Screen`
   stack. It is bottom-anchored inside `hero_body` and one owner must arbitrate
   the column. Unchanged from 2026-09-01 and still the right call.
2. Three 7x7 glyph primitives (up triangle, down triangle, square), each in filled
   and 1px-outline form. Primitives, not font glyphs. Not `draw_selection`'s
   accent helper — overloading the selection primitive with a second meaning
   forces a branch at every later read site.
3. Two new palette constants: `STATUS_ERROR_DIM`, `STATUS_WARNING_DIM` (~45-50%
   toward `BACKGROUND`). They must be distinguishable **from each other** and from
   `TEXT_SECONDARY` at 8px — verify on a zoomed capture, the two bright forms are
   already hue-adjacent. **DONE 2026-09-08: verified distinguishable on a zoomed
   capture** (an earlier round had asserted this in a doc comment without
   performing the check; it has now actually been performed).
4. `IdlePolicy` gains a **third, expiring floor**: `on_fault_wake(now)` plus a
   `fault_hold_until: Option<Instant>` checked in `tick` alongside `mute_or_zero`.
   Same pattern as the mute floor, differing only in that it expires and in that
   the storm limiters (§7.4) live beside it. It stays `Platform`-free.
5. `redraw_after = min(1 s, next tier boundary)` on the hero while the strip is
   non-empty; nothing scheduled when empty.
6. The strip's `PaintKey` contribution folds the **derived tier**, never
   `last_seen`.
7. ~~Confirm the 12px row slot and the 16-character name cap~~ **CLOSED
   2026-09-08, both measured and both hold:** `helvB08` line-height is 10px (12px
   slot fits), and the longest name `USB SUPPLY LOW` measures 94px against the
   105px field. No names re-cut, no silent truncation, nothing further owed here.
8. **Overflow selection (§6.4).** The strip needs, per repaint, both the derived
   tier and `last_seen` *ordering* to maintain the displayed set — but the
   `PaintKey` still folds **only the derived tier and the drawn values**, never
   `last_seen` (item 6 is unchanged and remains the binding constraint). The
   displayed set is state carried across frames, not recomputed presentation, so
   it must live in `FaultLog` (or beside it), **not in a widget** — widgets do not
   survive frames on this project.

**Ruby (implementer) — `pico-link-9eq2.3`:**
1. `FaultLog` in `core`: a **fixed array of 6 entries**, one per key. No `Vec`, no
   allocation on a fault event. Entry: `{ key, count: u16 saturating, first_seen,
   last_seen }`. Tier and retirement derived at render time.
2. `Event::FaultRaised { key }` over the existing event ring.
3. Wake path in `ui-ffi`'s `pl_ui_push_event`, modelled **exactly** on the
   existing volume wake (`ui.idle.on_input()` at the event site) — but **without**
   setting anything equivalent to `volume_wake_since_last_tick`, because a fault
   must not extend `last_input`.
4. Constants, all in one place: `FAULT_LIVE_WINDOW = 20 s`, `FAULT_RETIRE = 120 s`,
   `FAULT_WAKE_HOLD = 20 s`, `FAULT_WAKE_COOLDOWN = 5 min`,
   `FAULT_WAKE_SESSION_CAP = 6`.
5. X binding on Home: labelled `why?` and live **only** while the strip is
   non-empty; inert and unlabelled otherwise. B remains Back everywhere.
6. `why?` page on the existing field-list mechanic. Relative times only.
7. Fixtures, captured **zoomed** per the project rendering rule: empty (must be
   pixel-identical to today's Home); one live `Filled` row; one live `Starved` row;
   three keys spanning both directions and both freshness tiers; overflow at 5+;
   banner + full 4-row strip + stat line all present; a key transitioning
   Live -> Recent -> Retired; the `why?` page with a scroll.

**Tess — what must be proved on hardware, not in a test:**
1. Blank the panel (60 s at Home root, state the navigator depth). Inject an
   audible fault. **Photograph the panel; report the chroma delta.** It must light.
2. It must go dark again ~20 s later with no further input.
3. Inject a `Concealed` fault while blanked: the panel must **stay dark**, and the
   row must be present when woken by a button.
4. Inject 20 faults across 10 different keys in 60 s: count the wakes. Must be
   <= 6, and the panel must be dark at the end.
5. Press a button during the hold: the ordinary 60 s screensaver must take over.
6. Emulator: the whole strip must be demonstrable headless with no hardware.

---

## 13. Open, with my recommendations

| Question | My recommendation |
|---|---|
| Live window / wake hold = 20 s | Ship 20. It is the number tied to "he looks up a few seconds later." Most likely tunable. |
| Retirement = 120 s | Ship 120, up from the old 60. If the strip feels stale, cut to 90 before cutting the Live window. |
| Session wake cap = 6 | Ship 6. Conservative on purpose; raise only if Andreas reports missing faults. |
| X = `why?` while non-empty | **Ship it.** This reverses my 2026-09-01 recommendation; §8.1 gives the reason. |
| Does `Settings > About > Advanced` still get built? | Yes, separately. `why?` is fault-scoped; Advanced is the continuous HUD. Not a blocker for this bead. |
| Overflow: first-seen vs most-recently-active (§6.2 vs §6.4) | **Ruled 2026-09-08: recency selects, first-seen presents.** See §6.4; the contradiction is closed and the section rewritten. |
| Should the over/under pair share one row with a flipping glyph? | **No.** They are opposite ends of one quantity but he needs both session counts, and a glyph that flips under him is a moving target. Two keys, two rows, stable order. |
