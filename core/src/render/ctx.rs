//! Frame-scoped facts every widget may read while drawing.
//!
//! `RenderCtx` is sampled **once per frame** by `App::render` and threaded
//! through the entire render call (`measure`, `render`,
//! `chrome_contribution`), so every widget and the chrome observe the SAME
//! instant. A pull-based clock (each widget calling `Clock::now()` on its
//! own) cannot promise that, and the tearing that results is exactly what
//! this type exists to prevent.
//!
//! See `.planning/decisions/2026-08-31-render-ctx-frame-scoped-clock.md` for
//! the full design and the alternatives it rejects (a `Clock` trait object,
//! time folded into the model, an `Rc<Cell<Instant>>` mailbox). The seam
//! choice is settled there — this module is the implementation, not a place
//! to re-litigate it.

use core::time::Duration;

use embedded_graphics::prelude::Size;
use embedded_graphics::primitives::Rectangle;

use crate::platform::Instant;

/// Frame-scoped facts every widget may read while drawing.
///
/// `Copy`, 8 bytes, no heap — cheap enough to pass by value everywhere.
/// `#[non_exhaustive]` plus a constructor and accessors so the next
/// frame-scoped fact (dim state, a reduce-motion setting, a frame counter)
/// is an additive field, not another break of every widget signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RenderCtx {
    now: Instant,
    /// The frame damage rect, or `None` for "no clip declared, draw
    /// everything". See [`Self::with_damage`] / [`Self::damage`] /
    /// [`Self::needs`] and
    /// `.planning/design/2026-09-06-damage-rect-render-and-partial-blit.md`
    /// section 4. **There is no producer of `Some` yet** (bead
    /// `pico-link-7h5.3` is additive-only); [`Self::at`] always leaves
    /// this `None`, which is what keeps every existing widget and test
    /// unaffected.
    ///
    /// This field is a hint for widgets to SKIP drawing sub-regions that
    /// cannot have changed -- it is not a clip rectangle to hand to
    /// `embedded_graphics::draw_target::DrawTargetExt::clipped()`.
    /// `clipped()` still shapes and rasterises everything upstream of the
    /// `DrawTarget` boundary and only discards the resulting writes, so it
    /// saves memory stores and essentially none of the CPU that motivates
    /// this design (see design doc section 1). A widget should use
    /// [`Self::needs`] to decide whether to call into its own drawing code
    /// at all, not merely to narrow what a drawing call clips to.
    damage: Option<Rectangle>,
}

impl RenderCtx {
    /// Builds a `RenderCtx` for a frame rendered at `now`, with no damage
    /// rect declared (`damage: None`, meaning "draw everything"). This is
    /// deliberate: it is what keeps the ~250 existing tests, and every
    /// widget that does not yet consult `damage()`/`needs()`, compiling
    /// and passing unchanged.
    #[must_use]
    pub const fn at(now: Instant) -> Self {
        Self { now, damage: None }
    }

    /// Returns a copy of this `RenderCtx` with its damage rect set to `r`.
    /// No producer exists yet (bead `pico-link-7h5.3` is additive-only);
    /// the frame damage pass that calls this lands in `pico-link-7h5.4`.
    #[must_use]
    pub const fn with_damage(self, r: Rectangle) -> Self {
        Self {
            damage: Some(r),
            ..self
        }
    }

    /// The current frame's damage rect, if one has been declared. `None`
    /// means no clip was declared for this frame -- treat the whole
    /// widget as needing a redraw, exactly like today's behaviour.
    #[must_use]
    pub const fn damage(&self) -> Option<Rectangle> {
        self.damage
    }

    /// True when `area` could contribute visible pixels this frame --
    /// i.e. no damage rect is declared (`None` => always `true`, matching
    /// today's "redraw everything" behaviour) or `area` intersects the
    /// declared damage rect.
    ///
    /// This is the SKIP primitive the design calls for (section 1, section
    /// 4): a widget calls this to decide whether to run its own drawing
    /// code for a sub-region at all, rather than running it unconditionally
    /// and relying on `DrawTargetExt::clipped()` to discard the writes --
    /// the latter still pays for rasterising everything it discards.
    #[must_use]
    pub fn needs(&self, area: Rectangle) -> bool {
        match self.damage {
            None => true,
            Some(rect) => rect.intersection(&area).size != Size::zero(),
        }
    }

    /// The instant this frame is being rendered at.
    #[must_use]
    pub const fn now(&self) -> Instant {
        self.now
    }

    /// The elapsed [`Duration`] between this frame's instant and `earlier`,
    /// saturating to [`Duration::ZERO`] rather than underflowing if
    /// `earlier` is actually later than `self.now()` -- see
    /// [`Instant::saturating_duration_since`].
    #[must_use]
    pub const fn elapsed_since(&self, earlier: Instant) -> Duration {
        self.now.saturating_duration_since(earlier)
    }
}

#[cfg(test)]
mod tests {
    use super::RenderCtx;
    use crate::platform::Instant;
    use core::time::Duration;

    #[test]
    fn at_and_now_round_trip() {
        let now = Instant::from_micros(1_000);
        let ctx = RenderCtx::at(now);
        assert_eq!(ctx.now(), now);
    }

    #[test]
    fn elapsed_since_computes_forward_span() {
        let earlier = Instant::from_micros(1_000);
        let now = Instant::from_micros(1_500);
        let ctx = RenderCtx::at(now);
        assert_eq!(ctx.elapsed_since(earlier), Duration::from_micros(500));
    }

    #[test]
    fn elapsed_since_saturates_when_earlier_is_actually_later() {
        let earlier = Instant::from_micros(2_000);
        let now = Instant::from_micros(1_000);
        let ctx = RenderCtx::at(now);
        assert_eq!(ctx.elapsed_since(earlier), Duration::ZERO);
    }

    #[test]
    fn elapsed_since_same_instant_is_zero() {
        let now = Instant::from_micros(42);
        let ctx = RenderCtx::at(now);
        assert_eq!(ctx.elapsed_since(now), Duration::ZERO);
    }
}
