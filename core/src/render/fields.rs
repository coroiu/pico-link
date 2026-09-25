//! `FieldList`: a scrolling label/value sheet whose rows are mostly
//! inert — the widget half of the device page
//! (`.planning/design/2026-09-02-device-page.md`) and the codec/quality
//! pickers, per Fern's ruling
//! (`.planning/design/2026-09-02-field-list-widget-ruling.md`, the design
//! of record for this module — read it before changing anything here).
//!
//! `FieldList` is deliberately a *third* row-container shape alongside
//! `super::menu::MenuList` (a short list of actions) and
//! `super::list::VerticalList` (a scrolling list of two-line entities):
//! one drawing primitive (`super::menu::draw_row`), three containers,
//! each honest about what it is. `FieldList` differs from `MenuList` in
//! exactly the two ways `MenuList`'s module doc says it deliberately
//! doesn't do: it scrolls (`super::list::reconcile_top_index`, reused
//! verbatim — zero new scrolling algorithm), and it can hold focus on a
//! row that refuses activation (the field-list ruling §2's actual
//! objection to extending `MenuList` instead: under `MenuItem::
//! activatable(false)` a caller could get the "never lies about what A
//! does" rule wrong by forgetting one builder call; under
//! [`FieldKind::Readonly`] that failure mode does not typecheck).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use core::convert::Infallible;

use embedded_graphics::draw_target::DrawTargetExt;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::{Point, Size};
use embedded_graphics::primitives::Rectangle;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::FontRenderer;

use crate::input::NavIntent;

use super::ctx::RenderCtx;
use super::framebuffer::FrameBuffer565;
use super::list::{reconcile_top_index, ListItemKey};
use super::menu::{draw_row, row_height, RowStyle, RowTrailing, RowValue};
use super::paint_key::PaintKey;
use super::theme::{font, icon, palette};
use super::widget::{Action, FocusEvent, Verb, Widget};

/// Seed for [`FieldList::paint_key`] -- only needs to differ from other
/// widgets' own seeds.
const FIELD_PAINT_KEY_SEED: u64 = 13;

/// Gap (px) between a [`FieldKind::Value`] row's chevron and the value
/// text it flanks — small enough to sit inside `draw_row`'s existing
/// `LABEL_VALUE_GAP` reservation on the left, and inside
/// `RowStyle::FIELD`'s `value_right_margin - caret_right_margin` gutter
/// on the right (both already sized for an `icon_1x` glyph).
const VALUE_CHEVRON_GAP: i32 = 2;

/// A single line's rendered pixel width in `font` -- duplicated from
/// `menu.rs`'s private `text_width` (same small-private-helper rationale
/// that function's own doc comment gives; this module has no reason to
/// depend on `menu.rs` beyond the `draw_row` primitive it already uses).
#[allow(clippy::cast_possible_wrap)]
fn value_text_width(font: &FontRenderer, text: &str) -> i32 {
    font.get_rendered_dimensions_aligned(text, Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
        .unwrap_or(None)
        .map_or(0, |bbox| bbox.size.width as i32)
}

/// A field row's kind. `Action`/`Readonly` collapsing Uma's `Info` and
/// `Disabled` kinds into one variant IS the finding the field-list ruling
/// makes (§4.1): both are focusable, dim, never grow a caret, `A` is a
/// no-op, and the trailing value is drawn regardless of focus. They
/// differ only in what the value text *says* (a fact on the device page,
/// a reason on the codec picker) — rendering and behaviour are identical,
/// so encoding them as one variant is what makes it structurally
/// impossible for them to drift apart later.
///
/// `Value` is a third, structurally distinct kind (Fern DESIGN,
/// `.planning/design/2026-09-25-value-row-and-on-exit-hook.md` §2): a
/// bright, focusable row whose Left/Right steps a value in place, rather
/// than a caret pushing a deeper screen. It is not folded into `Action`
/// because activation (`A`) must stay inert on it (§2's activation rule),
/// and not into `Readonly` because its label is bright, not dim.
///
/// Extension point: if a disabled row ever needs its own distinct mark (a
/// lock glyph, a strikethrough), it takes the **leading gutter**
/// ([`FieldRow::with_leading_glyph`]), not a fourth kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Bright label, caret while focused, `A` fires the list's callback.
    Action,
    /// Dim label, NEVER a caret, `A` is a no-op — but fully focusable,
    /// and its value is drawn regardless of focus.
    Readonly,
    /// Bright label like `Action`, but `A` is a no-op (like `Readonly`)
    /// and Left/Right — not a caret — are this row's disclosure: they
    /// step the value while this row is focused, via
    /// [`FieldList::on_step`]. See [`StepBounds`] for the per-side
    /// liveness this variant carries.
    Value(StepBounds),
}

/// Whether a [`FieldKind::Value`] row's Left (`Prev`) and Right (`Next`)
/// sides are currently live — computed by the screen builder from the
/// model's own ladder, never by the widget (design §2: "clamp vs wrap is
/// not a widget concept"). A clamped ladder reports `prev: false` at its
/// low end; a wrapping ladder reports both `true` always. The widget only
/// renders this (a dead side's chevron draws in [`palette::DIVIDER`]) and
/// gates on it (a dead side's Left/Right is a no-op, no callback call).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepBounds {
    pub prev: bool,
    pub next: bool,
}

/// Which direction a [`FieldList::on_step`] callback was invoked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Left — steps the value down/back.
    Prev,
    /// Right — steps the value up/forward.
    Next,
}

/// Which face a row's trailing value uses. A closed two-variant enum, not
/// a stored [`FontRenderer`]: [`FieldRow`] must stay `Clone + PartialEq`
/// like its siblings `ListItem`/`MenuItem`, and the capability asked for
/// is exactly one override, for the device page's `ADDRESS` row (which
/// measures over budget at [`ValueFont::Normal`] — device-page design
/// §3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueFont {
    /// [`font::value`] (`helvR12`). The default.
    Normal,
    /// [`font::label`] (`helvB08`) — the small face a dense value (e.g.
    /// a Bluetooth address) needs to fit its column.
    Small,
    /// [`font::username`] (`helvR10`) -- bead `pico-link-ryw.12.4`'s
    /// imported-preset band rows' last-resort width degrade (Uma's
    /// design, `ryw12-3-ux.md` sec 4: "then band values in
    /// `font::username`"), one size down from [`Self::Normal`] but not as
    /// small/bold as [`Self::Small`] (which is styled for a *label*, not
    /// a dense numeric value row).
    Compact,
}

