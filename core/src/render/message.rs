//! [`MessageView`]: a shared "nothing to show here" content widget — an
//! optional large icon, a headline, and an optional subline, all centered
//! in whatever area it's given.
//!
//! A general framework-level primitive, not tied to any one caller's
//! content states: a waiting/empty/error state for a list view, a "this
//! item was removed" gone state for a detail view, or a stubbed
//! not-yet-implemented placeholder screen (`crate::app::placeholder_screen`)
//! can all reuse this same widget rather than each hand-rolling their own
//! centered icon/headline/subline layout — three or more call sites is
//! exactly the point past which "just leave it wherever it happened to be
//! written" stops
//! being good enough, per the ADR's "shared `MessageView` widget" seam.
//!
//! A real [`Widget`] (not just a free function) so it can be dropped
//! directly into a [`super::screen::Screen`]'s widget list (the
//! placeholder-screen use case) as well as called inline from another
//! widget's own `render` (the list/detail "content state" use case) —
//! both call sites just need `Widget::render`'s exact signature, which
//! this type provides either way.

// Identical allow (and rationale) as `pico_link_core::render`/`render::list`: this
// module does the same `embedded-graphics` `Point`(i32)/`Size`(u32)
// coordinate math directly (centering math over a widget-supplied area), so
// the same justification applies — no display this project targets is
// anywhere near large enough for these conversions to wrap, truncate, or
// lose a sign in practice.
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use alloc::string::String;
use core::convert::Infallible;

use embedded_graphics::{
    draw_target::DrawTargetExt,
    pixelcolor::Rgb565,
    prelude::{Point, Size},
    primitives::Rectangle,
};
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};

use super::framebuffer::FrameBuffer565;
use super::list::{name_top_offset, username_top_offset};
use super::theme::{font, palette};
use super::widget::Widget;

/// Extra top padding (beyond the label's own baseline offset) before the
/// first line of the message, so it isn't glued to the chrome's title-bar
/// boundary (or, for a placeholder screen, the top of the content area).
const MESSAGE_TOP_PADDING: i32 = 16;

/// Approximate square footprint (px) of `font::icon_4x()` glyphs — u8g2
/// scales `open_iconic_all` in even multiples of its ~8px 1x unit, so 4x is
/// ~32px. Only used to budget vertical space before the headline; a few
/// pixels of slack either way just changes the gap, not correctness.
const MESSAGE_ICON_SIZE: i32 = 32;

/// Gap (px) between the icon (when present) and the headline text.
const MESSAGE_ICON_GAP: i32 = 12;

/// A centered "nothing to show here" (or "here's a stub") message: an
/// optional large icon, a bold headline, and an optional muted subline.
///
/// Every color defaults to the theme's normal reading (icon:
/// [`palette::BRAND_BRIGHT`], headline: [`palette::TEXT_PRIMARY`]) —
/// override only what a specific state needs (e.g. an error headline in
/// [`palette::STATUS_ERROR`]) via the `with_*` builders.
pub struct MessageView {
    icon: Option<char>,
    icon_color: Rgb565,
    headline: String,
    headline_color: Rgb565,
    subline: Option<String>,
}

impl MessageView {
    #[must_use]
    pub fn new(headline: impl Into<String>) -> Self {
        Self {
            icon: None,
            icon_color: palette::BRAND_BRIGHT,
            headline: headline.into(),
            headline_color: palette::TEXT_PRIMARY,
            subline: None,
        }
    }

    #[must_use]
    pub fn with_icon(mut self, icon: char) -> Self {
        self.icon = Some(icon);
        self
    }

    #[must_use]
    pub fn with_icon_color(mut self, color: Rgb565) -> Self {
        self.icon_color = color;
        self
    }

    #[must_use]
    pub fn with_headline_color(mut self, color: Rgb565) -> Self {
        self.headline_color = color;
        self
    }

    #[must_use]
    pub fn with_subline(mut self, subline: impl Into<String>) -> Self {
        self.subline = Some(subline.into());
        self
    }
}

impl Widget for MessageView {
    fn measure(&self, constraints: Size) -> Size {
        constraints
    }

    /// # Errors
    ///
    /// Returns `Infallible`'s uninhabited variant in practice — see
    /// [`Widget::render`]'s doc comment for why the `Result` return exists
    /// at all.
    fn render(&self, area: Rectangle, target: &mut FrameBuffer565) -> Result<(), Infallible> {
        render_message(area, self.icon, self.icon_color, &self.headline, self.headline_color, self.subline.as_deref(), target);
        Ok(())
    }
}

