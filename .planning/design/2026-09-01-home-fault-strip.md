# Home fault strip: red rows for what is going wrong

Bead `pico-link-h62`. Author: Uma (UX designer), 2026-09-01.

**GOAL:** Home shows, in red, what is currently going wrong with the audio
pipeline, so Andreas can tell *which side of the pipe* caused the noise he just
heard — without that display fighting the hero codec word, and without becoming
a second diagnostic overlay competing with `pico-link-8jp`.

## 1. The organizing principle: stage, not error code

Andreas's ask ends with the reason: *"so it's easier to troubleshoot/identify
what I'm hearing."* That is not a request for an error log. It is a request to
answer one question fast: **is it my Mac, is it the dongle, or is it the radio?**

So every row is prefixed with a fixed-width **stage tag**:

| Tag | Segment | "Whose fault, roughly" |
|---|---|---|
| `IN` | host -> dongle over USB | the computer |
| `ENC` | inside the dongle (pacing, encode, resync) | us |
| `AIR` | dongle -> headphones over Bluetooth | the radio / the room |

The tag column is fixed-width and left-aligned so the eye can scan **only that
column** and get the answer in one saccade. Everything else on the row is
elaboration.

**Rows are named for what the user experiences, not for the counter that
detected it.** Several counters may feed one row. This is the de-proliferation
rule; without it every new firmware counter grows a row and the strip
degenerates into a debug log that nobody reads.

## 2. The fault catalogue (six rows, total, ever)

