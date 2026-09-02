# Ruling: the field-list widget model — one row primitive, two containers

Bead `pico-link-ay0` (epic), answering the decision Uma handed over in
`.planning/design/2026-09-02-device-page.md` §8 and §10 ("For Fern").
Author: Fern (frontend architect), 2026-09-02.

**This document is the widget contract. Ruby implements from it without
re-deciding anything.** Where it disagrees with `2026-09-02-device-page.md`,
that document's *intent* is preserved and its *mechanism* is corrected — every
such case is called out explicitly with the reason.

Depends on / amends:

- `.planning/design/2026-09-02-device-page.md` — the page spec. §8's list of six
  capabilities is the input to this ruling. §5's "clear the trailing slot to
  BACKGROUND" is **declined** (§7.1 below); §5's "ellipsise the label" is
  **deferred** (§4.6).
- `.planning/design/2026-09-01-home-alignment-grid.md` — `L = 12`, `R = 194`.
  Honoured. No third scheme invented.
- `.planning/decisions/2026-08-11-ui-framework-reuse-vs-rewrite.md` — fixed
  chrome regions, one content widget, widgets own their internal layout. This
  ruling stays inside it: nothing here touches `compute_chrome`, and all new
  layout is *internal to a widget*, which is exactly what that ADR's
  consequences section reserves for `VerticalList`/`MenuList` "and similar".

---

## 1. The ruling, in one sentence

> **Extend the shared row primitive `menu::draw_row` so it can express every
> capability §8 asks for, and add one new container `FieldList` in
> `core/src/render/fields.rs` that uses it. `MenuList` keeps its two callers
> and its shipped metrics, and is ported onto the same extended primitive in
> the same change. One drawing path, permanently.**

Neither of Uma's two options, taken literally, is right — and the reason is
worth stating because it decides the shape:

**Of the six capabilities in §8, four are properties of a *row*, not of a
*list*.** Value-and-caret-together (2), per-row trailing font (4), the leading
glyph gutter (5) and the 12/12 margins (6) all live inside `draw_row`
regardless of which container calls it. They have to be built there whichever
option is chosen. So the "six additions is a lot to call reuse" framing
overcounts the container decision by four.

Once those four land in the primitive, the container question shrinks to two
items: a per-row activation gate (1) and scrolling (3). And **that** is the
choice worth arguing.

---

## 2. Why a new container rather than extending `MenuList`

`MenuList` has exactly two callers today (`home.rs:186`, the menu face's two
rows; `confirm.rs:83`, the destructive-confirm's two rows). Both are short,
unscrolled, all-rows-activatable action menus, and its module doc says so in as
many words (`menu.rs:270-277`).

Extending it would make six of nine features dead for two of three callers, and
— the actual objection — it would **turn type-enforced rules back into builder
discipline.** Uma's §4 says an Info row can never grow a caret and `A` can never
fire on it. Under `MenuItem::activatable(false)` those are two independent
builder calls a caller can get half-right, silently, forever. Under
`FieldKind::Readonly` they are one enum variant and a caller *cannot* get them
out of step. On a page whose entire purpose is "never show the user a control
that lies", that is not ceremony; it is the requirement.

Second objection: `ConfirmView` is the highest-consequence widget in the product
(it is the last thing between a fat finger and re-pairing). A change motivated
by a diagnostics page should not be able to alter it by accident. Under this
ruling the only thing that touches `ConfirmView` is a mechanical port to
`RowStyle::MENU`, whose acceptance criterion is *byte-identical pixels*.

Third: the split is honest about what the two types are.
`MenuList` = a short list of actions. `FieldList` = a scrolling label/value
sheet, most of whose rows are inert. `VerticalList` = a scrolling list of
two-line entities. Three shapes, three types, **one row renderer between the
first two** — which is the constraint the brief set.

**The cost I am accepting, stated plainly:** `FieldList` duplicates roughly 50
lines of `MenuList`'s container boilerplate (clamped `move_selection`, the
`on_focus` focus/activate switch, `is_focusable`). I considered re-expressing
`MenuList` as a `FieldList` preset to remove it and rejected that as a
big-bang: it rewrites the confirm dialog's guts for a 50-line saving. If a
*fourth* single-line-row container is ever proposed, that is the moment to fold
`MenuList` into `FieldList`, not now. Filed as §8 item 6.

