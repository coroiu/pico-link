# Home alignment: one grid, two rules, no shield

Beads `pico-link-nvj` (alignment) and `pico-link-d9y` (the Bitwarden shield).
Author: Uma (UX designer), 2026-09-01.

**GOAL:** Andreas: *"The text position seems a bit random and not aligned at
all."* He is right, and this document says exactly why, in measured pixels, and
gives the constants that fix it.

Companion documents, both of which this one amends or depends on:
- `.planning/design/2026-08-28-on-device-ui.md` section 6 — the design of record.
- `.planning/design/2026-09-01-home-fault-strip.md` section 7 — the vertical
  budget. **Amended by section 8 below; the two documents now agree.**

---

## 1. The evidence

Captured with the committed fixture, not from constants:

```
cargo run --example home_screenshots -p pico-link-core -- <out-dir>
```

Ink extents measured off `02_connected_ldac.png` (device "Sony WH-1000XM5",
LDAC, 990 kbps), reduced back to 1x and thresholded against `BACKGROUND`:

| Element | ink y | ink x | left edge | right edge |
|---|---|---|---|---|
| Title text "Pico Link" | 1..11 | 21..78 | **21** | 78 |
| Device name | 25..39 | 9..152 | **9** | 152 |
| Hero word `LDAC` | 49..73 | 60..146 | **60** | 146 |
| Bitrate `990 kbps` | 85..100 | 99..162 | 99 | **162** |

**Five text elements. Five different left edges. Four different right edges.
Not one pair of elements on this screen shares an edge with another.** That is
not a subjective complaint about taste; it is the literal absence of a grid, and
it is what the eye reports as "random".

Three specific defects fall out:

**D1 — three different anchoring strategies in four lines.** The device name is
left-anchored at `LEFT_MARGIN = 8`; the hero word is *centred* on the content
region's centre (x=103); the bitrate is *right*-anchored inside an invisible
120px slot that is itself centred on x=103, so it ends at x=162. Three rules,
none of which produce a shared edge with any other.

**D2 — the title bar is misaligned with the body by exactly 12px.** The title
text starts at x=21 (after the shield mark plus its gap); the device name,
directly beneath it, starts at x=9. Two left edges, 12px apart, stacked
vertically 14px apart. This is the single most visible misalignment on the
screen and it is caused by `draw_shield_mark` — which is `pico-link-d9y`'s
Bitwarden-era brand glyph. **The two beads are one bug.**

**D3 — the vertical rhythm is an accumulator, not a grid.** `hero.rs` walks a
`cursor_y` through four independently chosen pads: `TOP_PADDING = 8`,
`GAP_NAME_TO_HERO = 8`, `GAP_HERO_TO_BITRATE = 4`, `GAP_TO_NEXT = 10`,
`GAP_BANNER_TO_STAT = 8`. Five magic numbers, no common divisor, and because
they accumulate, **showing the banner moves the stat strip down 28px.** An
element that changes position depending on whether an unrelated element is
present is the thing the fault-strip design's "nothing ever moves" rule exists
to prevent.

**D4 — the composite ships as three bands out of four.** `with_stat_line` has
**zero callers** (`grep -rn with_stat_line core/src emulator/src` outside
`hero.rs` returns nothing). So the bottom band never renders, the block clumps
into the top 45% of the content area, and 139px below it is void.

---

## 2. The fix in one sentence

> **Two vertical rules and one fixed vertical grid. Every element sits on a
> rule. Nothing's position depends on anything else's presence.**

---

## 3. Horizontal: two rules, L = 12 and R = 194

Content region is `x 0..205` (206 wide), `y 16..239` (224 tall) — 240 minus the
16px title bar and the 34px rail.

- **Left rule `L = 12`.** Absolute panel x. Shared by the *title bar text* and
  every left-aligned content element, so the title and the device name finally
  stack on one edge.
- **Right rule `R = 194`.** `206 - 12`. Symmetric gutter. The bitrate's fixed
  slot ends here, as will the fault strip's value column.

| Element | Rule | Alignment |
|---|---|---|
| Title text | `L = 12` | left |
| Title-bar right cluster (BT glyph, status dot) | panel right − 12 = **228** | right |
| Device name | `L = 12` | left, max width `R − L = 182` |
| **Hero codec word** | `L = 12` | **left** (was: centred) |
| Bitrate | `R = 194` | right, inside the existing 120px cleared slot (now x 74..194) |
| Banner bar | full content width | text inset to `L = 12` |
| Stat strip | `L = 12` | left |
| Fault-strip rows (`h62`) | tag at `L = 12`, value right at `R = 194` | |

