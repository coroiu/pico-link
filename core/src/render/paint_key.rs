//! A cheap, total summary of everything that affects a widget's pixels in a
//! given frame — the vocabulary the damage-rect render pass (bead
//! `pico-link-7h5`) uses to decide which widgets to skip.
//!
//! See `.planning/design/2026-09-06-damage-rect-render-and-partial-blit.md`
//! section 3 for the full design; this module implements section 3.1 only.
//! **There is no consumer yet.** [`crate::render::widget::Widget::paint_key`]
//! defaults to [`PaintKey::ALWAYS`], so nothing changes what gets painted
//! until a widget opts in (bead `pico-link-7h5.5`).
//!
//! # The contract
//!
//! Two renders of the same widget whose [`PaintKey`]s compare equal MUST
//! produce byte-identical pixels within that widget's area. Get this wrong
//! and a widget silently goes stale on screen — there is no test that can
//! catch a bad `paint_key` impl other than rendering and comparing pixels,
//! so treat every `paint_key` override as load-bearing correctness code,
//! not a performance nicety.
//!
//! # The mechanism this exists to serve is a SKIP, not a CLIP
//!
//! `PaintKey` (and the `RenderCtx::damage` it feeds, see `super::ctx`) exist
//! so `Screen::render` can avoid *calling* `Widget::render` at all for a
//! widget that cannot have changed. Do not reach for
//! `embedded_graphics::draw_target::DrawTargetExt::clipped()` as "the same
//! idea" — `clipped()` still shapes every glyph and walks its bitmap and
//! only discards the writes at the end, so it saves the memory stores and
//! essentially none of the CPU. The skip has to happen above `render`, by
//! never calling it, which is exactly what a `paint_key` comparison lets a
//! caller do cheaply.
//!
//! # The time trap
//!
//! A `paint_key` must fold the **quantised visual consequence** of time,
//! never the raw instant from `ctx.now()`:
//!
//! - Folding raw `now()` (e.g. `PaintKey::of(seed).fold(ctx.now().as_micros())`)
//!   makes the key different on literally every frame, so the widget is
//!   repainted every frame regardless — a silent no-op that "compiles and
//!   works" while buying nothing.
//! - Folding nothing time-related freezes a widget whose appearance is
//!   actually time-driven (an elapsed-seconds readout, the OUT meter's
//!   stale/live decision) — a visible bug, because nothing else will ever
//!   mark it dirty once its non-time state stops changing.
//!
//! The correct middle: fold the *decision* time produces, not time itself
//! — e.g. fold `sample.received_at` plus the already-computed boolean
//! `ctx.elapsed_since(received_at) >= STALE_AFTER`, or fold a whole-seconds
//! counter for a readout that only changes once a second.
//!
//! **Mechanical review rule:** every widget that overrides
//! [`crate::render::widget::Widget::redraw_after`] must fold time (in the
//! quantised sense above) into its `paint_key`, and no widget that does
//! not override `redraw_after` should fold time into its `paint_key` at
//! all. `redraw_after` is already the project's registry of "this widget's
//! appearance depends on the clock" (see that method's doc comment), so a
//! `paint_key` that disagrees with it is a bug in one of the two methods.

/// A cheap, total summary of everything that affects a widget's pixels in a
/// given frame. See the module docs for the full contract, the SKIP-not-CLIP
/// framing, and the time trap.
#[derive(Debug, Clone, Copy)]
pub struct PaintKey(Option<u64>);

impl PaintKey {
    /// Never equal to anything, including itself — the default every
    /// widget starts at. A widget compared against `ALWAYS` (on either
    /// side) is always treated as changed, which is what leaves every
    /// widget repainted every frame until it opts in to a real key (bead
    /// `pico-link-7h5.5`). Represented as `None` internally so the
    /// `PartialEq` impl below can special-case it without relying on a
    /// sentinel `u64` value that a real seed could theoretically collide
    /// with.
    pub const ALWAYS: PaintKey = PaintKey(None);

    /// Starts a new key folding in an initial `u64` seed. Typically a
    /// stable per-widget-kind discriminant, followed by `.fold(...)` calls
    /// for whatever state affects this widget's pixels this frame.
    #[must_use]
    pub const fn of(seed: u64) -> PaintKey {
        PaintKey(Some(seed))
    }