---

## 3. The extended row primitive (`core/src/render/menu.rs`)

### 3.1 New types

```rust
/// Row metrics. One per LIST, not per row -- a list whose rows have
/// different margins is not a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowStyle {
    /// x inset from the row's left edge to the leading gutter (or, when
    /// `leading_gutter == 0`, to the label itself).
    pub left_margin: i32,
    /// x inset from the row's RIGHT edge to the trailing value's right
    /// edge. This is the alignment grid's `R`.
    pub value_right_margin: i32,
    /// x inset from the row's RIGHT edge to the caret's right edge.
    /// Must be <= `value_right_margin`. When it is strictly less, the
    /// caret occupies the gutter to the RIGHT of the value column, so
    /// gaining focus never moves the value. See 4.2.
    pub caret_right_margin: i32,
    /// Reserved width (px) of the leading glyph gutter, applied to EVERY
    /// row whether or not that row carries a glyph, so labels align
    /// whether checked or not. 0 = no gutter.
    pub leading_gutter: i32,
    /// Padding above and below the single text line. Drives `row_height`.
    pub vertical_padding: i32,
}

impl RowStyle {
    /// `MenuList`'s shipped metrics, reproduced exactly. Do not change
    /// these numbers in the porting step -- see 8.1's acceptance test.
    pub const MENU: Self = Self {
        left_margin: 8, value_right_margin: 6, caret_right_margin: 6,
        leading_gutter: 0, vertical_padding: 10,
    };
    /// The field list: the alignment grid's 12/12, a compact row, and a
    /// caret that lives right of the value column.
    pub const FIELD: Self = Self {
        left_margin: 12, value_right_margin: 12, caret_right_margin: 4,
        leading_gutter: 0, vertical_padding: 6,
    };
    /// `FIELD` plus the pickers' current-choice check gutter.
    pub const FIELD_GUTTERED: Self = Self { leading_gutter: CHECK_GUTTER_WIDTH, ..Self::FIELD };
}

/// Width of the leading glyph gutter: an `icon_1x` glyph (~8px) plus a
/// 4px gap before the label.
pub(crate) const CHECK_GUTTER_WIDTH: i32 = 12;

/// A row's right-aligned value text. Drawn REGARDLESS of `selected` --
/// today's `Trailing::Label` semantics, unchanged and still the point
/// (`menu.rs:55-60`).
pub(crate) struct RowValue<'a> {
    pub text: &'a str,
    pub color: Rgb565,
    pub font: &'a FontRenderer,
}

/// A row's trailing content. A STRUCT, not an enum: the value and the
/// caret are independent, which is capability (2).
pub(crate) struct RowTrailing<'a> {
    pub value: Option<RowValue<'a>>,
    /// Whether this row draws a disclosure caret WHILE SELECTED.
    /// Unchanged `Trailing::Caret` semantics.
    pub caret: bool,
}
```

### 3.2 `Trailing` is deleted, not extended

`pub enum Trailing` (`menu.rs:61`) has no user outside `menu.rs`
(`grep -rn "Trailing::" core/src emulator/src` returns only `menu.rs`) and is
not re-exported from `render/mod.rs`. Its `Label`-xor-`Caret` exclusivity is an
artefact of the one toggle-row case it was written for, and §8 item 2 is that
artefact being mistaken for a rule. **Delete it.** `MenuItem::trailing()` lowers
`OwnedTrailing` straight to a `RowTrailing`. Adding a fourth variant instead
would leave two vocabularies for one concept, which is the thing this ruling
exists to prevent.

### 3.3 New signature

