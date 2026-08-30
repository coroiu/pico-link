# The widget-facing clock seam: a frame-scoped `RenderCtx`, not a `Clock` trait object

**Date:** 2026-08-31
**Status:** Accepted (design of record; not yet implemented)
**Bead:** `pico-link-znb.10`
**Author:** Fern (fe-architect)
**Corrects:** `.planning/design/2026-08-28-on-device-ui.md` sections 2, 20, 21
(marked inline at each site, not silently rewritten)

## Context

`pico-link-a67` landed the FFI half of "give the core a clock": `pl_ui_tick`
(`ui-ffi/src/lib.rs:344`) forwards `now_us` into `App::tick`
(`core/src/app.rs:348`), and `App::now_us` is exposed at `core/src/app.rs:355`.
That parameter is not discarded — the design doc's section 2 and section 20,
written before `a67` landed, are stale on that point and are corrected by this
ADR's companion edits to that file.

The remaining problem is real: `App::now_us` is a getter no widget can reach.
`Widget::render` (`core/src/render/widget.rs`, ~line 170) receives only an area
and a framebuffer; `Screen::render` (`core/src/render/screen.rs:289`) has no
time parameter either. `Screen::chrome_contribution`
(`core/src/render/screen.rs:223`) is called from inside `Screen::render`
(`:362`) and is equally time-blind — a seam that only covers `Widget::render`
cannot produce a live title readout or an animated status dot.

Two additional facts, found during this design and not previously on record:

- **`App` has no `Platform` on the firmware path.** `PlUi`
  (`ui-ffi/src/lib.rs:268`) is `{ app: App, malformed_tag_count: u32 }`, built
  as a bare `App::new(width, height)` (`ui-ffi/src/lib.rs:311`). C owns the
  loop; there is no `Clock`, no `Platform`, nothing to hand a widget.
- **The emulator never ticks the app.** The only `App::tick` call sites in the
  repo are `ui-ffi/src/lib.rs:498` and one test at `core/src/app.rs:1232`.
  `core/src/run.rs` samples `platform.clock().now()` at lines 375/380/399/
  449/452/456 but never forwards it to the app, so `App::now_us` is
  **permanently 0** in both headless and windowed modes. Any seam that ships
  without fixing this animates on hardware and freezes in the emulator — the
  exact three-mode divergence this project keeps paying for. Verified
  independently by the orchestrator, not just claimed by Fern.

The bead names this an expensive-to-reverse architecture decision, not a
convenience call, and asks for scoring against section 20's "eight payoffs."

## Decision

**A frame-scoped `RenderCtx` value, sampled once per frame and threaded
through the render call — not a `Clock` trait object, not time folded into the
model.**

New module `core/src/render/ctx.rs`:

```rust
use core::time::Duration;
use crate::platform::Instant;

/// Frame-scoped facts every widget may read while drawing. Sampled once
/// per frame by `App::render`, so every widget and the chrome observe the
/// SAME instant — a pull-based clock cannot promise that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RenderCtx {
    now: Instant,
}

impl RenderCtx {
    #[must_use] pub const fn at(now: Instant) -> Self { Self { now } }
    #[must_use] pub const fn now(&self) -> Instant { self.now }
    #[must_use] pub const fn elapsed_since(&self, earlier: Instant) -> Duration {
        self.now.saturating_duration_since(earlier)
    }
}
```

Reuses `platform::Instant` (`platform.rs:95`) unchanged — no second time type.
`#[non_exhaustive]` plus a constructor plus accessors so the next frame-scoped
fact (dim state, a reduce-motion setting, a frame counter) is an additive
field, not another break of every widget signature.

Trait changes, all four, in `core/src/render/widget.rs`:

```rust
fn measure(&self, constraints: Size, ctx: &RenderCtx) -> Size;
fn render(&self, area: Rectangle, ctx: &RenderCtx, target: &mut FrameBuffer565)
    -> Result<(), Infallible>;
fn chrome_contribution(&self, _ctx: &RenderCtx) -> Option<ChromeContribution> { None }
fn redraw_after(&self, _ctx: &RenderCtx) -> Option<Duration> { None }   // NEW
```

`measure` gets `ctx` too, deliberately: `measure` and `render` run in the same
frame and must agree. Payoff 5 (suppress sub-500ms transition UI) changes a
widget's requested *height*; a time-blind `measure` paired with a time-aware
`render` tears.

`render` stays `&self`. Appearance must be a pure function of (state, area,
time). That purity is what makes a headless screenshot at a pinned instant
reproducible, and it is the property a `Clock` handle would destroy.

