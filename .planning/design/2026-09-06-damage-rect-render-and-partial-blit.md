# Damage-rect render + partial blit

- **Date:** 2026-09-06
- **Bead:** `pico-link-7h5` (to become an epic)
- **Author:** Fern (frontend architect)
- **Status:** Proposed. Design only; no code changed by this document.
- **Reads against:** `.planning/decisions/2026-09-02-core1-allocation-and-the-repaint-ceiling.md`
  (measurements and hazards banked there are **not** re-derived here),
  `.planning/decisions/2026-08-31-render-ctx-frame-scoped-clock.md`,
  `.planning/design/2026-09-02-dirty-gate-across-the-ffi-seam.md`,
  `.planning/design/2026-09-03-vertical-out-meter.md`.

## 0. The one-sentence version

The dirty gate answered *"does anything need painting?"*; this answers
*"which pixels?"* — and the answer has to reach the **widget tree walk**,
not just the `DrawTarget` and not just the SPI transfer, because Rust
rasterisation is ~65% of a painted frame.

## 1. Why clipping the DrawTarget is not enough

`DrawTargetExt::clipped()` filters at the `draw_iter` boundary. A clipped
u8g2 glyph render still shapes the glyph, still walks its bitmap, still
produces the pixel iterator — the clip only discards the writes at the end.
The same is true of every styled primitive. So a clip applied only at the
target saves the memory stores and essentially none of the CPU that the
25 ms is made of.

Therefore the load-bearing mechanism in this design is **not a clip. It is a
skip**: `Screen::render` must not call `Widget::render` at all for widgets
that cannot have changed. The clip is a second-order refinement on top of the
skip, for widgets that are internally composite.

## 2. The four-level model

| Level | Question | Where it lives | Status |
|---|---|---|---|
| L0 | Does anything need painting at all? | `App::dirty()` / `pl_ui_dirty` | **exists** (`pico-link-vxc`) |
| L1 | Which *widgets* changed? | `Widget::paint_key` + a per-screen cache | new |
| L2 | Which *part* of a changed widget changed? | `Widget::damage_hint` + `RenderCtx::damage()` | new |
| L3 | Which pixels must reach the panel? | `PlRenderOut.rects` -> `st7789_blit_rect` | new |

L1 is where the 65% is won. L3 is where the 35% is won. **L1 without L3 is
still a large win; L3 without L1 is the trap the ADR names.**

## 3. L1 — per-widget paint keys

### 3.1 The contract

```rust
/// A cheap, total summary of everything that affects this widget's pixels
/// in this frame. Two renders with equal keys MUST produce byte-identical
/// pixels within the widget's area.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PaintKey(u64);

impl PaintKey {
    /// Never equal to anything, including itself. The default: a widget
    /// that has not opted in is repainted every frame, exactly as today.
    pub const ALWAYS: PaintKey;
    pub fn of(seed: u64) -> PaintKey;      // fold state in
    pub fn fold(self, v: u64) -> PaintKey;  // chainable
}

pub trait Widget {
    fn paint_key(&self, _ctx: &RenderCtx) -> PaintKey { PaintKey::ALWAYS }
    fn damage_hint(&self, _area: Rectangle, _ctx: &RenderCtx) -> Option<Rectangle> { None }
    // ...existing methods unchanged...
}
```

`ALWAYS` as the default is the whole migration strategy: adding L1 changes
no rendered pixel and no test, because every widget is still repainted every
frame until it opts in. Widgets then opt in one at a time (bead 7h5.5),
each opt-in independently reviewable and independently revertible.

### 3.2 `paint_key` must be a function of state **and** `ctx`

This is the subtlety that will bite an implementer who does not read this
section. Several widgets' appearance depends on `ctx.now()` — the OUT
meter's stale/live decision, the connecting-phase elapsed readout, anything
animated. A key must fold in **the quantised visual consequence of time, not
the raw instant**:

- OUT meter: fold `sample.received_at` and the boolean
  `ctx.elapsed_since(received_at) >= OUT_LEVEL_STALE_AFTER` — not `now()`.
- An elapsed readout showing whole seconds: fold the whole-second count.

