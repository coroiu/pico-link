# Design: the button rail and the chrome edge region

**Bead:** `pico-link-znb.5` (E2) · **Date:** 2026-08-29 · **Author:** Fern (Frontend Architect) · **Status:** proposed

## Verdict — one bead, minus one impossible requirement

**Keep `pico-link-znb.5` as a single bead.** The chrome region, the label fields,
the render path and the hint-bar removal are not separable: an interim state
where a 34px rail and an 18px hint bar both claim to be the control legend is a
screen that contradicts itself, and design section 4 rule 2 becomes
unverifiable. One commit.

**But one requirement in the bead is not satisfiable and is carved out into a
follow-on bead.** The bead says the rail's edge and slot order "must derive from
the SAME constant that maps GPIO to `NavIntent` (see `core/src/input.rs`)".
`core/src/input.rs` contains no such constant — it is the `NavIntent` enum and
nothing else (verified: zero `const`/`static` declarations). **The GPIO→intent
mapping lives in C**, at `firmware/src/input.c:36-48`, on the far side of the
FFI, in a language `core/` cannot and must not reach.

That table has *already been rotated once* (`input.c:15-35`), and is marked
UNVERIFIED ON HARDWARE. So the rotation is not "parked" — it has moved, in C,
silently, with no counterpart in `core/`. A rail hard-coded to `Right` today
would be the second half of that same divergence.

**Seam:** this bead defines the single orientation constant *in Rust* and makes
the rail (and the future gauge) derive from it. A **follow-on bead** moves the
transform out of the C pin table into `core/`, so the C table becomes the
identity mapping and one constant genuinely drives both the d-pad semantics and
the rail. That bead depends on this one and on the rotation being settled;
**this bead must not wait for it.** Its "done" test — flipping the constant
mirrors the rail — is fully provable here.

---

## ORCHESTRATOR CORRECTION, 2026-08-29 — read before implementing §3

Fern describes the C table as a **180° flip**. The comment at `input.c:15-35`
says that too, and the comment's *reasoning* is internally sound. But the table
as it actually stands is a **90° rotation**, not a 180° one:

```
PIN_UP    (GP2)  -> LEFT
PIN_LEFT  (GP16) -> DOWN
PIN_DOWN  (GP18) -> RIGHT
PIN_RIGHT (GP20) -> UP
```

That is a consistent quarter-turn cycle (up→left→down→right→up). A true 180°
flip would be up↔down and left↔right. Both facts are reconcilable — the comment
says the *prior* mapping (for `MADCTL=0x60`) was itself a quarter turn and that
180° was applied on top of it, which composes to the quarter turn now present —
but the net physical-to-logical relationship is **a quarter turn, not a half
turn**, and it is UNVERIFIED ON HARDWARE.

**Consequence for this design:** `PanelOrientation` as a two-value enum
(`ButtonsRight` / `ButtonsLeft`) assumes the only possible divergence is a 180°
flip, which puts the buttons on the left or the right. Under a quarter turn the
button column would land on the **top or bottom** edge instead. Two values may
therefore be insufficient.

**Ruling: implement §3 as specified with two values anyway.** Reasons: the
quarter turn is unverified and may be a bug in the C table rather than the truth
about the hardware; the product fact from Andreas is unambiguous (buttons are a
column on the right, d-pad on the left); and `Edge` is already a four-value
concept in Fern's design, so widening `PanelOrientation` later is additive, not
a refactor. **But `carve_edge` must handle all four `Edge` values from day one**
— do not write it as a left/right special case, or the widening becomes the
rewrite this design exists to avoid. Note the discrepancy in the module doc
comment so the next reader does not have to rediscover it.

---

## Context

- `core/src/render/chrome.rs:31-68` — three stacked full-width regions,
  closed-form, saturating.
- `core/src/render/screen.rs:132-140` (`Screen::hint`), `:290-416` (`render`),
  `:403-413` (hint draw).
- `core/src/render/widget.rs:119-156` — current `ChromeContribution`: `title`,
  `readout`, `hint`, `status`, `link`, `fallback`.
- `core/src/render/navigator.rs:225-229` — the only production `compute_chrome`
  caller.
- `firmware/src/input.c:15-48` — the real GPIO→intent table, already rotated.
- `core/src/render/theme.rs:165` `font::hero()` (helvB24, E3 landed), `:175`
  `font::hint()` = **helvR08**, exactly the rail font the design asks for. No
  new font needed.

## 1. The seam: a fourth region inside the same closed-form computation