/// Draws a centered content message — an optional large icon, a headline,
/// and an optional subline — into `area`.
///
/// Horizontally centered (`HorizontalAlignment::Center`) per the approved
/// M1 design language: this reads as a deliberate "nothing to show here"
/// state, not a truncated list row.
///
/// Not `pub`: [`MessageView::render`] is the only (and sufficient) public
/// entry point — every caller either drops a `MessageView` into a
/// `Screen`'s widget list or calls `Widget::render` on one directly, so
/// this free function has no reason to be reachable independently.
fn render_message(
    area: Rectangle,
    icon: Option<char>,
    icon_color: Rgb565,
    headline: &str,
    headline_color: Rgb565,
    subline: Option<&str>,
    target: &mut FrameBuffer565,
) {
    let mut clipped = target.clipped(&area);
    let center_x = area.top_left.x + area.size.width as i32 / 2;

    let text_top = area.top_left.y
        + MESSAGE_TOP_PADDING
        + if icon.is_some() { MESSAGE_ICON_SIZE + MESSAGE_ICON_GAP } else { 0 };

    if let Some(icon_char) = icon {
        let mut buf = [0_u8; 4];
        let icon_str: &str = icon_char.encode_utf8(&mut buf);
        let _ = font::icon_4x().render_aligned(
            icon_str,
            Point::new(center_x, area.top_left.y + MESSAGE_TOP_PADDING),
            VerticalPosition::Top,
            HorizontalAlignment::Center,
            FontColor::Transparent(icon_color),
            &mut clipped,
        );
    }

    let _ = font::name().render_aligned(
        headline,
        Point::new(center_x, text_top + name_top_offset()),
        VerticalPosition::Top,
        HorizontalAlignment::Center,
        FontColor::Transparent(headline_color),
        &mut clipped,
    );

    if let Some(subline) = subline {
        let _ = font::username().render_aligned(
            subline,
            Point::new(center_x, text_top + username_top_offset()),
            VerticalPosition::Top,
            HorizontalAlignment::Center,
            FontColor::Transparent(palette::TEXT_SECONDARY),
            &mut clipped,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::FrameBuffer565;

    // Content-area height: screen height minus `chrome::TITLE_BAR_HEIGHT`
    // (16) and `chrome::HINT_BAR_HEIGHT` (18) on the 240x240 Pico Plus 2 W
    // panel (Epic B2) -- recomputed, not scaled, from the retired
    // 320x170 panel's own title/hint subtraction.
    const AREA: Rectangle = Rectangle::new(Point::new(0, 0), Size::new(240, 206));

    #[test]
    fn a_headline_only_message_draws_ink_in_its_default_color() {
        let mut fb = FrameBuffer565::new(240, 240);
        MessageView::new("Waiting for sync...").render(AREA, &mut fb).unwrap();

        let any_headline_ink = fb.pixels().any(|p| p.1 == palette::TEXT_PRIMARY);
        assert!(any_headline_ink, "the headline should have drawn something in the default text color");
    }

    #[test]
    fn an_overridden_headline_color_draws_in_that_color_instead() {
        let mut fb = FrameBuffer565::new(240, 240);
        MessageView::new("Sync error").with_headline_color(palette::STATUS_ERROR).render(AREA, &mut fb).unwrap();

        let any_error_ink = fb.pixels().any(|p| p.1 == palette::STATUS_ERROR);
        assert!(any_error_ink, "an overridden headline color should be visible");
        let any_default_ink = fb.pixels().any(|p| p.1 == palette::TEXT_PRIMARY);
        assert!(!any_default_ink, "the default headline color should not appear when overridden");
    }

    #[test]
    fn a_subline_is_optional_and_changes_the_rendered_output_when_present() {
        let mut fb_without = FrameBuffer565::new(240, 240);
        MessageView::new("Nothing here yet").render(AREA, &mut fb_without).unwrap();

        let mut fb_with = FrameBuffer565::new(240, 240);
        MessageView::new("Nothing here yet").with_subline("Check back later").render(AREA, &mut fb_with).unwrap();

        let without: Vec<Rgb565> = fb_without.pixels().map(|p| p.1).collect();
        let with: Vec<Rgb565> = fb_with.pixels().map(|p| p.1).collect();
        assert_ne!(without, with, "adding a subline must change the rendered output");
    }
}