Folding raw `now()` makes every widget permanently dirty (silent no-op, the
change "works" and buys nothing). Folding nothing time-related makes a
time-driven widget freeze (visible bug). Both failure modes must be called
out in review. `Widget::redraw_after` already tells you which widgets are
time-driven: **every widget that overrides `redraw_after` must fold time
into `paint_key`, and no widget that doesn't should.** That is a mechanical,
checkable review rule.

### 3.3 The frame damage pass, in `Screen::render`

Ordering, replacing today's clear-then-draw-everything:

1. **Layout once.** Compute `(widget_index, area)` for every widget, plus
   the two chrome pseudo-regions (title bar, button rail). Layout already
   happens each frame; it is not new cost.
2. **Collect keys.** For each widget and each chrome pseudo-region, take
   `paint_key(ctx)`.
3. **Diff against the cache.** `Screen` holds
   `Vec<(PaintKey, Rectangle, Option<Rectangle>)>` from the last *painted*
   frame. A slot is dirty if: the cache is empty/short (first frame after a
   push, pop, `replace_root`, resize, or wake), its area moved (damage =
   old area ∪ new area), or its key changed.
4. **Narrow.** For each dirty slot, if `damage_hint` returns `Some(r)`, use
   `r ∩ area`; otherwise use `area`.
5. **Union** the dirty rects into the **frame damage rect** (§5 on why one
   rect for now), then clamp to the framebuffer.
6. **Fill only the damage rect** with `palette::BACKGROUND` — this replaces
   `Navigator::render`'s `target.clear(...)`.
7. **Render every widget whose area intersects the frame damage rect**, in
   the existing stack/z order, with `ctx` carrying the damage rect.
8. **Commit the cache**, and return the frame damage rect.

Step 7 is the correctness invariant and it is easy to get wrong: you repaint
**intersecting** widgets, not **changed** widgets. Repainting only the
changed ones loses any widget that overlaps a repainted region. Today's
layout is non-overlapping, but the chrome/rail/content adjacency and any
future overlay (a toast, a modal) makes this a rule worth having from day
one — and it costs nothing when nothing overlaps.

### 3.4 Full-damage triggers (must be exhaustive)

Force damage = whole framebuffer on: first frame; `Navigator` push/pop/
`replace_root`; framebuffer resize; wake from display blank; cache miss of
any kind; and the existing 1 s `PL_FORCED_REPAINT_MS` backstop (which stays
— do **not** remove it as part of this work; it is the net under an
incorrect `paint_key`).

### 3.5 Chrome is not exempt

The title bar (title text, readout, status dot, BT glyph) and the button
rail (four slots of text + hairlines) are drawn directly by `Screen::render`,
not by widgets, and they are not cheap. They get paint keys of their own,
derived from the resolved `ChromeContribution` + `can_go_back` + the
screen's static labels. If they are exempted "for now", a meter-only frame
still repaints the whole rail and the win is materially smaller. Not
optional.

## 4. L2 — sub-widget damage, via `RenderCtx`

`RenderCtx` grows one field. It is `#[non_exhaustive]`, `Copy`, and its own
ADR anticipates exactly this kind of additive growth, so this is the seam
the project already chose for frame-scoped facts — no widget signature
changes anywhere.

```rust
pub struct RenderCtx {
    now: Instant,
    damage: Option<Rectangle>,   // None == "no clip declared, draw everything"
}

impl RenderCtx {
    pub const fn at(now: Instant) -> Self;                    // unchanged, damage: None
    pub const fn with_damage(self, r: Rectangle) -> Self;     // new
    pub const fn damage(&self) -> Option<Rectangle>;
    /// True when `area` could contribute visible pixels this frame.
    pub fn needs(&self, area: Rectangle) -> bool;             // None => true
}
```

`at()` keeping `damage: None` is what leaves ~250 existing tests compiling
and passing untouched.

A composite widget then does, inside its own `render`:

```rust
if ctx.needs(meter_strip) { /* draw the two columns */ }
if ctx.needs(hero_body)   { /* draw the codec word etc. */ }
```

