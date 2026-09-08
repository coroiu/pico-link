//! The Home fault strip's glyph column: three 7x7 shape **primitives** —
//! up triangle, down triangle, a 5x5-centred square — each in filled and
//! 1px-outline form (design `.planning/design/2026-09-07-home-fault-
//! strip.md` §3, §12 Fern item 2). Bead `pico-link-9eq2.3.3`.
//!
//! Deliberately **primitives, not font glyphs**: `helvB08_tf` has no
//! reliable arrow glyphs, and — per the bead's explicit instruction —
//! deliberately **not** `super::theme::draw_selection`'s accent helper,
//! which draws a different 4px accent bar for a different purpose;
//! overloading it with a second meaning would force a branch at every
//! future read site of that function.
//!
//! Direction mapping (design §3): the up triangle means "the buffer
//! filled" (over-run), the down triangle means "the buffer starved"
//! (under-run), and the square means "not a level fault" (link/congestion/
//! resync). Fill (filled vs outline) carries freshness, not direction: a
//! filled glyph is Live, an outline glyph is Recent — see
//! `super::hero`'s `FaultTier`.

use core::convert::Infallible;

use embedded_graphics::draw_target::DrawTarget;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::{Point, Primitive, Size};
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, Triangle};
use embedded_graphics::Drawable;

/// The three glyph shapes (design §3's table) — deliberately capped at
/// three; a fourth is named in the design as "a design regression".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultGlyphShape {
    /// Up triangle — "the level went up past the top" (over-run/overflow).
    Up,
    /// Down triangle — "the level went down past the bottom" (under-run/
    /// starvation).
    Down,
    /// 5x5 square, vertically centred in the 7x7 box — a non-directional
    /// fault (link/congestion/resync).
    Square,
}

/// The 7x7 box's edge length (px) every glyph is drawn inside, per design
/// §3's table.
pub const FAULT_GLYPH_SIZE: u32 = 7;

/// Draws one fault glyph with its top-left corner at `top_left`, in
/// `color`, filled (`Live` tier) or outlined 1px (`Recent` tier) per
/// `filled`.
///
/// # Errors
///
/// Returns `Infallible`'s uninhabited variant in practice — `D::Error` is
/// `Infallible` for every real caller (the framebuffer target), matching
/// every other drawing function in this render core.
pub fn draw_fault_glyph<D>(target: &mut D, top_left: Point, shape: FaultGlyphShape, filled: bool, color: Rgb565) -> Result<(), Infallible>
where
    D: DrawTarget<Color = Rgb565, Error = Infallible>,
{
    match shape {
        FaultGlyphShape::Square => {
            // 5x5, centred in the 7x7 box: a 1px inset on every side.
            let rect = Rectangle::new(top_left + Point::new(1, 1), Size::new(5, 5));
            if filled {
                rect.into_styled(PrimitiveStyle::with_fill(color)).draw(target)?;
            } else {
                rect.into_styled(PrimitiveStyle::with_stroke(color, 1)).draw(target)?;
            }
        }
        FaultGlyphShape::Up => {
            // Apex at top-centre, base along the bottom edge — "the level
            // went up past the top".
            let triangle = Triangle::new(top_left + Point::new(3, 0), top_left + Point::new(0, 6), top_left + Point::new(6, 6));
            if filled {
                triangle.into_styled(PrimitiveStyle::with_fill(color)).draw(target)?;
            } else {
                triangle.into_styled(PrimitiveStyle::with_stroke(color, 1)).draw(target)?;
            }
        }
        FaultGlyphShape::Down => {
            // Apex at bottom-centre, base along the top edge — "the level
            // went down past the bottom".
            let triangle = Triangle::new(top_left + Point::new(0, 0), top_left + Point::new(6, 0), top_left + Point::new(3, 6));
            if filled {
                triangle.into_styled(PrimitiveStyle::with_fill(color)).draw(target)?;
            } else {
                triangle.into_styled(PrimitiveStyle::with_stroke(color, 1)).draw(target)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::framebuffer::FrameBuffer565;

    fn render(shape: FaultGlyphShape, filled: bool) -> FrameBuffer565 {
        let mut fb = FrameBuffer565::new(16, 16);
        draw_fault_glyph(&mut fb, Point::new(4, 4), shape, filled, Rgb565::new(31, 31, 31)).unwrap();
        fb
    }

    fn lit_pixel_count(fb: &FrameBuffer565) -> usize {
        fb.pixels().filter(|p| p.1 == Rgb565::new(31, 31, 31)).count()
    }

    #[test]
    fn every_shape_draws_at_least_one_lit_pixel_filled_and_outline() {
        for shape in [FaultGlyphShape::Up, FaultGlyphShape::Down, FaultGlyphShape::Square] {
            assert!(lit_pixel_count(&render(shape, true)) > 0, "{shape:?} filled must draw ink");
            assert!(lit_pixel_count(&render(shape, false)) > 0, "{shape:?} outline must draw ink");
        }
    }

    #[test]
    fn a_filled_shape_draws_strictly_more_ink_than_its_own_outline() {
        // The defining visual difference between Live (filled) and Recent
        // (outline) freshness (design §3) -- proven per shape rather than
        // assumed from the drawing calls above.
        for shape in [FaultGlyphShape::Up, FaultGlyphShape::Down, FaultGlyphShape::Square] {
            let filled = lit_pixel_count(&render(shape, true));
            let outline = lit_pixel_count(&render(shape, false));
            assert!(filled > outline, "{shape:?}: filled ({filled}) must draw more ink than its outline ({outline})");
        }
    }

    #[test]
    fn up_and_down_triangles_are_visually_distinct() {
        // The whole point of the glyph column (design §2): the two
        // directional shapes must not coincide pixel-for-pixel.
        let up: alloc::vec::Vec<_> = render(FaultGlyphShape::Up, true).pixels().collect();
        let down: alloc::vec::Vec<_> = render(FaultGlyphShape::Down, true).pixels().collect();
        assert_ne!(up, down, "the up and down triangles must render differently");
    }

    #[test]
    fn the_square_is_not_confusable_with_either_triangle() {
        let square: alloc::vec::Vec<_> = render(FaultGlyphShape::Square, true).pixels().collect();
        let up: alloc::vec::Vec<_> = render(FaultGlyphShape::Up, true).pixels().collect();
        let down: alloc::vec::Vec<_> = render(FaultGlyphShape::Down, true).pixels().collect();
        assert_ne!(square, up);
        assert_ne!(square, down);
    }

    #[test]
    fn every_glyph_stays_within_its_7x7_box() {
        // No shape may draw outside the `FAULT_GLYPH_SIZE`x`FAULT_GLYPH_SIZE`
        // box at `top_left` -- a glyph bleeding into the name column would
        // corrupt the row anatomy design §4 specifies.
        let origin = Point::new(4, 4);
        for shape in [FaultGlyphShape::Up, FaultGlyphShape::Down, FaultGlyphShape::Square] {
            for filled in [true, false] {
                let fb = render(shape, filled);
                for pixel in fb.pixels() {
                    let (point, color) = (pixel.0, pixel.1);
                    if color == Rgb565::new(31, 31, 31) {
                        assert!(
                            point.x >= origin.x
                                && point.x < origin.x + FAULT_GLYPH_SIZE as i32
                                && point.y >= origin.y
                                && point.y < origin.y + FAULT_GLYPH_SIZE as i32,
                            "{shape:?} filled={filled} drew outside its 7x7 box at {point:?}"
                        );
                    }
                }
            }
        }
    }
}