```rust
pub(crate) fn draw_row<D>(
    target: &mut D,
    row_rect: Rectangle,
    style: &RowStyle,
    label: &str,
    label_color: Rgb565,
    label_font: &FontRenderer,
    leading: Option<char>,          // rendered with `font::icon_1x`, in the gutter
    trailing: &RowTrailing<'_>,
    selected: bool,
) -> Result<(), Infallible>
```

`leading` is `None` and ignored when `style.leading_gutter == 0`. The gutter is
still reserved when `leading` is `None` and the width is non-zero — that is the
whole point of it being in the style.

### 3.4 Row height becomes style-and-font derived

```rust
pub(crate) fn row_height(style: &RowStyle, font: &FontRenderer) -> u32 {
    (style.vertical_padding * 2 + line_height(font)) as u32
}
```

`MenuList` calls `row_height(&RowStyle::MENU, &font::value())` and gets today's
number unchanged. The existing test at `menu.rs:446` updates its call site only.

**Ruby: do not hardcode 28.** §5's "28px" is a prediction from an assumed 16px
`helvR12` line height; the real number comes from the `"Agjpqy"` probe
(`menu.rs:160`). The constant you write is `vertical_padding: 6`; the height is
derived. See §5 for the tests that make the arithmetic self-checking.

---

## 4. `FieldList` (`core/src/render/fields.rs`, new)