This is a **skip inside the widget**, matching §1: it prevents rasterisation,
it does not merely discard writes. Widgets that ignore `ctx.damage()` remain
correct (they just redraw more of themselves than needed) — but only because
step 6 already repainted the background under the whole damage rect and step
7 only calls widgets that intersect it.

## 5. One rect now, a rect list at the seam

Core computes and reports **one bounding rect** in phase 1. Rationale: the
motivating case (meter live, everything else static) has exactly one changed
region; a multi-rect union has a real cost in core, in the FFI, and in the
blit loop; and a bounding box over two distant changes degrades to a
correct-but-large repaint, which is a performance event, not a bug.

**But the FFI and the C blit path are specified for a rect *list* from the
start** (`rect_count` + array), so growing to 2-4 rects later is a core-side
change and *not* another versioned FFI change. Design the seam wide, ship
the implementation narrow.

Known degradation to accept and watch: a frame in which the title-bar status
dot and the meter both change produces a bbox covering most of the panel.
Rare, self-limiting, correct. If measurement later shows it is not rare,
that is the trigger for the rect list, and it needs no new FFI.

## 6. The FFI shape

Consistent with the `PlEvent`/`PlCommand` discipline established by
`pico-link-a67`: a `#[repr(C)]` struct carrying its own `version`, checked by
the consumer, with a safe degradation path on mismatch.

```rust
#[repr(C)]
pub struct PlDamageRect { pub x: u16, pub y: u16, pub w: u16, pub h: u16 }

#[repr(C)]
pub struct PlRenderOut {
    pub version: u32,                  // PL_RENDER_ABI_VERSION
    pub px: *const u16,                // WHOLE framebuffer, row-major, native u16
    pub px_len: usize,                 // == stride * height
    pub stride: u16,                   // pixels per row (framebuffer width)
    pub rect_count: u16,               // 0 == nothing to paint this frame
    pub rects: *const PlDamageRect,    // borrowed until the next mutating call
}

pub const PL_RENDER_ABI_VERSION: u32 = 1;

pub unsafe extern "C" fn pl_ui_render_ex(ui: *mut PlUi, out: *mut PlRenderOut);
```

Rules:

- `px` still points at the whole framebuffer and the buffer stays single and
  un-copied. A rect is a **sub-rectangle with stride**, never a packed
  sub-buffer — copying pixels to pack them would spend on the CPU exactly what
  the design is trying to save.
- Same borrow lifetime as today: valid until the next
  `pl_ui_input`/`pl_ui_tick`/`pl_ui_render*`. `rects` shares that lifetime.
- C checks `out.version == PL_RENDER_ABI_VERSION` and, on mismatch, blits the
  full frame. A version mismatch must degrade to *slow and correct*, never to
  *fast and wrong*.
- `rect_count == 0` means "clean" and C must not blit. Note this is
  reachable only if C called render without checking `pl_ui_dirty`; keep the
  existing gate, keep this as a belt.
- **Core reports the true rect. C quantises it to what its transport can
  do** (§7). Core must not know that this panel prefers full-width bands —
  the emulator surfaces will use the exact rect.
- `pl_ui_render` (the old, unversioned `px`/`px_len` pair) is **kept
  unchanged** while `pl_ui_render_ex` lands and is proven on hardware, then
  deleted in its own bead. Two small reviewable steps, and a working bisect
  point in between.
- The header is cbindgen-generated (`firmware/CMakeLists.txt:182`) — nothing
  is hand-edited in `firmware/include/pico_link_ui.h`.

## 7. ST7789: replacing the once-at-init window contract

The current contract (`firmware/src/st7789.c:150-155`, and the comment in
`st7789_blit_framebuffer`) is: set CASET/RASET **once at init**, then every
blit issues bare RAMWR and relies on the panel's write-pointer wrapping. That
is a whole-frame-only contract. It must be **retired**, not conditionalised —
an `if (partial) { set window }` patch leaves two contracts in one driver and
the full-frame path silently depending on the panel's wrap behaviour.

Target structure — **one path to the panel, and it always sets the window**:

```c
static void st7789_set_window(uint16_t x0, uint16_t y0, uint16_t x1, uint16_t y1);
void st7789_blit_rect(const uint16_t *fb, uint16_t stride,
                      uint16_t x, uint16_t y, uint16_t w, uint16_t h);
```

- `st7789_blit_rect` sets the window, issues RAMWR, then DMAs.
- `st7789_blit_framebuffer(fb, n)` becomes a one-line wrapper calling
  `st7789_blit_rect(fb, W, 0, 0, W, H)`. The init solid fill goes through
  `st7789_set_window` too.
- Consequence and the reason for this shape: **the full-frame case exercises
  the same window code as the partial case on every single frame**, so a
  window bug cannot hide until the day the meter first draws.
- The init-time CASET/RASET is deleted and replaced with a comment naming the
  retired M1b "set the window once" contract by name, so nobody re-derives it
  from the wrapping behaviour.
- Inclusive end coordinates (`x0+w-1`), as the existing init code already
  documents.

### 7.1 Phase 1 restricts to full-width row bands

Phase 1 asserts `x == 0 && w == stride`. Then the source pixels are
contiguous — `fb + y*stride`, count `h*stride` — and it stays **one DMA
transfer**, with CASET unchanged and only RASET varying. No per-row DMA, no
chaining, minimal new failure surface in the driver.

`main.c` performs the quantisation (`x=0, w=PANEL_WIDTH`, keep `y`/`h`), and
the quantisation lives in C, next to the panel, not in core.

**Say the cost out loud:** the OUT meter is a *vertical* strip on the right,
which is the pessimal shape for a row-band blit. Its exact rect is ~48x174;
its row-band is 240x174, i.e. ~72% of the panel, so the blit only drops
~13.4 ms -> ~9.7 ms. Meanwhile the render for a meter-only frame drops from
~25 ms to well under 2 ms. Frame ~38.5 ms -> ~11 ms. That is the phase-1
number to expect, and it is enough — but do not let anyone report it as
"the blit is now cheap".

### 7.2 Phase 2, gated on measurement, not on taste

Column-clipped blit (per-row chained DMA: `h` transfers of `w` pixels) takes
the meter band to ~2.2 ms. Build it **only if** phase 1's measured numbers
show the blit has become the dominant term. Ballpark for sizing the
decision: ~174 DMA setups at a few hundred nanoseconds each is small against
the transfer, but it is new driver complexity against a panel this project
has already been bitten by.

### 7.3 The hazard that must be measured, not reasoned about

MADCTL is `0x60` — MV set (row/column exchange), MX set (column mirror).
With MV set, CASET/RASET address the **panel's** axes, not the framebuffer's,
so a framebuffer row band is not necessarily a RASET range. Full-window blits
are insensitive to this; partial ones are not. The driver's own comments
(`st7789.c`, the four-candidate MADCTL investigation) further record that this
panel misbehaves in MY-set modes — i.e. it is a panel whose addressing has
already surprised us once.

**Do not derive the transform on paper.** Bead 7h5.1 is a hardware spike:
paint a single known band (e.g. framebuffer rows 60..119 in a distinct
colour, everything else another colour) through an explicit CASET/RASET,
photograph it (the flash+webcam-capture-in-one-command workflow), and record
the measured framebuffer-rect -> panel-window mapping in the bead. Everything
downstream depends on that one fact.

## 8. The OUT meter: general mechanism, no fast path

**Agreed — general mechanism, and I do not dissent.** Concretely:

- `HeroStatusView::paint_key` folds the codec word, fallback state, bitrate,
  banner, and the OUT sample (`received_at`, the six 0-255 levels, and the
  quantised stale boolean per §3.2).
- Its `area` is the whole content region, which is too coarse to win
  anything, so it implements the **generic** `damage_hint` (§3.1): when only
  the OUT-sample part of its key changed, return the meter strip rect
  (`METER_STRIP_WIDTH`-derived, both channel columns as **one** rect per §5);
  otherwise return `None` and take the whole area.
- Inside `render`, it wraps its body sections in `ctx.needs(...)`.

