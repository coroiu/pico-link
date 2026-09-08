# Composite damage: why Home repaints 240 rows, and the region-set contract that fixes it

Bead: `pico-link-4ube`. Author: Fern (frontend architect). Status: **design of record**,
supersedes nothing; extends `2026-09-06-damage-rect-render-and-partial-blit.md`.

Advisory document. No code in this repo was changed to produce it. Every step below
is for Ruby (`rust-embedded-supervisor`).

---

## 1. The headline, before the detail

The bead states one defect. There are **three**, they are serially blocking, and
**fixing only the one in the bead changes nothing measurable on hardware.**

| # | Defect | Effect on Home while streaming |
|---|---|---|
| B1 | `HomeView` never overrides `damage_region_key`/`damage_hint` | `Screen` can never narrow Home |
| B2 | `Navigator::replace_at`/`replace_root` set `force_full_damage` unconditionally, and `App::on_levels_changed` -> `refresh_stack` runs on **every OUT-level sample** | even a fixed `HomeView` would be overridden every meter tick |
| B3 | `main.c:745` quantises the damage rect to a **full-width row band** (`x=0, w=PANEL_WIDTH`), and `meter_footprint` is deliberately **full height** | a perfect narrowing still blits 224 of 240 rows |

So the honest ceiling for "fix B1 + B2" alone is **240 -> 224 rows, ~6.7%**. That is
not worth shipping as a performance change on its own, and it must not be reported
as one.

The narrowing *can* be made to pay for itself here — by a wide margin — but only via
a change to the component model, not to `HomeView`. See section 5: if the meter
reports **per-segment regions** instead of one whole-strip rectangle, a typical
decay frame damages one or two ~12px bands, which the row-band blit we already ship
turns into roughly **90% fewer blitted rows, with zero C changes**. That is the
design's actual deliverable. B1 and B2 are prerequisites for it, not the win.

---

## 2. Evidence

- `core/src/render/home.rs:325-547` — `impl Widget for HomeView` implements `measure`,
  `is_focusable`, `on_focus`, `on_intent`, `chrome_contribution`, `activation`,
  `handles_back`, `render`, `paint_key`, `redraw_after`. **Not** `damage_hint`,
  **not** `damage_region_key`.
- `core/src/render/widget.rs:380` — `damage_region_key` defaults to `PaintKey::ALWAYS`.
- `core/src/render/paint_key.rs:74` — `ALWAYS` is `PaintKey(None)`, never equal to
  anything including itself.
- `core/src/render/screen.rs:664` — narrowing is gated on
  `old_slot.region_key == new_slot.region_key`, which for `HomeView` is permanently
  false. Falls through to `new_slot.area` = the whole content region.
- `core/src/render/hero.rs:1042-1061` — `HeroStatusView::damage_hint` /
  `damage_region_key` are correct and are **never called on the shipped Home
  screen**, because `Screen` only asks its own top-level widget, which is
  `HomeView` (`home.rs:142`, `Screen::new(HOME_TITLE, vec![Box::new(view)])`).
- `core/src/render/navigator.rs:266` — `replace_at` sets `self.force_full_damage = true`
  unconditionally. `core/src/app.rs:2088` — `refresh_stack` calls `replace_at` for
  every stack entry. `core/src/app.rs:2494` — `on_levels_changed` ends in
  `self.refresh_stack()`. The OUT meter sample is the highest-frequency event on the
  device while streaming, and it is the one event that forces a full repaint.
- `firmware/src/main.c:745-747` — `st7789_blit_rect(px, PANEL_WIDTH, 0, rect->y,
  PANEL_WIDTH, rect->h)`. The reported `x`/`w` are **discarded**; column clipping is
  deferred to `pico-link-7h5.11`.