`ChromeLayout` gains `rail: Rectangle` and `orientation: PanelOrientation`.
`compute_chrome` keeps computing every region in one pass — no constraint
solving, no flexbox. Carve order: **title off the top (full width, untouched) →
rail off one side of the remaining band → content is the remainder.**

The title bar stays full-width and spans *over* the rail column. This is
deliberate: it means the entire shield/readout/link-glyph/status-dot
right-cursor arithmetic at `screen.rs:327-386` is **byte-for-byte unaffected**,
which removes the largest regression surface in the bead. The rail's top
boundary is the title bar's existing bottom hairline.

Factor the carve as a private helper:

```rust
fn carve_edge(band: Rectangle, edge: Edge, width: u32) -> (Rectangle /*band*/, Rectangle /*rest*/)
```

The gauge bead (F5/E16) calls `carve_edge` a second time with
`orientation.gauge_edge()` and `GAUGE_WIDTH` and changes nothing else. **Do not
add a `gauge` field now** — an always-zero public field is dead API that invites
misuse. The guarantee against a second refactor is the helper, not a placeholder
rect.

### Options rejected — read these, they are what stops the rewrite

- **REJECTED: paint the rail in `Navigator::render` (or at the end of
  `Screen::render`) as a decoration on top of a still-240-wide content area.**
  Smallest diff, and the one an implementer will drift toward. Wrong for a
  reason invisible in tests: content would still be laid out 240px wide and the
  rail would *overpaint* it. It looks correct until a `VerticalList` selection
  highlight (`theme::draw_selection`) runs underneath the labels, or a hero
  codec word extends beneath them. Painting over is not the same as not drawing
  there. Also, labels are resolved from the screen's static defaults *merged
  with the focused widget's* `ChromeContribution`, and
  `Screen::chrome_contribution` is `pub(super)` and focus-private
  (`screen.rs:195-197`), so the navigator would have to duplicate that
  resolution. **The rail's width must be subtracted from content, therefore it
  must be in `ChromeLayout`.**
- **REJECTED: a wrapping layer that pre-shrinks the screen rect and calls
  `compute_chrome` on the remainder.** Creates two coordinate spaces; every
  existing consumer of a chrome rect has to know which one it is in; the title
  bar loses full width and its right-cursor arithmetic must be re-derived; and
  the gauge becomes a third nested wrapper.
- **REJECTED: the rail as a `Widget` in the content stack.** The widget stack is
  vertical-only and content-scoped. A rail is chrome: present on every screen
  regardless of widgets, drawn from screen-level resolved state, and must never
  participate in `measure`/focus. And every screen author would have to remember
  to add it — which guarantees a screen ships without one.
- **REJECTED: making `compute_chrome` general.** Standing ADR (see Risks — that
  ADR file does not exist on disk).

## 2. The hint bar dies

**Remove `HINT_BAR_HEIGHT`, `ChromeLayout::hint`, `Screen::hint`,
`Screen::with_hint`, `ChromeContribution::hint`, `HINT_SIDE_MARGIN`, and the
draw block at `screen.rs:403-413`.**