Nothing about that is meter-shaped. `damage_hint` + `ctx.needs` is the answer
for any widget that is internally composite, and the hero is simply the first
one that is. **Explicit non-goal: no meter-specific code in `Screen`,
`Navigator`, `FrameBuffer565`, the FFI, or `st7789.c`.** If an implementer
finds themselves writing "if this is the meter" anywhere below
`render/hero.rs`, the design has been misread.

One structural note for later, not for this epic: the reason the hero needs
`damage_hint` at all is that `Screen` stacks widgets **vertically only**, so
a strip that sits *beside* the hero body cannot be its own sibling widget.
A horizontal layout container would make the meter a widget with its own
small area and `damage_hint` would be unnecessary for it. That is a real gap
in the layout engine and worth its own bead eventually — but retrofitting a
layout container is a bigger change than this epic, and `damage_hint` is
independently needed for widgets that are genuinely composite. Do not
conflate the two.

## 9. Emulator and the three run modes

Damage is **advisory** on the host. `minifb_surface` and `headless_surface`
keep flushing the whole persistent framebuffer — they are host-native and
free, and a full flush of a partially-updated persistent buffer is exactly
correct. The value of the emulator here is the **equivalence property test**
(§10, A4), which is where damage correctness is actually proven; hardware
proves the panel window, not the damage logic.

## 10. Acceptance criteria

Measured under a live LDAC stream to the WH-1000XM4, on the home screen,
with the OUT meter live and nothing else changing. Baseline first, on `main`,
same session, same conditions.

**A1 (load-bearing). `PL_LOOP_PHASE_UI_RENDER` p50 on meter-only frames
drops from ~25 ms to < 6 ms** (target < 3 ms). This is the criterion. If A1
fails, the change is **rejected**, not tuned — a partial blit with an
unclipped render is precisely the outcome the ADR warns produces a
convincing-looking `PL_LOOP_PHASE_BLIT` improvement and no frame-rate
improvement.

**A2. `PL_LOOP_PHASE_BLIT` p50 on meter-only frames drops from ~13.4 ms to
<= 10 ms.** Modest by construction (§7.1). Report it as such.

**A3. Superloop iteration rate improves measurably** against the p50
20 ms/iteration baseline banked in `pico-link-ajj`. Report before/after; do
not pre-commit to a target number.

**A4 (correctness, host). Damage-render equals full-frame render.** For every
screen: render frame N, mutate one piece of state, render frame N+1 through
the damage path, and assert the resulting framebuffer is **pixel-identical**
to a full-frame render of the same state in a fresh framebuffer. Model it on
the existing `dirty_gate_freshness_invariant_holds_for_every_screen` harness
in `core/src/app.rs` — same "prove it for every screen, not the one you
thought of" shape. Additionally assert that pixels outside the reported rect
are byte-identical to frame N.

**A5 (hardware).** The 7h5.1 band photograph confirms the window transform;
then 60 s of live meter with no visible ghosting, stale pixels, or torn
bands, photographed.

**A6.** The 1 s forced full-frame repaint backstop is still present and still
fires (it is the net under an incorrect `paint_key`).

**A7.** Test count does not regress; `cargo test --workspace` green (name the
invocation and the count — plain vs `--workspace` differ on this repo).

## 11. Named measurements (things this design deliberately does not guess)

- **M-1 (blocking, hardware).** The framebuffer-rect -> panel-window transform
  under MADCTL `0x60`. §7.3. Everything downstream of the blit depends on it.
- **M-2 (host, cheap).** What fraction of the ~25 ms render is **layout/
  `measure`** versus **rasterisation**? The whole design assumes layout is
  small and rasterisation dominates. If `measure` turns out to be, say, 8 ms
  of the 25 ms, then skipping `render` alone caps the win around 3x and the
  design needs a layout cache on top (cache `(constraints, paint_key) ->
  Size`). Run it before 7h5.4 so the answer can shape that bead rather than
  invalidate it. Do it in the emulator with a criterion-style bench or a
  simple instrumented loop — no hardware needed.
- **M-3 (deferred, gated).** Whether the row-band blit is the dominant term
  after phase 1. Decides 7h5.11.

## 12. Bead breakdown

`pico-link-7h5` becomes an **epic**.

