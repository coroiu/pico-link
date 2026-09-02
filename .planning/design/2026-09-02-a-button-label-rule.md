# The A-button label rule

**Bead:** `pico-link-vmq` · **Author:** Uma (UX) · **Date:** 2026-09-02
**Status:** Ruling. Supersedes nothing; **amends** the design of record
(`.planning/design/2026-08-28-on-device-ui.md`) section 4 by adding rule 4, and
**subsumes** the field-list ruling's §4.4 `chrome_contribution` clause
(`.planning/design/2026-09-02-field-list-widget-ruling.md`) into a general
mechanism that preserves its behaviour exactly.

Andreas, 2026-09-02: *"the A button is gray without a hint most of the times."*

---

## 1. What is actually on screen today

Measured, not read. Headless emulator driven over `POST /api/input`, plus
`cargo run --example {devices,wizard}_screenshots -p pico-link-core`, rails
cropped to the right 34px and inspected at 3x/5x.

`Screen::with_button_labels` has **zero production call sites** — every screen's
static `ButtonLabels` is `Default`, i.e. all four slots `Inert`. So the rail is
*entirely* whatever the focused widget's `chrome_contribution` happens to
return, and only three widgets in the tree implement it at all
(`HomeView`, `PairingWizardView`, `DevicesListView`, plus `FieldList`, which is
not wired to a screen yet). `VerticalList`, `MenuList` and `ConfirmView` — the
three widgets that actually own most of the app's activatable rows — do not
implement it, so their screens fall through to `Inert`.

### Per-screen audit

| # | Screen / phase | Focused widget | What A does today | Rail A today | Verdict |
|---|---|---|---|---|---|
| 1 | Home — status face | `HomeView(Status)` | toggles to the menu face | `devs` **Live** | OK |
| 2 | Home — menu face | `HomeView(Menu)` | activates Bluetooth / Settings row | `select` **Live** | OK |
| 3 | **Devices** | `DevicesListView` | device row → device detail; "Pair new headphones" → wizard (or forget-picker at the 8-device cap) | **Inert** | **BUG** |
| 4 | **Pick one to forget** | `VerticalList` | pushes the forget-confirm for that device | **Inert** | **BUG** |
| 5 | **Forget device?** | `ConfirmView` | commits Cancel / Forget, pops | **Inert** | **BUG — worst case** |
| 6 | Device detail (stub) | none (`vec![]`) | nothing | Inert | OK |
| 7 | Settings (stub) | none (`vec![]`) | nothing | Inert | OK |
| 8 | **Pair — Scanning** | `PairingWizardView` | activates the selected discovered device → Connecting | **Inert** | **BUG** |
| 9 | Pair — Nothing found | `PairingWizardView` | restarts discovery | `scan` **Live** | OK |
| 10 | Pair — Connecting | `PairingWizardView` | nothing | Inert | OK |
| 11 | Pair — Not responding | `PairingWizardView` | nothing (X = `keep`) | Inert | OK |
| 12 | Pair — Failed | `PairingWizardView` | nothing (X = `retry` when retryable) | Inert | OK |
| 13 | Pair — Succeeded | `PairingWizardView` | nothing (auto-dismiss) | Inert | OK |
| 14 | Store-corrupt boot (Devices) | `DevicesListView` | same as #3 | **Inert** | **BUG** (same defect as #3) |

**Four distinct screens, covering every path through the MVP, where A works and
the rail says nothing.** Andreas is describing a real majority: of the eight
states where A does anything, five are silent.

Case 5 is the one that should have been caught in review. On a *destructive
confirmation*, A is the only committing control on the device, and it renders in
`palette::DIVIDER` with no text — while the focused "Cancel" row simultaneously
draws a disclosure caret promising that something is one press away. The screen
contradicts itself.

### Two inverse defects found in the same rail

Not the bead's subject; recording them because they are the same failure class
(the rail asserting something untrue) and both were visible in the same
captures.

- **Home, status face: B renders `back` and does nothing.** `Screen::handles_back`
  is a static per-screen `bool` and Home sets it `true` so B can fold the menu
  face back to the status face. On the status face there is nothing to fold, and
  `HomeView::on_intent`'s `Back` arm returns `Action::None` at the root
  (`home.rs:247-268`). Design section 4 rule 2 is violated in its literal
  direction: a labelled-but-dead affordance, on the one screen the user sees
  most. See §7.
- **Pair — degraded success: X renders `codec` and does nothing.** Acknowledged
  in `wizard.rs:418-425` — the codec picker (E11) is not built. Also a rule 2
  violation; the honest render until E11 lands is `Inert`. See §7.