1. **Two legends that can disagree is strictly worse than one.** A screen
   setting `hint: "X: rescan"` while the rail shows X dim is a live lie on
   screen, and design section 4 rule 2 ("an unlabelled X does nothing, so a
   mispress is free") becomes unverifiable by inspection.
2. **It is already the same content.** Every existing hint in the repo is the
   string `"Up/Down  Select  Back"` — literally the control legend the rail
   supersedes, only less precise (it does not say *which* button).
3. 18px of 240 (7.5%) for text the rail says better, on a screen whose design
   budget is 188x224.

**Nothing the hint bar can do survives it.** Prose ("Put your headphones in
pairing mode") belongs in content per design section 9 phase 1 — instructional
body copy, never chrome. D-pad affordance is covered: Up/Down is universally
"move focus" (section 4), and Home's Up/Down=volume exception is displayed by
the gauge (F5).

**Migration — exactly 5 call sites, all identical, all
`.with_hint("Up/Down  Select  Back")`:**

| File | Line | Action |
|---|---|---|
| `core/src/app.rs` | 228 | drop; the placeholder root screen gets no static labels (A/X/Y inert, B dim at depth 1) |
| `core/examples/render_scene.rs` | 18 | drop |
| `core/tests/render_png_dump.rs` | 31 | drop |
| `emulator/examples/render_via_surfaces.rs` | 39 | drop |
| `emulator/tests/surface_parity.rs` | 42 | drop |

No caller needs a replacement string.

## 3. Parameterisation — one axis

New module `core/src/panel.rs` (**not** under `render/`: input will consume it
in the follow-on bead).

```rust
pub enum PanelOrientation { ButtonsRight, ButtonsLeft }

/// TODAY'S VALUE. The single knob. Flipping this must flip the rail edge,
/// the rail's vertical slot order, and (later) the gauge edge, together.
pub const PANEL: PanelOrientation = PanelOrientation::ButtonsRight;

impl PanelOrientation {
    pub const fn button_edge(self) -> Edge;      // Right | Left
    pub const fn gauge_edge(self) -> Edge;       // always the opposite
    pub const fn slot_order(self) -> [Button; 4];
}
```

**The part that is easy to get wrong: a 180° rotation flips the vertical order
too.** `ButtonsRight` → `[A, B, X, Y]` top to bottom. `ButtonsLeft` →
`[Y, X, B, A]` top to bottom. Flipping only the edge produces a rail that is
mirrored horizontally and *lying vertically* — precisely the failure mode the
parameterisation exists to prevent. The mirror test must assert both.

See the ORCHESTRATOR CORRECTION above: `Edge` must be a full four-value concept
and `carve_edge` must handle all four, even though `PanelOrientation` ships with
two values.

`compute_chrome(size)` keeps its signature and reads `PANEL`. Add
`compute_chrome_for(size, orientation)` for tests. `ChromeLayout` carries
`orientation` as a field so it reaches `Screen::render` with zero extra
plumbing.

`Button` is a logical identity (A/B/X/Y); `slot_order()` maps physical position
→ logical button. Labels are stored per logical button and looked up through the
order. That decoupling is what makes the mirror test meaningful rather than
cosmetic.

## 4. Label fields — inert distinct from absent

```rust
// core/src/render/rail.rs
pub enum Button { A, B, X, Y }

pub enum ButtonLabel {
    /// This button does nothing on this screen. Dim letter, no text.
    Inert,
    /// Live, labelled. <= 5 characters (see clipping rule).
    Live(String),
}
```

On `ChromeContribution`, four named fields (matching the existing
`..Default::default()` construction style used throughout `screen.rs`'s tests):

```rust
pub a: Option<ButtonLabel>,
pub b: Option<ButtonLabel>,
pub x: Option<ButtonLabel>,
pub y: Option<ButtonLabel>,
```

**The Option-of-Option is the whole point and must not be flattened:**

| Value | Meaning | Renders |
|---|---|---|
| `None` | *No opinion* — defer to the screen's static label | whatever the screen says |
| `Some(Inert)` | *Actively dead on my watch* — override the screen's label with nothing | dim letter, no text |
| `Some(Live(s))` | Labelled | letter + `s`, `TEXT_SECONDARY` |

A focused widget that suppresses a screen-level X action (a modal sub-state
where "rescan" would be wrong) must be able to say so, and that is not the same
as having no opinion. Flattening to `Option<String>` loses it.

Plus one accessor so the rail can iterate in physical order:

```rust
impl ChromeContribution { pub fn button(&self, b: Button) -> Option<&ButtonLabel>; }
```

`Screen` gains `buttons: ButtonLabels` (four `ButtonLabel`, default `Inert`) and
`with_button_labels(a, b, x, y)`. Resolution per slot: contribution `Some(_)`
wins; `None` falls back to the screen's static label. Same shape as
`title`/`readout` today.

**B is special and must not be authored per-screen.** B's text is a constant
(`"back"`), never overridable — design rule 3, and a per-screen B label is
exactly how B stops meaning Back. Its *liveness* is a navigator fact, not a
screen fact: at stack depth 1 there is nothing behind you. So `Screen::render`
gains one parameter, `can_go_back: bool`, passed from `Navigator::render` as
`self.depth() > 1`. A screen that handles back internally (Home's menu↔status
face toggle, section 7) declares `Screen::handles_back(true)`, which ORs into
`can_go_back`. Keep the text constant either way.

## 5. Region arithmetic — actual numbers

`RAIL_WIDTH: u32 = 34`. `TITLE_BAR_HEIGHT` unchanged at 16.

**240x240, `ButtonsRight` (today):**

| Region | Rect |
|---|---|
| title | `(0,0) 240x16` — unchanged |
| rail | `(206,16) 34x224` |
| content | `(0,16) 206x224` |

**240x240, `ButtonsLeft`:** rail `(0,16) 34x224`, content `(34,16) 206x224`.

**With the future gauge (`GAUGE_WIDTH = 18`, `ButtonsRight`):** gauge
`(0,16) 18x224`, content `(18,16) 188x224` — **exactly the design's stated
188x224.** So the design's number already assumes both edges; this bead lands
206x224 and the gauge bead takes it to 188. Say so in the doc comment or someone
will "fix" the discrepancy.

**Slots:** 224 / 4 = 56 exactly, no remainder. Slot *i* (physical, top to
bottom) = `(rail.x, 16 + 56*i, 34, 56)`, holding `slot_order()[i]`.

Within a slot, centred on `rail.x + 17`: the button letter in `font::label()`
(helvB08) at `slot_mid_y - 7`, the label in `font::hint()` (helvR08, the
design's face) at `slot_mid_y + 7`. `TEXT_SECONDARY` when live, `DIVIDER` when
inert (letter only). One 1px `DIVIDER` vertical hairline down the rail's inner
edge and 3 horizontal `DIVIDER` hairlines between slots — the same hairline
language as the existing title-bar divider at `screen.rs:302-308`, not a new
visual idiom. Exact vertical offsets are Uma's to tune; the structure is Fern's.

**Clipping, not ellipsis:** draw each slot through
`target.clipped(&slot_rect)`. Never truncate with "…", never marquee — the
marquee is already retired (`widget.rs:172-175`). Budget labels at ≤5
characters; every label in the design fits (`devs`, `link`, `set`, `back`,
`why?`).

**Content-width check against real screens:** `VerticalList` rows lay out
against their given `area`, so 206 is a no-op for them. `font::hero()` helvB24
rendering `"LDAC"` is ~70px — comfortable at 206 and still at 188.
`font::name()` helvB12 device names truncate around 20 chars at 240, ~17 at 206;
the untruncated name lives in the Device detail title (design section 10), so
this is acceptable and already anticipated.

**Saturation preserved.** `rail_width = RAIL_WIDTH.min(band.width)`; content
takes the remainder, possibly zero. 128x32 → title 16, rail `(94,16) 34x16`,
content `(0,16) 94x16`. 10x5 → title `(0,0) 10x5`, rail `(0,5) 10x0`, content
`(0,5) 0x0`. No panic, no underflow.

## Input model

Unchanged by this bead — `NavIntent` already carries `ShortcutX`/`ShortcutY`
(`core/src/input.rs:50-52`). What changes is that the rail makes the existing
model **honest**: `ShortcutX` arriving at a screen whose resolved X label is
`Inert` is a documented no-op, and the user can see that before pressing. B →
`NavIntent::Back` → pop, with the rail's dim-B at depth 1 telling the truth
about the root screen. The follow-on bead is what makes the d-pad's
*directional* semantics derive from `PANEL` too.

## Migration plan (incremental)

1. **`core/src/panel.rs`** — `PanelOrientation`, `Edge` (four values),
   `Button`, `PANEL`, `button_edge`/`gauge_edge`/`slot_order`. Pure, no
   rendering. Unit-test `slot_order` for both values including the vertical
   reversal.
2. **`core/src/render/chrome.rs`** — delete `HINT_BAR_HEIGHT` and
   `ChromeLayout::hint`; add `RAIL_WIDTH`, `rail`, `orientation`, the private
   `carve_edge` (all four edges), and `compute_chrome_for`. Rewrite the four
   existing tests with reasoning in the commit message.
3. **`core/src/render/rail.rs`** — `Button` (re-export), `ButtonLabel`,
   `ButtonLabels`, and `draw_rail(rail, orientation, labels, target)`.
   Self-contained; the only place slot geometry exists.
4. **`core/src/render/widget.rs`** — `ChromeContribution`: remove `hint`, add
   `a`/`b`/`x`/`y` and the `button()` accessor. Update the doc comment (it says
   "title bar + hint bar" in three places).
5. **`core/src/render/screen.rs`** — remove `hint`/`with_hint`, the hint draw
   block and `HINT_SIDE_MARGIN`; add `buttons`/`with_button_labels`/
   `handles_back`; `render` gains `can_go_back: bool`, resolves the four slots,
   calls `draw_rail` when `chrome.rail.size.width > 0`.
6. **`core/src/render/navigator.rs:228`** — pass `self.depth() > 1`.
7. **`core/src/render/mod.rs:61`** — export the new types.
8. **Five `.with_hint` call sites** (table above) — drop.
9. **Tests + a zoomed headless PNG pair** (live vs inert) for Tess.

## Hacks to retire

- **`HINT_BAR_HEIGHT`'s 12→18 bump** (`chrome.rs:20-29`) — a constant hand-tuned
  to make cramped text feel less cramped in a bar that is now deleted. Do **not**
  carry that tuning into the rail's slot metrics; derive them from 224/4.
- **`HINT_SIDE_MARGIN`'s 4→8 bump** (`screen.rs:39-43`) — same story, same fate.
- **`with_hint("Up/Down  Select  Back")` repeated verbatim at five call sites** —
  a copy-pasted legend nobody maintains, itself the evidence that a free-text
  hint was the wrong contract.
- **Adjacent, out of scope, flagged anyway:** `draw_shield_mark`
  (`screen.rs:71-89`) still paints a *Bitwarden-era shield* on every screen of a
  Bluetooth audio dongle. Filed separately.

## Risks / open questions

1. **The ADR everyone cites does not exist.** `chrome.rs:3-6`, `widget.rs:6` and
   the bead all cite
   `.planning/decisions/2026-08-11-ui-framework-reuse-vs-rewrite.md`. The only
   files in `.planning/decisions/` are the two 2026-08-27 ones, the 2026-08-26
   one, and `INDEX.md` (verified). The rule is being honoured by convention
   against a dangling citation. Scribe should either write the ADR or correct
   three source files' references.
2. **Rotation divergence is live, not hypothetical.** `firmware/src/input.c` is
   at the `MADCTL=0xA0` mapping, marked UNVERIFIED ON HARDWARE, while `PANEL`
   ships as `ButtonsRight`. Somebody must physically confirm which edge the
   buttons are on *after* the current MADCTL and set `PANEL` accordingly. That
   belongs to `pico-link-d7k`/`zzq`, not here. See also the ORCHESTRATOR
   CORRECTION: the net transform in that table is a quarter turn, not a half
   turn.
3. **Depth-1 B: dim or absent?** Fern ruled dim-with-letter (teaches the button,
   honest that it does nothing). Uma may overrule; it changes one match arm.
4. **Follow-on bead needed:** unify the GPIO→intent rotation into
   `core/src/panel.rs` so the C table becomes identity. Depends on this bead and
   on the rotation being settled.

## What the tests must assert

This project's record here is poor — `screen.rs:502-507`
`any_pixel_near_the_right_title_edge` samples a hand-picked 20-column strip and
asserts "some pixel of this colour exists". Acceptable for a glyph whose exact
metrics come from a font; **not** acceptable for the rail, where every rect is
computed and therefore exactly assertable.

1. **Exact rects, not inequalities.**
   `assert_eq!(chrome.content, Rectangle::new(Point::new(0,16), Size::new(206,224)))`
   and `assert_eq!(chrome.rail, Rectangle::new(Point::new(206,16), Size::new(34,224)))`.
   Not `assert!(content.width < 240)`.
2. **Invariants replacing the deleted stacking test.**
   `title.height + content.height == screen.height`;
   `rail.y == content.y && rail.height == content.height`;
   `rail.width + content.width == screen.width`; rail ∩ content is empty.
   `full_width_is_preserved_in_every_region` (`chrome.rs:106-112`) becomes
   **title-only** full width, with reasoning.
3. **The mirror test — the one that proves parameterisation is real.** Using
   `compute_chrome_for`: under `ButtonsRight`, A's slot is `(206,16,34,56)`;
   under `ButtonsLeft`, A's slot is `(0,184,34,56)` — *both* axes moved. Then
   render a screen with `a: Some(Live("devs"))` and everything else `Inert`, and
   assert `TEXT_SECONDARY` pixels exist **inside A's slot rect and in none of
   the other three slot rects**, under both orientations. The negative half is
   what makes it a proof.
4. **Live vs inert are distinguishable.** `x: Some(Live("link"))` →
   `TEXT_SECONDARY` present in the X slot rect. `x: Some(Inert)` → `DIVIDER`
   present *and* `TEXT_SECONDARY` absent in that same rect. Both directions; the
   absence assertion is load-bearing (the merged link-glyph tests got this shape
   right at `screen.rs:536-551` — copy the discipline, drop the fuzzy strip).
5. **Defer vs override.** Screen static `x = Live("link")` + widget `x: None` →
   "link" pixels present. Same screen + widget `x: Some(Inert)` → those pixels
   gone, `DIVIDER` present. Without this test the Option-of-Option is
   decoration.
6. **Overflow.** `a: Some(Live("abcdefghijklmnop"))` → for every row `y` in the
   rail band, no non-`BACKGROUND` pixel at any `x < chrome.rail.top_left.x`.
   This is the class of bug that has twice passed weak checks on this project.
7. **B liveness.** `Navigator::new(root)` → B slot has `DIVIDER`, no
   `TEXT_SECONDARY`. After `push` → B slot has `TEXT_SECONDARY`.
8. **Saturation.** 128x32 and 10x5 produce the exact rects listed above without
   panic.
9. **Tess:** zoomed headless PNGs of one screen with a mixed rail (A live, B
   live, X live, Y inert), inspected at zoom per the standing
   rendering-verification rule, not at 1x.