| # | Row | Stage | Severity | Fires on | Value slot |
|---|---|---|---|---|---|
| 1 | `HOST AUDIO LOW` | `IN` | red when mute engaged, amber otherwise | `supply_q8 < 192` sustained ~300 ms (2ap.1d's existing debounce) | supply ratio, `0.50` |
| 2 | `USB GAP` | `IN` | amber | regime-A ring-empty skips (`stop_ring_empty`) with supply healthy | gaps in window |
| 3 | `ENC OVERRUN` | `ENC` | red | encoder missed its real-time quota | worst ms |
| 4 | `RESYNC` | `ENC` | amber | freshness trim dropped frames (`resync_drops`) | frames dropped |
| 5 | `LINK LOST` | `AIR` | red | ACL / A2DP dropped and recovered | reconnect count |
| 6 | `CONGESTED` | `AIR` | red | radio could not take the media packets (L2CAP stalls **and** dropped media frames, merged) | frames dropped |

Row 6 is Andreas's "radio packet loss (or transmission loss)". L2CAP
send-stalls and dropped media frames are deliberately **one row**: they are the
same user-facing fact ("the radio couldn't keep up"), and the mechanism split
belongs in Advanced diagnostics, not on Home.

Row 1's severity is **dynamic**: under-supply that we are concealing by muting
is audible (red); under-supply we are riding out is not (amber). This is why
regime-B mute does **not** get a row of its own — the mute is our *response* to
under-supply, not an independent fault, and giving it a second row would report
one event twice.

### 2.1 What deliberately gets NO row

Naming the exclusions is as load-bearing as the catalogue, because otherwise
every future counter argues for a row:

- **Codec fallback** — the hero word turns amber and the persistent banner
  explains it (design 6.2). A red row saying the same thing would be a third
  link in a five-link chain that already works.
- **Muted volume** — the MUTED banner (6.3).
- **No link** — the hero word already reads `NO LINK` in red.
- **Host idle / auto-pause** — the bitrate line already reads `idle`. This is a
  normal state, not a fault. A row here would train Andreas to ignore the strip.
- **Anything measured while no stream exists.** Faults before a stream is up are
  meaningless; the strip does not exist in that state.

### 2.2 The gate for adding a seventh row

All five, or it goes to Advanced diagnostics only:

1. Backed by a **measured counter**, never an inference.
2. Has an **audible or user-noticeable** consequence.
3. Name fits **<= 14 characters, uppercase**.
4. Belongs to exactly one of `IN` / `ENC` / `AIR`.
5. Is **not already stated elsewhere on Home**.

## 3. De-duplication, counting, and ordering

**A row is keyed by (stage, fault kind). There is at most one row per key,
ever.** A repeat increments the count and refreshes `last_seen`; it never
appends. This is the whole answer to "a repeating fault must never scroll a
useful one off screen" — with only six possible keys and one row each, the strip
is structurally incapable of scrolling.

- Count renders `x12`, saturating at `x99+`. `x1` renders as nothing (a count of
  one is the default; printing it is noise).
- **Order is first-seen, stable, never re-sorted.** The first fault sits at the
  very bottom edge; the next appears above it; the strip grows upward toward the
  hero. This is Andreas's "stack them from the bottom", verbatim.

**Recency is encoded in brightness, not position.** This is the central call.
Sorting by most-recent would put the freshest thing under your eye — but it
makes rows jump between glances, and a moving row on a screen you look at for
1.5 seconds is worse than a still one you have to scan. So instead each row has
a freshness tier:

| Tier | Window since `last_seen` | Rendering |
|---|---|---|
| **Live** | 0-2 s | Full `STATUS_ERROR` / `STATUS_WARNING`, plus a 4px filled tick in the left margin |
| **Recent** | 2-60 s | Dimmed variant of the same colour, no tick |
| **Retired** | > 60 s | Row is gone |

So "what just happened" is answered by the one bright row with a tick, wherever
it sits, and nothing ever moves.

## 4. Ageing out, and where the history goes

**Retirement is 60 s of silence, and it is computed at render time from
`now - last_seen` — no timer, no background task, no retirement event.**

The clearing itself is information: a fault that stops firing visibly goes away,
which tells Andreas "whatever that was, it stopped." A strip that never cleared
would be a permanent accusation.

**Nothing is lost when a row retires.** The session tally — every key, its total
count, first/last seen, and the underlying per-counter breakdown — lives in
**Settings > About > Advanced**, which design section 19 already specifies ("an
`Advanced` sub-view for last-error and link stats"). That gives the two-tier
split that makes this coherent:

> **Home answers "is something wrong, and which side of the pipe."
> Advanced answers "by how much."**

60 s is the tunable most likely to need adjusting from real use. Start at 60.

## 5. How this composes with `pico-link-8jp` — the explicit ruling

`8jp` wants a live USB supply ratio and an episode count on screen. Shipped
independently, Home would carry a red row reading `IN HOST AUDIO LOW x3` **and**
a separate numeric readout reading `SUPPLY 0.50 EP 3` — two objects stating one
fact in two vocabularies, on the screen whose whole job is a one-glance answer.
That is the incoherence to prevent, and the fix is not coordination, it is
absorption:

**`8jp` is not a widget. It is the detail payload of one `h62` row.**

1. **The episode count IS the row's repeat count.** There is no separate episode
   counter anywhere on Home. `x3` *is* the episode count. One number, one place.
2. **The supply ratio IS the row's value slot.** Every fault row has an optional
   right-aligned live value field; `8jp` is simply the first user of it.

```
| IN   HOST AUDIO LOW        x3   0.50 |
```

That single row discharges both beads on Home.

3. **The ratio is not shown when healthy.** A permanently-displayed `1.00` is a
   debug HUD: it fails the glance test, it competes with the hero for the "what
   number do I look at" slot, and a number that never moves teaches you to stop
   reading it. Under the design's own honesty rule, the healthy state of a fault
   display is *absence*.
4. **The always-on instrument panel is Advanced, not Home.** Supply ratio,
   episode count, mute ms, resync drops, ring depth, per-counter breakdowns —
   all of it, continuously, with no legibility budget pressure and no
   competition with the hero. That is where a bring-up numeric HUD belongs.
5. **The value slot is a rendering contract, not a per-fault special case.**
   `FaultValue` is one small enum (`Ratio(q8)`, `Count(u16)`, `Millis(u16)`);
   adding a value to a future row costs a variant, not a layout.

**Consequence for `8jp`'s scope:** its on-screen deliverable becomes (i) the
value slot on the `IN HOST AUDIO LOW` row and (ii) the Advanced diagnostics
page. It adds no Home element of its own. Its firmware half
(`pl_usb_supply_q8()` and the windowed ratio, 2ap.1a) is unchanged and is the
data source for both.

## 6. Visual spec

**Typography.** Row text in `font::label()` (`helvB08`), uppercase, for tag,
name and count; value in the same face. 8px is the established secondary size on
this screen, and it is legible at 30-50 cm **for short all-caps tokens** — which
is precisely why rule 2.2(3) caps names at 14 characters. Prose at 8px would not
be legible; `IN HOST AUDIO LOW` is.

**Colour.** Two severities, defined by **audibility**, not by abstract
seriousness:

| Colour | Meaning |
|---|---|
| `STATUS_ERROR` (red) | You are hearing this, or you did within the last 2 s. |
| `STATUS_WARNING` (amber) | Measured degradation we concealed. You probably did not hear it. |
| `STATUS_ERROR_DIM` / `STATUS_WARNING_DIM` (**new**) | The 2-60 s "recent" tier of each. |

This distinction pays for itself immediately: **if Andreas hears something and
only amber rows are showing, that is itself a finding** — it means our
concealment is not as inaudible as the design claims. A single red-only scheme
would have thrown that signal away.

Amber is already the banner colour, but the shapes are unmistakably different
(banner = full-width `SURFACE_ELEVATED` filled bar; fault row = unfilled text on
`BACKGROUND` with a left tick), so the reuse is safe.

**States.**
- **Empty** — the strip occupies **zero pixels**. Not a "no faults" label, not a
  rule, nothing. The healthy state must be indistinguishable from
  never-having-had-a-fault; a persistent "all clear" is a reminder of a problem
  you no longer have, and it costs the hero its air.
- **Focused / selected** — none. The strip is **not focusable and has no binding
  of its own** (see section 8).
- **Loading / boot** — the strip does not exist before a stream exists.
- **Overflow** — see section 7.

## 7. Vertical budget

> **AMENDED 2026-09-01 by `.planning/design/2026-09-01-home-alignment-grid.md`
> (bead `pico-link-nvj`).** The table below was computed against `hero.rs`'s
> old accumulating `cursor_y` rhythm, in which showing the banner moved the
> stat strip down 28px. That rhythm is replaced by a fixed grid; the hero
> composite's bands are now at constant y in every state. **The fault strip's
> own geometry is unchanged** — this amendment only restates what sits above
> it, and improves the guarantee.

Content area is **206 x 224** (240 minus the 34px rail, minus the 16px title
bar). The hero composite's rhythm, per the alignment grid:

| Element | y band | Fixed? |
|---|---|---|
| device name | 28..43 | yes |
| hero slot | 56..87 | yes |
| bitrate | 92..108 | yes |
| banner, when shown | 116..135 | yes — fixed slot, not cursor-derived |
| stat strip | 144..154 | yes — same rows with or without a banner |

The strip is **bottom-anchored**, unchanged:

- bottom padding 6 -> strip bottom edge at **y = 218**
- row slot **12px** (8px ink + 4px leading)
- cap **4 rows** = 48px, plus a 1px `DIVIDER` rule and 5px of air above = **54px**
- strip top at **y = 164**

**Clearance from the stat strip to the strip's divider is a constant 9px, in
every combination of banner/no-banner and stream/no-stream.** This replaces the
old "66px no banner / 38px with banner" — the number is smaller but it is now
*invariant*, which is what the strip actually needed.

**The cap of 4 is a legibility choice, not a space constraint** — the geometry
would take 6 rows. It is capped at 4 because once more than four distinct kinds
of fault are live simultaneously, the individual identity has stopped being the
useful information; "a lot is wrong" is. At overflow, show the 3 most-recently-
active rows plus:

```
| +3 MORE - SETTINGS > ABOUT           |
```

**Row alignment:** the stage tag is left-aligned to the shared left rule
**`L = 12`**; the value slot is right-aligned to the shared right rule
**`R = 194`**, the same rule the bitrate uses. Fault rows are on the page grid,
not a private one.

**Contention priority, stated once so nobody has to negotiate it later:**

> hero word > banner > fault strip > stat line.

> **STRUCK by the alignment amendment:** the old rule "if the fallback banner
> and four live fault rows collide, the stat line is dropped" is now
> unreachable. With fixed slots the banner, the stat strip and a full four-row
> strip coexist with clearance to spare, so nothing ever collides and there is
> no drop behaviour to implement. The priority ordering above is retained only
> as a tiebreaker of record for any *future* element that wants space on Home.

## 8. Input, motion and cost

**No new binding. The strip is read-only.** The route to detail is the
already-labelled `Y > set` -> About -> Advanced. Home's input contract is
already the most exception-laden part of the design (section 4's "Home
exception"); adding a fifth exception to reach a diagnostic page would cost more
than the two presses it saves.

> **One judgement call for Andreas, flagged rather than silently declined.** X is
> inert on Home today and stays inert until Device detail (E11) lands. A `diag`
> label on X while it is otherwise unbound would give one-press diagnostics to
> the person who literally asked for troubleshooting. **I recommend against it**:
> E11 will claim X for `link`/`why?`, and a binding that exists for one release
> and then disappears is worse than one that never existed. Say the word and I
> will spec it as a temporary; my default is no.

**Motion.**
- The strip's count field updates at **most 1 Hz**, regardless of how fast the
  underlying fault fires. This is not polish — **a diagnostic that repaints on
  every fault event will, at 18fps full-frame redraw, cause the very SPI/CPU
  contention it is reporting.** A fault display that makes the fault worse is a
  self-defeating instrument.
- Tier transitions (2 s and 60 s boundaries) are scheduled via `redraw_after`
  (the `RenderCtx` clock seam,
  `.planning/decisions/2026-08-31-render-ctx-frame-scoped-clock.md`), never
  polled. `redraw_after = min(1 s, next tier boundary)`.
- **A row appearing is a single-frame change. No slide-in, no fade.** An
  animated entrance is full-frame redraws for its whole duration, on the one
  screen where cost matters most.

**Sleep.** Design 6.1's rule extends verbatim: **never fully blank while the
strip is non-empty — dim only.** A fault you must press a button to read has
failed. Additionally, a fault appearing while the screen is dimmed **undims to
full for the 2 s live window, then returns to dim** — but only on a **new row
key**, and at most once per 30 s, so a repeating fault cannot strobe the
backlight.

**Scope.** The strip belongs to **Home's status face only.** It does not follow
you into Devices, Settings or the wizard, and it does not appear on the menu
face. Faults still accumulate while you are elsewhere, so returning to Home
shows the correct state. **Declined:** a global fault dot in the title bar — the
bar already carries the Bluetooth and USB glyphs, and a third indicator on every
screen is permanent clutter for a rare event that `B, B` reaches in two presses.

## 9. ASCII sketches

**Healthy — the strip does not exist.** This is the common case and it must cost
zero pixels.

```
 +--------------------------------+-----+
 | (bt) PICO LINK           (usb) |     |
 +--------------------------------+  A  | devs
 | Sony WH-1000XM5                |     |
 |                                +-----+
 |          L D A C               |  B  |
 |                                |     |
 |            909 kbps            +-----+
 |                                |  X  |
 | USB 48K 24-BIT                 |     |
 |                                +-----+
 |                                |  Y  | set
 |                                |     |
 +--------------------------------+-----+
```

**One live fault — this is `h62` + `8jp` on one row.**

```
 | Sony WH-1000XM5                |     |
 |          L D A C               |  A  | devs
 |            909 kbps            |  B  |
 | USB 48K 24-BIT                 |  X  |
 |                                |  Y  | set
 |--------------------------------|
 |# IN   HOST AUDIO LOW  x3  0.50 |   <- red, 4px tick, live
 +--------------------------------+-----+
```

**Three faults, mixed severity and freshness.** Note the stage column scanning
vertically: two `IN`, one `AIR` — the computer is the suspect.

```
 |--------------------------------|
 |  ENC  RESYNC             x2    |   <- amber, dim  (2-60s)
 |  IN   USB GAP           x14    |   <- amber, dim
 |# IN   HOST AUDIO LOW  x3  0.50 |   <- red, live, ticked
 +--------------------------------+
```

**Overflow.**

```
 |--------------------------------|
 |  +3 MORE - SETTINGS > ABOUT    |
 |  AIR  CONGESTED          x7    |
 |  ENC  OVERRUN            x1    |
 |# IN   HOST AUDIO LOW  x9  0.50 |
 +--------------------------------+
```

**Contention: fallback banner AND a full strip. The stat line is dropped.**

```
 | Sony WH-1000XM5                |
 |          S B C                 |   <- amber hero
 |            328 kbps            |
 |################################|
 |# HEADPHONES DON'T SUPPORT LDAC |   <- banner keeps its slot
 |################################|
 |                                |      (stat line dropped)
 |--------------------------------|
 |  AIR  CONGESTED          x7    |
 |  ENC  RESYNC             x2    |
 |# IN   HOST AUDIO LOW  x3  0.50 |
 +--------------------------------+
```

**Row anatomy (206px wide).**

```
 x=4   x=12    x=34                x=152   x=164   x=198
  |     |       |                    |       |       |
  [tick][ TAG ][ NAME ..............][ xNN ][ VALUE ]
   4px   22px    118px (~23 chars)    26px    34px
```

## 10. Rationale

- **The stage tag is the feature.** Everything else here is bookkeeping. Andreas
  is not debugging a counter, he is deciding whether to blame his Mac. Three
  tags in a fixed column answer that before he has read a single word.
- **Keying by (stage, kind) makes the overflow problem structurally
  impossible**, rather than solving it with a scroll policy. There are six
  possible rows and one row each. No scrolling, no eviction heuristic, no "which
  one do I drop" judgement at runtime.
- **Recency as brightness rather than position** buys a still screen. On a
  display read for 1.5 s at a time by someone who was doing something else, a
  row that moved between glances costs more than the recency ordering was worth.
- **Absence as the healthy state** is what keeps the strip from fighting the
  hero. The hero is a reassurance mechanic; a permanent diagnostic band
  underneath it would convert Home from "everything is fine" into "here is a
  dashboard", which is the wrong product.
- **Absorbing `8jp` into a row's value slot** is the whole answer to "one
  coherent surface". Two overlays would have been a coordination problem
  forever; one row with an optional value is a component.
- **The audible/concealed colour split** turns the strip into an instrument that
  can falsify our own concealment claims, which on this project is worth more
  than the visual simplicity it costs.

## 11. Handoff

**Fern (fe-architect):**
1. The strip is **part of the hero composite**, not a sibling widget on the
   `Screen` stack. This is load-bearing: `Screen` stacks widgets vertically with
   no notion of bottom-anchoring, and the "drop the stat line when banner + 4
   rows collide" rule needs one owner arbitrating the whole column. Sibling
   widgets cannot express it.
2. Two new palette constants: `STATUS_ERROR_DIM`, `STATUS_WARNING_DIM` (~45-50%
   toward `BACKGROUND` from their bright forms).
3. `redraw_after = min(1 s, next tier boundary)` on the hero widget when the
   strip is non-empty; nothing scheduled when it is empty.
4. The 4px freshness tick should be a **plain filled rect, not
   `draw_selection`'s accent helper.** Home has no selection so there is no live
   collision, but overloading the selection primitive with a second meaning is
   exactly the kind of cheap conflation that forces a branch at every later read
   site.
5. Confirm the 12px row slot against `helvB08`'s real rendered height
   (`line_height` probe); 8+4 is my estimate, not a measurement.

**Ruby (implementer):**
1. `FaultLog` in core: a **fixed array of 6 entries**, one per key — not a
   growable `Vec`, and no allocation per fault event. Entry:
   `{ key, count: u16 (saturating), first_seen: Instant, last_seen: Instant, value: Option<FaultValue> }`.
2. `FaultValue` enum: `Ratio(u16 /* q8 */)`, `Count(u16)`, `Millis(u16)`.
3. Retirement and freshness tiers are **derived at render time** from
   `now - last_seen`. No timer, no retirement event, no background task.
4. `Event::FaultRaised { key, value }` arrives over the **existing `bt.c` MPSC
   ring**, the same vehicle 2ap.1d uses — explicitly **not** a new Rust call from
   IRQ context (`pico-link-6o2` is already a latent bug of that exact shape).
5. Test fixtures, captured zoomed per the project rendering rule: empty; one
   live red row; three rows spanning both severities and both freshness tiers;
   overflow at 4+; banner + full strip with the stat line dropped; a row
   transitioning live -> recent -> retired.

**Ada (architect):**
1. One event carrying (fault key, value), **edge-triggered and debounced in C**
   — same shape and same reason as 2ap.1d's 300 ms-in / 2 s-out debounce. The
   debounce must be C-side so a 6 Hz superloop (`pico-link-p1r`) cannot miss
   edges.
2. Counters for `ENC OVERRUN` and `AIR CONGESTED` may not exist yet. If they do
   not, **ship those two rows absent, never faked** (design section 15). The
   strip is correct with two, four or six rows in the catalogue.
3. `IN HOST AUDIO LOW` is already fully sourced by 2ap.1a's
   `pl_usb_supply_q8()` / `pl_usb_supply_seq()` — build that row first; it is
   the one with real data behind it today.

**Scope note for `pico-link-8jp`:** its Home deliverable is now the value slot on
row 1 plus the Advanced diagnostics page. It adds no independent Home element.
Its firmware half is unchanged.

## 12. Open, and my recommendation

- **60 s retirement window** — the most likely tunable. Start at 60.
- **`diag` on X** — recommend no; see section 8. Andreas's call.
- **Settings > About > Advanced does not exist yet.** The strip ships and stands
  alone without it; the overflow row's `SETTINGS > ABOUT` pointer and the
  session tally both need it, so it should be a follow-on bead, not a blocker.

---

**Caveat from the author:** Uma could not read `pico-link-h62` or
`pico-link-8jp` directly (no Bash tool at the time of writing). She worked from
Andreas's verbatim quote for `h62` and from the 2ap design doc's section 4 plus
the orchestrator's summary for `8jp`. If `8jp` contains a requirement beyond
"live USB supply ratio and episode count on screen", the composition ruling in
section 5 is the part to re-check.
