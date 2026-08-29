//! The button rail: a labelled A/B/X/Y legend drawn along the chrome's
//! [`super::chrome::ChromeLayout::rail`] region on every screen, per
//! `.planning/design/2026-08-29-button-rail-edge-region.md`. Self-contained
//! — the only place slot geometry exists.

use alloc::string::String;
use core::convert::Infallible;

use embedded_graphics::draw_target::DrawTargetExt;
use embedded_graphics::prelude::{Point, Primitive, Size};
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use embedded_graphics::Drawable;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};

pub use crate::panel::Button;
use crate::panel::PanelOrientation;

use super::framebuffer::FrameBuffer565;
use super::theme::{font, palette};

/// A single button's label state for the rail.
///
/// The `Option<ButtonLabel>` this is wrapped in on [`ButtonLabels`] and
/// [`super::widget::ChromeContribution`] is deliberate and must not be
/// flattened to `Option<String>`: `None` at the contribution level means
/// "no opinion, defer to the screen's static label", which is a distinct
/// third state from both variants here. See the design doc section 4's
/// three-row table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ButtonLabel {
    /// This button does nothing on this screen right now. Dim letter, no
    /// text — per design rule 2, an inert X/Y is how the four-button
    /// contract stays honest (a mispress on a dim button is always free).
    Inert,
    /// Live and labelled. Budget at most 5 characters — see
    /// [`draw_rail`]'s clipping discipline; every label in the design fits
    /// (`devs`, `link`, `set`, `back`, `why?`).
    Live(String),
}

/// The four buttons' resolved [`ButtonLabel`]s, keyed by logical
/// [`Button`] identity (not by physical slot position — [`draw_rail`]
/// looks each one up through [`PanelOrientation::slot_order`], which is
/// what makes a `PANEL` flip move the *label* with the button instead of
/// leaving it in a now-wrong slot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ButtonLabels {
    pub a: ButtonLabel,
    pub b: ButtonLabel,
    pub x: ButtonLabel,
    pub y: ButtonLabel,
}

impl ButtonLabels {
    #[must_use]
    pub fn get(&self, button: Button) -> &ButtonLabel {
        match button {
            Button::A => &self.a,
            Button::B => &self.b,
            Button::X => &self.x,
            Button::Y => &self.y,
        }
    }
}

impl Default for ButtonLabels {
    fn default() -> Self {
        Self {
            a: ButtonLabel::Inert,
            b: ButtonLabel::Inert,
            x: ButtonLabel::Inert,
            y: ButtonLabel::Inert,
        }
    }
}

/// Letter glyph for a logical button, drawn at the top of its slot
/// regardless of live/inert state (an inert button still teaches the user
/// which letter it is; only its color and the text line beneath it
/// change).
const fn letter(button: Button) -> &'static str {
    match button {
        Button::A => "A",
        Button::B => "B",
        Button::X => "X",
        Button::Y => "Y",
    }
}

/// Draws the button rail into `rail` (a
/// [`super::chrome::ChromeLayout::rail`] rect), one 56px-tall slot per
/// button in `orientation`'s physical top-to-bottom
/// [`PanelOrientation::slot_order`], each looked up in `labels` by logical
/// identity.
///
/// Draws nothing if `rail` is zero-size (the saturated-layout case on a
/// screen too small for a rail) or has fewer than 4 rows to split among
/// slots — callers should guard on `rail.size.width > 0` per the design's
/// migration plan, but this function is safe to call unconditionally
/// either way.
///
/// # Errors
///
/// Returns `Infallible`'s uninhabited variant in practice — see
/// `Widget::render`'s doc comment for why the `Result` return exists at
/// all.
pub fn draw_rail(
    rail: Rectangle,
    orientation: PanelOrientation,
    labels: &ButtonLabels,
    target: &mut FrameBuffer565,
) -> Result<(), Infallible> {
    if rail.size.width == 0 || rail.size.height == 0 {
        return Ok(());
    }

    let slot_order = orientation.slot_order();
    let slot_count = slot_order.len() as u32;
    let slot_height = rail.size.height / slot_count;
    if slot_height == 0 {
        // Too short to give every slot at least one row; drawing would
        // just produce overlapping/zero-height garbage. Saturate to
        // nothing rather than guess.
        return Ok(());
    }

    // Inner-edge hairline, running the rail's full height, one pixel wide
    // on whichever side borders the content region.
    let hairline_x = match orientation.button_edge() {
        crate::panel::Edge::Left => rail.top_left.x + rail.size.width as i32 - 1,
        // `Right`'s hairline sits at the rail's own left column, same as
        // the `Top`/`Bottom` defensive fallback below (the rail is only
        // ever carved along Left/Right today -- see `carve_edge`'s
        // all-four-edges doc comment).
        crate::panel::Edge::Right | crate::panel::Edge::Top | crate::panel::Edge::Bottom => rail.top_left.x,
    };
    let hairline = Rectangle::new(Point::new(hairline_x, rail.top_left.y), Size::new(1, rail.size.height));
    hairline.into_styled(PrimitiveStyle::with_fill(palette::DIVIDER)).draw(target)?;

    let letter_font = font::label();
    let text_font = font::hint();

    for (i, &button) in slot_order.iter().enumerate() {
        let slot_top = rail.top_left.y + (i as u32 * slot_height) as i32;
        let slot_rect = Rectangle::new(Point::new(rail.top_left.x, slot_top), Size::new(rail.size.width, slot_height));

        // Horizontal hairline between slots (not before the first).
        if i > 0 {
            let divider = Rectangle::new(Point::new(rail.top_left.x, slot_top), Size::new(rail.size.width, 1));
            divider.into_styled(PrimitiveStyle::with_fill(palette::DIVIDER)).draw(target)?;
        }

        let label = labels.get(button);
        let (letter_color, text) = match label {
            ButtonLabel::Inert => (palette::DIVIDER, None),
            ButtonLabel::Live(s) => (palette::TEXT_SECONDARY, Some(s.as_str())),
        };

        let slot_mid_x = slot_rect.top_left.x + slot_rect.size.width as i32 / 2;
        let slot_mid_y = slot_rect.top_left.y + slot_rect.size.height as i32 / 2;

        // All drawing for this slot goes through a clip on the slot's own
        // rect: the overflow-safety requirement from the design (a long
        // label must never bleed past the rail's own edge, let alone into
        // an adjacent slot or the content region).
        let mut slot_target = target.clipped(&slot_rect);

        let _ = letter_font.render_aligned(
            letter(button),
            Point::new(slot_mid_x, slot_mid_y - 7),
            VerticalPosition::Center,
            HorizontalAlignment::Center,
            FontColor::Transparent(letter_color),
            &mut slot_target,
        );

        if let Some(text) = text {
            let _ = text_font.render_aligned(
                text,
                Point::new(slot_mid_x, slot_mid_y + 7),
                VerticalPosition::Center,
                HorizontalAlignment::Center,
                FontColor::Transparent(palette::TEXT_SECONDARY),
                &mut slot_target,
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::chrome::compute_chrome_for;
    use embedded_graphics::prelude::Size as EgSize;

    #[test]
    fn slot_rects_have_no_remainder_on_a_240x240_panel() {
        let chrome = compute_chrome_for(EgSize::new(240, 240), PanelOrientation::ButtonsRight);
        // 224 / 4 == 56 exactly -- the design's stated no-remainder split.
        assert_eq!(chrome.rail.size.height, 224);
        assert_eq!(chrome.rail.size.height / 4, 56);
    }
}