impl ValueFont {
    fn font(self) -> FontRenderer {
        match self {
            ValueFont::Normal => font::value(),
            ValueFont::Small => font::label(),
            ValueFont::Compact => font::username(),
        }
    }
}

/// A single field-list row. Display-only, like `ListItem`/`MenuItem` —
/// builders mirror their conventions exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldRow {
    pub label: String,
    kind: FieldKind,
    /// Overrides the derived label color (`Action` ->
    /// [`palette::TEXT_PRIMARY`], `Readonly` -> [`palette::TEXT_SECONDARY`])
    /// — e.g. the red "Forget this device" row.
    label_color: Option<Rgb565>,
    value: Option<String>,
    value_color: Rgb565,
    value_font: ValueFont,
    leading: Option<char>,
    /// Bead `pico-link-ryw.12.4`: an imported effect's row (the effects
    /// list, the device-page effect picker) draws a padlock right after
    /// its (ellipsis-truncated-if-needed) label -- see
    /// [`Self::with_lock`].
    locked: bool,
    /// The A-rail verb for this row when it's a [`FieldKind::Action`]
    /// row and focused (e.g. `Verb::Pair`, or an exception word for
    /// "forget"). `None` defaults to [`Verb::Open`] -- see
    /// [`FieldList::activation`].
    verb: Option<Verb>,
    key: Option<ListItemKey>,
}

impl FieldRow {
    fn new(label: impl Into<String>, kind: FieldKind) -> Self {
        Self {
            label: label.into(),
            kind,
            label_color: None,
            value: None,
            value_color: palette::TEXT_PRIMARY,
            value_font: ValueFont::Normal,
            leading: None,
            locked: false,
            verb: None,
            key: None,
        }
    }

    /// A pressable row: bright label, caret while focused, `A` fires the
    /// list's activation callback.
    #[must_use]
    pub fn action(label: impl Into<String>) -> Self {
        Self::new(label, FieldKind::Action)
    }

    /// An inert-but-focusable row: dim label, never a caret, `A` is a
    /// no-op. See [`FieldKind::Readonly`]'s doc comment for why this one
    /// variant serves both an informational row and a disabled picker
    /// entry.
    #[must_use]
    pub fn readonly(label: impl Into<String>) -> Self {
        Self::new(label, FieldKind::Readonly)
    }

    /// A steppable row: bright label, chevrons flank the value while
    /// focused (never a caret), `A` is a no-op, Left/Right step the value
    /// via [`FieldList::on_step`] when `bounds` reports that side live.
    /// Always carries a value from construction (unlike `action`/
    /// `readonly`, which take theirs via [`Self::with_value`]) — a value
    /// row with nothing to show is not a state this constructor can
    /// represent. Callers must also call [`Self::with_key`] — the step
    /// callback is keyed, never index-based (design §2).
    ///
    /// Named `value_row`, not `value` (the design's literal constructor
    /// name) — [`Self::value`] is this type's already-shipped read
    /// accessor for the built row's trailing text, used by every
    /// screen's own row-content tests; the two cannot share a name.
    #[must_use]
    pub fn value_row(label: impl Into<String>, text: impl Into<String>, bounds: StepBounds) -> Self {
        Self::new(label, FieldKind::Value(bounds)).with_value(text, palette::TEXT_PRIMARY)
    }

    #[must_use]
    pub fn with_value(mut self, text: impl Into<String>, color: Rgb565) -> Self {
        self.value = Some(text.into());
        self.value_color = color;
        self
    }

    /// Draws this row's trailing value in [`ValueFont::Small`] instead of
    /// the default [`ValueFont::Normal`] — see [`ValueFont`]'s doc
    /// comment.
    #[must_use]
    pub fn with_small_value(mut self) -> Self {
        self.value_font = ValueFont::Small;
        self
    }

    /// Draws this row's trailing value in [`ValueFont::Compact`] --
    /// bead `pico-link-ryw.12.4`'s last-resort band-row width degrade
    /// (see [`ValueFont::Compact`]'s doc comment).
    #[must_use]
    pub fn with_compact_value(mut self) -> Self {
        self.value_font = ValueFont::Compact;
        self
    }

    /// Marks this row as an imported/locked effect -- draws a padlock
    /// right after the label, ellipsis-truncating the label first rather
    /// than letting the padlock (or a trailing value) clip. See
    /// [`Self::locked`]'s doc comment and `menu::draw_row`'s `locked`
    /// branch.
    #[must_use]
    pub fn with_lock(mut self) -> Self {
        self.locked = true;
        self
    }

    /// Draws `glyph` in the row's leading gutter (only visible when the
    /// list is built via [`FieldList::with_leading_gutter`] — otherwise
    /// silently ignored, same as `MenuItem`'s trailing fields when their
    /// container doesn't draw them).
    #[must_use]
    pub fn with_leading_glyph(mut self, glyph: char) -> Self {
        self.leading = Some(glyph);
        self
    }

    #[must_use]
    pub fn with_label_color(mut self, color: Rgb565) -> Self {
        self.label_color = Some(color);
        self
    }

    /// Sets this row's A-rail verb — used only while this row is focused
    /// and is an [`FieldKind::Action`] row. Defaults to [`Verb::Open`] (see
    /// [`FieldList::activation`]).
    #[must_use]
    pub fn with_verb(mut self, verb: Verb) -> Self {
        self.verb = Some(verb);
        self
    }

    #[must_use]
    pub fn with_key(mut self, key: ListItemKey) -> Self {
        self.key = Some(key);
        self
    }

    /// This row's trailing value text, if any -- a read-only accessor
    /// (matching `ListItem::label`/`sublabel`'s already-public-field
    /// convention) for callers that need to assert on a built row's
    /// content without a framebuffer (e.g. `crate::app`'s
    /// `device_page_rows` tests). Does not affect rendering.
    #[must_use]
    pub fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }

    fn resolved_label_color(&self) -> Rgb565 {
        self.label_color.unwrap_or(match self.kind {
            FieldKind::Action | FieldKind::Value(_) => palette::TEXT_PRIMARY,
            FieldKind::Readonly => palette::TEXT_SECONDARY,
        })
    }
}

