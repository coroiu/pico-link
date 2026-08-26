//! `ConfirmView`: a single, self-contained pushed-`Screen` widget for a
//! destructive-action confirmation — a centered headline (+ optional
//! subline) above a [`MenuList`] of rows.
//!
//! Deliberately **one widget**, not a `Screen` composed of a
//! [`super::message::MessageView`] stacked above a [`MenuList`]: a
//! `Screen`'s widget list stacks vertically via `Widget::measure`, which
//! works fine for two *independent* widgets, but per-screen focus memory
//! and button-B-Back-cancels are simplest to reason about with exactly
//! one focusable widget per pushed screen (see `Navigator::dispatch`'s
//! "known simplification" doc comment for the general hazard of more than
//! one). `ConfirmView` instead *wraps* a [`MenuList`] internally (the same
//! "adapter over a reused content widget" shape a store-backed menu
//! screen can use over `VerticalList`), drawing its own headline into the
//! top slice of its assigned area and delegating everything else
//! (selection, activation, the row drawing itself) to the wrapped list.
//!
//! Back "cancels" for free: `Navigator::dispatch(NavIntent::
//! Back)` (button B) always pops the current screen unconditionally (see
//! that module's doc comment) — popping a pushed `ConfirmView` back to
//! Settings is exactly what "Cancel" does too, so no special-cased Back handling is
//! needed here.

#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use alloc::string::String;
use alloc::vec::Vec;
use core::convert::Infallible;

use embedded_graphics::draw_target::DrawTargetExt;
use embedded_graphics::prelude::{Point, Size};
use embedded_graphics::primitives::Rectangle;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::FontRenderer;

use crate::input::NavIntent;

use super::framebuffer::FrameBuffer565;
use super::menu::{MenuItem, MenuList};
use super::theme::{font, palette};
use super::widget::{Action, FocusEvent, Widget};

/// Top padding (px) before the headline's first line. A local constant,
/// not a reuse of `message.rs`'s private `MESSAGE_TOP_PADDING` — see
/// `menu.rs`'s `line_height` doc comment for the established "duplicated
/// rather than shared, this module has no other reason to depend on that
/// one" rationale, applied here a third time.
const HEADLINE_TOP_PADDING: i32 = 10;
/// Gap (px) between the headline's main line and its subline, if any.
const HEADLINE_LINE_GAP: i32 = 4;
/// Gap (px) between the headline block and the first row.
const HEADLINE_BOTTOM_GAP: i32 = 10;

