//! Panel orientation: the single knob that says which edge of the 240x240
//! panel the physical A/B/X/Y button column is mounted on, and therefore
//! which edge the button rail (`pico-link-znb.5`/E2) and — later — the
//! volume gauge (F5/E16) render against.
//!
//! Lives at the crate root, not under `render/`, because the follow-on bead
//! (unifying the GPIO->`NavIntent` rotation currently hard-coded into
//! `firmware/src/input.c` — see that file's `MADCTL=0xA0` table, marked
//! UNVERIFIED ON HARDWARE) will need `crate::input` to read this same
//! constant. `core/src/input.rs` itself carries no such constant today —
//! only the `NavIntent` enum — which is why that unification is a separate,
//! dependent bead rather than folded into this one.
//!
//! **Known discrepancy, read before "fixing" the two-value enum:** the
//! design doc this module implements
//! (`.planning/design/2026-08-29-button-rail-edge-region.md`) documents that
//! the C GPIO table's *actual* net transform (as opposed to what its own
//! comment claims) is a quarter turn, not a half turn — which would put the
//! button column on the top or bottom edge instead of left/right.
//! `PanelOrientation` ships with only `ButtonsRight`/`ButtonsLeft` anyway,
//! because the quarter-turn reading is unverified on hardware and the
//! product fact from Andreas is unambiguous (today: buttons in a column on
//! the right, d-pad on the left). [`carve_edge`] (in `render::chrome`)
//! nonetheless handles all four [`Edge`] values from day one, so widening
//! this enum later is additive, not a rewrite.

/// One of the four edges of a rectangular panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

/// A physical button identity, independent of where on screen it is
/// currently drawn. `slot_order()` maps *physical top-to-bottom position*
/// to one of these; labels are looked up by this identity, not by slot
/// index, which is what makes the mirror test
/// (`render::chrome::tests::mirroring_the_orientation_moves_both_axes`)
/// meaningful rather than cosmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    A,
    B,
    X,
    Y,
}

/// Which edge of the panel the physical A/B/X/Y button column is mounted
/// on. A 180-degree panel flip moves the buttons from one side to the
/// other *and* reverses their physical top-to-bottom order — see
/// [`PanelOrientation::slot_order`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelOrientation {
    /// Buttons in a column on the right edge, d-pad on the left. Today's
    /// hardware value.
    ButtonsRight,
    /// Buttons in a column on the left edge, d-pad on the right (a
    /// 180-degree panel flip from `ButtonsRight`).
    ButtonsLeft,
}

/// **TODAY'S VALUE. The single knob.** Flipping this must flip the rail
/// edge, the rail's vertical slot order, and (later) the gauge edge,
/// together — see the module doc comment for why this constant, and not a
/// hard-coded `Edge::Right`, is what `render::chrome::compute_chrome`
/// reads.
pub const PANEL: PanelOrientation = PanelOrientation::ButtonsRight;

impl PanelOrientation {
    /// The edge the button rail renders against.
    #[must_use]
    pub const fn button_edge(self) -> Edge {
        match self {
            PanelOrientation::ButtonsRight => Edge::Right,
            PanelOrientation::ButtonsLeft => Edge::Left,
        }
    }

    /// The edge the (future, Tier 2) volume gauge renders against —
    /// always the edge opposite the buttons. Exposed now so the gauge
    /// bead can call `render::chrome::carve_edge` a second time without
    /// this bead adding a `gauge` field or placeholder rect to
    /// `ChromeLayout`.
    #[must_use]
    pub const fn gauge_edge(self) -> Edge {
        match self {
            PanelOrientation::ButtonsRight => Edge::Left,
            PanelOrientation::ButtonsLeft => Edge::Right,
        }
    }

    /// The four buttons in physical top-to-bottom order on the rail.
    ///
    /// A 180-degree flip reverses this order, not just the edge: mounting
    /// the same physical button cluster upside-down on the opposite edge
    /// means the button that was physically topmost (A) is now physically
    /// bottommost. Flipping only `button_edge()` and keeping the same
    /// slot order would produce a rail that is mirrored horizontally but
    /// *lying* about the vertical order — exactly the failure mode this
    /// function exists to prevent.
    #[must_use]
    pub const fn slot_order(self) -> [Button; 4] {
        match self {
            PanelOrientation::ButtonsRight => [Button::A, Button::B, Button::X, Button::Y],
            PanelOrientation::ButtonsLeft => [Button::Y, Button::X, Button::B, Button::A],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_right_is_the_button_edge_and_left_is_the_gauge_edge() {
        assert_eq!(PanelOrientation::ButtonsRight.button_edge(), Edge::Right);
        assert_eq!(PanelOrientation::ButtonsRight.gauge_edge(), Edge::Left);
    }

    #[test]
    fn buttons_left_is_the_button_edge_and_right_is_the_gauge_edge() {
        assert_eq!(PanelOrientation::ButtonsLeft.button_edge(), Edge::Left);
        assert_eq!(PanelOrientation::ButtonsLeft.gauge_edge(), Edge::Right);
    }

    #[test]
    fn flipping_orientation_reverses_slot_order_not_just_edge() {
        assert_eq!(
            PanelOrientation::ButtonsRight.slot_order(),
            [Button::A, Button::B, Button::X, Button::Y]
        );
        assert_eq!(
            PanelOrientation::ButtonsLeft.slot_order(),
            [Button::Y, Button::X, Button::B, Button::A]
        );
    }

    #[test]
    fn todays_hardware_value_is_buttons_right() {
        assert_eq!(PANEL, PanelOrientation::ButtonsRight);
    }
}
