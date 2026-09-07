//! [`Spacer`]: a non-focusable, non-drawing widget that reserves a fixed
//! vertical height in a [`super::screen::Screen`]'s widget stack.
//!
//! `Screen` has no padding/margin concept of its own -- it stacks its
//! widgets from the top of the content region with no gap (see
//! `Screen::render`'s layout step). Most screens don't need one: a list or
//! menu fills its whole content region and draws its own internal
//! spacing. The device page is the first screen that needs a fixed gutter
//! *before* its first row that isn't itself a widget's own internal
//! concern (`.planning/design/2026-09-07-device-page-and-single-select-
//! picker.md` §3.3), so this is the general, reusable primitive for that
//! rather than a one-off inset bolted onto `FieldList`.
//!
//! Safe to draw nothing: `Screen::render` fills the whole frame damage
//! rect with [`palette::BACKGROUND`] before drawing any widget (see that
//! method's fill-then-draw step), so a `Spacer`'s area is already the
//! background colour by the time its (empty) `render` would run.

use core::convert::Infallible;

use embedded_graphics::prelude::Size;
use embedded_graphics::primitives::Rectangle;

use super::ctx::RenderCtx;
use super::framebuffer::FrameBuffer565;
use super::paint_key::PaintKey;
use super::widget::Widget;

/// Seed for [`Spacer::paint_key`] -- only needs to differ from other
/// widgets' own seeds (each screen only ever compares a widget's key
/// against its own previous frame, per `PaintKey::fold`'s doc comment).
const SPACER_PAINT_KEY_SEED: u64 = 14;

/// A fixed-height, invisible widget -- see the module doc comment.
pub struct Spacer {
    height: u32,
}

impl Spacer {
    /// Reserves `height` pixels of vertical space in the widget stack it's
    /// placed in.
    #[must_use]
    pub const fn new(height: u32) -> Self {
        Self { height }
    }
}

impl Widget for Spacer {
    /// Reserves exactly `self.height`, at whatever width it's given --
    /// this widget has no horizontal opinion, only a vertical one.
    fn measure(&self, constraints: Size, _ctx: &RenderCtx) -> Size {
        Size::new(constraints.width, self.height)
    }

    /// Draws nothing -- see the module doc comment for why that's safe.
    fn render(&self, _area: Rectangle, _ctx: &RenderCtx, _target: &mut FrameBuffer565) -> Result<(), Infallible> {
        Ok(())
    }

    /// A constant folding only `height`: nothing about a `Spacer`'s
    /// (nonexistent) pixels ever changes, so this is never the reason a
    /// screen repaints, and unlike the default `PaintKey::ALWAYS` it
    /// doesn't force one either. `redraw_after` is not overridden (stays
    /// `None`), so per the mechanical review rule this must not fold time
    /// -- it doesn't.
    fn paint_key(&self, _ctx: &RenderCtx) -> PaintKey {
        PaintKey::of(SPACER_PAINT_KEY_SEED).fold(u64::from(self.height))
    }

    // `is_focusable` is not overridden -- the default `false` is correct:
    // a `Spacer` is furniture, never a focus stop.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Instant;
    use crate::render::chrome::compute_chrome;
    use crate::render::screen::Screen;
    use alloc::boxed::Box;
    use alloc::rc::Rc;
    use alloc::vec;
    use core::cell::Cell;

    fn test_ctx() -> RenderCtx {
        RenderCtx::at(Instant::from_micros(0))
    }

    /// A minimal widget that records its own area's top-left `y` into a
    /// shared cell, so a test can observe exactly where `Screen::render`
    /// placed it -- the thing this module's layout test needs to prove
    /// about `Spacer`.
    struct ProbeWidget {
        height: u32,
        top_seen: Rc<Cell<Option<i32>>>,
    }

    impl Widget for ProbeWidget {
        fn measure(&self, constraints: Size, _ctx: &RenderCtx) -> Size {
            Size::new(constraints.width, self.height)
        }

        fn render(&self, area: Rectangle, _ctx: &RenderCtx, _target: &mut FrameBuffer565) -> Result<(), Infallible> {
            self.top_seen.set(Some(area.top_left.y));
            Ok(())
        }
    }

    #[test]
    fn spacer_measure_reports_its_fixed_height_at_any_width() {
        let spacer = Spacer::new(12);
        let ctx = test_ctx();
        assert_eq!(spacer.measure(Size::new(205, 1000), &ctx), Size::new(205, 12));
        assert_eq!(spacer.measure(Size::new(1, 1000), &ctx), Size::new(1, 12));
    }

    #[test]
    fn spacer_is_never_focusable() {
        assert!(!Spacer::new(12).is_focusable());
    }

    #[test]
    fn spacer_paint_key_is_stable_across_calls_and_varies_with_height() {
        let ctx = test_ctx();
        let a = Spacer::new(12).paint_key(&ctx);
        let b = Spacer::new(12).paint_key(&ctx);
        let c = Spacer::new(13).paint_key(&ctx);
        assert!(a == b, "two Spacers with the same height must compare equal");
        assert!(a != c, "two Spacers with different heights must compare unequal");
    }

    /// The test §3.3's fix exists for: a `Spacer(12)` placed above another
    /// widget in a real, rendered `Screen` puts that widget's area top at
    /// `y = 28` (16px title bar + 12px gutter), matching `fields.rs`'s own
    /// budget tests' assumption that row 1 starts at `y = 28`.
    #[test]
    fn spacer_above_a_widget_pushes_its_top_to_16_plus_the_spacer_height() {
        let top_seen = Rc::new(Cell::new(None));
        let probe = ProbeWidget { height: 100, top_seen: Rc::clone(&top_seen) };
        let mut screen = Screen::new("Device", vec![Box::new(Spacer::new(12)), Box::new(probe)]);
        screen.initialize_focus();
        let chrome = compute_chrome(Size::new(240, 240));
        let mut fb = FrameBuffer565::new(240, 240);
        screen.render(&chrome, false, &test_ctx(), true, &mut fb).unwrap();
        assert_eq!(top_seen.get(), Some(28), "row content must start at y=28 (16px title bar + 12px spacer)");
    }
}
