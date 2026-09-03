# Vertical stereo OUT meter beside the button rail

**Date:** 2026-09-03
**Author:** Uma (UX)
**Bead:** `pico-link-ky8`
**Status:** Design of record for the OUT meter. **Supersedes section 3/6's
horizontal-bars placement** in `.planning/design/2026-08-28-on-device-ui.md`
(the meter itself — its commissioning, its colour semantics and the
absent-never-frozen rule — is unchanged; only its placement, size and segment
count are).

---

## 1. Goal

Andreas, 2026-09-02: *"in my design i had it vertically next to the rail, I think
that would be a better place."* Move the stereo OUT level meter from two
horizontal 8-segment rows under the bitrate (`core/src/render/hero.rs`,
`STAT_TOP`) to a vertical stereo pair in its own strip immediately inboard of
the button rail, matching his original sketch.

This is a placement change, not a redesign. The meter's semantics, colour zones,
peak-hold and staleness rule all survive.

---

## 2. Measurements (re-derived 2026-09-03)

All measured against the real faces via `get_rendered_dimensions_aligned`, not
asserted.

| Face | String | w x h |
|---|---|---|
| `font::hero()` | `LDAC` | 87 x 25 |
| | `SBC` | 67 x 25 |
| | `AAC` | 68 x 25 |
| | `aptX HD` | 125 x 32 |
| | `NO LINK` | **131 x 25** |
| `font::name()` | line height | 16 |
| | `Sony WH-1000XM5` | **144** |
| `font::value()` | line height | 16 |
| | `909 kbps` | 64 |
| `font::label()` | line height | 10 |
| | `MUTED  Press Up to raise` | **128** |
| | `Codec fell back to SBC` | 113 |
| | `USB 48K 24-BIT` | 79 |
| | `LINK \|\|\|\|.` | 41 |
| | `OUT` | 21 |

Chrome today: title bar 16px; band abs y16-240 (224 tall); `RAIL_WIDTH` 34;
content `(0,16) 206x224`; rail `(206,16) 34x224` split into four 56px slots.

Hero internals (content-relative): `TOP_PADDING` 12, `LEFT_MARGIN` /
`RIGHT_MARGIN` 12, `GAP_NAME_TO_HERO` 12, `HERO_SLOT_HEIGHT` 32,
`GAP_HERO_TO_BITRATE` 4, `BANNER_TOP` 100, `STAT_TOP` 128.

Derived: the hero word's top rule sits at content-rel **y40** (abs y56).

---

## 3. The measurement that decides the layout

The obvious implementation — carve the meter strip off `chrome.content` along
`button_edge()` — is **wrong**, and one number says why.

A 48px strip taken off the whole content drops the device-name budget from
`206 - 24 = 182px` to `158 - 24 = 134px`. `Sony WH-1000XM5` measures **144px**.
The name would truncate to `Sony WH-1000X...` — losing the model digit, which is
the single token that tells two paired Sonys apart. Andreas's own headphones are
a WH-1000XM3.

The hero codec word is protected by Andreas's original ruling ("if the meter
crowds the hero, the meter loses"). The device name is protected by the same
logic: it answers *which headphones*, and a meter is not worth that answer.

**Resolution: carve the strip out of the band *below* the name row.** The name
keeps the full 206px. Nothing regresses.

---

## 4. Geometry

Expressed against the same parameterisation the rail already uses. The panel
rotation (`pico-link-zzq`) is unresolved and fixing it flips both edges; nothing
below names a fixed edge.

```rust
// 1. the name row keeps the full content width
let (name_band, rest) = carve_edge(content, Edge::Top, NAME_BAND_HEIGHT);
// 2. the meter strip hugs the same edge the rail is on
let (strip, hero_body) = carve_edge(rest, orientation.button_edge(), METER_STRIP_WIDTH);
// 3. within the strip: rail-facing and content-facing, never left/right
let (_trail_gap, s)  = carve_edge(strip, orientation.button_edge(), TRAIL_GAP);
let (_lead_gap, pair) = carve_edge(s, orientation.button_edge().opposite(), LEAD_GAP);
```

Three `carve_edge` calls, all already covered by
`chrome::tests::rail_and_content_never_overlap` across both orientations.
**Zero left/right literals in the edge logic.**

### Constants