### 4.1 Row kinds — two, not three, and that IS the finding

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Bright label, caret while focused, `A` fires the list's callback.
    Action,
    /// Dim label, NEVER a caret, `A` is a no-op -- but fully focusable,
    /// and its value is drawn regardless of focus.
    ///
    /// This single variant is BOTH the device page's "informational" row
    /// (pico-link-znb.13) AND the codec picker's "disabled but focusable"
    /// row (pico-link-znb.9). They differ only in what the value text
    /// says: a fact on one screen, a reason on the other. Rendering and
    /// behaviour are identical, so they are one variant, and making them
    /// one variant is what makes it impossible for them to drift apart.
    Readonly,
}
```

Uma's §4 table lists three kinds. Two of them (`Info`, `Disabled`) have
identical rows in every column of that table. Encoding them as one variant is
the type-level statement of THE FINDING; encoding them as two would be the
finding written down and then immediately not acted on.

Extension point, so nobody has to guess later: if a disabled row ever needs a
distinct mark (a lock glyph, a strikethrough), it takes the **leading gutter**,
not a third kind.

### 4.2 The caret never moves the value

`RowStyle::FIELD` sets `caret_right_margin: 4` against `value_right_margin: 12`.
Content region is 206 wide, so the value column's right edge is `R = 194`
(exactly the Home grid) and the caret occupies roughly `194..202`, in the
right-hand gutter, 4px clear of the rail.

This is a ruling, and the alternative was worse. Right-aligning both to `R` and
shifting the value left when the caret appears would move the value on focus —
the exact defect the Home grid document spends §3.1 arguing against, on the
screen where the value is the payload. Reserving the caret slot *inside* the
content region instead would pull every value 8px off the product's right rule,
so the device page's value column would no longer line up with Home's bitrate.
Putting the caret in the gutter costs 8px of the 12px gutter on focused Action
rows only, and costs alignment nothing.

**Verify at zoom** (project discipline): if `icon_1x`'s caret measures wider
than 8px, widen the gutter usage by lowering `caret_right_margin` to 2 — do
**not** move the value column.

### 4.3 `FieldRow`

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldRow {
    pub label: String,
    kind: FieldKind,
    label_color: Option<Rgb565>,    // override; None => derived from `kind`
    value: Option<String>,
    value_color: Rgb565,            // default TEXT_PRIMARY
    value_font: ValueFont,
    leading: Option<char>,
    activate_label: Option<String>, // the A-rail word for this row
    key: Option<ListItemKey>,
}

/// Which face a row's trailing value uses. A closed two-variant enum, not
/// a stored `FontRenderer`: `FieldRow` must stay `Clone + PartialEq` like
/// its siblings `ListItem`/`MenuItem`, and the capability asked for is
/// exactly one override, for `ADDRESS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueFont {
    /// `theme::font::value()` (helvR12). The default.
    Normal,
    /// `theme::font::label()` (helvB08) -- the small face `ADDRESS` needs
    /// (device-page design 3.6, which measured helvR12 as over budget).
    Small,
}
```

Builders, mirroring `ListItem`/`MenuItem` conventions exactly:

`FieldRow::action(label)` · `FieldRow::readonly(label)` ·
`.with_value(text, color)` · `.with_small_value()` · `.with_leading_glyph(ch)` ·
`.with_label_color(c)` · `.with_activate_label(word)` · `.with_key(k)`

Default label colours: `Action -> palette::TEXT_PRIMARY`,
`Readonly -> palette::TEXT_SECONDARY`. `with_label_color` overrides both (the
red `Forget this device` row is `action(...).with_label_color(STATUS_ERROR)`).

### 4.4 `FieldList`

```rust
pub struct FieldList {
    rows: Vec<FieldRow>,
    selected: usize,
    /// The row index scrolled to the top. `Cell` for the same reason
    /// `VerticalList::top_index` is one: reconciliation runs in `render`,
    /// which takes `&self`.
    top_index: Cell<usize>,
    focused: bool,
    style: RowStyle,
    on_activate_index: Option<Box<dyn Fn(usize) -> Action>>,
}
```

Builders: `new(rows)` · `on_activate_index(cb)` · `with_selected(i)` ·
`with_selected_identity(prev_key, prev_index)` · `with_focused(b)` ·
`with_scroll_top(i)` (see §4.7) · `with_leading_gutter()` (switches
`style` to `RowStyle::FIELD_GUTTERED`) · `selected_index()` · `selected_key()`.

`Widget` impl:

- `measure` -> `constraints`. It fills its area and manages overflow by
  scrolling, exactly as `VerticalList` does.
- `is_focusable` -> `!rows.is_empty()`.
- `on_intent` — `Up`/`Down`/`JumpBy` move the selection clamped across **all**
  rows, Readonly included. **Focus must never skip a row.** On press-edge-only
  input with no key repeat (`firmware/src/input.c:120`), a cursor that jumps by
  an amount the user cannot predict is a navigation failure, and on the codec
  picker the skipped row's text is the entire payload.
- `on_focus(Activated)` — **the activation gate lives here, in the widget**:

  ```rust
  FocusEvent::Activated => match self.rows.get(self.selected).map(|r| r.kind) {
      Some(FieldKind::Action) => self.on_activate_index.as_ref()
          .map_or(Action::None, |cb| cb(self.selected)),
      _ => Action::None,
  }
  ```

  Not in the caller's closure. A caller that forgets the check must not be able
  to make an inert row act.
- `render` — clip to `area`; `visible_rows = (area.height / row_h).max(1)`;
  `top = list::reconcile_top_index(self.top_index.get(), self.selected,
  visible_rows, self.rows.len())`; persist it; draw with the same
  fully-offscreen early-out `VerticalList::render` uses. The selection
  highlight (`theme::draw_selection`) draws on Readonly rows too — that is
  `znb.9`'s explicit requirement that focus be *visibly* resting on a dim row.
- `selected_index` / `selected_key` — overridden, same contract as
  `VerticalList`.
- `chrome_contribution` — returns **only** `a`:
  `Action` row -> `ButtonLabel::Live(row.activate_label or "open")`;
  `Readonly` row -> `ButtonLabel::Inert`. This makes the design's rule 2 ("an
  unlabelled A does nothing, so a mispress is always free") structural rather
  than remembered. A page wrapper that also wants `title`/`x` merges its own
  fields over this one's result.
- `redraw_after` — **not implemented; the default `None` is correct and
  deliberate.** Nothing in a field list is time-driven. See §6.

### 4.5 Scrolling: new capability, zero new algorithm

`list::reconcile_top_index` (`list.rs:330`) is already `pub(crate)`, already
row-index-based and therefore resolution-independent, already covered by its own
test module (`list.rs:1060`), and its own doc comment already anticipates "other
views' per-field scrolling needs". `FieldList` calls it verbatim. The new code
is a `Cell<usize>`, the `visible_rows` division and the offscreen skip — about
ten lines, all of them copied from a tested call site.

Its doc comment's warning applies unchanged and must be obeyed: **the
reconciliation runs in `render`, never in `on_intent`**, because `on_intent`
does not know the viewport height.

`MenuList` does **not** get scrolling. Its doc comment at `menu.rs:270-277`
stays true because its two callers still have two rows each.

**The peek, and why the page needs a top gutter.** With `vertical_padding: 6`
and a 16px `helvR12` line the row is 28px, and 224px of content divides into
exactly 8 rows with nothing left over — so a 9th row would be invisible with no
hint that it exists. The device page's wrapper therefore insets its area by the
12px top gutter Uma's §5 band table already specifies, leaving 212px: 7 full
rows plus a 16px partial-row **scroll peek**, which is the same affordance
`list.rs:208` chose its `ROW_HEIGHT` to produce. Tier 1's 7 rows fill it; the
8th (LDAC quality) peeks and scrolls. That is the affordance, and it costs no
chrome. **No `readout` on this page** — the title bar is spoken for by the full
untruncated device name.

### 4.6 Label truncation: clip now, ellipsis later, list-wide when it comes

`draw_row` today does no truncation at all and relies on the outer clip, which
means a long label runs *underneath* a right-aligned value. Fix it in the
primitive: measure the value first, then draw the label into a sub-rectangle
clipped at `value_left_edge - LABEL_VALUE_GAP` (`GAP = 8`).

**Clip, do not ellipsise.** An ellipsis needs a measure-per-prefix loop, which
is O(label) font measurements per row per frame on a device with a real frame
budget, and this render core has already retired one character-walking text hack
(`widget.rs:208-211`, "the retired character-skip marquee"). Every label on this
page is a constant we chose and none overflows; the clip is a safety net, not a
feature. If a real label ever overflows, an ellipsis belongs in `draw_row` where
every list gets it at once — which is precisely the payoff of one drawing path.
Flagged to Uma in §9.

### 4.7 A defect I found in the existing code, and it will bite this page hard

`App::rebuild_root` (`app.rs:1149`) replaces the Devices screen with a freshly
built `VerticalList` on **every** model change. `with_selected_identity` carries
the *selection* forward; **nothing carries `top_index`**, which is
`Cell::new(0)` on the new list. So on any scrolled list, an unrelated event
resets the scroll to 0 and `reconcile_top_index` then re-lands the selected row
at the *bottom* viewport edge rather than where the user left it. The list
visibly jumps.

It is latent on Devices today (4 rows fit, so nothing scrolls in practice). It
will **not** be latent on the device page, which has 7-9 rows and rebuilds on
every codec / A2DP / link event — i.e. on exactly the events the user is sitting
there watching.

**Fix, in scope for this work:**

```rust
// core/src/render/widget.rs, alongside `selected_index` / `selected_key`
/// This widget's scroll-top row index, if it scrolls. Exists for the same
/// reason `selected_index`/`selected_key` do: a caller that rebuilds a
/// screen from live model state must carry the user's viewport forward,
/// not just their cursor.
fn scroll_top(&self) -> Option<usize> { None }
```

Implemented by `FieldList` and `VerticalList`, forwarded by every wrapper
(`DevicesListView`, the new `DeviceDetailView`), threaded through
`Navigator::scroll_top_at(depth)` next to the existing `selected_key_at` /
`selected_index_at`, and consumed via `FieldList::with_scroll_top` /
`VerticalList::with_scroll_top`. This fixes Devices at the same time, for free.

**Wrappers must forward it.** This is the same failure mode as `pico-link-vxc`'s
D2 (`redraw_after` swallowed by a composite): a default that silently returns
`None` in a wrapper looks like "no scroll" instead of "I forgot".

---

## 5. Tests that make the arithmetic self-checking

Ruby writes these; Tess verifies the screenshots. Named because the numbers in
Uma's document are predictions and these turn them into assertions.

**Primitive / metrics**
1. `row_height(&RowStyle::MENU, &font::value())` equals the pre-change
   `row_height()` value. (Guards the port.)
2. `row_height(&RowStyle::FIELD, ..) < row_height(&RowStyle::MENU, ..)`.
3. **Peek invariant:** with the device page's available height
   (`224 - 12` top gutter), `visible_rows * field_row_height < available`, so a
   row below the fold always shows a sliver. Fails loudly if anyone retunes
   `vertical_padding`.
4. Tier-1 fit: 7 rows are fully visible without scrolling.
5. `RowStyle::FIELD.caret_right_margin < RowStyle::FIELD.value_right_margin`
   (the "the caret never moves the value" invariant, as a type-level assert).

**Behaviour**
6. `A` on a `Readonly` row returns `Action::None`, *even with a callback
   registered that would panic if called*. (`znb.9` + `znb.13`.)
7. Focus traversal **includes** `Readonly` rows — `Down` from row 0 of a list
   whose rows 1..5 are all `Readonly` lands on row 1, not row 6.
8. A caret is drawn only on a focused `Action` row: 3 render cases (Action
   focused / Action unfocused / Readonly focused) with a pixel assertion in the
   caret slot.
9. A value is drawn on an unfocused row (the `Trailing::Label` semantics that
   survive from `menu.rs:55-60`).
10. `ValueFont::Small` measures narrower than `Normal` for the 17-char address
    string, and the address row's ink fits inside `R - L = 182`. (The measured
    overflow Uma flagged, closed as a test.)
11. `with_selected_identity` carries selection by key across a rebuild that
    inserts/removes other rows.
12. **Scroll carry-forward:** scroll to the last row, rebuild the screen from an
    unrelated model event, assert `scroll_top` is unchanged. (§4.7.)
13. Leading gutter: labels start at the same x whether or not the row carries a
    glyph.

**Freshness (the dirty gate)**
14. Every new screen builder is added to `freshness_cases()` in `app.rs:1759`
    as `Freshness::Static`: device page connected / disconnected / pinned-and-
    fell-back / nameless, codec picker, quality picker. An entry missing here is
    an entry the gate has no proof about — that is the table's stated contract.

---

## 6. The dirty-gate hazards this design must clear (and does)

The FFI blit gate (`pl_ui_dirty`, `pico-link-vxc`) means a widget that changes
without dirtying now silently **freezes**. Three checks:

1. **`FieldList` requests no `redraw_after`.** Nothing on it is time-driven; the
   default `None` is the honest answer and the freshness table proves it.
2. **`top_index` is mutated during `render`, and that is safe.** It is derived
   purely from `selected`, which only ever changes via `on_intent` — i.e.
   through the input path that already sets `dirty`. The reconciliation is an
   idempotent clamp (`list.rs:298-306`), so running it inside the same frame the
   selection changed produces the correct scroll in that frame. No redraw
   request is needed and none must be added.
3. **The real hazard is upstream and belongs to Ada/Ruby, not to this widget:**
   every new `PlEvent` that feeds a device-page row (codec, sample rate, A2DP
   state, USB-in format, pin-not-honoured) **must mark the app dirty**. A row
   whose data arrives without dirtying will show a stale value indefinitely,
   which on *this* page is indistinguishable from the fault it exists to
   diagnose. Say so in the FFI bead.

---

## 7. Hacks to retire — do not port these forward

**7.1 The BACKGROUND slot clear (`hero.rs:363`).** Uma's §5 asks the field
list's value slot to be "cleared to BACKGROUND first ... so `990 kbps` -> `SBC`
never leaves debris". **Declined.** `Navigator::render` clears the entire
framebuffer to `palette::BACKGROUND` on every frame (`navigator.rs:307`), and
its own comment at `navigator.rs:421` records that it was added *because* of the
ghosting bug the per-slot clear was defending against. The slot clear in
`hero.rs` is the older workaround, now redundant, and porting it into a
per-row-per-frame path would spend a fill on every row of every field list to
defend against a condition that cannot occur. **File a small follow-up to delete
the `hero.rs` one too** — a redundant defence that looks load-bearing is how the
next person concludes the frame is *not* cleared.

**7.2 `menu.rs`'s margin justifications are stale.** `TEXT_LEFT_MARGIN = 8`
(`menu.rs:139-143`) is documented as matching "a detail view's field-row left
margin" — that detail view was the predecessor project's and no longer exists.
The number is now unanchored, and it is 4px off the alignment grid the whole
product moved to. Keep the *number* as the `MENU` preset (changing it is a
visual change, out of scope here) but **delete the false rationale** and replace
it with "legacy metrics, pre-alignment-grid; see §7.3".

**7.3 The alignment grid is being applied screen by screen, and the two
`MenuList` screens still violate it.** Home's menu face and `ConfirmView` both
draw at 8/6 while everything else has moved to 12/12. That is a real, visible
inconsistency and it is *not* this work's job — but it must be a filed bead, not
a thing everyone silently notices. §8 item 6.

**7.4 `Trailing`'s Label-xor-Caret exclusivity.** An artefact of the single
toggle-row case it was written for, being cited as a design constraint (§8 item
2). Deleted, per §3.2.

**7.5 "A row beyond the viewport is simply not drawn"** (`menu.rs:274-277`).
Documented as acceptable when every caller had two rows. Ported forward into a
7-9 row page it becomes the design of record's "a missing row is
indistinguishable from a layout bug", arriving by default. `FieldList` scrolls;
this acceptance does not travel with it.

**7.6 `row_height()`'s hardcoded `font::value()`** (`menu.rs:172`) — it computes
a height for a font the caller may not be using. Fixed by §3.4 taking the font
as a parameter.

---

## 8. Migration plan — five steps, each independently reviewable

Steps 1 and 2 are **pure `core/`, no FFI, no firmware, and can start
immediately** — they do not wait on Ada's seam work. That separation is the
main practical payoff of splitting the primitive from the page.

1. **Row primitive** -> Ruby. `RowStyle` / `RowValue` / `RowTrailing`, the
   leading gutter, the per-value font, caret-and-value composition, the label
   clip (§4.6), `row_height(style, font)`; delete `Trailing`; port `MenuList` to
   `RowStyle::MENU`. **No new screen, no behaviour change.**
   *Acceptance: existing `menu`/`confirm`/`home` tests pass unchanged, and the
   committed Home-menu-face and confirm screenshots are BYTE-IDENTICAL. If they
   are not, the port is wrong — do not adjust the screenshots.*

2. **`FieldList`** -> Ruby. `core/src/render/fields.rs` per §4, plus the
   `Widget::scroll_top` seam and its `VerticalList` implementation and
   `Navigator` accessor (§4.7). Unit tests 1-13 from §5. **Still no screen.**

3. **The device page** -> Ruby, *needs Ada's events first for the live rows*.
   `build_device_detail_screen` (`app.rs:995`) takes an address rather than a
   title; a `DeviceDetailView` wrapper in the exact shape of `DevicesListView`
   (`app.rs:873`) applies the 12px top gutter, intercepts `ShortcutX` for
   `drop`/`link`, contributes the untruncated name as `title`, and forwards
   `selected_index` / `selected_key` / `scroll_top` / `redraw_after`. Freshness
   cases added (§5 item 14). Rows dashed/absent exactly per the page design's
   §3 — `SIGNAL` **absent**, `USB IN` **dashed**.
   *A row whose event does not exist yet ships dashed, never faked, never
   omitted.*

4. **Codec picker + LDAC quality picker** -> Ruby. `FieldList` with
   `with_leading_gutter()`; unavailable entries are `FieldKind::Readonly` with
   the reason as their value; `Unknown` entries stay `FieldKind::Action`
   (`znb.9`'s three-state correction). Screenshot with focus resting on a dim
   row.

5. **Devices `X` rebinding + one `build_forget_confirm`** -> Ruby, landing in
   the same change as step 3 (otherwise Devices briefly has no route to the
   destination that replaced its X).

6. **Follow-up beads, filed now, not done now:** (a) grid conformance for the
   two `MenuList` screens (§7.3); (b) delete `hero.rs`'s redundant slot clear
   (§7.1); (c) fold `MenuList` into `FieldList` *if and only if* a fourth
   single-line-row container is ever proposed (§2).

---

## 9. What this closes, and what each bead keeps

**Neither bead is closed outright by this ruling, and I am not going to claim
otherwise.** What is closed is the *widget capability they share*.

**`pico-link-znb.9` (E6):** its **Piece 2** — "a `MenuItem` that is DIMMED,
FOCUSABLE, and NON-ACTIVATABLE, with its reason rendered inline" — is fully
answered by `FieldKind::Readonly` plus `RowValue` (the reason goes in the value
slot, **not** a sublabel, per Uma's correction and because a sublabel doubles
row height on the axis that binds). Delivered by migration step 2.
*Still open in `znb.9`:* Piece 1, the three-state `(codec, availability, reason)`
data over the `PlEvent` surface (Ada), and the picker screen itself (step 4).

**`pico-link-znb.13` (E11):** its **reuse mandate is discharged by this
document** — the answer to "report which you chose and why BEFORE implementing"
is §2 and §3, and the answer to "a new widget here is a failure unless you can
say what specifically neither can express" is: the activation gate, the
scrolling viewport and the caret-plus-value composition, of which the last two
are also unavailable in `VerticalList` and the first is unavailable in both.
Delivered by steps 1-2.
*Still open in `znb.13`:* the device-page screen itself — rows, values, degraded
states, the Forget route — which is migration step 3 and belongs to `ay0`'s page
child.

Recommendation to the orchestrator: create `ay0` children for steps 1, 2, 3+5
and 4; mark `znb.9` and `znb.13` as *related* to the step-2 bead and let each
keep only its non-widget half. Do not close either until its own half lands.

---

## 10. Open questions

1. **For Uma — the A-rail word per Action row.** The page design says A is
   "(per row)" but never gives the words. `FieldRow::with_activate_label` exists
   for exactly this; the default I have specified is `"open"`. Proposed:
   `CODEC` -> `pick`, `LDAC QUALITY` -> `pick`, `Forget this device` ->
   `forget`. All within `rail.rs`'s 5-char budget. Confirm or replace.
2. **For Uma — ellipsis vs clip on an over-long label (§4.6).** I have ruled
   clip, on frame-budget grounds and because no label on this page can overflow.
   If you want a true ellipsis it is a separate, list-wide change to `draw_row`
   that benefits `VerticalList` too — say so and it gets its own bead.
3. **For Uma — caret slot at zoom (§4.2).** The caret lives in the 12px right
   gutter, ~4px clear of the rail. That is tight. It needs an eyes-on zoomed
   screenshot before it is called done; the fallback (lower
   `caret_right_margin`) is specified and does not touch the value column.
4. **For Ada — the dirty obligation (§6.3).** Every new device-page event must
   mark the app dirty. Not a widget concern, but the page freezes silently if it
   is missed, so it should be stated in the FFI bead rather than assumed.
5. **Not decided here, deliberately:** whether `MenuList` eventually becomes a
   `FieldList` preset. That call belongs to whoever proposes a fourth
   single-line-row container, with the evidence they will have and I do not.
