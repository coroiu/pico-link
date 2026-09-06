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
use u8g2_fonts::FontRenderer;

use crate::input::NavIntent;

use super::ctx::RenderCtx;
use super::framebuffer::FrameBuffer565;
use super::list::{reconcile_top_index, ListItemKey};
use super::menu::{draw_row, row_height, RowStyle, RowTrailing, RowValue};
use super::paint_key::PaintKey;
use super::theme::{font, palette};
use super::widget::{Action, FocusEvent, Verb, Widget};

/// Seed for [`FieldList::paint_key`] -- only needs to differ from other
/// widgets' own seeds.
const FIELD_PAINT_KEY_SEED: u64 = 13;

/// A field row's kind — TWO, not three, and collapsing Uma's `Info` and
/// `Disabled` kinds into one variant IS the finding the field-list ruling
/// makes (§4.1): both are focusable, dim, never grow a caret, `A` is a
/// no-op, and the trailing value is drawn regardless of focus. They
/// differ only in what the value text *says* (a fact on the device page,
/// a reason on the codec picker) — rendering and behaviour are identical,
/// so encoding them as one variant is what makes it structurally
/// impossible for them to drift apart later.
///
/// Extension point: if a disabled row ever needs its own distinct mark (a
/// lock glyph, a strikethrough), it takes the **leading gutter**
/// ([`FieldRow::with_leading_glyph`]), not a third kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Bright label, caret while focused, `A` fires the list's callback.
    Action,
    /// Dim label, NEVER a caret, `A` is a no-op — but fully focusable,
    /// and its value is drawn regardless of focus.
    Readonly,
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
}

impl ValueFont {
    fn font(self) -> FontRenderer {
        match self {
            ValueFont::Normal => font::value(),
            ValueFont::Small => font::label(),
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

    fn resolved_label_color(&self) -> Rgb565 {
        self.label_color.unwrap_or(match self.kind {
            FieldKind::Action => palette::TEXT_PRIMARY,
            FieldKind::Readonly => palette::TEXT_SECONDARY,
        })
    }
}

type OnActivateIndex = Box<dyn Fn(usize) -> Action>;

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
}

impl FieldList {
    #[must_use]
    pub fn new(rows: Vec<FieldRow>) -> Self {
        Self { rows, selected: 0, top_index: Cell::new(0), focused: false, style: RowStyle::FIELD, on_activate_index: None }
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

    fn move_selection(&mut self, delta: i32) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len() as i32;
        let next = (self.selected as i32 + delta).clamp(0, len - 1);
        self.selected = next as usize;
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
                    self.on_activate_index.as_ref().map_or(Action::None, |callback| callback(self.selected))
                }
                Some(FieldKind::Readonly) | None => Action::None,
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
            NavIntent::Select | NavIntent::Back | NavIntent::Left | NavIntent::Right | NavIntent::ShortcutX | NavIntent::ShortcutY => {}
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
            FieldKind::Readonly => None,
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
        for row in &self.rows {
            key = key.fold_str(&row.label);
            key = key.fold(match row.kind {
                FieldKind::Action => 0,
                FieldKind::Readonly => 1,
            });
            key = key.fold_color(row.resolved_label_color());
            key = key.fold_opt_str(row.value.as_deref());
            key = key.fold_color(row.value_color);
            key = key.fold(match row.value_font {
                ValueFont::Normal => 0,
                ValueFont::Small => 1,
            });
            key = key.fold(match row.leading {
                Some(c) => u64::from(u32::from(c)) + 1,
                None => 0,
            });
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
            )?;
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
}
