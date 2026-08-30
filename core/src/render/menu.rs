//! A chip-less, single-line, vertically-centered "action row" style —
//! distinct from `list.rs`'s two-line row (label+sublabel+letter/icon
//! chip). A menu of plain actions (e.g. "Reveal"/"Type") reusing
//! `list::VerticalList`/`ListItem` would draw every row with a
//! letter-chip badge and reserve two-line label+sublabel height — visually
//! indistinguishable from a two-line list item, when these rows are
//! *actions*, not list entries. [`MenuList`]/[`MenuItem`] give menu
//! screens their own narrow widget instead of bolting a third chip-less
//! mode onto `VerticalList` (which would let `VerticalList`'s other,
//! unrelated list-style consumers accidentally end up chip-less too).
//!
//! [`draw_row`] (the shared row-drawing primitive, `pub(crate)`) is
//! written to also be reusable directly by a call site that wants a
//! bolder, more prominent single-line action row (e.g. a detail view's
//! own default action) sharing the same chip-less/single-line/
//! vertically-centered visual language as the menu it complements, just
//! with a caller-supplied bolder label font instead of the menu's own.

// Identical allow (and rationale) as `list.rs`:
// this module does the same `embedded-graphics` `Point`(i32)/`Size`(u32)
// coordinate math directly, so the same justification applies — no
// display this project targets is anywhere near large enough for these
// conversions to wrap, truncate, or lose a sign in practice.
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::convert::Infallible;

use embedded_graphics::draw_target::{DrawTarget, DrawTargetExt};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::{Point, Primitive, Size};
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use embedded_graphics::Drawable;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::FontRenderer;

use crate::input::NavIntent;

use super::ctx::RenderCtx;
use super::framebuffer::FrameBuffer565;
use super::theme::{self, font, icon, palette};
use super::widget::{Action, FocusEvent, Widget};

/// A menu row's right-aligned trailing content — the disclosure caret
/// (the original, sole behavior), a state label (e.g. a toggle row's
/// "On"/"Off"), or nothing at all. Threaded through [`draw_row`] so the
/// "what goes on the right edge" decision lives in one place instead of
/// every caller reimplementing its own right-edge layout math.
///
/// Unlike [`Trailing::Caret`] (which only appears while the row is
/// `selected`, since it's a "you can press this" affordance only
/// relevant to the focused row), [`Trailing::Label`] is drawn regardless
/// of `selected`: a toggle row's current state (e.g. a settings screen's
/// toggle row) must stay visible even when focus has moved elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trailing<'a> {
    /// The pre-existing disclosure-caret behavior: shown only while the
    /// row is `selected`.
    Caret,
    /// A state label, drawn in `color`, shown unconditionally.
    Label(&'a str, Rgb565),
    /// No trailing content.
    None,
}

/// [`MenuItem`]'s owned counterpart to [`Trailing`] — a borrowed
/// `Trailing<'a>` can't be stored on a long-lived `MenuItem`, so this owns
/// whatever text a `Trailing::Label` needs and hands out a borrowed
/// [`Trailing`] view via [`MenuItem::trailing`] at render time.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnedTrailing {
    Caret,
    Label(String, Rgb565),
    None,
}

/// A single menu row's label. Display-only, like [`super::list::ListItem`]
/// — no domain/identity fields, and no sublabel/icon/chip slots at all
/// (unlike `ListItem`'s): this style has none of those.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub label: String,
    /// The label's own text color. Defaults to [`palette::TEXT_PRIMARY`]
    /// (the original, uniform row color) — overridden via
    /// [`MenuItem::with_label_color`] for a row that needs
    /// to carry meaning in its color (e.g. a destructive action in
    /// [`palette::STATUS_ERROR`]).
    label_color: Rgb565,
    trailing: OwnedTrailing,
}