- `core/src/render/hero.rs:506-510` — `meter_footprint` returns
  `Rectangle::new(Point::new(strip.x, area.top_left.y), Size::new(strip.w, area.size.height))`
  — 48px wide (`METER_STRIP_WIDTH`), **full content height**. Deliberate ("that
  widening is deliberate", `hero.rs:500`) and exactly wrong for a row-band blit.
- `core/src/render/chrome.rs:27,32` — `TITLE_BAR_HEIGHT = 16`, `RAIL_WIDTH = 34`
  (the rail is a right-hand *column*, so it costs no rows). Content band = 224 rows.
- Hardware: `dirty_rows` min=mean=max=240 across two ~50s LDAC-streaming windows
  (Tess, `pico-link-1n4.5`).

### Why review missed it

`hero.rs:1690-1700` builds a test-only `HeroSlot` proxy widget that forwards
`damage_hint`/`damage_region_key` to a shared `HeroStatusView`, and drives it through
a real `Screen::render`. That test is sound and it passes. It proves the *machinery*.
It cannot fail because of `HomeView`, because `HomeView` is not in it — the proxy
stands where the production composite stands. **A Screen-level test built on a
purpose-made proxy widget tests the framework, not the product.** This is the same
shape as the recorded `damage-keys-must-fold-only-what-is-drawn` lesson: a
damage-tracking defect with every test green.

---

## 3. Is `damage_hint` + `damage_region_key` the right shape for composites? No.

It is *sufficient* — section 4 gives the correct two-line forwarding for `HomeView`
— but it is the wrong shape, for three reasons that are all silent-failure modes:

1. **The invariant linking the two methods is prose only.** `damage_region_key` must
   equal "everything in `paint_key` except what is inside `damage_hint`". `hero.rs`
   honours this by sharing `body_paint_key` between them, by hand. Nothing enforces
   it. Get it wrong in the tight direction and pixels go stale (the `7h5.9`
   postmortem); get it wrong in the loose direction and the narrowing silently never
   fires (this bead).
2. **"Not implemented" and "always dirty" are the same value.** `PaintKey::ALWAYS`
   was chosen as the opt-in default precisely so unmigrated widgets behave as before.
   The cost is that there is no observable difference between a widget that has
   thought about damage and concluded "always", and one that has never heard of it.
   That is why B1 was invisible for a whole epic.
3. **Correct forwarding differs by composite kind and nothing captures that.**
   `HomeView` is a *switch* (one child fills the area). `HeroStatusView` is a
   *stack* (bands within its own area). `Screen` is also a stack, of widget slots,
   and has hand-rolled its own version of the diff — including the `index >= 2`
   magic offset for the two chrome pseudo-slots (`screen.rs:664`). Three composition
   shapes, three bespoke implementations, one shared unwritten invariant.

### The proposed shape: `PaintPlan`

Replace the trio (`paint_key`, `damage_hint`, `damage_region_key`) with **one**
method returning a small ordered set of keyed regions.

```rust
/// One keyed sub-rectangle of a widget's area.
pub struct PaintRegion {
    pub rect: Rectangle,
    pub key: PaintKey,
}

/// What a widget will paint this frame, region by region.
/// Fixed capacity, no allocation. Regions must COVER the widget's area
/// (their union must contain it); overlap is allowed and only costs precision.
pub struct PaintPlan { /* identity: u64, regions: heapless-style [PaintRegion; N], len */ }

fn paint_plan(&self, area: Rectangle, ctx: &RenderCtx) -> PaintPlan {
    PaintPlan::whole(area, self.paint_key(ctx))   // default: today's behaviour
}
```

Diff rule, owned by `Screen`, applied identically to every plan:

- `identity` differs, or region count differs -> whole area damaged (union of the old
  and new areas). This is the `cache_miss` case, now per-widget instead of per-screen.
- Otherwise compare region `i` old vs new: if `rect` moved, damage
  `union(old.rect, new.rect)`; else if `key` changed, damage `new.rect`; else nothing.
- Frame damage is the union over all changed regions of all widgets (see section 6
  on emitting more than one rect).

What this buys, concretely:

- **The "everything outside" invariant becomes structural.** There is no "outside":
  every pixel belongs to a named region that carries its own key. Failure mode 1
  above is not merely discouraged, it is unrepresentable.
- **Composition is mechanical.** A switch composite returns
  `child.paint_plan(area, ctx).with_identity_folded(face_discriminant)`. A stack
  composite concatenates children's plans (offsetting rects). A decorator passes it
  through. No widget ever hand-derives a complement key again.
- **`Screen` stops being a special case.** `Screen`'s slot loop *is* a paint plan:
  chrome title, chrome rail, then one sub-plan per widget. The `index >= 2` offset
  and the separate `paint_cache`/`PaintSlot` type both disappear into one
  `PaintPlan` diff used at both levels. This is the systems payoff, not cosmetics.
- **Precision becomes expressible.** Section 5 depends entirely on a widget being
  able to say "these sixteen bands, each keyed separately", which the single-rect
  `damage_hint` cannot say at all.
- **Overflow degrades safely.** A plan that would exceed capacity collapses to one
  whole-area region with the fold of every key — always correct, merely less precise.
  Suggested capacity: 20 (the meter needs 16 + legend + hold caps; see section 5).
- **`ctx.needs(rect)` is unchanged.** Widgets keep gating their own rasterisation
  exactly as `hero.rs:638,866` already do.

Migration is mechanical and non-breaking if `paint_key` is kept as the default
plan's key: only widgets that want >1 region implement `paint_plan`, and
`damage_hint`/`damage_region_key` are deleted once `hero.rs` moves over.

---

## 4. The fixes, in dependency order

### Stage 1 — B1: `HomeView` forwards its active face's damage (small)

`HomeView` is a *switch*, not a general composite: `measure` returns the full
constraints (`home.rs:326`) and `render` hands `area` unchanged to exactly one child
(`home.rs:491`). So the forwarding is exact and total, and the only thing `HomeView`
adds on top of the child is *which face is showing*.

```
damage_hint(area, ctx)      -> active child's damage_hint(area, ctx)
damage_region_key(ctx)      -> PaintKey::of(HOME_PAINT_KEY_SEED)
                                 .fold(face_discriminant)      // 0 status, 1 menu
                                 .fold_key(active child's damage_region_key(ctx))
```

Correctness argument, to be written into the doc comment:

- Everything `HomeView` paints is painted by the active child; it draws nothing of
  its own. So "everything outside `damage_hint`" for `HomeView` is exactly
  "everything outside `damage_hint`" for the active child.
- The face discriminant must be folded, and folded into the **region** key, not only
  `paint_key`: a face flip changes the entire area, which the region key must
  reject. Fold order must match `paint_key`'s (`home.rs:531-538`) so the two stay
  legible side by side.
- The hidden face's key must **not** be folded, exactly as `paint_key` already
  argues (`home.rs:519-522`).
- `MenuList` has no `damage_region_key` override, so on the menu face the fold
  yields `ALWAYS` (`paint_key.rs:156-161`, `fold_key` of an `ALWAYS` child is
  `ALWAYS` — already tested at `paint_key.rs:308`). Menu therefore keeps today's
  whole-area behaviour, automatically and correctly. This is the migration guarantee
  working as designed; do not special-case it.

### Stage 2 — B2: carry the paint cache across an in-place screen rebuild (the real blocker)

`Screen::paint_cache` is the only cross-frame memory in the render path — that is the
entire justification for `damage_region_key` being diffed by `Screen` rather than by
the widget (`widget.rs:353-370`). But `replace_at`/`replace_root` swap in a brand-new
`Screen`, whose `paint_cache` is empty, so `force_full_damage = true` is *honest*
given the current structure. It is a sledgehammer standing in for a missing carry.

`ScreenCarry` (`app.rs:2082-2086`) already exists as the mechanism for carrying
viewport state across a live rebuild. The paint cache is the same kind of carry.

Design:

- `Navigator::replace_at(index, screen)` moves the outgoing screen's `paint_cache`
  into the incoming one **iff** `old.id().is_some() && old.id() == new.id()`, and
  does **not** set `force_full_damage` in that case.
- Any other case (id `None` on either side, ids differ, push/pop/truncate/resize,
  explicit `force_full_damage()`) keeps today's unconditional full damage.
- The carry is safe because every subsequent guard still runs: a widget-count change
  trips `cache_miss` (`screen.rs:648`); a moved widget trips the `moved` branch; a
  changed widget in the same slot trips the key diff, because each widget kind seeds
  its `paint_key` with a distinct constant (`HOME_PAINT_KEY_SEED = 15` and friends).
  Requiring equal `ScreenId` means the same builder produced both screens, so slot
  *structure* is stable by construction.

Without Stage 2, Stage 1 is unobservable on hardware: `on_levels_changed` fires
`refresh_stack` on every OUT sample, and that is precisely the frame the meter
narrowing exists to make cheap.

### Stage 3 — B3: make the narrowing worth having (section 5)

### Stage 4 — optional, measure first: column clipping (`pico-link-7h5.11`)

`st7789_blit_rect` currently requires `x == 0 && w == stride` so the source stays
contiguous. Honouring `x`/`w` means 224 separate 96-byte DMA transfers instead of one
107KB transfer for the meter strip. **Per-transfer setup overhead may eat the entire
win.** Do not schedule this on the assumption that 81% fewer pixels means 81% less
time — measure a strided blit against the row-band blit on hardware before designing
around it. If setup dominates, the alternatives are a scratch packing buffer (pack
the columns contiguously, one DMA) or simply not doing it, because section 5 gets
most of the win without touching C at all.

---

## 5. Where the win actually is: per-segment meter regions

`theme::draw_vertical_level_meter` lights N of `VERTICAL_METER_SEGMENT_COUNT` (16)
discrete segments per channel, and `hero.rs`'s `paint_key` already computes exactly
the quantised segment counts (`segments_l`, `segments_r`, `hold_segments_l`,
`hold_segments_r`, `hero.rs:1010-1027`). Over a ~190px bar that is roughly 12px per
segment.

Today the widget reports one 48x224 rectangle, which the C blit widens to 240x224 —
all 224 content rows — no matter how little changed. **A one-segment decay step is
being paid for as a full-height repaint.**

Under `PaintPlan`, `HeroStatusView` emits:

- one region for the hero body (key = `body_paint_key()`), and
- one region **per segment band**, spanning both channels' columns, keyed by that
  band's lit/unlit state for L, for R, and by whether either hold cap sits in it.

`Screen`'s cross-frame diff then computes which bands changed — the thing the widget
structurally cannot know itself, because widget instances do not survive frames.

Expected effect on the blit we already ship, no C change:

- one channel decays one step: **1 band, ~12 rows of 240 (~95% saving)**
- both channels decay one step at nearby levels: ~2 adjacent bands, ~24 rows
- worst realistic case (L and R at opposite ends of the bar both step): the bounding
  union spans between them, still typically under half the content band
- a real body change (codec word, kbps rung, device name): full content band, as today

This is a strictly larger win than column clipping (Stage 4) and it needs no
firmware change, because it converts the meter's damage from a *column* into
*rows* — which is the granularity the shipped blit actually has.

### Architectural principle worth writing down

**On this device, damage is row-granular. Frequently-changing UI should therefore be
laid out in horizontal bands, and a full-height element that changes often is the
most expensive shape available.** Today the two highest-frequency elements are the
title-bar volume (a 16px top band — cheap, correct shape) and the OUT meter (a
full-height right-hand column — the worst shape). Per-segment regions fix the meter
without moving it. **A horizontal meter would also fix it and is Uma's and Andreas's
call, not mine** — I am not proposing a visual change, only recording that the
current vertical strip is fighting the blit and that section 5 is the way to keep the
vertical design and still get the win.

---

## 6. Multiple damage rects

`ui-ffi/src/lib.rs:323` already declares `last_damage_rects: [PlDamageRect; 1]` with
the note that growing it "only grows how many of these slots get filled, not the FFI
shape", and `main.c:730-747` already branches on `rect_count`. Once section 5 lands,
a single bounding union is what limits it: two bands 8 segments apart get unioned
into one 100-row rect.

Recommendation: **do not** grow this in the same change as sections 4-5. Land the
single-rect version, measure, and only then decide whether 2-4 rects pay. `main.c`
would need a loop over `rect_count` and a guard that the rects do not overlap (they
must not double-blit). Note the C side reads only `rects[0]` today and the ABI
version check (`main.c:722`) is the correct place to gate a shape change.

---

## 7. The regression test that would have caught this

The failure was a Screen-level test built on a proxy widget. The fix is a test that
starts from **`App` and a real event**, and asserts on the damage rectangle
`Screen::render` returns.

**T1 — the primary regression test (fails today for both B1 and B2).**
Location: `core/src/app.rs` tests (it must go through `App`, not `Screen`, or it
cannot see B2).

```
let mut app = App::new(240, 240);
// connect an LDAC device, publish one LevelsChanged so a meter exists
app.render(&mut fb);                       // frame 1: full damage, primes the cache
app.advance_clock(...);                    // enough for one segment to decay
app.handle_event(Event::LevelsChanged{ .. });   // ONLY the meter changes
let damage = app.render(&mut fb);          // frame 2

assert!(damage.size.height < CONTENT_HEIGHT,
        "a meter-only change must not damage the whole content band");
assert!(contains(meter_footprint(content_area), damage),
        "a meter-only change must stay inside the meter strip");
```

Assert an inequality against the content band, not a magic number — the number moves
with section 5 and the test should not.

**T2 — the two-sided guard.** The mirror of T1: change something in the hero **body**
(codec word, or the kbps rung) with no new OUT sample, and assert the damage
**does** cover the body. T1 alone can be satisfied by a narrowing that is too
aggressive; T2 is what makes the pair safe. Both are cheap and deterministic — the
returned `Rectangle` is a pure function of core state, so this is not a timing test
in a unit test's clothing.

**T3 — the class guard, for every screen.** Extend the existing A4 property test
(`damage_rendered_frame_matches_a_full_frame_render_of_the_same_state_for_every_screen`,
`core/src/app.rs`). Its current arms both tick to the same final instant before
rendering, so it cannot see a narrowing bug that only manifests across *intermediate*
frames (`hero.rs:1606-1614` says so explicitly). Change the damage arm to render N
successive frames incrementally through the real damage path, and compare the final
framebuffer against one full render at the final instant. That is the general
"narrowed too much" property, for every screen at once, and it is what makes section
5's 16-region meter safe to ship.

**T4 — the anti-proxy rule, as a review rule not a test.** Any test of the damage
pass must instantiate the **production** screen builder (`build_home_screen`,
`build_devices_screen`, ...). A test-local widget that forwards damage correctly
proves only that a correct widget would work. Write this into the design doc's
review checklist and into `widget.rs`'s doc comment.

---

## 8. Hacks to retire (do not port these forward)

- **`PaintKey::ALWAYS` as the opt-in default for `damage_region_key`.** It made
  "never implemented" indistinguishable from "genuinely always dirty" and that is
  precisely why this survived an epic. `PaintPlan`'s default (one region, whole area,
  `paint_key`) is honest: >1 region *is* the opt-in signal, and it is observable.