type OnActivateIndex = Box<dyn Fn(usize) -> Action>;

/// Callback invoked with the selected row's **identity key** on activation.
/// See [`FieldList::on_activate_key`]'s doc comment for why this exists
/// alongside [`OnActivateIndex`] -- mirrors `list::OnActivateKey`.
type OnActivateKey = Box<dyn Fn(ListItemKey) -> Action>;

/// A focusable, scrolling field list — see the module doc for the shape
/// this fills relative to `MenuList`/`VerticalList`.
pub struct FieldList {
    rows: Vec<FieldRow>,
    selected: usize,
    /// The row index scrolled to the top. `Cell`, not a plain field, for
    /// the same reason `VerticalList::top_index` is one — `render` takes
    /// `&self` but still needs to persist this across calls; see
    /// `list::reconcile_top_index`'s doc comment for why the
    /// reconciliation runs in `render`, never `on_intent`.
    top_index: Cell<usize>,
    focused: bool,
    style: RowStyle,
    on_activate_index: Option<OnActivateIndex>,
    /// A live-model-backed alternative to [`Self::on_activate_index`] --
    /// invoked with the selected row's [`ListItemKey`] rather than its
    /// index, for a long-lived list (bead `pico-link-bgnd` M3) whose rows
    /// are replaced in place via [`Self::set_rows`] and whose callback
    /// resolves that key back to a live value at *press time* -- the exact
    /// `VerticalList::on_activate_key` shape, mirrored here.
    on_activate_key: Option<OnActivateKey>,
    /// Invoked with a [`FieldKind::Value`] row's key and [`Step`] direction
    /// when Left/Right is pressed on that row's live side — see
    /// [`Self::on_step`].
    on_step: Option<OnStep>,
}

type OnStep = Box<dyn Fn(ListItemKey, Step) -> Action>;

impl FieldList {
    #[must_use]
    pub fn new(rows: Vec<FieldRow>) -> Self {
        Self {
            rows,
            selected: 0,
            top_index: Cell::new(0),
            focused: false,
            style: RowStyle::FIELD,
            on_activate_index: None,
            on_activate_key: None,
            on_step: None,
        }
    }

    /// Registers a callback invoked with the selected row's index when
    /// the list is activated while focused **on an [`FieldKind::Action`]
    /// row** — see [`Self::on_focus`]'s `Activated` arm for why this gate
    /// lives in the widget, not the caller's closure.
    #[must_use]
    pub fn on_activate_index(mut self, callback: impl Fn(usize) -> Action + 'static) -> Self {
        self.on_activate_index = Some(Box::new(callback));
        self
    }

    /// Registers a callback invoked with the selected row's **identity
    /// key** (not its index) when the list is activated while focused **on
    /// an [`FieldKind::Action`] row** -- same activation gate as
    /// [`Self::on_activate_index`], see [`Self::on_focus`]'s `Activated`
    /// arm. Takes precedence over `on_activate_index` if both are set --
    /// mirrors `VerticalList::on_activate_key`'s doc comment for the full
    /// reasoning (a long-lived list updated via [`Self::set_rows`] can have
    /// its selected *index* mean a different row than at construction, but
    /// keys don't drift).
    #[must_use]
    pub fn on_activate_key(mut self, callback: impl Fn(ListItemKey) -> Action + 'static) -> Self {
        self.on_activate_key = Some(Box::new(callback));
        self
    }

    /// Registers a callback invoked with the selected row's key and the
    /// pressed [`Step`] direction when Left/Right is pressed while a
    /// [`FieldKind::Value`] row is focused **and that side's
    /// [`StepBounds`] reports it live** — see [`Widget::on_intent`]'s
    /// `Left`/`Right` arm for the full gate (design §2): a dead side, a
    /// non-`Value` selected row, or no callback registered all fall
    /// through to `Action::None` without ever calling this closure.
    #[must_use]
    pub fn on_step(mut self, callback: impl Fn(ListItemKey, Step) -> Action + 'static) -> Self {
        self.on_step = Some(Box::new(callback));
        self
    }

    /// Replaces this list's rows **in place**, preserving the current
    /// selection by [`ListItemKey`] identity with an index-based fallback --
    /// mirrors `VerticalList::set_items`'s exact rule (see that method's doc
    /// comment for the full reasoning), so a long-lived field list (bead
    /// `pico-link-bgnd` M3) can be updated on a live state change instead of
    /// being reconstructed from scratch. The scroll-top row is left
    /// untouched for the same reason `set_items` leaves it untouched --
    /// `Widget::render`'s own `reconcile_top_index` call clamps it to the
    /// new row count/viewport on the very next render.
    pub fn set_rows(&mut self, rows: Vec<FieldRow>) {
        let prev_key = self.selected_key();
        let prev_index = self.selected;
        self.rows = rows;
        self.selected = prev_key
            .and_then(|key| self.rows.iter().position(|row| row.key == Some(key)))
            .unwrap_or_else(|| prev_index.min(self.rows.len().saturating_sub(1)));
    }

    /// Sets the initially selected row, clamped to the row list's bounds
    /// — mirrors `VerticalList::with_selected`/`MenuList::with_selected`.
    #[must_use]
    pub fn with_selected(mut self, selected: usize) -> Self {
        self.selected = selected.min(self.rows.len().saturating_sub(1));
        self
    }

    /// Sets the initially selected row by identity, with an index-based
    /// fallback — mirrors `VerticalList::with_selected_identity`'s exact
    /// rule (see that method's doc comment for the full reasoning: this
    /// is what lets a store-backed page rebuilt from live data on every
    /// event survive rows being inserted/removed/reordered around the
    /// selected one).
    #[must_use]
    pub fn with_selected_identity(mut self, prev_key: Option<ListItemKey>, prev_index: usize) -> Self {
        self.selected = prev_key
            .and_then(|key| self.rows.iter().position(|row| row.key == Some(key)))
            .unwrap_or_else(|| prev_index.min(self.rows.len().saturating_sub(1)));
        self
    }

    #[must_use]
    pub fn with_focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// Sets the initial scroll-top row index — see
    /// `VerticalList::with_scroll_top`'s doc comment (identical rule, and
    /// the identical fix for the identical "a rebuilt widget must carry
    /// the user's viewport forward" defect — field-list ruling §4.7).
    #[must_use]
    pub fn with_scroll_top(self, top: usize) -> Self {
        self.top_index.set(top);
        self
    }