    /// Chains another `u64` into this key. Order matters (folding `a` then
    /// `b` need not equal folding `b` then `a`), so keep a widget's fold
    /// order stable across its own calls — comparing keys from two
    /// different widget *kinds* is meaningless anyway, since each screen
    /// only ever compares a widget's key against its own previous frame.
    #[must_use]
    pub const fn fold(self, v: u64) -> PaintKey {
        match self.0 {
            // ALWAYS folds to itself: nothing can turn "always dirty" into
            // a comparable key by accident.
            None => self,
            Some(acc) => {
                // A standard 64-bit mix (splitmix64's finalizer), chosen
                // only for cheap, well-distributed avalanche — this is not
                // a hash used for anything security-sensitive.
                let mut x = acc ^ v.wrapping_add(0x9E37_79B9_7F4A_7C15);
                x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                x ^= x >> 31;
                PaintKey(Some(x))
            }
        }
    }

    /// Chains a UTF-8 string into this key -- convenience over folding a
    /// string's bytes a `u64` at a time by hand. Internally a plain FNV-1a
    /// over the string's bytes: this project has no `std`/heap-hasher
    /// dependency to reach for, and a `paint_key` is not a hash used for
    /// anything security-sensitive, so a simple, well-known, allocation-free
    /// hash is exactly the right amount of machinery.
    #[must_use]
    pub fn fold_str(self, s: &str) -> PaintKey {
        self.fold(fnv1a(s.as_bytes()))
    }
}

const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

impl PartialEq for PaintKey {
    /// `ALWAYS == ALWAYS` is `false` — see [`PaintKey::ALWAYS`]'s doc
    /// comment. This is why `PaintKey` cannot derive `PartialEq`.
    fn eq(&self, other: &Self) -> bool {
        match (self.0, other.0) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for PaintKey {}

#[cfg(test)]
mod tests {
    use super::PaintKey;

    #[test]
    fn always_is_never_equal_to_itself() {
        #[allow(clippy::eq_op)]
        let equal = PaintKey::ALWAYS == PaintKey::ALWAYS;
        assert!(!equal);
    }

    #[test]
    fn always_is_never_equal_to_a_real_key() {
        assert_ne!(PaintKey::ALWAYS, PaintKey::of(42));
        assert_ne!(PaintKey::of(42), PaintKey::ALWAYS);
    }

    #[test]
    fn same_seed_and_fold_sequence_is_equal() {
        let a = PaintKey::of(1).fold(2).fold(3);
        let b = PaintKey::of(1).fold(2).fold(3);
        assert_eq!(a, b);
    }

    #[test]
    fn different_seed_is_not_equal() {
        assert_ne!(PaintKey::of(1), PaintKey::of(2));
    }

    #[test]
    fn different_folded_value_is_not_equal() {
        assert_ne!(PaintKey::of(1).fold(2), PaintKey::of(1).fold(3));
    }

    #[test]
    fn fold_order_can_matter() {
        // Not a hard guarantee for every pair of values, but true for this
        // pair, which is enough to document that order is significant and
        // callers must keep a stable fold order.
        let a = PaintKey::of(0).fold(1).fold(2);
        let b = PaintKey::of(0).fold(2).fold(1);
        assert_ne!(a, b);
    }

    #[test]
    fn fold_str_of_the_same_text_is_equal() {
        let a = PaintKey::of(1).fold_str("hello");
        let b = PaintKey::of(1).fold_str("hello");
        assert_eq!(a, b);
    }

    #[test]
    fn fold_str_of_different_text_is_not_equal() {
        assert_ne!(PaintKey::of(1).fold_str("hello"), PaintKey::of(1).fold_str("world"));
    }

    #[test]
    fn fold_str_of_empty_and_absent_are_distinguishable() {
        // Not a `fold_str` guarantee on its own -- folding a distinguishing
        // presence tag before `fold_str` (as `screen::fold_opt_str` does)
        // is what actually keeps `Some("")` from colliding with `None`.
        // This test just proves `fold_str("")` is a stable, well-defined
        // value in the first place (not e.g. a no-op equal to not folding
        // at all).
        assert_ne!(PaintKey::of(1).fold_str(""), PaintKey::of(1));
    }

    #[test]
    fn key_is_copy_and_clone() {
        let a = PaintKey::of(7);
        let b = a;
        #[allow(clippy::clone_on_copy)]
        let c = a.clone();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }
}