- **`screen.rs:664`'s `index >= 2` chrome offset.** A magic constant encoding "the
  first two slots are pseudo-widgets". `PaintPlan` folds chrome and widgets into one
  region list with one diff.
- **`force_full_damage = true` on every `replace_at`.** A correctness sledgehammer
  substituting for a missing cache carry, and the single largest reason the epic's
  work is invisible on hardware.
- **`meter_footprint`'s deliberate full-height widening (`hero.rs:497-510`).** It was
  chosen for a rectangle blit. The shipped blit is a full-width row band, so the
  widening converts a 48x190 change into 240x224 of blitted pixels. Keep the helper
  for the body/meter split inside `render`, but stop using it as the damage report
  once section 5 lands.
- **The row-band quantisation itself (`main.c:745`)** is not a hack — it is a
  documented phase-1 decision — but it is now load-bearing in a way nobody
  re-derived: it silently discards `x`/`w`, so every core-side effort to narrow
  horizontally is currently worth exactly nothing. Any future damage work must state
  which axis it narrows.

---

## 9. Recommended bead breakdown for Ruby

1. **B1 + B2 + T1 + T2** — one bead. They are only meaningful together, and T1 is the
   proof. Expected measured result: `dirty_rows` 240 -> 224 while streaming. **Report
   it as "the machinery is now live", not as a performance win.**