### 3.1 Why the hero word becomes left-aligned

This is the one judgement call in the document, so here is the whole argument.

1. **Andreas's own sketch already does this.** Design of record section 6:
   ```
   |[]|      L D A C                   |
   |[]|      909 kbps                  |
   ```
   Both indented to a *common column*. The shipped centring is implementation
   drift from the sketch, not a design decision anyone made.
2. **A centred hero moves when the codec changes — at the exact moment you must
   notice the change.** `LDAC` → `SBC` shifts *both* edges of the word inward.
   The glance that the hero exists to serve then has to re-find the word before
   reading it. Left-anchored, only the right edge moves: the word changed, the
   layout did not. This is the same "nothing moves" principle as the fault
   strip, applied to the most important element on the screen.
3. **A centred element cannot share an edge with anything.** With four elements
   on the screen and one of them centred, the centred one is permanently
   ungrid-able. Removing it is what makes a grid possible at all.
4. **The bitrate stays right-aligned**, per design section 6's numeric rule
   ("every number is right-aligned into a fixed slot cleared to `BACKGROUND`
   first" — so `990 → 90` never shifts the remaining digits). Hero on `L`,
   bitrate on `R`, 4px apart, reads as one deliberate unit spanning the full
   measure. That is a composition; three arbitrary anchors is not.

The right-aligned bitrate is then the *only* right-aligned element in the
content area — and it earns it, because it is the only number.

---

## 4. Vertical: a fixed grid, no accumulator

Every y below is **absolute panel y**. The widget's `area.top_left.y` is 16, so
subtract 16 for the relative constants in section 7.

| Band | y | h | Rule |
|---|---|---|---|
| Title bar | 0..15 | 16 | chrome |
| top gutter | 16..27 | 12 | matches the horizontal gutter — a uniform 12px frame |
| **Device name** | 28..43 | 16 | `L`, `font::name` |
| gap | 44..55 | 12 | |
| **Hero slot** | 56..87 | 32 | `L`, `font::hero`, fixed slot (unchanged, see `HERO_SLOT_HEIGHT`'s existing aptX-HD rationale) |
| gap | 88..91 | 4 | tight on purpose: hero + bitrate are one unit |
| **Bitrate** | 92..108 | 17 | `R`, `font::value` |
| gap | 109..115 | 7 | |
| **Banner slot** (when shown) | 116..135 | 20 | full width, text at `L` |
| gap | 136..143 | 8 | |
| **Stat strip** | 144..154 | 11 | `L`, `font::label` uppercase |
| gap | 155..163 | 9 | |
| **Fault strip** (`h62`) | 164..218 | 54 | divider at 164, rows below |
| bottom gutter | 219..239 | 21 | |

**The banner and the stat strip are now at fixed y, not cursor-derived.** This
is the structural change, and it is worth more than the pixels: showing or
hiding the banner no longer moves the stat strip, and the fault strip's
clearance is a constant 9px in every state instead of 66px or 38px depending on
what else happens to be on screen.

The empty band at 109..115 and the empty banner slot when no banner is showing
are **deliberately left empty**. Reclaiming them would make the layout
state-dependent, which is exactly the defect being fixed.

---

## 5. `pico-link-d9y`: the shield comes out and nothing replaces it

`core/src/render/screen.rs:71-89` `draw_shield_mark` paints
`icon::SHIELD` — the predecessor Bitwarden hardware-key brand mark — in the
title bar of **every** screen.

**Decision: delete it. Put nothing in its place.**

Three reasons, in order of weight:

1. **It is the cause of D2.** It is the only thing pushing the title text off
   the left rule. Removing it is not a cosmetic tidy-up; it is the alignment
   fix.
2. **It is a lie on every screen.** A shield means "your secrets are protected".
   Pico Link protects nothing. On `Devices` and `Settings` it is pure noise.
3. **A brand mark beside the word "Pico Link" is redundant, and beside
   "Devices" it is wrong.** The title bar already identifies the product on
   Home, by name, in the largest chrome type on the panel. On a 240px screen a
   glyph that adds no information costs 15px of the most valuable horizontal
   real estate the layout has.

**Considered and rejected as replacements:**

- *A Bluetooth or headphone glyph.* The title bar's right side **already**
  carries a live Bluetooth link glyph whose colour encodes link state. A second,
  static Bluetooth mark on the left would be the same symbol twice, once
  meaningful and once decorative — and the decorative one trains you to stop
  reading the meaningful one.
- *A new Pico Link logo.* There isn't one, drawing one is out of scope, and the
  screen it would live on already says the words.
- *A back-affordance caret on pushed screens.* B is the universal Back and is
  permanently labelled in the rail. Putting a second back-affordance in the
  title bar creates an unpressable-looking one, which is worse than none.

The title bar's correct composition is therefore: **title text on `L`, live
status glyphs on the right, nothing else.** That is one identifier and one
live indicator — the minimum that earns 16px on a 240px panel.

If the empty left slot ever feels bare, the honest thing to put there is not a
brand mark but a **state** glyph, and there is no state on this product that
isn't already better shown by the hero word or the right-side glyphs. Leave it
empty.

---

## 6. ASCII sketch — the corrected grid

Left rule `|` at x=12, right rule `|` at x=194.

**Connected, streaming (the common case):**
```
+----------------------------------------+-----+
| Pico Link                        (bt)  |     |     y 0..15
+----------------------------------------+  A  | devs
| Sony WH-1000XM5                        |     |     y 28..43
|                                        +-----+
| LDAC                                   |  B  | back  y 56..87
|                             990 kbps   |     |     y 92..108
|                                        +-----+
|                                        |  X  |
|                                        |     |
|                                        +-----+
| USB 48K 24-BIT                         |  Y  | set   y 144..154
|                                        |     |
+----------------------------------------+-----+
 ^L                                    R^
```

**Fell back, with banner and one fault row (worst realistic density):**
```
+----------------------------------------+-----+
| Pico Link                        (bt)  |     |
+----------------------------------------+  A  | devs
| Sony WH-1000XM5                        |     |
|                                        +-----+
| SBC                                    |  B  | back   amber
|                             328 kbps   |     |
|########################################+-----+
|#Headphones don't support LDAC         #|  X  | why?   y 116..135
|########################################|     |
| USB 48K 24-BIT                         +-----+       y 144..154
|----------------------------------------|  Y  | set   divider y 164
| IN  HOST AUDIO LOW        x3     0.50  |     |
+----------------------------------------+-----+
```

Every text run in both sketches begins at `L` or ends at `R`. Nothing floats.

---

## 7. Constants to change — for Ruby, no decisions left

### `core/src/render/hero.rs`

| Constant | From | To | Note |
|---|---|---|---|
| `LEFT_MARGIN` | `8` | `12` | now the single left rule `L` |
| `NAME_RIGHT_MARGIN` | `8` | **rename** `RIGHT_MARGIN = 12` | used by the name, the bitrate slot and the stat strip — one rule, one constant |
| `TOP_PADDING` | `8` | `12` | uniform 12px frame |
| `GAP_NAME_TO_HERO` | `8` | `12` | |
| `HERO_SLOT_HEIGHT` | `32` | `32` | **unchanged** — its aptX-HD descender rationale still holds |
| `GAP_HERO_TO_BITRATE` | `4` | `4` | **unchanged** — deliberately tight |
| `BITRATE_SLOT_WIDTH` | `120` | `120` | unchanged; only the slot's x moves |
| `GAP_TO_NEXT` | `10` | **delete** | replaced by a fixed banner slot |
| `GAP_BANNER_TO_STAT` | `8` | **delete** | replaced by a fixed stat slot |
| `BANNER_HEIGHT` | `20` | `20` | unchanged |
| `BANNER_TEXT_INSET` | `8` | `12` | banner text joins the left rule |
| — | new | `BANNER_TOP: i32 = 100` | y relative to `area.top_left.y` (abs 116) |
| — | new | `STAT_TOP: i32 = 128` | y relative to `area.top_left.y` (abs 144) |

### `core/src/render/hero.rs` — `Widget::render` body

1. **Hero word.** Replace
   `Point::new(center_x, hero_y)` + `HorizontalAlignment::Center` with
   `Point::new(area.top_left.x + LEFT_MARGIN, hero_y)` +
   `HorizontalAlignment::Left`. `center_x` then has no remaining use — delete it.
2. **Bitrate slot.** Replace the slot origin
   `Point::new(center_x - BITRATE_SLOT_WIDTH as i32 / 2, cursor_y)` with
   `Point::new(area.top_left.x + area.size.width as i32 - RIGHT_MARGIN - BITRATE_SLOT_WIDTH as i32, cursor_y)`.
   The `render_aligned` call already right-aligns to `slot_rect`'s right edge —
   leave it alone. The slot becomes content-x `74..194`.
3. **Delete `cursor_y` entirely.** The banner draws at
   `area.top_left.y + BANNER_TOP`; the stat strip draws at
   `area.top_left.y + STAT_TOP`. Neither reads a running cursor, and the
   `else { cursor_y += GAP_TO_NEXT; }` branch disappears with it. This is the
   change that makes the banner's presence stop moving the stat strip.
4. Also add a guard test: **`stat_line` renders at the same y with and without a
   banner.** That is the regression this refactor exists to prevent, and it is
   assertable by rendering both and comparing the ink rows in the 144..154 band.

### `core/src/render/screen.rs`

| Change | Detail |
|---|---|
| `TITLE_SIDE_MARGIN` | `6` → `12` (both sides; left now equals `L`) |
| `TITLE_ELEMENT_GAP` | `6` — unchanged |
| `draw_shield_mark` (lines 71-89) | **delete the function and its call site.** The title text's start x becomes `title_left_x + TITLE_SIDE_MARGIN` directly. |
| `theme::icon::SHIELD` | now unreferenced. Remove it, or keep it with an `#[allow(dead_code)]`-equivalent if other probes use it — Ruby's call, it is not a design question. |

### `core/src/render/home.rs`

**Wire the stat strip.** `with_stat_line` currently has no callers, which is why
the bottom band never renders (D4). Add `.with_stat_line(...)` on the connected
branch of `HomeView::new`.

- Content is the **USB input format**, uppercase, e.g. `"USB 48k 24-bit"` (the
  widget uppercases it itself).
- **Only when there is a live host stream.** Per design section 15's
  "absent, never frozen and never faked" rule: no stream, no stat line. Do not
  print a nominal or last-known format.
- If `BtModel` does not currently carry the USB alt-setting's rate/depth, **omit
  the call and file a follow-up** rather than hardcoding `48k 24-bit`. The grid
  is unaffected — the band simply stays empty, and nothing else moves, which is
  the whole point of the fixed slots.

---

## 8. Amendment to `2026-09-01-home-fault-strip.md` section 7

That document's vertical-budget table was computed against the *old*
accumulator rhythm. Superseded by section 4 above. The substantive changes:

- Hero composite now ends at **y=108** (bitrate) in every state, not 98/126.
- Banner at a **fixed** 116..135; stat strip at a **fixed** 144..154.
- Clearance from the stat strip to the fault strip's divider is a **constant
  9px**, in every combination of banner/no-banner and stream/no-stream —
  replacing the old "66px no banner / 38px with banner".
- The fault strip's own geometry is **unchanged**: divider at y=164, rows in
  164..218, 12px slots, cap of 4 rows, bottom edge 218.
- **Section 7's contention rule is now dead and should be struck.** It said
  "if the fallback banner and four live fault rows collide, the stat line is
  dropped." With fixed slots nothing can collide: the banner, the stat strip and
  a full four-row strip coexist with clearance to spare. Keep the *priority
  ordering* sentence (hero > banner > fault strip > stat line) as a tiebreaker
  of record for any future element, but delete the drop behaviour — it is
  unreachable code waiting to be written.

`h62`'s section 6 value-slot spec gains one constraint: the fault row's value
column is right-aligned to **`R = 194`**, the same rule as the bitrate.

---

## 9. Out of scope, deliberately

- **Making the hero bigger.** There is now more usable width (the word starts at
  x=12 instead of floating), and `helvB24` may no longer be the ceiling. Worth
  revisiting *after* this lands and can be judged on a real panel — changing the
  size and the alignment in one step makes neither assessable.
- **A `NO LINK` placeholder in the empty device-name band.** Today `home.rs`
  passes `""` for the name when disconnected, so the top band is blank and the
  hero sits alone. Design state 3 ("First run, nothing paired") wants
  "No headphones paired" / "A to pair" there. That is a content change with a
  `BtModel` data dependency, not an alignment fix — file separately.
- **The title bar's right-hand cluster sitting above the rail column rather
  than on `R`.** It insets 12px from the *panel* edge (x=228), not from the
  content edge, because the title bar is full-width chrome and its right end is
  visually over the rail, not over the content measure. This is intentional and
  is not a third rule: nothing in the content area is expected to align to it.

---

## 10. Verification Ruby owes

Not "tests pass and it launches" — the project rule is explicit that a 1x PNG is
not evidence.

1. Re-run `cargo run --example home_screenshots -p pico-link-core -- <dir>` and
   inspect all three PNGs **at zoom**.
2. Measure the ink extents the same way section 1 did and assert the table:
   title text and device name both start at x=12; the hero word starts at x=12;
   the bitrate's ink ends at or just inside x=194.
3. Render the banner and no-banner states and confirm the stat strip's ink
   occupies the **same rows** in both.
4. Render `aptX HD` and confirm the descender still clears the bitrate band —
   `HERO_SLOT_HEIGHT` is unchanged, so this should hold, but it is the one
   existing invariant this change could disturb.
5. Commit the updated fixtures so a future diff catches a regression.