**`redraw_after` is the half that makes the seam actually work**, and it is
why this is an architecture decision rather than a parameter. Today
`App::tick` deliberately does not mark the app dirty (`app.rs:851-860`) and
`Runner::step` renders only `if app.dirty()` (`run.rs:373`). Give widgets time
without a redraw source and a spinner still never repaints in the emulator.
The naive fix — tick always marks dirty — throws away the flush-skip on every
screen forever and full-frame-blits a static Home at 18fps, contending with
audio over SPI (section 20's own argument for the deferred dirty-rect item,
E22). So instead: a widget *declares* when its appearance would next differ;
`Screen` takes the min over its widgets; `Navigator` delegates to the current
screen; `App` stores `next_redraw_at: Option<Instant>` recomputed at render,
and `App::tick` sets `dirty` only when it comes due. Widgets that do not care
return `None` and pay nothing.

### Where it plugs in (file and line)

| Site | Change |
|---|---|
| `core/src/render/ctx.rs` | new file, `RenderCtx` |
| `core/src/render/widget.rs:203` | `measure` gains `ctx` |
| `core/src/render/widget.rs:219` | `render` gains `ctx`, before `target` |
| `core/src/render/widget.rs:252` | `chrome_contribution` gains `ctx` |
| `core/src/render/widget.rs` (new, after 252) | `redraw_after` |
| `core/src/render/screen.rs:223` | `Screen::chrome_contribution` gains `ctx`, forwards |
| `core/src/render/screen.rs:340` | `Screen::render` gains `ctx`; forwards at `:362` (chrome), `:442` (measure), `:446` (render) |
| `core/src/render/screen.rs` (new) | `Screen::redraw_after` = min over widgets |
| `core/src/render/navigator.rs:292` | `Navigator::render` gains `ctx`; new `Navigator::redraw_after` delegating to `current()` |
| `core/src/app.rs:970` | `App::render` builds `RenderCtx::at(Instant::from_micros(self.now_us))`, and after rendering stores `next_redraw_at` |
| `core/src/app.rs:858` | `App::tick` additionally sets `dirty` when `now >= next_redraw_at` |
| `core/src/app.rs:499-567` | `App` gains `next_redraw_at: Option<Instant>` |
| `core/src/run.rs` (~line 340, before the `app.dirty()` gate at `:373`) | `app.tick(frame_start.as_micros())` — closes the emulator gap above |
| `firmware/src/main.c:429` | already correct, no change (Fern stayed out of `firmware/`) |

## Rationale

### The `no_std` + `alloc` and platform boundary, checked explicitly

`RenderCtx` is `Copy`, 8 bytes, no heap, no `Drop`. `Instant`
(`platform.rs:95`) and `Duration` (`core::time`) are already in-tree. No new
dependency of any kind, and nothing here reaches toward a platform crate —
`core/` stays compiler-enforced platform-free per this repo's stated rule.
`&RenderCtx` is a concrete reference type, so `Widget` remains dyn-compatible
and `Box<dyn Widget>` keeps working — this is exactly where a generic clock
parameter would have failed (see rejected alternative below). Minor follow-up
noted for the implementer: re-export `Instant` from `crate::render` so
render-path code does not import a type from a module named `platform`.

### Testability

A test constructs `RenderCtx::at(Instant::from_micros(n))` and calls
`widget.render(area, &ctx, &mut fb)` directly. Time is pinned by value — no
clock injection, no shared-cell plumbing, no run loop. Headless screenshots of
an animating widget become byte-reproducible, extending the zoomed-PNG
verification discipline to animation. `run.rs:727-735`'s existing
`ControllableClock` covers the run-loop half.

## Alternatives considered

**A `Clock` trait object (`&dyn Clock`), reusing or extending
`platform::Clock` (`platform.rs:150`) — rejected on three independent
strikes.** Do not reuse it here, do not extend it, and do not add a second
one; bypass it entirely and leave it exactly as it is, the run loop's platform
capability.
1. *It does not exist on the firmware path.* `PlUi` holds a bare `App`
   (`ui-ffi/src/lib.rs:268, 311`). Core would have to synthesize a fake
   `Clock` wrapping `now_us` — a shim whose only job is to satisfy a trait
   nobody can actually supply.
2. *Pull-based time tears within a frame.* Two `now()` calls in one frame can
   return different values; the title readout and the content can disagree.
   Frame time must be sampled once.
3. *`Clock` carries `sleep` (`platform.rs:157`).* Handing a widget a blocking
   sleep on a single-threaded ~18fps superloop that also runs BTstack is a
   footgun with zero upside. Separately, a generic `C: Clock` parameter would
   break `Box<dyn Widget>` outright (`widget.rs:14-21`) — only the `dyn` form
   is even shape-legal, and it still loses on strikes 1 and 3.