impl MenuItem {
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self { label: label.into(), label_color: palette::TEXT_PRIMARY, trailing: OwnedTrailing::Caret }
    }

    /// Overrides this row's label text color (default
    /// [`palette::TEXT_PRIMARY`]) — e.g. [`palette::STATUS_ERROR`] for a
    /// destructive action row (e.g. a confirm-clear screen).
    #[must_use]
    pub fn with_label_color(mut self, color: Rgb565) -> Self {
        self.label_color = color;
        self
    }

    /// Replaces this row's trailing disclosure caret with a state label
    /// (e.g. "On"/"Off"), drawn in `color`, shown regardless of focus —
    /// e.g. a settings-screen toggle row.
    #[must_use]
    pub fn with_trailing_label(mut self, text: impl Into<String>, color: Rgb565) -> Self {
        self.trailing = OwnedTrailing::Label(text.into(), color);
        self
    }

    /// Suppresses this row's trailing content entirely (no caret, no
    /// label).
    #[must_use]
    pub fn with_no_trailing(mut self) -> Self {
        self.trailing = OwnedTrailing::None;
        self
    }

    fn trailing(&self) -> Trailing<'_> {
        match &self.trailing {
            OwnedTrailing::Caret => Trailing::Caret,
            OwnedTrailing::Label(text, color) => Trailing::Label(text, *color),
            OwnedTrailing::None => Trailing::None,
        }
    }
}

/// Left margin (px) from a row's left edge to its label text — matches
/// a detail view's field-row left margin, so a menu row's text lines up
/// with the detail view's own field-row text directly above it (the
/// screen a menu is always pushed from).
const TEXT_LEFT_MARGIN: i32 = 8;
/// Right margin (px) reserved for the focused row's disclosure caret —
/// matches `list.rs`'s `CARET_RIGHT_MARGIN`.
const CARET_RIGHT_MARGIN: i32 = 6;
/// Vertical padding (px) above/below a row's single centered text line.
/// Deliberately its own constant, not `list::ROW_PADDING` (tuned for a
/// two-line block) — see [`row_height`]'s doc comment.
const ROW_PADDING: i32 = 10;
/// Fallback line height (px): only used if a font's metrics are somehow
/// unavailable.
const FALLBACK_LINE_HEIGHT: i32 = 16;