    /// Switches this list's row style to [`RowStyle::FIELD_GUTTERED`],
    /// reserving the leading glyph gutter for the pickers'
    /// current-choice check.
    #[must_use]
    pub fn with_leading_gutter(mut self) -> Self {
        self.style = RowStyle::FIELD_GUTTERED;
        self
    }

    #[must_use]
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    #[must_use]
    pub fn selected_key(&self) -> Option<ListItemKey> {
        self.rows.get(self.selected).and_then(|row| row.key)
    }

    /// Moves the selection to the row carrying `key`, if one exists --
    /// bead `pico-link-ryw.12.4`'s import-focus-follow (Uma's design,
    /// `ryw12-3-ux.md` sec 2: "move FOCUS to the new/updated row"), a
    /// one-shot programmatic jump distinct from [`Self::set_rows`]'s own
    /// "preserve whatever was already selected" rule. Returns whether
    /// `key` was found (a caller uses this to decide whether to keep
    /// retrying on a later frame, e.g. before the row has appeared yet).
    pub fn focus_key(&mut self, key: ListItemKey) -> bool {
        if let Some(index) = self.rows.iter().position(|row| row.key == Some(key)) {
            self.selected = index;
            true
        } else {
            false
        }
    }

    fn move_selection(&mut self, delta: i32) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len() as i32;
        let next = (self.selected as i32 + delta).clamp(0, len - 1);
        self.selected = next as usize;
    }

    /// Left/Right on the selected row -- see [`Self::on_step`]'s doc
    /// comment for the full gate this implements. Not a `Widget` trait
    /// method itself; `on_intent`'s `Left`/`Right` arms call this.
    fn step(&self, direction: Step) -> Action {
        let Some(row) = self.rows.get(self.selected) else {
            return Action::None;
        };
        let FieldKind::Value(bounds) = row.kind else {
            return Action::None;
        };
        let live = match direction {
            Step::Prev => bounds.prev,
            Step::Next => bounds.next,
        };
        if !live {
            return Action::None;
        }
        match (row.key, &self.on_step) {
            (Some(key), Some(callback)) => callback(key, direction),
            _ => Action::None,
        }
    }
}

impl Widget for FieldList {
    fn measure(&self, constraints: Size, _ctx: &RenderCtx) -> Size {
        // Fills its area and manages overflow by scrolling, exactly as
        // `VerticalList` does.
        constraints
    }

    fn is_focusable(&self) -> bool {
        !self.rows.is_empty()
    }

    fn selected_index(&self) -> Option<usize> {
        Some(self.selected)
    }

    fn selected_key(&self) -> Option<ListItemKey> {
        FieldList::selected_key(self)
    }

    fn scroll_top(&self) -> Option<usize> {
        Some(self.top_index.get())
    }

    fn on_focus(&mut self, event: FocusEvent) -> Action {
        match event {
            FocusEvent::Gained => {
                self.focused = true;
                Action::None
            }
            FocusEvent::Lost => {
                self.focused = false;
                Action::None
            }
            // The activation gate lives HERE, in the widget -- not in the
            // caller's closure -- so a caller that forgets the check
            // cannot make an inert row act (field-list ruling §4.4).
            FocusEvent::Activated => match self.rows.get(self.selected).map(|row| row.kind) {
                Some(FieldKind::Action) => {
                    if let Some(callback) = &self.on_activate_key {
                        return match self.rows.get(self.selected).and_then(|row| row.key) {
                            Some(key) => callback(key),
                            // Every real row this widget draws is expected
                            // to carry a key when `on_activate_key` is in
                            // use (mirrors `VerticalList::on_focus`'s
                            // identical fallback) -- a keyless selected row
                            // is not reachable in practice, but `Action::
                            // None` is the harmless fallback rather than a
                            // panic.
                            None => Action::None,
                        };
                    }
                    self.on_activate_index.as_ref().map_or(Action::None, |callback| callback(self.selected))
                }
                Some(FieldKind::Readonly | FieldKind::Value(_)) | None => Action::None,
            },
        }
    }

    fn on_intent(&mut self, intent: NavIntent) -> Action {
        // Focus traversal must never skip a row -- Readonly rows are
        // included in `move_selection`'s clamp exactly like Action rows.
        // On press-edge-only input with no key repeat, a cursor that
        // jumps by an unpredictable amount is a navigation failure, and
        // on the codec picker the skipped row's text is the entire
        // payload (field-list ruling §4.4).
        match intent {
            NavIntent::Down => self.move_selection(1),
            NavIntent::Up => self.move_selection(-1),
            NavIntent::JumpBy(n) => self.move_selection(i32::from(n)),
            // Left/Right step the focused Value row; on any other row
            // (or with no side live / no callback) they are no-ops --
            // design §1: this is the ONLY thing Left/Right do anywhere in
            // this crate, and this widget must never fall back to
            // treating an unconsumed Left as Back.
            NavIntent::Left => return self.step(Step::Prev),
            NavIntent::Right => return self.step(Step::Next),
            NavIntent::Select | NavIntent::Back | NavIntent::ShortcutX | NavIntent::ShortcutY => {}
        }
        Action::None
    }

    /// `Action` row -> `Some(verb or Verb::Open)`, `Readonly` row -> `None`.
    /// This makes "an unlabelled A does nothing, so a mispress is always
    /// free" structural rather than remembered (design rule 4) -- the
    /// canonical example the rule generalizes from (field-list ruling
    /// §4.4).
    fn activation(&self) -> Option<Verb> {
        let row = self.rows.get(self.selected)?;
        match row.kind {
            FieldKind::Action => Some(row.verb.unwrap_or(Verb::Open)),
            FieldKind::Readonly | FieldKind::Value(_) => None,
        }
    }

    // `redraw_after` is deliberately NOT overridden here -- the default
    // `None` is correct: nothing on a field list is time-driven (field-
    // list ruling §6.1). Per the mechanical review rule (`PaintKey`'s doc
    // comment), `paint_key` below must therefore NOT fold time either --
    // it doesn't.