**Time folded into the model, the way `BtModel` is — rejected.** `BtModel`
(`app.rs:334`) is event-folded domain state: `Clone + PartialEq`, changes
rarely, and every change triggers a full screen rebuild
(`App::rebuild_root`, `app.rs:620`). Time changes every frame. Folding it in
means either rebuilding the whole screen tree ~18 times a second, or carving
out a "this field does not count for dirtiness" exception — a
constraint-driven special case this project has already agreed not to port
forward.

**An `Rc<Cell<Instant>>` shared mailbox, matching the existing
`wizard_phase`/`home_face` pattern — the strongest rival, rejected.** Recorded
honestly as the closest call: it needs no signature change at all, which is a
genuine advantage. It loses on four counts:
- Time-dependence becomes invisible to the type system — nothing in a
  widget's trait signature says it may depend on time.
- The plumbing does not vanish, it moves somewhere worse — every time-aware
  widget must be *constructed* with the handle, growing the already-large
  `build_*_screen` signatures (six parameters at `app.rs:421-428`).
- It lets a widget read time inside `on_intent`, reintroducing the
  two-times-in-one-frame tearing problem the frame-scoped approach exists to
  prevent.
- Pinning time in a test means reaching into the right `Rc` instead of
  passing a value — strictly worse for the reproducibility this ADR wants.

Given the bead's explicit "expensive to reverse," the explicit parameter that
cannot be misused wins over the smaller diff.

**A bare `now: Instant` parameter instead of a struct — rejected, narrowly.**
Functionally and performance-wise identical to `RenderCtx`. Loses only
because every future frame-scoped fact (dim state, reduce-motion, a frame
counter) would reopen every widget signature again. A one-field `Copy` struct
costs nothing at runtime and makes the next addition additive instead of
another mechanical rewrite across every widget.

## Section 20's "eight payoffs," scored honestly

Section 20 of the design of record claimed the clock "unlocks eight things...
near-zero cost, eight payoffs." That oversells the widget-facing seam. Scored
against the same list:

- **Delivered outright (3):** liveness during multi-second waits (payoff 1 —
  the one this project has already paid for once, in the form of a static
  screen indistinguishable from a hang; `redraw_after` is what makes this real
  rather than nominal); wizard auto-dismiss without a C-side timer (payoff 3;
  note `Event::WizardAutoDismiss` already exists C-driven at `app.rs:673,
  743-758` — whether to retire that C timer is separate follow-up work, not
  this bead); suppressing sub-500ms transition UI (payoff 5, given `ctx` on
  `measure` and the event-timestamp addition below).
- **Delivered conditional on an addition (1):** a determinate 10.24s scan bar
  (payoff 2) needs more than frame time — core must know when the scan
  *started*. See the event-timestamp note below.
- **Dead (1):** "Last connected N days ago" (payoff 4). This contradicts
  section 21's E26 in the same design doc, which already cut it on merit:
  this board has no RTC. Section 20 and section 21 disagreed with each other
  before this ADR; section 21 was right.
- **Out of scope (1):** "dim/blank timing owned by core" (payoff 6). That
  policy already lives in `run::Runner`'s idle/deep-sleep tiers in the
  emulator, which already has a real `Clock` and already works there. On
  firmware there is no `Runner` at all, so "core owns it" there would mean
  re-homing the whole policy behind the FFI — a separate, larger piece of
  work.
- **Merely unblocked, not delivered (2):** the bitrate smoothing window
  (payoff 7, blocked on E20 — no live bitrate exists yet) and peak-hold decay
  (payoff 8, blocked on E17 — no peak/RMS data exists yet).

**Net: 3 outright, 1 conditional, 1 dead, 1 out of scope, 2 unblocked-only.**
"Eight payoffs for one parameter" is not an accurate description of the
widget-facing work; the design doc's section 20 is corrected accordingly
(marked inline, not deleted).

### The event-timestamp addition, stated once

A *timestamp* is domain state and belongs in the model; the *current time* is
not, and does not. `App::handle_event` should stamp `self.now_us` onto the
phases that need a duration — `WizardPhase::Scanning { started: Instant }`,
`WizardPhase::Connecting { started: Instant, .. }` (`app.rs:275-300`). Widgets
then render `ctx.elapsed_since(started)`. This is what makes payoffs 2 and 5
land in full.

## Consequences

- **`core/` still never depends on a platform crate.** This ADR adds one new
  file and four trait-signature changes, all within `core/`, all built from
  types already in-tree (`platform::Instant`, `core::time::Duration`).
- **Every widget implementation's `measure`/`render`/`chrome_contribution`
  signature changes.** This is a mechanical, wide diff (step 2 of the
  migration order below) across `list.rs`, `menu.rs`, `hero.rs`, `home.rs`,
  `message.rs`, `confirm.rs`, `wizard.rs`. It should land with zero
  behavioural change and the existing test suite green, as its own
  reviewable commit.
