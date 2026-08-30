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
}

impl RenderCtx {
    /// Builds a `RenderCtx` for a frame rendered at `now`.
    #[must_use]
    pub const fn at(now: Instant) -> Self {
        Self { now }
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