| Name | Value | Derivation |
|---|---|---|
| `NAME_BAND_HEIGHT` | **40** | `TOP_PADDING 12 + line_height(name) 16 + GAP_NAME_TO_HERO 12` — derived, not magic |
| `METER_STRIP_WIDTH` | **48** | `12 lead + 12 L + 4 gap + 12 R + 8 trail` |
| `LEAD_GAP` (content-facing) | 12 | the uniform 12px frame |
| `CHANNEL_WIDTH` | 12 | |
| `CHANNEL_GAP` | 4 | |
| `TRAIL_GAP` (rail-facing) | 8 | |
| `SEGMENT_HEIGHT` | 9 | |
| `SEGMENT_GAP` | 2 | |
| `SEGMENT_COUNT` | **16** | `16*9 + 15*2 = 174` |
| `BLOCK_BOTTOM_INSET` | 10 | from the content band's bottom edge |

### Placement

- Strip spans content-rel **y40..224** (abs y56..240), 48px wide, on
  `button_edge()`.
- Meter block is **174px tall, bottom-pinned** at content-rel y214 (abs y230,
  10px above the panel bottom) and grows **upward**. The bottom is the datum
  because that is where level grows from; the top is wherever it lands.
- 40 + 174 = 214, so the block's top edge lands at content-rel y40 = abs y56 =
  **exactly the hero codec word's top rule**. Full scale and the hero word share
  a horizontal rule.

### Gap rhythm

4 (between channels) < 8 (to the rail) < 12 (to the hero). Gaps increase
outward, so Gestalt proximity binds the two columns into **one** stereo
instrument rather than two unrelated gauges, and keeps the pair from reading as
part of the rail chrome.

### ASCII (today's `ButtonsRight`; a flip mirrors the strip and the rail together)

```
+--------------------------------------------------+  y0
| (title bar)                                      |  16
+--------------------------------------+-----+-----+
|  Sony WH-1000XM5              OUT    |     |  A  |  28..44   name spans FULL 206
+--------------------------------------|-----|-----|
|                                      | . . |     |  56  <- block top == hero top rule
|  L D A C                             | . . |  B  |
|                                      | # # |     |
|  909 kbps                            | # # |     |
|                                      | # % |  X  |
|                                      | # # |     |
|  [ banner slot ]                     | # # |     |
|                                      | # # |  Y  |
|  USB 48K 24-BIT                      | # # |     |
|                                      | # # |     |  230 <- block bottom
+--------------------------------------+-----+-----+  240
   <----------- 158 (hero body) ------> <-48-> <34>
                                          L R
```

`#` filled, `.` unlit (`DIVIDER`), `%` peak-hold cap.

---

## 5. Segment count: 16, up from 8

The vertical move takes the meter from a 62px row to a 174px column. Eight
segments in that space would be ~20px blocks reading as a crude three-state
light — all the height, none of the information.

- **16 x 9px segments** stay individually resolvable at 30-50cm. The rail's
  letter glyphs are smaller and legible at that distance.
- The 11px pitch keeps motion **coarse and quantised**, honouring section 6's
  Home motion rule. The column steps; it does not creep.
- **Peak-hold reads *better*, not worse:** the cap is one 12x9 highlighted
  block versus today's 6x8. The columns are wider than the old rows were tall.

**Colour zones keep the *proportions*, not the indices.** Today: green bottom
5/8, amber 2/8, red top 1/8. At 16: **green bottom 10/16, amber next 4/16, red
top 2/16.** An implementer who ports the literal indices from
`theme::level_segment_color` halves the red zone and the meter quietly means
something different. This is the single most likely porting bug in this spec.

---

## 6. Stereo order is deliberately *not* parameterised

Flipping `PANEL` changes `button_edge()` and `slot_order()` and nothing else.
Nothing mirrors the framebuffer: the renderer draws in user-visible coordinates
and the driver's MADCTL is set once so (0,0) is the user's top-left. Text still
reads left-to-right under `ButtonsLeft`.

Therefore **screen-left is screen-left in both orientations. L is always the
left column, R the right**, per universal mixer/DAW convention. Only the
*strip's edge* follows `button_edge()`. Wiring the channel order to the
orientation knob would silently swap the channels on a flip — a bug nobody would
catch by looking.

**Legend:** a single `OUT` in `font::label()` / `TEXT_SECONDARY` (21px, fits the
28px pair), centred over the pair, sitting in the name band at content
x~172-193. The longest name ends at x156, leaving 16px clear. **No per-column
L/R letters** — 5-6px glyphs at arm's length are noise, and adjacency plus
correlated motion already reads as stereo.

---

## 7. Absent, never frozen — and it reads better here

Unchanged: `OUT_LEVEL_STALE_AFTER` 600ms, `OUT_LEVEL_REFRESH_INTERVAL` 250ms.

**The `OUT` label vanishes with the columns.** A label with nothing under it
reads as broken, not as silent.