- **A standalone bug is fixed as part of this work, not incidentally.**
  `core/src/run.rs` never forwards its sampled clock time into `App::tick` —
  verified independently (not just Fern's claim): the only `App::tick` call
  sites in the repo are `ui-ffi/src/lib.rs:498` and a test at
  `core/src/app.rs:1232`. `App::now_us` is therefore permanently 0 in
  headless and windowed modes today. Migration step 3 below fixes this in
  three lines and should be its own commit, independently reviewable and
  independently revertable from the `RenderCtx` plumbing itself.
- **Firmware ignores `redraw_after` until a follow-up bead.** `pl_ui_render`
  renders unconditionally by documented contract
  (`ui-ffi/src/lib.rs:501-508`) and C blits every frame regardless. So the
  SPI/audio-contention win this seam sets up lands only in the emulator until
  `ui-ffi` exposes a dirty query to C. Whether one already exists was not
  checked (out of scope: staying out of `firmware/`). This is a correctness-
  neutral gap — behaviour is right either way — and belongs in its own named
  bead, not folded into this one.
- **Retiring the C-side wizard auto-dismiss timer becomes possible, but is
  not decided here.** Keep both the C timer and the new core-side liveness
  until the seam has shipped once and been seen on hardware, then decide
  under a named follow-up bead.
- **Two hacks are explicitly rejected as implementation paths and must not be
  ported forward:**
  - **"Tick always marks dirty" as the liveness mechanism.** It is the
    obvious shortcut, it works, and it permanently costs the flush-skip,
    full-frame-blitting a static screen at ~18fps into contention with
    audio. `redraw_after` exists specifically to prevent this. If a review
    sees `App::tick` setting `dirty` unconditionally, that is the hack
    landing instead of the design.
  - **Per-widget `Rc<RefCell<_>>` mailboxes as a general answer to "this
    widget needs a fact."** They were the right tool for `wizard_phase`/
    `home_face` (state that must survive a screen rebuild). Time is not that
    kind of fact, and each new mailbox grows `build_*_screen`'s parameter
    list, already at six (`app.rs:421-428`).
  - A second time type or a second clock trait is also rejected — see
    Alternatives above; `platform::Instant` is already the right primitive
    and is already platform-free despite living in a module named
    `platform`.

## Implementation order (for whoever builds this — do not re-derive the reasoning above)

Each step compiles and is independently reviewable; do not collapse them.

1. `core/src/render/ctx.rs` — `RenderCtx` as specified, re-export `Instant`
   from `crate::render`. Pure addition, nothing calls it yet. Tests:
   constructor/accessors/`elapsed_since` saturation.
2. Thread `ctx` through the render path, with no widget reading it yet — all
   sites in the table above except `run.rs` and `redraw_after`. Mechanical
   across every widget file. Success criterion: the existing suite stays
   green with signature-only edits and zero behavioural diff; report the test
   count and the invocation used.
3. Feed the emulator — `Runner::step` calls `app.tick(frame_start.as_micros())`
   before the `app.dirty()` gate at `run.rs:373`. Test with
   `ControllableClock` that `App::now_us` advances under `run`. Standalone bug
   fix; its own commit even though it is three lines.
4. `redraw_after` — trait default `None`, `Screen` mins over widgets,
   `Navigator` delegates, `App` holds `next_redraw_at` set at render and
   consulted in `tick`. Tests: a stub widget requesting 100ms leaves
   `App::dirty()` false at +50ms and true at +150ms; a default widget never
   goes dirty from `tick` alone. Do not make `tick` unconditionally dirty.
5. Event timestamps — `WizardPhase::Scanning`/`Connecting` gain
   `started: Instant`, stamped in `App::handle_event`.
6. One real consumer, to prove it end to end — wizard phase 4/5 liveness. Two
   headless captures at pinned instants must differ, inspected at zoom. Then
   stop: what actually moves on screen is a UX call, not an implementation
   call.

### Open items flagged but not resolved by this ADR

- Whether `ui-ffi` should expose a dirty/redraw-due query to C, so firmware
  gets the SPI/audio-contention benefit `redraw_after` sets up — needs a
  follow-up bead; not designed here.
- Whether to retire the C-side wizard auto-dismiss timer once this seam has
  shipped and been seen on hardware — a separate, later decision.
- Whether `ctx` on `measure` (the widest part of the step-2 diff) is worth
  its churn — Fern's call is yes, for frame coherence with `render`, needed
  for payoff 5. If implementation pushes back on the diff size, that is the
  one line item to re-discuss, not silently drop.