/// Same "Agjpqy" worst-case single-line probe `menu.rs`'s `line_height`
/// uses — duplicated for the same reason (see that fn's doc comment).
fn line_height(font: &FontRenderer) -> i32 {
    font.get_rendered_dimensions_aligned("Agjpqy", Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
        .unwrap_or(None)
        .map_or(16, |bbox| bbox.size.height as i32)
}

/// A destructive-confirmation screen: a headline (+ optional subline)
/// above a menu of rows. See the module doc for why this wraps a
/// [`MenuList`] rather than composing a `Screen` of two widgets.
pub struct ConfirmView {
    headline: String,
    subline: Option<String>,
    list: MenuList,
}

impl ConfirmView {
    /// `rows` becomes the wrapped [`MenuList`]'s items verbatim — build
    /// them with [`MenuItem::with_label_color`]/[`MenuItem::
    /// with_trailing_label`] as needed (e.g. a destructive row in
    /// [`palette::STATUS_ERROR`]).
    #[must_use]
    pub fn new(headline: impl Into<String>, rows: Vec<MenuItem>) -> Self {
        Self { headline: headline.into(), subline: None, list: MenuList::new(rows) }
    }

    #[must_use]
    pub fn with_subline(mut self, subline: impl Into<String>) -> Self {
        self.subline = Some(subline.into());
        self
    }

    /// Registers the wrapped [`MenuList`]'s activation callback — see
    /// [`MenuList::on_activate_index`].
    #[must_use]
    pub fn on_activate_index(mut self, callback: impl Fn(usize) -> Action + 'static) -> Self {
        self.list = self.list.on_activate_index(callback);
        self
    }

    /// The vertical footprint (px) the headline block occupies before the
    /// first row starts — top padding, the headline's own line, the
    /// subline's line (if present), and the bottom gap.
    fn headline_height(&self) -> i32 {
        let mut height = HEADLINE_TOP_PADDING + line_height(&font::name());
        if self.subline.is_some() {
            height += HEADLINE_LINE_GAP + line_height(&font::username());
        }
        height + HEADLINE_BOTTOM_GAP
    }
}

impl Widget for ConfirmView {
    fn measure(&self, constraints: Size) -> Size {
        constraints
    }

    fn is_focusable(&self) -> bool {
        self.list.is_focusable()
    }

    fn on_focus(&mut self, event: FocusEvent) -> Action {
        self.list.on_focus(event)
    }

    fn on_intent(&mut self, intent: NavIntent) -> Action {
        self.list.on_intent(intent)
    }

    /// # Errors
    ///
    /// Returns `Infallible`'s uninhabited variant in practice — see
    /// [`Widget::render`]'s doc comment for why the `Result` return exists
    /// at all.
    fn render(&self, area: Rectangle, target: &mut FrameBuffer565) -> Result<(), Infallible> {
        let mut clipped = target.clipped(&area);
        let center_x = area.top_left.x + area.size.width as i32 / 2;
        let mut y = area.top_left.y + HEADLINE_TOP_PADDING;

        let _ = font::name().render_aligned(
            self.headline.as_str(),
            Point::new(center_x, y),
            VerticalPosition::Top,
            HorizontalAlignment::Center,
            FontColor::Transparent(palette::TEXT_PRIMARY),
            &mut clipped,
        );
        y += line_height(&font::name());

        if let Some(subline) = &self.subline {
            y += HEADLINE_LINE_GAP;
            let _ = font::username().render_aligned(
                subline.as_str(),
                Point::new(center_x, y),
                VerticalPosition::Top,
                HorizontalAlignment::Center,
                FontColor::Transparent(palette::TEXT_SECONDARY),
                &mut clipped,
            );
        }

        let rows_top = area.top_left.y + self.headline_height();
        let rows_height = (area.top_left.y + area.size.height as i32 - rows_top).max(0) as u32;
        let rows_area = Rectangle::new(Point::new(area.top_left.x, rows_top), Size::new(area.size.width, rows_height));
        self.list.render(rows_area, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::theme::palette;

    fn rows() -> Vec<MenuItem> {
        vec![MenuItem::new("Cancel"), MenuItem::new("Clear everything").with_label_color(palette::STATUS_ERROR)]
    }

    #[test]
    fn is_focusable_when_it_has_rows() {
        let view = ConfirmView::new("Clear all 3 items?", rows());
        assert!(view.is_focusable());
    }

    #[test]
    fn a_single_row_variant_is_still_focusable() {
        let view = ConfirmView::new("Nothing here yet", vec![MenuItem::new("Back")]);
        assert!(view.is_focusable());
    }

    #[test]
    fn activation_routes_to_the_wrapped_lists_callback_with_the_selected_index() {
        let mut view = ConfirmView::new("Clear all 3 items?", rows()).on_activate_index(|index| {
            assert_eq!(index, 1, "activation must report the selected row's index");
            Action::PopView
        });
        view.on_intent(NavIntent::Down); // Cancel -> Clear everything
        let action = view.on_focus(FocusEvent::Activated);
        assert!(matches!(action, Action::PopView));
    }

    #[test]
    fn cancel_is_index_zero_the_default_selection() {
        // Uma's design: Cancel must be default-focused (the safe row) —
        // proven the same way `MenuList`'s own tests prove default
        // selection: activating with no prior `Down`/`Up` reports index 0.
        let mut view = ConfirmView::new("Clear all 3 items?", rows()).on_activate_index(|index| {
            assert_eq!(index, 0, "Cancel (row 0) must be selected by default, with no navigation");
            Action::PopView
        });
        let action = view.on_focus(FocusEvent::Activated);
        assert!(matches!(action, Action::PopView));
    }

    #[test]
    fn render_draws_the_headline_above_the_rows_without_panicking() {
        let mut view = ConfirmView::new("Clear all 3 items?", rows()).with_subline("This can't be undone.");
        view.on_focus(FocusEvent::Gained);
        // 206: content-area height on the 240x240 Pico Plus 2 W panel
        // (Epic B2) -- screen height minus TITLE_BAR_HEIGHT (16) and
        // HINT_BAR_HEIGHT (18), recomputed rather than scaled from the
        // retired 320x170 panel's own 136.
        let mut fb = FrameBuffer565::new(240, 206);
        let area = Rectangle::new(Point::new(0, 0), Size::new(240, 206));
        view.render(area, &mut fb).unwrap();

        let any_headline_ink = fb.pixels().any(|p| p.1 == palette::TEXT_PRIMARY);
        assert!(any_headline_ink, "the headline should have drawn something in the default text color");
        let any_destructive_ink = fb.pixels().any(|p| p.1 == palette::STATUS_ERROR);
        assert!(any_destructive_ink, "the destructive row's custom label color should be visible");
    }

    #[test]
    fn render_with_no_subline_still_fits_the_rows_below_the_headline() {
        let mut view = ConfirmView::new("Nothing here yet", vec![MenuItem::new("Back")]);
        view.on_focus(FocusEvent::Gained);
        let mut fb = FrameBuffer565::new(240, 206);
        let area = Rectangle::new(Point::new(0, 0), Size::new(240, 206));
        view.render(area, &mut fb).unwrap();

        let any_selection_ink = fb.pixels().any(|p| p.1 == palette::SURFACE_ELEVATED);
        assert!(any_selection_ink, "the single Back row must still render (and be selected)");
    }
}
