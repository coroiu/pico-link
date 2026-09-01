# UI framework: fixed chrome regions + linear stacks, not a general layout engine

**Date:** 2026-08-11
**Status:** Accepted

## Provenance note (written 2026-09-01, bead pico-link-3i8)

This ADR did not exist as a file until now, even though `core/src/render/
chrome.rs`, `core/src/render/widget.rs`, `core/src/render/mod.rs` and
`.planning/design/2026-08-29-button-rail-edge-region.md` have all cited it
by this exact filename since the day the code landed. Commit `2cbdec8`
("W3 render core", 2026-08-11) itself says in its message that it is
"replacing the retired gui/simple_gui RGBA rasterizer per the UI-framework
ADR" — so the decision was real and was already being treated as settled
on the day the render core was built. It was simply never written down as
a standing file, which is bead pico-link-3i8's whole complaint: a rule
enforced by convention but unauditable.

This document reconstructs the decision from the commit history and from
what the citing comments have claimed it decided for the three and a half
weeks since. Where the reconstruction is solid (backed by a commit message
or a diff), it's stated as fact. Where it isn't, it's flagged below rather
than invented. Treat this ADR as retroactively accepted at the 2026-08-11
date it documents, not as a new decision made today.

## Context

The render core that became `core/` (then `bhk-core`, built for the
predecessor Bitwarden-hardware-key project this repo was forked from — see
CLAUDE.md's Repo layout section) had already been through one layout
engine before `compute_chrome` existed.

That first attempt lived under `gui/simple_gui` and, per its own commit
history (`c759c26` "Start adding flexbox", 2024-04-19 through `075c736`
"implement column layout", 2024-05-21 — over a year before this ADR's
date), built toward a general CSS-flexbox-style layout system: a
style/layout tree, child-size-aware layout, then a rewrite specifically
to make layout *not* depend on child sizes, then column layout on top of
that. Commit `2cbdec8` (2026-08-11), which created `compute_chrome` and
retired that engine, calls it in its own message "the retired gui/
simple_gui RGBA rasterizer."

The device this code targets is display- and resource-constrained (at the
time, a 320x170 T-Embed panel; today a 240x240 Pico Plus 2 W panel over
SPI, still `no_std` + `alloc`). Every screen the product needs is one of a
small, closed set of shapes: a title bar, a content area that is a single
widget (a list, a menu, a message, a confirm dialog), and — at the
2026-08-11 date — a hint bar; the hint bar was later removed in favor of
the labelled button rail (`pico-link-znb.5`, 2026-08-29), which chrome.rs
now computes in its place. Nothing in the actual product ever needed
sibling widgets positioned relative to each other, wrapping, percentage
splits, or any of the composition flexbox exists to solve.

## Decision

**Chrome is fixed regions, not a general layout engine.** `compute_chrome`
divides the screen into a small, fixed number of named rectangular
regions (title bar, button rail, content area) using fixed pixel
constants for the bar/rail sizes, with only the *regions themselves*
computed from the screen size passed in — no hardcoded screen dimension
anywhere in `core`. Within the content region, composition is a linear
stack of one widget at a time (the `Widget` trait salvaged and
reimplemented from `simple_gui::components::Component`, per
`core/src/render/widget.rs`'s own header), not a tree of independently
positioned children.

This means: no flex/grid-style constraint solving, no generic
parent-relative sizing, no nested layout trees. A new screen either fits
the fixed chrome shape or it doesn't ship as a new screen.

## Rationale

- **The old flexbox attempt is exactly what a general layout engine costs
  on this class of hardware**: several commits building a style tree, a
  layout tree, then a full rewrite because child-size-aware layout turned
  out to be the wrong shape, before it produced a shippable UI. That is
  real engineering cost for generality the product's actual screen set
  never used.
- **The product's screens are a closed, small set of shapes.** Every
  screen so far — Home, Devices, device detail, the codec picker, message,
  confirm — decomposes into title + content + button rail with one active
  content widget. A fixed-region model is not a restriction being worked
  around; it is what the actual UI needs.
- **A `no_std` + `alloc` render loop on an RP2350 has a real frame budget**
  (see `core/run.rs`). Fixed-region computation is a handful of saturating
  arithmetic operations; a constraint-solving layout pass is not, and its
  cost scales with tree complexity the product doesn't have.
- **Salvage over rewrite, but reimplemented cleanly.** `widget.rs`'s
  header says explicitly that `Widget`, `Action` and `FocusEvent` are
  "salvaged concepts, reimplemented cleanly on `embedded-graphics`" from
  `simple_gui::components` — the decision was not to discard everything
  from the prior project, only its general-purpose layout ambition.

## Alternatives considered

- **Keep and finish the flexbox engine.** Rejected — chrome.rs's own
  words are that this ADR "rejects both the old flexbox attempt and a new
  general-purpose one." The old engine represents sunk, real effort, but
  effort spent doesn't change what the product's screen set actually
  needs.
- **Design a new general-purpose layout engine from scratch**, avoiding
  the specific mistakes of the flexbox attempt (e.g. child-size-aware
  layout). Also rejected, for the same reason: generality this product's
  UI never exercises is speculative cost, not insurance.

I cannot reconstruct with confidence which specific alternative shapes
(if any) were weighed beyond these two — e.g. whether a lighter
constraint-based system short of full flexbox was considered and rejected,
or whether the choice was always binary (fixed regions vs. general
engine). The historical record (commit messages, the code itself) doesn't
say, and I'm not going to invent a more detailed debate than actually
happened.

## Consequences

- `compute_chrome` and any future chrome-region work must stay a small,
  fixed set of named regions sized by constants, not grow into
  constraint-based or child-size-aware layout. This is the rule
  `chrome.rs`, `widget.rs` and `mod.rs` cite today, and the rule
  `.planning/design/2026-08-29-button-rail-edge-region.md` §2 relies on
  when it argues a free-text hint bar and a labelled button rail must not
  coexist as two legends.
- A screen that doesn't fit title + content + button rail (one active
  content widget) is a signal to reconsider the screen's design, not a
  reason to extend the layout model.
- `VerticalList`, `MenuList` and similar widgets stay responsible for
  their own internal layout (row heights, scrolling) — chrome only ever
  hands them a single content `Rectangle`.
- This decision is scoped to on-device chrome layout. It says nothing
  about, and does not constrain, the FFI seam, the platform trait seams
  (`DisplaySurface`/`InputSource`/`Clock`/`Storage`), or any decision made
  after 2026-08-27's C-first pivot.
