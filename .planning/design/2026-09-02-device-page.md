# Device page: per-device settings, codec choice, and the field-list mechanic

Beads `pico-link-23q` (the page) and `pico-link-znb.13` (E11, the field-list
widget), with a correction handed to `pico-link-znb.9` (E6, codec availability).
Author: Uma (UX designer), 2026-09-02.

**GOAL.** Andreas, 2026-09-01: *"Device page. I want to be able codecs, etc."*
This document fills `build_device_detail_screen` (`core/src/app.rs:995`), which
today is `Screen::new(title, vec![])` — a labelled destination with nothing in
it.

Documents this amends or depends on:

- `.planning/design/2026-08-28-on-device-ui.md` — design of record. **Sections
  10 and 11 are the spec this document implements.** Two deliberate amendments
  are marked **AMENDMENT** below; everything else follows it.
- `.planning/design/2026-09-01-remembered-devices.md` section 4 — what the
  Devices screen does today. **Amended by section 9 below** (the Devices X
  binding).
- `.planning/design/2026-09-01-home-alignment-grid.md` — `L = 12`, `R = 194`.
  Reused verbatim; no third alignment scheme is invented here.

---

## 1. The one-sentence shape

> **One scrolling field list on one screen. Every row is `LABEL` on `L = 12`
> and a value on `R = 194`. Rows you can press are bright and grow a caret
> when focused; rows you can only read are dim and never do. The destructive
> one is last, red, and behind a confirm.**

Everything below is that sentence with the numbers filled in.

---

## 2. Entry, exit, depth

| Gesture | From | Result |
|---|---|---|
| `A` on the connected row | Devices | Device page, depth 1 |
| `X` on any paired row | Devices | Device page, depth 1 (**changed** — see §9) |
| `X` | Home (menu face) | Device page for the connected device, depth 1 |
| `B` | Device page | Back to wherever it was pushed from |
| `A` on `CODEC` | Device page | Codec picker, depth 2, terminal |
| `A` on `LDAC QUALITY` | Device page | Quality picker, depth 2, terminal |
| `A` on `Forget this device` | Device page | `ConfirmView`, focused on Cancel |

`A` on any other row does nothing at all. That is not a gap; it is §4's row
kinds doing their job.

### 2.1 The button rail on this page

| Button | Label | Meaning |
|---|---|---|
| A | (per row) | Activate the focused row — inert on informational rows |
| B | `back` | Universal Back. Never anything else, on any screen, ever. |
| X | `drop` when connected / `link` when not | Disconnect / connect this device |
| Y | `Inert` | Nothing. A mispress is free. |

**AMENDMENT to section 10.** The design of record says X is Disconnect *"and
only when the device is actually connected; otherwise the slot is inert and
unlabelled."* Make the disconnected slot **`link`** instead of inert. Rule 2 —
"an unlabelled X or Y does nothing, so a mispress is always free" — is about
never having a *labelled* control that lies; it is satisfied by both states
being labelled and both being real. An inert slot is only justified when there
is nothing sensible to put there, and on a disconnected device's page the
single most likely intent in the room is "connect to this one". Leaving it
inert forces `B`, scroll, `A` — three presses to do the obvious thing on a
screen already showing exactly one device.

Both labels are 4 characters, inside `rail.rs`'s 5-char budget, and both come
from the vocabulary the product already uses ("the link", `LinkState`).
Rejected pairs: `on`/`off` (shortest and clearest as a toggle, but `off` on a
screen titled with the headphone's name can be misread as powering the dongle
down, and this product has no power-off); `conn.`/`disc.` (abbreviations are
exactly what a glanceable rail must not contain).

Neither X action needs a confirm. Both are reversible by pressing X again and
cost only the 2-8s reconnect the wizard's phase display already narrates.
Section 4 rule 1 reserves `ConfirmView` for the irreversible; spending it here
would devalue it on the row where it matters.

---

## 3. The rows, in order, with every value they can show