| Bead | Work | Deps | Agent |
|---|---|---|---|
| 7h5.1 | **M-1**: measure the ST7789 window transform under MADCTL 0x60 with a band photograph. Record in the bead. | — | Tess/Tex (hardware) |
| 7h5.2 | **M-2**: measure layout/`measure` vs rasterisation share of render, host-side. | — | Ruby |
| 7h5.3 | Core, additive: `PaintKey`, `Damage`, `RenderCtx::{damage,with_damage,needs}`, `Widget::{paint_key,damage_hint}` with `ALWAYS`/`None` defaults. No consumer. Zero pixel change, all tests green. | — | Ruby |
| 7h5.4 | Core: the damage pass in `Screen`/`Navigator` (§3.3), damage-rect background fill replacing `clear`, chrome pseudo-regions, full-damage triggers, `App::render` returns the rect. **Includes the A4 property test.** | 7h5.3, informed by 7h5.2 | Ruby |
| 7h5.5 | Real `paint_key`s for hero/home, list, menu, fields, message, rail. One widget at a time; each is independently revertible. | 7h5.4 | Ruby |
| 7h5.6 | FFI: `PlDamageRect`, `PlRenderOut`, `pl_ui_render_ex`, `PL_RENDER_ABI_VERSION = 1`. Old `pl_ui_render` retained. | 7h5.4 | Ruby |
| 7h5.7 | Firmware: `st7789_set_window` + `st7789_blit_rect`, single path, init fill and full-frame blit both routed through it, once-at-init contract deleted. **Still full-frame — no behaviour change**, proven by flash + photo. Also fixes the 38.6 ms misattribution comment at `firmware/src/watchdog_sup.c:24`. | 7h5.1 | Ruby |
| 7h5.8 | Firmware: `main.c` calls `pl_ui_render_ex`, quantises to a full-width row band, blits the rect. **Acceptance A1/A2/A3/A5 measured here.** | 7h5.6, 7h5.7 | Ruby then Tess |
| 7h5.9 | `HeroStatusView::damage_hint` narrowing to the meter strip + `ctx.needs` inside its render. Delivers the meter's win. | 7h5.5, 7h5.8 | Ruby |
| 7h5.10 | Retire `pl_ui_render` (delete symbol + tests) once 7h5.8 is green on hardware. | 7h5.8 | Ruby |
| 7h5.11 | *Optional, gated on M-3*: column-clipped blit via per-row chained DMA. | 7h5.8 | Ruby |
| 7h5.12 | Retune `OUT_LEVEL_REFRESH_INTERVAL` now the ceiling is lifted (C publishes every 50 ms, so 20 Hz is the data-side ceiling). One-line change + a measurement. | 7h5.9 | Ruby |

7h5.1, 7h5.2 and 7h5.3 are all ready immediately and independent — dispatch
in parallel. 7h5.1 needs exclusive hardware access (only one agent may hold
the board).

## 13. Risks

- **A dishonest `paint_key` is a stale-pixel bug that looks like a hardware
  glitch.** Mitigations, all three kept: `ALWAYS` default, the A4 equivalence
  property test over every screen, and the existing 1 s forced repaint.
- **The time-folding trap** (§3.2) has a silent failure mode (no win) and a
  loud one (frozen widget). The `redraw_after` <-> `paint_key` review rule is
  the check.
- **Repainting changed widgets instead of intersecting widgets** (§3.3 step 7)
  is the classic damage bug. It is currently invisible because nothing
  overlaps, which makes it a latent trap for the first overlay widget.
- **The MADCTL window transform** (§7.3) — measured, not derived.
- **Scope creep into a layout container.** §8's horizontal-layout observation
  is deliberately *not* in this epic.

## 14. Explicitly not in scope

- `pico-link-3uq` (blit_start/blit_wait split). The render/blit split demotes
  it, and this design does not depend on it. If it lands later, §7's single
  `st7789_blit_rect` path is where it splits, and its no-render-between-start-
  and-wait rule is unaffected by damage rects.
- Double-buffering (ADR alternative A4) — 115 KB, not needed.
- Core 1 for the display — rejected by the ADR, and damage-rect is the reason.