---

## 2. Why this drifted, in one sentence

`ChromeContribution.a` is an `Option<Option<ButtonLabel>>` in effect (absent /
`Inert` / `Live`), the default is "absent", and "absent" resolves to a **dim,
silent, but fully functional** A. The default is the wrong one: forgetting to
speak produces a lie that nobody notices, instead of a breakage that everybody
notices.

Every patch that labels one more screen leaves that default intact, so the next
screen re-introduces the bug. Hence: rule, not patch.

---

## 3. The rule

> **Rule 4 (amends design section 4).** **A's liveness and A's label are the same
> fact.** A widget has exactly one A-affordance, expressed once, as
> `Option<Verb>`. `Some(verb)` means A acts *and* the rail reads `verb`;
> `None` means A is dim *and* pressing it is a guaranteed no-op. There is no
> third state, and no way to express one.

Corollaries, all load-bearing:

1. **A never has a screen-level static label.** A is always the focused widget's
   property, because "activate the focused thing" is meaningless without a
   focused thing.
2. **The gate and the label read the same accessor.** They cannot disagree,
   because they are not two things.
3. **The default is `None`, and `None` is honest.** A widget that forgets is dim
   *and* inert — the developer's own A stops working, which they notice in the
   first manual test. Compare today's default, whose failure is invisible.
4. **`Inert` remains correct wherever activation genuinely does nothing** —
   `FieldList`'s `Readonly` rows, the wizard's Connecting/Failed/Succeeded
   phases, and the two empty stub screens. This rule does not touch those; it
   makes them the *only* dim-A cases.

Rule 4 is the exact mirror of rule 2 ("an unlabelled X or Y does nothing"),
extended to A with the stronger guarantee that A can no longer be unlabelled and
live at all. Rule 2's X/Y exemption stands unchanged: X and Y remain
screen-authored and may be `Inert` at will.

---

## 4. The vocabulary

A's job is constant everywhere — *activate the focused thing*. The specificity
belongs in the row text, which is sitting right next to the rail. So A's word
should name the **kind of consequence**, from a set small enough to be learned
once, and it must not become a per-screen thesaurus (`open` / `view` / `show` /
`go`).

**A takes one of exactly four words.**

| Word | Chars | Means | The user's expectation |
|---|---|---|---|
| `open` | 4 | Pushes a deeper screen about the focused row. | Nothing changed. B brings me straight back. |
| `select` | 6 | Commits the focused row's choice on *this* screen. | Something changed, or is about to. |
| `pair` | 4 | Begins pairing with the focused device. | The radio is about to do work; the screen becomes a progress view. |
| `scan` | 4 | (Re)starts discovery. The screen has no focused row. | The list is about to repopulate. |

Plus **one named exception**: Home's status face keeps `devs`, because it is
Andreas's own label from design section 4's rail table and it sits *inside* the
already-declared Home exception. It adds no new exception. It is spelled
`Verb::Exception("devs")` so that `grep -rn 'Verb::Exception' core/` audits the
entire app's exceptions in one command. **A second `Verb::Exception` is a UX
decision, not an implementation choice — bring it to me.**

### Width budget: 6 characters, measured

Rail is 34px wide, less a 1px hairline = 33px usable; label font is
`font::hint()` (`helvR08`), centred and clipped per slot by `rail.rs`. Measured
from the captures: `select` and `forget` (6 chars) render at ~27px — comfortable.
7 chars extrapolates to ~31px, inside the clip but touching both edges.
**Cap A's verbs at 6 characters.** All four fit with room. (`rail.rs` already
clips, so an over-long label truncates rather than bleeding — there is a test for
that — but the rule is "fits", not "doesn't crash".)

### Assignment per screen

| Screen / phase | Focused row | Verb |
|---|---|---|
| Home — status | (the hero) | `Exception("devs")` — unchanged |
| Home — menu | Bluetooth / Settings | **`open`** — changed from `select`; both rows push, and both draw a caret |
| Devices | a paired device | `open` |
| Devices | "Pair new headphones" | `pair` — including at the 8-device cap, where the forget-picker is the app making room, not a different intent |
| Pick one to forget | any device | `select` |
| Forget device? | Cancel / Forget | `select` |
| Pair — Scanning | a discovered device | `pair` |
| Pair — Nothing found | (no row) | `scan` — unchanged |
| Pair — Connecting / Not responding / Failed / Succeeded | — | `None` (dim) — unchanged |
| `FieldList` — `Action` row | — | `open` default, per-row override |
| `FieldList` — `Readonly` row | — | `None` (dim) — unchanged, and the reason this rule exists in this shape |
| Device detail (stub) / Settings (stub) | no widgets | `None` (dim) — unchanged |