2. **`PaintPlan`** — one bead, pure refactor. Replaces `paint_key`/`damage_hint`/
   `damage_region_key`, unifies `Screen`'s slot diff, no behaviour change. Land T3
   with it.
3. **Per-segment meter regions** — one bead. This is the win. Requires 2.
4. **Measure a strided/column-clipped blit** (`pico-link-7h5.11`) — a Tess
   measurement bead, not an implementation bead, until the DMA-overhead question is
   answered.
5. **Multiple damage rects across the FFI** — only if 3's measurement shows the
   bounding-union is the remaining limit.

## 10. Open questions for Andreas

1. **Scope.** Is the `PaintPlan` refactor (step 2) wanted, or only steps 1 and 3
   hacked onto the existing trio? My recommendation is to do it: step 3 needs a
   region *set*, which `damage_hint` cannot express at all, so the alternative is a
   second bespoke mechanism beside the first.
2. **Meter orientation.** Section 5 keeps the vertical stereo strip and still gets
   the win, so nothing forces a change. But if Uma ever revisits it, a horizontal
   meter is the shape this blit wants. Flagging, not proposing.
3. **Stopping rule.** If step 3's measured `dirty_rows` does not land under ~60 in a
   real streaming window, stop — the machinery does not pay for itself on Home and we
   should say so and delete the complexity rather than tune it.