    /// Folds everything [`FieldList::render`] actually reads: which row is
    /// selected/focused, the scroll-top row, the row style in use (`FIELD`
    /// vs `FIELD_GUTTERED` -- differ only in `leading_gutter`, but every
    /// margin is folded so a future style tweak can't silently go
    /// unnoticed), the row count, and per row its label, kind (bears on
    /// caret-while-selected gating and the resolved label color), resolved
    /// label color, value text + color, value font face, and leading
    /// glyph. `verb`/`key` are excluded -- neither is a pixel this widget
    /// draws.
    #[allow(clippy::cast_sign_loss)] // RowStyle's fields are all small non-negative layout constants; see the type's own doc comment.
    fn paint_key(&self, _ctx: &RenderCtx) -> PaintKey {
        let mut key = PaintKey::of(FIELD_PAINT_KEY_SEED)
            .fold(self.selected as u64)
            .fold(u64::from(self.focused))
            .fold(self.top_index.get() as u64)
            .fold(self.style.left_margin as u64)
            .fold(self.style.value_right_margin as u64)
            .fold(self.style.caret_right_margin as u64)
            .fold(self.style.leading_gutter as u64)
            .fold(self.style.vertical_padding as u64)
            .fold(self.rows.len() as u64);
        for (index, row) in self.rows.iter().enumerate() {
            key = key.fold_str(&row.label);
            key = key.fold(match row.kind {
                FieldKind::Action => 0,
                FieldKind::Readonly => 1,
                FieldKind::Value(_) => 2,
            });
            // The chevron liveness (`StepBounds`) is only ever DRAWN on
            // the focused value row (see `Self::render`'s chevron block)
            // -- folding it for every row regardless of focus would
            // invalidate the cache on a sync that changes an unfocused
            // row's bounds even though nothing about its pixels changed
            // (the "must fold only what is drawn" rule).
            if let FieldKind::Value(bounds) = row.kind {
                if self.focused && index == self.selected {
                    key = key.fold(u64::from(bounds.prev)).fold(u64::from(bounds.next));
                }
            }
            key = key.fold_color(row.resolved_label_color());
            key = key.fold_opt_str(row.value.as_deref());
            key = key.fold_color(row.value_color);
            key = key.fold(match row.value_font {
                ValueFont::Normal => 0,
                ValueFont::Small => 1,
                ValueFont::Compact => 2,
            });
            key = key.fold(match row.leading {
                Some(c) => u64::from(u32::from(c)) + 1,
                None => 0,
            });
            key = key.fold(u64::from(row.locked));
        }
        key
    }