Only one existing label changes text (Home menu `select` → `open`). That is
deliberate consistency work, not a typo — Ruby should not "fix" it back.

---

## 5. The enforcement mechanism

Three parts. Part (b) is the one that actually closes the hole; (a) and (c) are
what keep it closed.

### (a) A new trait method — one source of truth

```rust
/// The word this widget's A button should show, and — identically — whether
/// A does anything at all. `None` (the default) means BOTH "A is dim" and
/// "activating me is a guaranteed no-op": the two cannot disagree, because
/// `Screen` reads this same method for both purposes (see
/// `Screen::activate_focused`).
///
/// **A widget that wraps another widget must forward this**, exactly like
/// `redraw_after` / `scroll_top` / `selected_key` (pico-link-vxc, D2).
/// Unlike those, forgetting is loud: A visibly stops working.
///
/// The verb must not depend on time — only on this widget's own state and
/// its selected row. Chrome is NOT covered by `Screen::redraw_after`'s fold
/// (see `redraw_after`'s doc comment, D3), so a time-varying verb would go
/// stale under the `pl_ui_dirty()` blit gate. Selection- and model-driven
/// changes are already covered: `App::handle_input` marks dirty
/// unconditionally (`app.rs:1686`) and every `handle_event` mutation path
/// marks dirty at its own site.
fn activation(&self) -> Option<Verb> { None }
```

```rust
/// The fixed vocabulary for the A button. See
/// `.planning/design/2026-09-02-a-button-label-rule.md` §4. Every word is
/// <= 6 chars, the measured budget for a 34px rail slot in `font::hint()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// Pushes a deeper screen about the focused row. Nothing changes.
    Open,
    /// Commits the focused row's choice on this screen.
    Select,
    /// Begins pairing with the focused device.
    Pair,
    /// (Re)starts discovery. Used where the screen has no focused row.
    Scan,
    /// The one sanctioned escape hatch, with exactly one use today:
    /// Home's status face shows `devs` (design section 4's rail table,
    /// inside the already-declared Home exception).
    /// `grep -rn 'Verb::Exception' core/` audits every exception in the
    /// app. **Adding a second is a UX decision, not an implementation
    /// one.** Must be <= 6 chars.
    Exception(&'static str),
}
```

with `const fn as_str()` and a `const _: () = assert!(...)` on each unit
variant's length.

### (b) One choke point makes the wrong state unrepresentable

`Screen::activate_focused` (`screen.rs:292`) currently dispatches
unconditionally. It becomes:

```rust
pub(super) fn activate_focused(&mut self) -> Action {
    let Some(index) = self.focused_index else { return Action::None };
    // A's liveness IS its label. A widget that reports no verb cannot be
    // activated, so a dim A is a guaranteed no-op -- design rule 4.
    if self.widgets[index].activation().is_none() {
        return Action::None;
    }
    self.widgets[index].on_focus(FocusEvent::Activated)
}
```

and `resolve_button`'s A arm becomes, with no fallback to a screen static:

```rust
Button::A => match self.focused_widget().and_then(Widget::activation) {
    Some(verb) => ButtonLabel::Live(String::from(verb.as_str())),
    None => ButtonLabel::Inert,
},
```

This is what makes it structural rather than remembered: **the same call decides
both.** "A acts but is dim" is now not a bug you can write — it is a state the
program has no way to represent.

### (c) Delete the ways to express it wrongly

- **Delete `ChromeContribution::a`.** Its three production users (`home.rs` ×2,
  `fields.rs` ×1) migrate to `activation()`. With the field gone, there is no
  second channel through which a widget can author an A label, and therefore no
  way for a wrapper to override A into disagreement with the gate.
- **Drop the `a` parameter from `Screen::with_button_labels`** — signature
  becomes `with_button_labels(x, y)`. Zero production call sites today; removing
  it makes "a screen-level static A label" a compile error rather than a
  temptation. `ButtonLabels.a` stays as the *resolved* value the rail draws.
- **Make the activation callback builders take a verb.** `VerticalList::
  on_activate_index(verb, cb)`, `on_activate(verb, cb)`, `MenuList::
  on_activate_index(verb, cb)`, `ConfirmView::on_activate_index(verb, cb)`.
  You cannot install a handler without naming it; the compiler asks.
  Per-row override where rows differ (Devices' device rows say `open`, its
  "Pair new headphones" row says `pair`): `ListItem::with_verb(Verb)` /
  `MenuItem::with_verb(Verb)`, and `FieldRow`'s existing
  `activate_label: Option<String>` becomes `verb: Option<Verb>` — no free-form
  strings anywhere.

### (d) One central test, not one per screen

A per-screen assertion is the same scatter that caused this. Add **one** test in
`core` that, for every production screen builder, walks each reachable
state/phase and asserts the invariant directly:

```
for every production screen state:
    assert_eq!(rail_a_is_live(&screen), screen.focused_activation().is_some())