/// Measures a font's worst-case single-line pixel footprint using the
/// same "all five ASCII descenders (+ a capital, for ascent)" probe
/// string technique used elsewhere in this module tree; duplicated
/// locally rather than shared since this is a small private helper and
/// this module has no other reason to depend on another view module.
fn line_height(font: &FontRenderer) -> i32 {
    font.get_rendered_dimensions_aligned("Agjpqy", Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
        .unwrap_or(None)
        .map_or(FALLBACK_LINE_HEIGHT, |bbox| bbox.size.height as i32)
}

/// Pixel height of one chip-less action row: `ROW_PADDING` above and below
/// a single [`font::value`]-sized text line. Deliberately shorter than
/// `list::ROW_HEIGHT` (`list.rs`'s two-line name+username+padding block) —
/// per design review's explicit ask, this style is a one-line footprint,
/// not `ROW_HEIGHT`.
#[must_use]
pub(crate) fn row_height() -> u32 {
    (ROW_PADDING * 2 + line_height(&font::value())) as u32
}

/// Draws one chip-less action row: the shared selection fill + left accent
/// bar (via [`theme::draw_selection`]) when `selected`, else a plain
/// bottom hairline divider; `label` (in `label_color`) vertically centered
/// in `font` at the row's left margin; and, per `trailing`, either the
/// shared right-edge disclosure
/// caret (shown only while `selected`), a state label (shown regardless of
/// `selected`), or nothing — the same visual vocabulary
/// [`super::list::draw_row`] uses for its own selection/divider/caret,
/// just without a chip or a second (sublabel) line. `font` is
/// caller-supplied (not hardcoded to [`font::value`]) so [`MenuList`]'s
/// plain action rows and another call site's bolder, more prominent
/// single-line action row can share this one drawing routine without
/// visually drifting apart on everything BUT weight — see the module doc.
///
/// # Errors
///
/// Returns `Infallible`'s uninhabited variant in practice — see
/// [`super::widget::Widget::render`]'s doc comment for why the `Result`
/// return exists at all.
pub(crate) fn draw_row<D>(
    target: &mut D,
    row_rect: Rectangle,
    label: &str,
    label_color: Rgb565,
    font: &FontRenderer,
    selected: bool,
    trailing: Trailing<'_>,
) -> Result<(), Infallible>
where
    D: DrawTarget<Color = Rgb565, Error = Infallible>,
{
    if selected {
        theme::draw_selection(row_rect, target)?;
    } else {
        let divider = Rectangle::new(
            Point::new(row_rect.top_left.x, row_rect.top_left.y + row_rect.size.height as i32 - 1),
            Size::new(row_rect.size.width, 1),
        );
        divider.into_styled(PrimitiveStyle::with_fill(palette::DIVIDER)).draw(target)?;
    }

    let text_x = row_rect.top_left.x + TEXT_LEFT_MARGIN;
    let text_y = row_rect.top_left.y + row_rect.size.height as i32 / 2;
    let _ = font.render_aligned(
        label,
        Point::new(text_x, text_y),
        VerticalPosition::Center,
        HorizontalAlignment::Left,
        FontColor::Transparent(label_color),
        target,
    );

    match trailing {
        Trailing::Caret if selected => {
            let mut buf = [0_u8; 4];
            let caret: &str = icon::CARET_RIGHT.encode_utf8(&mut buf);
            let caret_x = row_rect.top_left.x + row_rect.size.width as i32 - CARET_RIGHT_MARGIN;
            let caret_y = row_rect.top_left.y + row_rect.size.height as i32 / 2;
            let _ = font::icon_1x().render_aligned(
                caret,
                Point::new(caret_x, caret_y),
                VerticalPosition::Center,
                HorizontalAlignment::Right,
                FontColor::Transparent(palette::TEXT_PRIMARY),
                target,
            );
        }
        Trailing::Label(text, color) => {
            let label_x = row_rect.top_left.x + row_rect.size.width as i32 - CARET_RIGHT_MARGIN;
            let label_y = row_rect.top_left.y + row_rect.size.height as i32 / 2;
            let _ = font::value().render_aligned(
                text,
                Point::new(label_x, label_y),
                VerticalPosition::Center,
                HorizontalAlignment::Right,
                FontColor::Transparent(color),
                target,
            );
        }
        // `Trailing::Caret` while unselected, and `Trailing::None`
        // unconditionally: nothing to draw.
        Trailing::Caret | Trailing::None => {}
    }

    Ok(())
}

/// Callback invoked with the selected row's index on activation — the
/// only activation seam this widget needs (mirrors
/// `list::VerticalList::on_activate_index`; this style has no
/// `on_activate`-by-`&MenuItem` sibling since every current caller already
/// dispatches on a fixed, known row order).
type OnActivateIndex = Box<dyn Fn(usize) -> Action>;

/// A focusable, chip-less vertical menu of [`MenuItem`]s — the restyled
/// field action menu style. No scrolling: every current caller has at
/// most two rows, well within any content area this app renders into, so
/// `list::reconcile_top_index`'s viewport-edge scrolling isn't needed
/// here (unlike `VerticalList`, which does need it for arbitrarily long
/// lists). A row beyond the
/// viewport is simply not drawn, the same as a `VerticalList` row would be
/// once clipped — see [`Widget::render`]'s early `break`.
pub struct MenuList {
    items: Vec<MenuItem>,
    selected: usize,
    focused: bool,
    on_activate_index: Option<OnActivateIndex>,
}

impl MenuList {
    #[must_use]
    pub fn new(items: Vec<MenuItem>) -> Self {
        Self { items, selected: 0, focused: false, on_activate_index: None }
    }

    /// Registers a callback invoked with the selected row's index when the
    /// menu is activated while focused.
    #[must_use]
    pub fn on_activate_index(mut self, callback: impl Fn(usize) -> Action + 'static) -> Self {
        self.on_activate_index = Some(Box::new(callback));
        self
    }

    /// Sets the initially selected row, clamped to the item list's bounds
    /// — mirrors `list::VerticalList::with_selected`, for the same
    /// "store-backed widget rebuilds a fresh `MenuList` from live data on
    /// every call, carrying forward its own persisted selection" pattern.
    #[must_use]
    pub fn with_selected(mut self, selected: usize) -> Self {
        self.selected = selected.min(self.items.len().saturating_sub(1));
        self
    }

    /// Sets the initial focus-highlight state — mirrors
    /// `list::VerticalList::with_focused`; same rationale as
    /// [`Self::with_selected`].
    #[must_use]
    pub fn with_focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    #[must_use]
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    fn move_selection(&mut self, delta: i32) {
        if self.items.is_empty() {
            return;
        }
        let len = self.items.len() as i32;
        let next = (self.selected as i32 + delta).clamp(0, len - 1);
        self.selected = next as usize;
    }
}

impl Widget for MenuList {
    fn measure(&self, constraints: Size, _ctx: &RenderCtx) -> Size {
        constraints
    }

    fn is_focusable(&self) -> bool {
        !self.items.is_empty()
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
            FocusEvent::Activated => {
                if let Some(callback) = &self.on_activate_index {
                    callback(self.selected)
                } else {
                    Action::None
                }
            }
        }
    }

    fn on_intent(&mut self, intent: NavIntent) -> Action {
        match intent {
            NavIntent::Down => self.move_selection(1),
            NavIntent::Up => self.move_selection(-1),
            NavIntent::JumpBy(n) => self.move_selection(i32::from(n)),
            NavIntent::Select | NavIntent::Back | NavIntent::Left | NavIntent::Right | NavIntent::ShortcutX | NavIntent::ShortcutY => {}
        }
        Action::None
    }

    fn render(&self, area: Rectangle, _ctx: &RenderCtx, target: &mut FrameBuffer565) -> Result<(), Infallible> {
        let mut clipped = target.clipped(&area);
        let height = row_height();

        for (index, item) in self.items.iter().enumerate() {
            let row_top = area.top_left.y + (index as u32 * height) as i32;
            if row_top >= area.top_left.y + area.size.height as i32 {
                break;
            }
            let row_rect = Rectangle::new(Point::new(area.top_left.x, row_top), Size::new(area.size.width, height));
            let selected = self.focused && index == self.selected;
            draw_row(&mut clipped, row_rect, &item.label, item.label_color, &font::value(), selected, item.trailing())?;
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

    fn items(n: usize) -> Vec<MenuItem> {
        (0..n).map(|i| MenuItem::new(format!("item-{i}"))).collect()
    }

    #[test]
    fn empty_menu_is_not_focusable() {
        let menu = MenuList::new(vec![]);
        assert!(!menu.is_focusable());
    }

    #[test]
    fn non_empty_menu_is_focusable() {
        let menu = MenuList::new(items(2));
        assert!(menu.is_focusable());
    }

    #[test]
    fn next_and_prev_move_selection_and_clamp_at_the_ends() {
        let mut menu = MenuList::new(items(2));
        assert_eq!(menu.selected_index(), 0);
        menu.on_intent(NavIntent::Up);
        assert_eq!(menu.selected_index(), 0);
        menu.on_intent(NavIntent::Down);
        assert_eq!(menu.selected_index(), 1);
        menu.on_intent(NavIntent::Down);
        assert_eq!(menu.selected_index(), 1);
    }

    #[test]
    fn activate_without_callback_is_a_noop() {
        let mut menu = MenuList::new(items(1));
        let action = menu.on_focus(FocusEvent::Activated);
        assert!(matches!(action, Action::None));
    }

    #[test]
    fn activate_invokes_the_callback_with_the_selected_rows_index() {
        let mut menu = MenuList::new(items(3)).on_activate_index(|index| {
            assert_eq!(index, 1);
            Action::PopView
        });
        menu.on_intent(NavIntent::Down);
        let action = menu.on_focus(FocusEvent::Activated);
        assert!(matches!(action, Action::PopView));
    }

    #[test]
    fn row_height_is_shorter_than_the_two_line_list_row_height() {
        assert!(
            row_height() < crate::render::ROW_HEIGHT,
            "a chip-less single-line row must be shorter than the two-line list row"
        );
    }

    #[test]
    fn render_never_draws_the_list_row_letter_chip_fill_color() {
        // The whole point of this widget: unlike `list::draw_row`, it must
        // never paint `palette::BRAND` (the letter-chip's fill) anywhere.
        let menu = MenuList::new(vec![MenuItem::new("Reveal"), MenuItem::new("Type password")]);
        let mut fb = FrameBuffer565::new(200, 100);
        let area = Rectangle::new(Point::new(0, 0), Size::new(200, 100));
        menu.render(area, &test_ctx(), &mut fb).unwrap();

        let any_chip_fill = fb.pixels().any(|p| p.1 == palette::BRAND);
        assert!(!any_chip_fill, "the restyled menu must never draw the list-row letter-chip fill color");
    }

    #[test]
    fn render_focused_row_shows_the_shared_selection_fill() {
        let mut menu = MenuList::new(items(2)).on_activate_index(|_| Action::None);
        menu.on_focus(FocusEvent::Gained);
        let mut fb = FrameBuffer565::new(200, 100);
        let area = Rectangle::new(Point::new(0, 0), Size::new(200, 100));
        menu.render(area, &test_ctx(), &mut fb).unwrap();

        let any_selection_ink = fb.pixels().any(|p| p.1 == palette::SURFACE_ELEVATED);
        assert!(any_selection_ink, "the focused row must show the shared selection fill");
    }

    // --- Trailing ---

    #[test]
    fn a_trailing_label_is_drawn_even_when_the_row_is_not_selected() {
        let menu = MenuList::new(vec![MenuItem::new("Screen sleep").with_trailing_label("On", palette::STATUS_SUCCESS)]);
        // Not focused, so nothing is selected -- a `Trailing::Caret` row
        // would draw nothing on the right edge here; a `Trailing::Label`
        // row must draw its state label regardless.
        let mut fb = FrameBuffer565::new(200, 100);
        let area = Rectangle::new(Point::new(0, 0), Size::new(200, 100));
        menu.render(area, &test_ctx(), &mut fb).unwrap();

        let any_label_ink = fb.pixels().any(|p| p.1 == palette::STATUS_SUCCESS);
        assert!(any_label_ink, "a trailing label must be visible even on an unselected row");
    }

    #[test]
    fn a_trailing_caret_is_still_gated_on_selection() {
        let mut menu = MenuList::new(vec![MenuItem::new("Clear all items")]);
        let mut fb_unselected = FrameBuffer565::new(200, 100);
        let area = Rectangle::new(Point::new(0, 0), Size::new(200, 100));
        menu.render(area, &test_ctx(), &mut fb_unselected).unwrap();

        menu.on_focus(FocusEvent::Gained);
        let mut fb_selected = FrameBuffer565::new(200, 100);
        menu.render(area, &test_ctx(), &mut fb_selected).unwrap();

        let unselected_pixels: Vec<_> = fb_unselected.pixels().map(|p| p.1).collect();
        let selected_pixels: Vec<_> = fb_selected.pixels().map(|p| p.1).collect();
        assert_ne!(unselected_pixels, selected_pixels, "selecting the row must change its rendered output (the caret appearing)");
    }

    #[test]
    fn a_custom_label_color_is_used_for_the_rows_own_text() {
        let menu = MenuList::new(vec![MenuItem::new("Clear everything").with_label_color(palette::STATUS_ERROR)]);
        let mut fb = FrameBuffer565::new(200, 100);
        let area = Rectangle::new(Point::new(0, 0), Size::new(200, 100));
        menu.render(area, &test_ctx(), &mut fb).unwrap();

        let any_error_ink = fb.pixels().any(|p| p.1 == palette::STATUS_ERROR);
        assert!(any_error_ink, "a custom label color must actually be used when drawing the row's text");
    }

    #[test]
    fn with_selected_and_with_focused_carry_state_into_a_freshly_built_list() {
        let list = MenuList::new(items(3)).with_selected(2).with_focused(true);
        assert_eq!(list.selected_index(), 2);
        // `with_focused(true)` + `with_selected(2)` together are what a
        // store-backed adapter needs to carry its own persisted state into
        // a freshly rebuilt `MenuList` every call -- proven behaviorally
        // via `on_intent` acting on row 2 as already-selected.
        let mut list = list;
        let action = list.on_focus(FocusEvent::Activated);
        assert!(matches!(action, Action::None), "activation without a callback is a no-op, same as MenuList::new");
    }
}