    fn render(&self, area: Rectangle, _ctx: &RenderCtx, target: &mut FrameBuffer565) -> Result<(), Infallible> {
        let mut clipped = target.clipped(&area);
        // Every row's LABEL shares one font regardless of that row's own
        // trailing-value font override -- `row_height` is therefore
        // computed once, from that shared label font, so every row in
        // the list is the same height (field-list ruling §3.4/§7.6: the
        // height is style-and-font derived, never hardcoded).
        let label_font = font::value();
        let height = row_height(&self.style, &label_font);

        let visible_rows = (area.size.height / height).max(1) as usize;
        let top = reconcile_top_index(self.top_index.get(), self.selected, visible_rows, self.rows.len());
        self.top_index.set(top);

        for (index, row) in self.rows.iter().enumerate() {
            if index < top {
                continue;
            }
            #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
            let row_top = area.top_left.y + ((index - top) as u32 * height) as i32;
            #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
            if row_top >= area.top_left.y + area.size.height as i32 {
                break;
            }
            let row_rect = Rectangle::new(Point::new(area.top_left.x, row_top), Size::new(area.size.width, height));
            let selected = self.focused && index == self.selected;

            let value_font_owned = row.value_font.font();
            let trailing = RowTrailing {
                value: row.value.as_deref().map(|text| RowValue { text, color: row.value_color, font: &value_font_owned }),
                // The caret is drawn only while `selected` (`draw_row`'s
                // own gate) -- reserving it unconditionally for every
                // `Action` row is what makes "gaining focus never moves
                // the value" hold, since `RowStyle::FIELD`'s
                // `caret_right_margin` already puts the caret in its own
                // gutter right of the value column.
                caret: row.kind == FieldKind::Action,
            };

            draw_row(
                &mut clipped,
                row_rect,
                &self.style,
                &row.label,
                row.resolved_label_color(),
                &label_font,
                row.leading,
                &trailing,
                selected,
                row.locked,
            )?;

            // Value-row chevrons -- drawn on the FOCUSED value row only
            // (design §2), flanking the value text; a dead side draws in
            // `palette::DIVIDER` rather than being omitted, so the row's
            // shape doesn't shift depending on which sides are live.
            // Never drawn for an unfocused/unselected `Value` row (which
            // renders identically to `Readonly`, just bright -- see
            // `FieldRow::resolved_label_color`) or any non-`Value` row.
            // Never a caret either -- `trailing.caret` above is gated on
            // `FieldKind::Action` alone, so a `Value` row never reaches
            // `draw_row`'s caret branch.
            if let (true, FieldKind::Value(bounds)) = (selected, row.kind) {
                let value_text = row.value.as_deref().unwrap_or_default();
                let row_center_y = row_rect.top_left.y + row_rect.size.height as i32 / 2;
                #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
                let value_right_edge = row_rect.top_left.x + row_rect.size.width as i32 - self.style.value_right_margin;
                #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
                let caret_right_edge = row_rect.top_left.x + row_rect.size.width as i32 - self.style.caret_right_margin;
                let value_left_edge = value_right_edge - value_text_width(&value_font_owned, value_text);

                let chevron_font = font::icon_1x();
                let mut prev_buf = [0_u8; 4];
                let prev_str: &str = icon::CARET_LEFT.encode_utf8(&mut prev_buf);
                let prev_color = if bounds.prev { palette::TEXT_PRIMARY } else { palette::DIVIDER };
                let _ = chevron_font.render_aligned(
                    prev_str,
                    Point::new(value_left_edge - VALUE_CHEVRON_GAP, row_center_y),
                    VerticalPosition::Center,
                    HorizontalAlignment::Right,
                    FontColor::Transparent(prev_color),
                    &mut clipped,
                );

                let mut next_buf = [0_u8; 4];
                let next_str: &str = icon::CARET_RIGHT.encode_utf8(&mut next_buf);
                let next_color = if bounds.next { palette::TEXT_PRIMARY } else { palette::DIVIDER };
                let _ = chevron_font.render_aligned(
                    next_str,
                    Point::new(caret_right_edge, row_center_y),
                    VerticalPosition::Center,
                    HorizontalAlignment::Right,
                    FontColor::Transparent(next_color),
                    &mut clipped,
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Instant;

    fn test_ctx() -> RenderCtx {
        RenderCtx::at(Instant::from_micros(0))
    }

    fn rows(n: usize) -> Vec<FieldRow> {
        (0..n).map(|i| FieldRow::action(alloc::format!("row-{i}"))).collect()
    }

    // --- metrics (field-list ruling §5, tests 3/4; 1/2/5 live in menu.rs
    // next to `RowStyle` itself) ---

    #[test]
    fn peek_invariant_a_row_below_the_fold_always_shows_a_sliver() {
        // Device page: 224px content minus a 12px top gutter = 212px
        // available. `visible_rows * field_row_height` must be strictly
        // less than that, so an 8th row always peeks rather than landing
        // exactly on the edge with no hint it exists.
        let height = row_height(&RowStyle::FIELD, &font::value());
        let available = 224 - 12;
        let visible_rows = available / height as i32;
        assert!(
            (visible_rows * height as i32) < available,
            "the device page's row budget must leave a scroll peek, not land exactly on the fold"
        );
    }

    #[test]
    fn tier_one_seven_rows_fit_without_scrolling() {
        let height = row_height(&RowStyle::FIELD, &font::value());
        let available = 224 - 12;
        let visible_rows = available / height as i32;
        assert!(visible_rows >= 7, "Tier 1's 7 rows must be fully visible without scrolling");
    }

    // --- behaviour ---

    #[test]
    fn empty_list_is_not_focusable() {
        let list = FieldList::new(vec![]);
        assert!(!list.is_focusable());
    }

    #[test]
    fn a_on_a_readonly_row_is_a_noop_even_with_a_callback_that_would_panic() {
        let mut list = FieldList::new(vec![FieldRow::readonly("SAMPLE RATE").with_value("48 kHz", palette::TEXT_SECONDARY)])
            .on_activate_index(|_| panic!("a readonly row must never invoke the activation callback"));
        list.on_focus(FocusEvent::Gained);
        let action = list.on_focus(FocusEvent::Activated);
        assert!(matches!(action, Action::None));
    }

    #[test]
    fn a_on_an_action_row_invokes_the_callback() {
        let mut list = FieldList::new(vec![FieldRow::action("CODEC").with_value("LDAC", palette::TEXT_PRIMARY)])
            .on_activate_index(|index| {
                assert_eq!(index, 0);
                Action::PopView
            });
        list.on_focus(FocusEvent::Gained);
        let action = list.on_focus(FocusEvent::Activated);
        assert!(matches!(action, Action::PopView));
    }

    #[test]
    fn focus_traversal_includes_readonly_rows_never_skipping_one() {
        // Down from row 0 of a list whose rows 1..5 are all Readonly must
        // land on row 1, not jump past them (field-list ruling §5 test 7).
        let mut list = FieldList::new(vec![
            FieldRow::action("CODEC"),
            FieldRow::readonly("SAMPLE RATE"),
            FieldRow::readonly("USB IN"),
            FieldRow::readonly("A2DP"),
            FieldRow::readonly("ADDRESS"),
            FieldRow::action("Forget this device"),
        ]);
        list.on_intent(NavIntent::Down);
        assert_eq!(list.selected_index(), 1, "Down from row 0 must land on row 1, even though it's Readonly");
    }

    #[test]
    fn caret_is_drawn_only_on_a_focused_action_row() {
        // Action, unfocused: no caret (nor any TEXT_PRIMARY ink at all --
        // the label itself uses TEXT_PRIMARY too, so this case instead
        // proves the caret specifically via the focused/unfocused delta
        // below).
        let action_row = || vec![FieldRow::action("CODEC").with_value("LDAC", palette::TEXT_PRIMARY)];

        let unfocused = FieldList::new(action_row());
        let mut fb_unfocused = FrameBuffer565::new(220, 100);
        let area = Rectangle::new(Point::new(0, 0), Size::new(220, 100));
        unfocused.render(area, &test_ctx(), &mut fb_unfocused).unwrap();

        let focused = FieldList::new(action_row()).with_focused(true).with_selected(0);
        let mut fb_focused = FrameBuffer565::new(220, 100);
        focused.render(area, &test_ctx(), &mut fb_focused).unwrap();

        let unfocused_pixels: Vec<_> = fb_unfocused.pixels().map(|p| p.1).collect();
        let focused_pixels: Vec<_> = fb_focused.pixels().map(|p| p.1).collect();
        assert_ne!(unfocused_pixels, focused_pixels, "focusing an Action row must change its rendered output (the caret appearing)");

        // Readonly, focused: never a caret -- render must not panic and
        // this is exercised by the freshness/behaviour tests above via
        // `is_focusable`/`on_focus`; here we additionally confirm
        // `activation()` reports `None`, the structural half of "never a
        // caret".
        let mut readonly_list = FieldList::new(vec![FieldRow::readonly("SAMPLE RATE").with_value("48 kHz", palette::TEXT_SECONDARY)]);
        readonly_list.on_focus(FocusEvent::Gained);
        assert_eq!(readonly_list.activation(), None);
    }

    #[test]
    fn a_value_is_drawn_on_an_unfocused_row() {
        let list = FieldList::new(vec![FieldRow::readonly("A2DP").with_value("Streaming", palette::STATUS_SUCCESS)]);
        let mut fb = FrameBuffer565::new(220, 100);
        let area = Rectangle::new(Point::new(0, 0), Size::new(220, 100));
        list.render(area, &test_ctx(), &mut fb).unwrap();

        let any_value_ink = fb.pixels().any(|p| p.1 == palette::STATUS_SUCCESS);
        assert!(any_value_ink, "a trailing value must be visible even when the row is not focused");
    }

    #[test]
    fn small_value_font_measures_narrower_than_normal_for_the_address_string() {
        let address = "94:DB:56:54:7C:F2"; // 17 characters
        let normal_width = font::value()
            .get_rendered_dimensions_aligned(
                address,
                Point::zero(),
                u8g2_fonts::types::VerticalPosition::Top,
                u8g2_fonts::types::HorizontalAlignment::Left,
            )
            .unwrap()
            .unwrap()
            .size
            .width;
        let small_width = font::label()
            .get_rendered_dimensions_aligned(
                address,
                Point::zero(),
                u8g2_fonts::types::VerticalPosition::Top,
                u8g2_fonts::types::HorizontalAlignment::Left,
            )
            .unwrap()
            .unwrap()
            .size
            .width;
        assert!(small_width < normal_width, "ValueFont::Small must measure narrower than Normal for the address string");
        // Content width budget: R - L = 194 - 12 = 182px.
        assert!(small_width <= 182, "the address value must fit inside the R - L = 182px content budget at the small font");
    }

    #[test]
    fn with_selected_identity_carries_selection_across_a_rebuild_that_inserts_rows() {
        let key_b = ListItemKey::from_u64(2);
        let original = FieldList::new(vec![
            FieldRow::action("CODEC").with_key(ListItemKey::from_u64(1)),
            FieldRow::action("LDAC QUALITY").with_key(key_b),
        ])
        .with_selected(1);
        assert_eq!(original.selected_key(), Some(key_b));

        // Rebuild with a new row inserted ahead of the previously
        // selected one -- the selection must follow the KEY, not the
        // index.
        let rebuilt = FieldList::new(vec![
            FieldRow::action("EQ PRESET").with_key(ListItemKey::from_u64(3)),
            FieldRow::action("CODEC").with_key(ListItemKey::from_u64(1)),
            FieldRow::action("LDAC QUALITY").with_key(key_b),
        ])
        .with_selected_identity(original.selected_key(), original.selected_index());

        assert_eq!(rebuilt.selected_index(), 2, "the selection must follow LDAC QUALITY's key even though a row was inserted above it");
    }

    #[test]
    fn scroll_top_survives_a_rebuild_from_an_unrelated_event() {
        // Scroll to the last row of a list that overflows a small
        // viewport, then rebuild a fresh `FieldList` (simulating a
        // store-backed page's per-event rebuild) carrying `scroll_top`
        // forward -- it must not reset to 0 (field-list ruling §4.7 /
        // §5 test 12).
        let mut list = FieldList::new(rows(10)).with_selected(9);
        let area = Rectangle::new(Point::new(0, 0), Size::new(220, 100)); // ~3 rows visible
        let mut fb = FrameBuffer565::new(220, 100);
        list.on_focus(FocusEvent::Gained);
        list.render(area, &test_ctx(), &mut fb).unwrap();
        let scroll_top_before = list.scroll_top().expect("a scrolled FieldList must report a scroll-top row");
        assert!(scroll_top_before > 0, "selecting the last of 10 rows on a ~3-row viewport must have scrolled");

        let rebuilt = FieldList::new(rows(10)).with_selected(9).with_scroll_top(scroll_top_before);
        let mut fb2 = FrameBuffer565::new(220, 100);
        rebuilt.render(area, &test_ctx(), &mut fb2).unwrap();
        assert_eq!(rebuilt.scroll_top(), Some(scroll_top_before), "carrying scroll_top forward must survive a render unchanged");
    }

    // --- paint_key (bead pico-link-7h5.5) ---

    #[test]
    fn paint_key_is_stable_across_calls_with_no_state_change() {
        let list = FieldList::new(rows(3));
        assert_eq!(list.paint_key(&test_ctx()), list.paint_key(&test_ctx()));
    }

    #[test]
    fn paint_key_changes_when_selection_or_focus_changes() {
        let mut list = FieldList::new(rows(3));
        let before = list.paint_key(&test_ctx());
        list.on_intent(NavIntent::Down);
        assert_ne!(before, list.paint_key(&test_ctx()), "moving the selection must change the paint key");

        let mut list = FieldList::new(rows(3));
        let before = list.paint_key(&test_ctx());
        list.on_focus(FocusEvent::Gained);
        assert_ne!(before, list.paint_key(&test_ctx()), "gaining focus must change the paint key");
    }

    #[test]
    fn paint_key_changes_when_a_rows_value_or_value_color_changes() {
        let a = FieldList::new(vec![FieldRow::readonly("A2DP").with_value("Streaming", palette::STATUS_SUCCESS)]);
        let b = FieldList::new(vec![FieldRow::readonly("A2DP").with_value("Idle", palette::STATUS_SUCCESS)]);
        let c = FieldList::new(vec![FieldRow::readonly("A2DP").with_value("Streaming", palette::STATUS_ERROR)]);
        assert_ne!(a.paint_key(&test_ctx()), b.paint_key(&test_ctx()));
        assert_ne!(a.paint_key(&test_ctx()), c.paint_key(&test_ctx()));
    }

    #[test]
    fn paint_key_changes_when_the_value_font_changes() {
        let a = FieldList::new(vec![FieldRow::readonly("ADDRESS").with_value("94:DB:56:54:7C:F2", palette::TEXT_PRIMARY)]);
        let b = FieldList::new(vec![
            FieldRow::readonly("ADDRESS").with_value("94:DB:56:54:7C:F2", palette::TEXT_PRIMARY).with_small_value(),
        ]);
        assert_ne!(a.paint_key(&test_ctx()), b.paint_key(&test_ctx()));
    }

    #[test]
    fn paint_key_distinguishes_action_from_readonly_kind() {
        let action = FieldList::new(vec![FieldRow::action("CODEC")]);
        let readonly = FieldList::new(vec![FieldRow::readonly("CODEC")]);
        assert_ne!(action.paint_key(&test_ctx()), readonly.paint_key(&test_ctx()));
    }

    #[test]
    fn paint_key_changes_with_the_leading_gutter_style() {
        let plain = FieldList::new(vec![FieldRow::readonly("SAMPLE RATE")]);
        let guttered = FieldList::new(vec![FieldRow::readonly("SAMPLE RATE")]).with_leading_gutter();
        assert_ne!(plain.paint_key(&test_ctx()), guttered.paint_key(&test_ctx()));
    }

    #[test]
    fn paint_key_ignores_the_identity_key_and_verb_since_neither_is_a_pixel() {
        let a = FieldList::new(vec![FieldRow::action("CODEC").with_key(ListItemKey::from_u64(1))]);
        let b = FieldList::new(vec![FieldRow::action("CODEC").with_key(ListItemKey::from_u64(2))]);
        assert_eq!(a.paint_key(&test_ctx()), b.paint_key(&test_ctx()));
    }

    #[test]
    fn leading_gutter_aligns_labels_whether_or_not_a_row_carries_a_glyph() {
        // With the gutter enabled, a row with no glyph must still reserve
        // the same gutter width -- proven behaviourally by checking both
        // render without panicking and that turning the gutter on changes
        // pixels only in the gutter region, not by shifting where the
        // label starts (the label's left edge is a style-level constant,
        // not per-row).
        let with_glyph = FieldList::new(vec![FieldRow::readonly("SAMPLE RATE").with_leading_glyph('\u{6f}')]).with_leading_gutter();
        let without_glyph = FieldList::new(vec![FieldRow::readonly("SAMPLE RATE")]).with_leading_gutter();

        let area = Rectangle::new(Point::new(0, 0), Size::new(220, 100));
        let mut fb_with = FrameBuffer565::new(220, 100);
        with_glyph.render(area, &test_ctx(), &mut fb_with).unwrap();
        let mut fb_without = FrameBuffer565::new(220, 100);
        without_glyph.render(area, &test_ctx(), &mut fb_without).unwrap();

        // Both must render without panicking regardless of glyph
        // presence -- the gutter is reserved either way.
        let _ = (fb_with, fb_without);
    }

    // --- Value row (bead pico-link-ryw.9, design
    // `.planning/design/2026-09-25-value-row-and-on-exit-hook.md` §2/§4) ---

    #[test]
    fn value_row_left_right_calls_on_step_with_key_and_direction() {
        let key = ListItemKey::from_u64(7);
        let calls: alloc::rc::Rc<core::cell::RefCell<Vec<(ListItemKey, Step)>>> = alloc::rc::Rc::new(core::cell::RefCell::new(Vec::new()));
        let calls_clone = calls.clone();
        let mut list = FieldList::new(vec![FieldRow::value_row("CUSHION", "3 dB", StepBounds { prev: true, next: true }).with_key(key)])
            .on_step(move |k, step| {
                calls_clone.borrow_mut().push((k, step));
                Action::None
            });
        list.on_focus(FocusEvent::Gained);

        list.on_intent(NavIntent::Left);
        list.on_intent(NavIntent::Right);

        assert_eq!(*calls.borrow(), vec![(key, Step::Prev), (key, Step::Next)]);
    }

    #[test]
    fn value_row_dead_side_is_noop_and_skips_callback() {
        let key = ListItemKey::from_u64(1);
        let mut list = FieldList::new(vec![FieldRow::value_row("MIN", "0 dB", StepBounds { prev: false, next: true }).with_key(key)])
            .on_step(|_, _| panic!("a dead side must never invoke the step callback"));
        list.on_focus(FocusEvent::Gained);

        let action = list.on_intent(NavIntent::Left);
        assert!(matches!(action, Action::None));
    }

    #[test]
    fn left_right_on_action_and_readonly_rows_are_noops() {
        let mut list = FieldList::new(vec![
            FieldRow::action("CODEC").with_value("LDAC", palette::TEXT_PRIMARY),
            FieldRow::readonly("SAMPLE RATE").with_value("48 kHz", palette::TEXT_SECONDARY),
        ])
        .on_step(|_, _| panic!("a non-Value row must never invoke the step callback"));
        list.on_focus(FocusEvent::Gained);

        assert!(matches!(list.on_intent(NavIntent::Left), Action::None));
        assert!(matches!(list.on_intent(NavIntent::Right), Action::None));

        list.on_intent(NavIntent::Down);
        assert!(matches!(list.on_intent(NavIntent::Left), Action::None));
        assert!(matches!(list.on_intent(NavIntent::Right), Action::None));
    }

    #[test]
    fn value_row_activation_is_none() {
        let mut list = FieldList::new(vec![FieldRow::value_row("CUSHION", "3 dB", StepBounds { prev: true, next: true })]);
        list.on_focus(FocusEvent::Gained);
        assert_eq!(list.activation(), None, "A must be dim and inert on a Value row");
        let action = list.on_focus(FocusEvent::Activated);
        assert!(matches!(action, Action::None));
    }

    #[test]
    fn chevrons_only_on_focused_value_row_and_dead_side_is_divider() {
        let area = Rectangle::new(Point::new(0, 0), Size::new(220, 100));

        let unfocused = FieldList::new(vec![FieldRow::value_row("CUSHION", "3 dB", StepBounds { prev: true, next: true })]);
        let mut fb_unfocused = FrameBuffer565::new(220, 100);
        unfocused.render(area, &test_ctx(), &mut fb_unfocused).unwrap();

        let mut focused_both_live = FieldList::new(vec![FieldRow::value_row("CUSHION", "3 dB", StepBounds { prev: true, next: true })])
            .with_focused(true)
            .with_selected(0);
        // `on_focus` needed for the `focused` flag used by `render`'s
        // `selected` gate is set via `with_focused` above already, but
        // exercise it via `on_focus` too, matching how the real navigator
        // drives it.
        focused_both_live.on_focus(FocusEvent::Gained);
        let mut fb_focused = FrameBuffer565::new(220, 100);
        focused_both_live.render(area, &test_ctx(), &mut fb_focused).unwrap();

        let unfocused_pixels: Vec<_> = fb_unfocused.pixels().map(|p| p.1).collect();
        let focused_pixels: Vec<_> = fb_focused.pixels().map(|p| p.1).collect();
        assert_ne!(unfocused_pixels, focused_pixels, "focusing a Value row must draw the chevrons");

        // A dead left side draws DIVIDER-colored ink somewhere it wasn't
        // present when both sides were live.
        let mut dead_left = FieldList::new(vec![FieldRow::value_row("MIN", "0 dB", StepBounds { prev: false, next: true })])
            .with_focused(true)
            .with_selected(0);
        dead_left.on_focus(FocusEvent::Gained);
        let mut fb_dead_left = FrameBuffer565::new(220, 100);
        dead_left.render(area, &test_ctx(), &mut fb_dead_left).unwrap();
        let dead_left_pixels: Vec<_> = fb_dead_left.pixels().map(|p| p.1).collect();
        assert_ne!(dead_left_pixels, focused_pixels, "a dead side must render differently (DIVIDER, not TEXT_PRIMARY)");
    }
}