```

Because (b) derives both from one call this cannot fail by construction — which
is the point: the test is a *regression tripwire on the mechanism*, and it fails
loudly the day someone reintroduces a second channel for A. Add a second,
cheaper test that is genuinely capable of failing: assert every `Verb` renders
at <= 6 characters, and that `Verb::Exception` appears exactly once in the
production tree.

### What is NOT enforceable, stated honestly

Wrapper forwarding (`DevicesListView` → `VerticalList`, `PairingWizardView` →
its inner list, `HomeView` → `MenuList`) cannot be type-enforced through
`Box<dyn Widget>`. It is mitigated, not eliminated: the failure mode flips from
silent-and-cosmetic to loud-and-functional (A stops working), and the central
test in (d) covers every production wrapper that exists. Do not claim more than
that in the commit message.

---

## 6. Interaction with the dirty gate

The firmware now blits only when `pl_ui_dirty()` is set, so a rail whose label
changed without a dirty mark would keep showing the old word.

- **Selection-driven verbs** (Devices' `open` vs `pair`; `FieldList`'s
  `Action` vs `Readonly`) are safe: `App::handle_input` marks dirty
  unconditionally for any non-empty intent batch (`app.rs:1686`).
- **Model-driven verbs** are safe: every `handle_event` mutation path already
  marks dirty at its own site.
- **Time-driven verbs are forbidden** by the `activation()` doc contract above,
  because `Screen::redraw_after` folds only over `self.widgets` and explicitly
  does not cover chrome (widget.rs, D3). Nothing in the vocabulary needs one.

No new dirty plumbing is required by this ruling. Ruby must not add any.

---

## 7. Two inverse defects — separate beads, same rail

Do not bundle these into the A change; they are separate diffs and separate
reviews. Both are design-section-4 rule 2 violations found while auditing.

**7.1 Home's status face shows a live `back` that does nothing.**
`Screen::handles_back` is static per screen, but Home's B is only meaningful on
the menu face. Recommended fix, symmetric with §5(a): a
`Widget::handles_back(&self) -> bool` defaulting to `false`, with
`HomeView` returning `self.face() == HomeFace::Menu`, and
`resolve_button`'s B arm reading `can_go_back || focused_widget_handles_back()`
instead of the screen flag. Home's status face then correctly renders B dim —
which is also *true*: at the root, with the status face showing, there is
nowhere to go back to.

**7.2 Pair — degraded success shows a live `codec` X with no destination.**
Already acknowledged in `wizard.rs:418-425`. Until the codec picker (E11) lands,
that arm must return `ButtonLabel::Inert`. One line, and it makes the rail stop
lying today rather than in a future milestone.

**7.3 (Noted, not ruled.)** Design section 4 says "Right = A" and "Left = B", but
`Navigator::dispatch` (`navigator.rs:290`) forwards `Left`/`Right` to the widget
untouched and no production widget handles them. Unrelated to the rail; flagged
so it is not lost.

---

## 8. Handoff

**Fern** — `activation()` is a new `Widget` trait method and one changed choke
point in `Screen`; it removes `ChromeContribution::a` and narrows
`with_button_labels`. That is your surface, not mine. The UX requirement I am
holding you to is exactly rule 4 in §3: A's liveness and A's label must be one
value read at one site. If you can get there with a different shape, take it —
but "widgets are asked to set a label" is not a different shape, it is the
current bug.

**Ruby** — implement §5 (a)-(d) and the §4 assignment table verbatim; there is
nothing left to decide in it. Ship §7.1 and §7.2 as their own beads. Verify per
the project rule: drive the headless emulator through all fourteen rows of §1's
table, capture, and inspect the rail **at zoom** — a 1x PNG will not show you
whether the A glyph is `DIVIDER` or `TEXT_SECONDARY`. The two screenshot
examples (`devices_screenshots`, `wizard_screenshots`) already render full
chrome and cover most of the table; extend them rather than writing new ones,
and commit the fixtures so the next diff catches a regression.