**Do not leave the unlit `DIVIDER` segments as a ghost outline.** That is a
frozen meter pinned at zero — precisely what the rule forbids. Absent means
absent. (The unlit segments do still draw *while live*: they give the lit
portion its scale.)

The vertical placement improves how absence reads, on its own merits:

- Under the bitrate (today), a vanished meter leaves **a hole in the middle of a
  text stack** — which reads as a rendering fault.
- Beside the rail, a vanished meter leaves **an empty gutter at the panel edge**
  — which reads as *nothing is there*, and that is the truth.

The periphery is the right home for a thing that legitimately comes and goes.

---

## 8. What this displaces

Hero body narrows 206 -> 158. Every Home string still fits, measured:

| Element | Needs | Slack in 158 |
|---|---|---|
| `NO LINK` | 131 + 12 = **143** | **15** (binding constraint) |
| `aptX HD` | 125 + 12 = 137 | 21 |
| MUTED banner | 128 + 2x12 insets = **152** | **6** (second tightest) |
| `Codec fell back to SBC` | 113 + 24 = 137 | 21 |
| Bitrate slot | 120 + 12 = 132 | 26 |
| `USB 48K 24-BIT` | 79 + 24 = 103 | 55 |

**Nothing is cut.** `BANNER_TOP` and `STAT_TOP` do not move.

**Removed:** the horizontal 2x8 meter at `STAT_TOP` in `hero.rs`. That is a net
win beyond the move — `hero.rs`'s own comment admits the meter and `stat_line`
share the `STAT_TOP` slot and "don't collide in practice" only because nothing
populates `stat_line` with live data yet. Vacating it unblocks the USB-format
strip.

**Watch:** `LINK ||||.` (41) + `USB 48K 24-BIT` (79) + a gap is ~132 in a 134
budget. The stat strip is now tight; a later bead may have to alternate the two
or drop `LINK`. Not this bead's call, but do not add a third stat field without
re-measuring.

**`VOL` is unaffected** — it was always destined for `gauge_edge()`, the
opposite edge, which `panel.rs` already reserves.

### Forward constraint for the VOL gauge (Tier 2)

If VOL later carves a 34px **full-height** strip off `gauge_edge()`, the hero
body drops to 124px and **`NO LINK` (143) no longer fits**. When VOL is
designed it must either carve below the same name band and be re-measured
against this table, or be a thin (<=14px) edge slider rather than a second rail.
Record this before the VOL bead is written.

---

## 9. Render budget

32 rects of 12x9 = **3456px fill per repaint at 4Hz**, ~6% of the panel, versus
768px today. 4.5x more fill but **zero extra glyph raster**, and the ~133ms
figure tracked as `pico-link-1n4` is glyph-dominated.

Bonus, and a real argument for the placement: the strip is a **disjoint
rectangle from every text element on Home**. The 4Hz meter repaint becomes the
ideal first damage-rect candidate once `pico-link-1n4` lands. Today the meter
overlaps `stat_line`'s band, so the two can never be independently invalidated.

---

## 10. Handoff

**Fern** needs to make expressible:
- A second and third `carve_edge` pass on Home, or a `ChromeLayout`-adjacent
  place for a screen-owned sub-region. This is *not* a `ChromeLayout` field —
  the strip is Home-specific, not chrome. `panel.rs` deliberately exposed
  `gauge_edge()` without adding a `ChromeLayout` field for exactly this reason;
  follow that precedent.
- `Edge::opposite()` (or an equivalent) so step 3 of section 4 needs no match.

**Ruby** builds:
1. A vertical variant of `theme::draw_level_meter` — bottom-aligned, growing
   upward, `SEGMENT_COUNT` parameterised (or a second constant), colour zones by
   **proportion** per section 5.
2. Home's two-step carve per section 4; `HeroStatusView` receives `hero_body`.
3. Delete the `STAT_TOP` horizontal meter block from `hero.rs::render` and its
   `METER_ROW_HEIGHT` / `METER_ROW_GAP` constants.
4. The `OUT` legend, drawn and blanked with the columns.
5. Keep `OUT_LEVEL_STALE_AFTER` / `OUT_LEVEL_REFRESH_INTERVAL` and
   `redraw_after` exactly as they are.

**Tess** proves: a headless screenshot at both `PanelOrientation` values showing
the strip and rail move together and the L/R columns do *not* swap; a capture
with `Sony WH-1000XM5` as the device name showing no ellipsis; a capture in
`NO LINK` state showing the hero word uncropped; a capture with a stale reading
showing the strip fully empty. Inspect at zoom, not 1x.

**Not designed here:** the VOL gauge, the `LINK` stat contention, damage-rect
invalidation.