Row order is **choices, then facts, then the destructive one.** Choices first
because they are why the user came; facts below because they are read, not
acted on; Forget last because physical distance is a safety feature (section
10's whole argument for it being a row rather than a button).

| # | Label | Kind | Trailing value | Present when |
|---|---|---|---|---|
| 1 | `CODEC` | Action | see §3.1 | always |
| 2 | `LDAC QUALITY` | Action | see §3.2 | effective-or-pinned codec is LDAC |
| 3 | `EQ PRESET` | Action | preset name, or `None` | **never in Tier 1** — see §7 |
| 4 | `SAMPLE RATE` | Info | `48 kHz`, or `—` | always |
| 5 | `USB IN` | Info | `—` in Tier 1 — see §3.4 | always |
| 6 | `A2DP` | Info | see §3.5 | always |
| 7 | `ADDRESS` | Info | `94:DB:56:54:7C:F2` | always |
| 8 | `Forget this device` | Action, destructive | `Trailing::Caret` only | always |

`SIGNAL` is **absent**, not dashed. `USB IN` is **dashed**, not absent. This is
the single most misread rule in the design of record (section 13) and the two
rows sit next to each other precisely so a reviewer can check both at once.
`SIGNAL` is cut because connected-link RSSI is unconfirmed and a permanently
dashed signal meter is a standing admission of ignorance on the product's
credibility surface; `USB IN` is dashed because the number exists in principle
and is merely not plumbed, which is what an em-dash *means*.

Tier 1 is therefore **7 rows** (8 when LDAC is in play — rows 2 and 3 are never
both present today).

### 3.0 The degraded-field rule, sharpened

Section 10: *"if a value is unavailable, show the label with an em-dash."*
That rule is about **live** fields. Sharpen it, because this page is the first
screen that shows both kinds:

> **A live field dashes when the link is down. A stored setting never dashes,
> because it is still true when nothing is connected.**

So on a paired-but-not-connected device: `SAMPLE RATE`, `USB IN` and `A2DP`
show `—`, while `CODEC` still shows the stored pin (or `Automatic`) and
`ADDRESS` still shows the address. Dashing the codec row on a disconnected
device would tell the user their setting had been lost, which is precisely the
opposite of what the persistence work just bought.

Dim the live-but-dashed values to `TEXT_SECONDARY`; leave stored values at
`TEXT_PRIMARY`.

### 3.1 `CODEC` — the headline

**Ruling: the user pins a codec per device. Pinning is a promise the product
keeps loudly or breaks loudly. It is never a silent preference and never a
hard force that can leave the user with no audio.**

Three states for this device's codec choice, stored in the record byte
`codec_id` that `pl_persist_device_record_t` already reserves:

- **`Automatic`** (default, and `codec_id == 0`) — negotiation walks
  `PL_CODECS` in array order, first match wins. This is exactly today's
  behaviour, so shipping the row changes nothing until someone presses it.
- **Pinned to a codec** — only that codec's SEP is offered at stream setup.
- (There is no global preference. Section 11's reasoning stands: a global
  default for headphones we have not met yet cannot be honoured, because the
  codec set is a per-device intersection computed after AVDTP discovery.)

Automatic is kept as a distinct entry rather than being collapsed into "pin the
top of the table" for two reasons: it is the only way *back* after a pin, and
it is the state that never produces the amber anomaly below — "give me your
best" and "give me LDAC specifically" are different wishes and only the second
one can be disappointed.

**What happens when a pin cannot be honoured.** Fall back to SBC and say so.
Never refuse to connect. A dongle that produces silence because of a settings
choice made three weeks ago is a worse product than one that plays and
complains — and the complaint is already designed: the Home hero turns amber
and X on Home is already labelled `why?` under fallback, routing here.

That gives the trailing value:

| Situation | Trailing text | Colour |
|---|---|---|
| Connected, `Automatic`, streaming LDAC | `LDAC` | `TEXT_PRIMARY` |
| Connected, pinned LDAC, honoured | `LDAC` | `TEXT_PRIMARY` |
| Connected, pinned LDAC, fell back | `SBC` | **`STATUS_WARNING`** |
| Connected, changed but user chose Later | `SBC pending` | `STATUS_WARNING` |
| Not connected, `Automatic` | `Automatic` | `TEXT_PRIMARY` |
| Not connected, pinned | `LDAC` | `TEXT_PRIMARY` |

**Amber carries the anomaly; the picker carries the explanation.** One colour
change says "you did not get what you asked for" without spending a single
extra pixel, and one `A` press gets the sentence. That is section 11's
"diagnosis and remedy are one gesture", one level deeper.

### 3.2 `LDAC QUALITY`

Present only when the effective-or-pinned codec is LDAC (section 10's rule).
Values: `990 kbps`, `660 kbps`, `330 kbps`, `Adaptive`. Stored in
`ldac_quality`; the default is whatever `codec_ldac.c` configures today —
**Ruby must read that constant, not take a number from this document.**

Considered and rejected: keeping the row permanently present but dimmed with
the reason "LDAC not active", by analogy with the codec picker's disabled rows.
Rejected because the analogy fails — a disabled codec row's reason text *is*
the payload the user came for, whereas "this device does not do LDAC" is
already stated one row up, in colour, on `CODEC`. A permanently dim row that
repeats its neighbour is noise, and this page has 240 vertical pixels.

**`Adaptive` has a stated consequence, and the picker states it.** Under ABR
there is no honest nominal bitrate to print, and section 15 forbids faking one.
So when quality is `Adaptive`, **Home's bitrate slot renders the word
`Adaptive`, right-aligned to `R = 194` in `font::value`, instead of a number.**
Not an em-dash: a dash means "we do not know", and we do know — it varies by
design. The picker's `Adaptive` row carries the trailing note `varies` so the
trade is visible at the moment of choosing, not discovered afterwards on Home.

### 3.3 `SAMPLE RATE`

The A2DP link's negotiated rate — `pl_codec_format_t.sample_rate_hz`, which C
already has. `48 kHz`. Dashes when not connected. Section 13 classifies this
"Expected", i.e. dash-not-cut.

### 3.4 `USB IN`

The host's stream format. **Dashed in Tier 1** and dashed until an event
carries it; do **not** synthesise it from the codec format, which is the
downstream side of the pipe and can differ. See §10 for the FFI ask.

### 3.5 `A2DP`

The one row that distinguishes "connected but silent" from "working", which is
the most common thing a user is actually trying to find out when they open
this page at all.

| Value | Colour | Means |
|---|---|---|
| `Streaming` | `STATUS_SUCCESS` | media transport open and packets flowing |
| `Idle` | `TEXT_SECONDARY` | signalling connected, no stream |
| `—` | `TEXT_SECONDARY` | not connected |

### 3.6 `ADDRESS`

`94:DB:56:54:7C:F2`, colons kept — the point of this row is comparing against
what a phone or laptop shows, and every other Bluetooth UI in the world prints
colons.

**It does not fit on one line at `font::value`.** Measured budget: content
width `R − L = 182px`; `helvR12` runs ≈7px/char, so 17 characters is ≈119px,
plus the label `ADDRESS` at ≈55px plus margins — over budget. Two ways out:

1. **Preferred: render this row's trailing value in `font::label` (helvB08)**
   instead of `font::value`. ≈4.5px/char → ≈77px, total ≈146px, comfortable.
   Small text is correct here anyway: an address is reference data you copy
   character by character with the device in your hand, not something you read
   at arm's length.
2. Fallback: a two-line row kind (label above value). Costs a new row kind, a
   variable row height, and breaks the page's uniform rhythm for its least
   important row.

Take 1. It costs the widget one per-row font override; see §8.

### 3.7 `Forget this device`

Label in `STATUS_ERROR`, `Trailing::Caret`, last row. `A` pushes the shared
`ConfirmView` **focused on Cancel**. Copy:

```
Forget "Sony WH-1000XM5"?

You will have to pair it again.

  A  Forget          B  Keep
```

On confirm: pop **two** levels (the confirm and the device page), landing on
Devices, which no longer lists it. Do not leave the page open on a device that
no longer exists.

**Forget must clear the device record's `preset_id` reference and must never
touch the global preset store.** Andreas's standing ruling — "if I forget a
device then the EQ will not be lost". Written here as an invariant for whoever
builds the preset store, because by then this document will be the only place
that remembers why the field is a reference and not a value.

---

## 4. Three row kinds, and the finding that collapses two beads

| Kind | Label colour | Trailing | Caret when focused | `A` |
|---|---|---|---|---|
| **Action** | `TEXT_PRIMARY` | value | yes | activates |
| **Info** | `TEXT_SECONDARY` | value | **never** | nothing |
| **Disabled** (picker only) | `TEXT_SECONDARY` | *reason text* | **never** | nothing |

**Focus lands on every row, including Info and Disabled ones.** For Disabled
rows that is `pico-link-znb.9`'s explicit requirement — the reason text is the
payload. For Info rows it is what makes scrolling predictable: a list where
focus skips rows moves by an amount the user cannot anticipate, which on
press-edge-only input with no key repeat is a genuine navigation failure rather
than a nuisance.

> **THE FINDING: `znb.9`'s "disabled but focusable" row and `znb.13`'s
> "informational" row are the same widget capability.** Both are: focusable,
> dim, no caret, `A` is a no-op, trailing text drawn regardless of focus. One
> capability closes both beads. Neither bead saw this, because each was
> specified from its own screen.

**Actionable-ness is signalled twice, statically and on focus.** Statically by
label colour, so the user can see what is pressable without moving focus — on a
diagnosis page you scan before you act. On focus by the caret, confirming that
`A` will do something *now*. Colour alone would be too subtle to be a promise;
the caret alone would require touring the whole list to discover that five of
its seven rows are inert.

---

## 5. Layout, in absolute panel pixels

Reusing the Home grid's rules exactly: **left rule `L = 12`, right rule
`R = 194`**, content region `x 0..205`, `y 16..239`, rail 34px on the right.

| Band | y | h |
|---|---|---|
| Title bar (device name) | 0..15 | 16 |
| top gutter | 16..27 | 12 |
| field rows, 28px each | 28..… | 28 × n |
| bottom gutter | remainder | ≥15 |

- **Row height 28px**, not `menu::row_height()`'s ~36. `menu.rs`'s
  `ROW_PADDING = 10` is tuned for a two-row action menu with room to spare;
  this page has 7-8 rows and 224px. 28 = 6px padding + a `helvR12` line. Eight
  rows land at `28..252` — over the edge — which is exactly why §8 asks for
  scrolling rather than trusting the arithmetic.
- **Row style margins are 12 left and 12 right**, panel-absolute, *not*
  `menu.rs`'s 8 and 6. The field list is given the full content width and
  honours the grid itself. The alternative — insetting the widget's area by 4px
  so that `4 + 8 = 12` — produces the right ink but an inset selection band and
  a magic 4 nobody can explain in six months. The alignment grid is an absolute
  rule; make the row style state it.
- Trailing values **right-align to `R = 194`** into a slot cleared to
  `BACKGROUND` first, per the Home grid's numeric rule, so `990 kbps` →
  `SBC` never leaves debris.
- Label is truncated with an ellipsis before the value is; the value is never
  truncated. Every value on this page is short and load-bearing; every label is
  a constant we chose.
- **Nothing on this page animates, scrolls text, or blinks.** `redraw_after`
  returns `None`; the page redraws on event only. Full-frame blits contend with
  audio on SPI (`pico-link-6wz`), and this is a screen people leave open.

### 5.1 Title bar

The **full, untruncated** device name on `L = 12`, `font::title` (helvB10),
contrasting with Home where it truncates. At ≈5.5px/char the 182px measure
holds ≈33 characters and the store's name cap is 32 bytes, so in practice this
never truncates. If a measured name does overflow: **ellipsise it. Never scroll
it.** A marquee on a diagnosis screen costs a full repaint every frame,
forever, for a screen the user is staring at while something is wrong.

Nameless devices use the Devices screen's existing label verbatim:
`(unknown device) 54:7C:F2`.

---

## 6. ASCII sketches

Left rule at x=12, right rule at x=194.

**Connected, LDAC, everything nominal:**

```
+----------------------------------------+-----+
| Sony WH-1000XM5                        |     |  y 0..15
+----------------------------------------+  A  | pick
| CODEC                            LDAC >|     |  y 28..55   bright + caret
| LDAC QUALITY                 990 kbps  |     |  y 56..83   bright
|                                        +-----+
| SAMPLE RATE                     48 kHz |  B  | back
| USB IN                               - |     |             dim
|                                        +-----+
| A2DP                         Streaming |  X  | drop
| ADDRESS             94:DB:56:54:7C:F2  |     |             small font
|                                        +-----+
| Forget this device                     |  Y  |             red
|                                        |     |
+----------------------------------------+-----+
 ^L                                    R^
```

**Connected, but a pinned LDAC fell back — the state this page exists for.**
`LDAC QUALITY` is still present, because §3.2's rule is *effective **or**
pinned is LDAC*, and the pin is LDAC:

```
+----------------------------------------+-----+
| Sony WH-1000XM5                        |     |
+----------------------------------------+  A  | pick
| CODEC                             SBC >|     |   amber
| LDAC QUALITY                 990 kbps  |     |
|                                        +-----+
| SAMPLE RATE                     44 kHz |  B  | back
| USB IN                               - |     |
|                                        +-----+
| A2DP                         Streaming |  X  | drop
| ADDRESS             94:DB:56:54:7C:F2  |     |
|                                        +-----+
| Forget this device                     |  Y  |   red
|                                        |     |
+----------------------------------------+-----+
```

Eight rows at 28px is `28..252` — **past the bottom edge**. This is the exact
state that makes scrolling non-optional (§8 item 3), and it is a realistic
state, not a contrived one.

**Paired, not connected — stored settings survive, live fields dash:**

```
+----------------------------------------+-----+
| (unknown device) 54:7C:F2              |     |
+----------------------------------------+  A  | pick
| CODEC                       Automatic >|     |
| SAMPLE RATE                          - |     |
|                                        +-----+
| USB IN                               - |  B  | back
| A2DP                                 - |     |
|                                        +-----+
| ADDRESS             94:DB:56:54:7C:F2  |  X  | link
| Forget this device                     |     |
|                                        +-----+
```

**Codec picker (depth 2, terminal), for a device that never offered LDAC:**

```
+----------------------------------------+-----+
| Codec                                  |     |
+----------------------------------------+  A  | pick
| v Automatic                    SBC now |     |
|   LDAC                      not offered|     |   dim, focusable
|                                        +-----+
| v SBC                    always availa.|  B  | back
|                                        |     |
+----------------------------------------+-----+
```

(only one check is ever drawn; both are shown above to place the gutter.)

---

## 7. Three things this page deliberately does NOT have

### 7.1 Rename — no. Not now, and probably not on this hardware.

Be honest about text entry here, because the cost is not close. An on-screen
keyboard on 240x240 driven by a 5-way with **no key repeat** and press-edge-only
input costs roughly 4-8 deliberate presses per character. A 16-character name
is 60-100 presses. Nobody does that twice, and the people who would do it once
are the people who would rather plug in a terminal.

The payoff is small, too: the name comes from the headphones and is usually
right. The case where it is wrong is the nameless device, and section 13
already accepted `(unknown device) 54:7C:F2` as a **first-class label, not a
placeholder**.

**But the underlying complaint is real and has a cheap answer.** The only
genuine failure is two nameless devices you cannot tell apart. That does not
need free text — it needs a **`LABEL` row offering a short fixed vocabulary**
(`Headphones`, `Earbuds`, `Speaker`, `Car`, `Desk`, `Spare`), assigned with one
`A` press from a single-select list identical in mechanics to the codec picker.
Zero text entry, solves the actual problem, reuses a widget we are building
anyway.

**Not in this bead**, because the store has no bytes for it — `name[32]` holds
the Bluetooth name and must keep doing so — and "a setting with no persistence
behind it does not appear". File it as a follow-up; it needs a store-schema
change and therefore Ada.

### 7.2 `EQ PRESET` — designed, not shipped

The DSP does not exist (`pico-link-ryw`). The *persistence* does — `preset_id`
is reserved and `pl_persist_do_write` is read-modify-write. That combination is
the trap: the row would save your choice perfectly and change nothing you can
hear. That is a worse lie than a toggle that forgets, because it survives a
reboot and therefore looks trustworthy.

**Ruling: the row is absent until the DSP exists.** Its slot is specified here
(row 3, between `LDAC QUALITY` and `SAMPLE RATE`, trailing = preset name or
`None`, Action kind, opens a single-select list of global presets) so that
adding it later moves nothing above it and requires no redesign.

The assignment semantics are fixed now, because they are the part that is
expensive to get wrong: **presets are global objects; a device holds a
reference.** Forgetting a device clears the reference and never the preset.

### 7.3 Group headers, section dividers, extra gaps

Rejected. `menu::draw_row` already draws a hairline under every unselected row,
so the rhythm is uniform for free; adding gaps between "choices" and "facts"
would spend 8-16 vertical pixels to restate what label colour already says.
Row kind is signalled by colour and caret, not by whitespace.

---

## 8. What the widget must do — and it is `MenuList`, extended

`pico-link-znb.13` says reuse over new, and asks what specifically neither
existing widget can express. Here is that answer, then the recommendation.

**What `MenuList` (`core/src/render/menu.rs:277`) already gives us, and it is
most of it:** one-line rows, `Trailing::Label` drawn *regardless of selection*
(exactly the "value stays visible when focus moves on" semantics a field list
needs — see the comment at `menu.rs:55-60`), `Trailing::Caret` drawn only on
the focused row (exactly "you can press this"), `with_label_color` for the red
Forget row, `on_activate_index`, and `with_selected`/`with_focused` for the
rebuild-from-live-data pattern this page uses on every event.

**What it cannot express:**

1. **A row that holds focus but refuses activation.** `on_activate_index` fires
   for every row. Needed by five of this page's seven rows and by the codec
   picker's unavailable entries. (`znb.9` + `znb.13`, one capability — §4.)
2. **A value *and* a caret on the same row.** `OwnedTrailing` is an enum: a row
   is `Label` or `Caret`, never both. `CODEC` must show `LDAC` always and a
   caret on focus.
3. **Scrolling.** Documented as deliberately absent at `menu.rs:270-277`
   ("every current caller has at most two rows"). This page has 7 today, 8 with
   LDAC, 9 once `EQ PRESET` lands. At 28px rows only 8 fit the 224px content
   region — a row beyond the viewport is currently *silently not drawn*, which
   is section 10's "a missing row is indistinguishable from a layout bug"
   failure arriving by default.
4. **A per-row trailing font.** `ADDRESS` needs `font::label`, everything else
   `font::value` (§3.6).
5. **A leading fixed-width glyph gutter** for the pickers' current-choice
   check. Trailing is occupied by the reason text, so the check must go on the
   left, in a reserved-width gutter so labels align whether checked or not.
6. **Its own row-style margins** — 12/12, not 8/6 (§5).

`VerticalList` (`list.rs:410`) is not the answer either: it scrolls (good) but
its row is a two-line name+sublabel block, taller than `MenuList`'s, so it fits
*fewer* rows on a page that is already short of them, and its sublabel puts the
value under the label rather than opposite it — which breaks the `R = 194` rule
the whole product now aligns to.

**RECOMMENDATION — Fern's call, not mine.** Extend `MenuList` rather than add a
fourth list type. Every capability above serves at least two screens (device
page, codec picker, LDAC quality picker, and Settings when it gets built), and
a fourth list widget that overlaps `MenuList` by 80% is how a render layer
starts to rot. Concretely: an `activatable: bool` on `MenuItem` (default
`true`) that gates `on_activate_index`; a `Trailing::LabelAndCaret` variant; a
leading-glyph slot; per-row trailing font; viewport scrolling delegated to
`list::reconcile_top_index`, which already exists and is already tested; and a
compact row-height/margins style selected by the caller.

That is six additions, which is a lot to call "reuse" — so if Fern's read is
that this has crossed the line into a distinct row style, the right shape is a
thin `FieldList` in `core/src/render/fields.rs` that **reuses
`menu::draw_row`** for drawing and owns only the scrolling, the row kinds and
the margins. What must not happen is a second, independently-drawn row that
drifts from `menu.rs` on selection fill, divider or caret. **Fern decides;
this document only states what has to be expressible.**

---

## 9. Forget: one destination, one route in — and the Devices X binding changes

`pico-link-23q` asks whether Forget belongs on this page given that Devices
already has it on X, and how to avoid two divergent routes.

**Ruling: Forget belongs ONLY here, and the Devices screen's `X` changes from
"forget confirm" to "open this device's page."**

The shipped binding (`.planning/design/2026-09-01-remembered-devices.md`
section 4) puts an irreversible action one `X` press away from a scrolling
list, with only the confirm between a fat finger and re-pairing. That is
exactly the "button you can fat-finger" that section 10's *"a destructive
action is safer as a row you must scroll to"* argument exists to prevent — and
that argument was written before the destination row existed, so Devices-X was
a reasonable interim. It is not reasonable now that there is somewhere better
to put it.

Restoring `X` = "manage selected device" also puts the Devices screen back in
line with the design of record's section 4 table, and it is the only way to
reach a **non-connected** device's page at all, which this page needs.

Two more things fall out and both are improvements:

- Deleting a device becomes: `X` (see the device) → scroll to the bottom →
  `A` → confirm-on-Cancel. Four deliberate acts, on a screen that shows you
  exactly which device you are about to lose, by its full untruncated name.
- The confirm text can name the device, because the page already does.

**One builder, one copy string.** `build_forget_confirm(addr, name, commands)`
is the single source; the device page's row and the store-full picker
(`build_forget_picker_screen`) both call it. A test should assert both routes
produce the same confirm with Cancel focused. Two entry points to one builder
is fine; two builders is the thing that drifts.

**This changes shipped behaviour → it needs its own bead for Ruby**, and it
should land in the same change as the page, or the Devices screen briefly has
no way to reach the destination that replaced its X.

---

## 10. Handoff

### For Ada (FFI / firmware — do not let anyone else spec these seams)

1. **A command to set a per-device codec pin and LDAC quality.** Payload
   carries the address, so it works on a device that is not connected. Writes
   `codec_id` / `ldac_quality` in the record.
2. **The pin write must bypass `persist.c`'s 2s settle timer.** Standing
   ruling: deferred writes lose to a power cycle. Andreas accepted "a little
   delay before sound" for pairing; he did not accept losing a setting he just
   chose. The 10s rate limit is fine; the settle is not.
3. **`znb.9`'s availability model needs three states, not a bool.** The bead
   says "(codec, available, reason)". `Unknown` is a distinct third state and
   it behaves differently in the UI: for a device we have never completed AVDTP
   discovery with, LDAC is **not** unavailable — it is unknown, the row reads
   `not yet known`, and it stays **activatable**, because pinning a hope for
   the next connection is a legitimate thing to want. Only `not offered` and
   `refused` are disabled. A bool would render "we have not asked yet"
   identically to "your headphones cannot do this", which is the exact
   misdiagnosis this whole page exists to prevent.
   Reason vocabulary, ≤14 chars each so it fits beside a 4-char codec word:
   `not offered` / `not yet known` / `refused` / `always available`.
4. **`CodecChanged` should carry `sample_rate_hz` and `bits_per_sample`**, for
   the `SAMPLE RATE` row. C already has both in `pl_codec_format_t`.
5. **`USB IN` needs its own event** (host stream format from the UAC alt
   setting). Until it exists the row is a dash — do **not** derive it from the
   codec format. Ada's call whether this is cheap enough to be Tier 1.
6. **An A2DP stream-state signal** (`Streaming` / `Idle`) for row 6.
7. **A "pin not honoured" signal** — whatever C reports when a pinned codec
   fell back. This drives the amber trailing on `CODEC`, the amber hero on
   Home, and Home's `why?` label. Without it, a broken promise is invisible,
   which is the one outcome §3.1 refuses.
8. **Whether LDAC quality can be applied without tearing down the stream.** If
   it can, the quality picker skips the reconnect confirm entirely and applies
   live — strictly better, and worth checking before building the confirm.

### For Fern (widget model)

- §8, in full. The one thing to decide: extend `MenuList` (recommended) versus
  a thin `FieldList` over `menu::draw_row`. Either is fine; a second
  independently-drawn row style is not.
- The row-kind collapse in §4 — one capability closes both `znb.9` and
  `znb.13`.
- Row-style margins of 12/12 (§5), because the alignment grid is a
  panel-absolute rule and every screen must now honour it.

### For Ruby (implementation)

- Fill `build_device_detail_screen` (`core/src/app.rs:995`) per §3, using
  whatever widget Fern lands.
- Take the device's identity as an address, not a title `String` — the current
  stub signature takes only a title, and every row here needs the record.
- Rows are rebuilt from live model state on every relevant event, carrying the
  selection forward by row identity, exactly as `build_devices_screen` already
  does with `with_selected_identity`.
- Read the LDAC quality default from `codec_ldac.c`; do not take §3.2's example
  numbers as the default.
- `redraw_after` → `None`. Nothing on this page animates.
- The Devices `X` rebinding (§9) — separate bead, same landing.

### Verification (Tess)

Headless PNGs **at zoom**, per the project's rendering-verification discipline:

1. Connected, `Automatic`, LDAC — all rows nominal.
2. Connected, pinned LDAC, fell back to SBC — `CODEC` trailing is amber.
3. Paired, not connected — live fields dashed, `CODEC` still showing its pin,
   X labelled `link`.
4. Nameless device — title reads `(unknown device) 54:7C:F2`.
5. Codec picker with LDAC dim and `not offered`, focus **on** the dim row.
6. Codec picker for a never-connected device — LDAC reads `not yet known` and
   is activatable.
7. The Forget confirm, focused on Cancel, naming the device.

Assertions worth their own tests: `SIGNAL` is absent while `USB IN` is dashed;
the caret appears only on Action rows; `A` on an Info row returns
`Action::None`; both forget routes produce the identical confirm; the longest
plausible name plus the longest trailing value do not collide at `R = 194`.
