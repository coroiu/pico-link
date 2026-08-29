//! Fixed chrome regions: every screen is laid out as a title bar, a button
//! rail, and a content area. This is deliberately *not* a general layout
//! engine (see
//! `.planning/decisions/2026-08-11-ui-framework-reuse-vs-rewrite.md`, which
//! rejects both the old flexbox attempt and a new general-purpose one in
//! favor of "fixed chrome regions + linear stacks").
//!
//! The bar/rail sizes are fixed pixel constants, not resolution-derived
//! fractions — but the *regions* are computed from whatever screen size is
//! passed in, and nothing here (or in any widget) hardcodes a screen
//! dimension. The same [`compute_chrome`] call works for a 240x240 Pico
//! Plus 2 W panel, a 128x32 HUZZAH32 OLED, or an arbitrary test framebuffer.
//!
//! The hint bar this module used to carry (a free-text control legend) is
//! gone as of `pico-link-znb.5`/E2: the labelled button rail supersedes it
//! as the on-screen control legend — see
//! `.planning/design/2026-08-29-button-rail-edge-region.md` section 2 for
//! why a free-text hint and a labelled rail must not coexist (two legends
//! that can disagree is strictly worse than one).

use embedded_graphics::prelude::{Point, Size};
use embedded_graphics::primitives::Rectangle;

use crate::panel::{Edge, PanelOrientation, PANEL};

/// Height of the title bar, in pixels.
pub const TITLE_BAR_HEIGHT: u32 = 16;
/// Width of the button rail, in pixels — the design's `RAIL_WIDTH`. Chosen
/// so 224px of usable vertical band (240 - `TITLE_BAR_HEIGHT`) splits into
/// four 56px slots with no remainder; see
/// `render::rail::tests::slot_rects_have_no_remainder`.
pub const RAIL_WIDTH: u32 = 34;

/// The chrome regions a screen renders into: a full-width title bar, a
/// button rail along one edge (see [`PanelOrientation`]), and the content
/// area occupying the remainder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromeLayout {
    pub title: Rectangle,
    pub content: Rectangle,
    pub rail: Rectangle,
    /// The orientation this layout was computed for — carried on the
    /// struct so it reaches `Screen::render` (and, later, the rail
    /// drawing code) with zero extra plumbing beyond `compute_chrome`'s
    /// existing single call site in `Navigator::render`.
    pub orientation: PanelOrientation,
}

/// Splits `band` into a fixed-`width` strip along `edge` and the
/// remainder, saturating rather than panicking if `width` exceeds the
/// band's extent along that axis. Returns `(edge_rect, rest_rect)`.
///
/// Handles all four [`Edge`] values even though [`PanelOrientation`] ships
/// today with only `ButtonsRight`/`ButtonsLeft` (`Edge::Right`/`Edge::Left`)
/// — see the ORCHESTRATOR CORRECTION in
/// `.planning/design/2026-08-29-button-rail-edge-region.md`: the C GPIO
/// table's net transform is a quarter turn, not the half turn its own
/// comment claims, so a third or fourth edge may turn out to be real. A
/// left/right-only `carve_edge` would turn that discovery into a rewrite
/// of this function instead of a `PanelOrientation` variant addition.
#[must_use]
pub fn carve_edge(band: Rectangle, edge: Edge, width: u32) -> (Rectangle, Rectangle) {
    match edge {
        Edge::Left => {
            let strip_width = width.min(band.size.width);
            let rest_width = band.size.width - strip_width;
            let strip = Rectangle::new(band.top_left, Size::new(strip_width, band.size.height));
            let rest = Rectangle::new(
                Point::new(band.top_left.x + strip_width as i32, band.top_left.y),
                Size::new(rest_width, band.size.height),
            );
            (strip, rest)
        }
        Edge::Right => {
            let strip_width = width.min(band.size.width);
            let rest_width = band.size.width - strip_width;
            let strip = Rectangle::new(
                Point::new(band.top_left.x + rest_width as i32, band.top_left.y),
                Size::new(strip_width, band.size.height),
            );
            let rest = Rectangle::new(band.top_left, Size::new(rest_width, band.size.height));
            (strip, rest)
        }
        Edge::Top => {
            let strip_height = width.min(band.size.height);
            let rest_height = band.size.height - strip_height;
            let strip = Rectangle::new(band.top_left, Size::new(band.size.width, strip_height));
            let rest = Rectangle::new(
                Point::new(band.top_left.x, band.top_left.y + strip_height as i32),
                Size::new(band.size.width, rest_height),
            );
            (strip, rest)
        }
        Edge::Bottom => {
            let strip_height = width.min(band.size.height);
            let rest_height = band.size.height - strip_height;
            let strip = Rectangle::new(
                Point::new(band.top_left.x, band.top_left.y + rest_height as i32),
                Size::new(band.size.width, strip_height),
            );
            let rest = Rectangle::new(band.top_left, Size::new(band.size.width, rest_height));
            (strip, rest)
        }
    }
}

/// Computes the chrome regions for a screen of the given size, using
/// today's hardware [`PanelOrientation`] ([`PANEL`]). Saturates rather
/// than panicking on very small screens: a screen too short for a full
/// title bar / too narrow for a full rail just gets a squeezed (possibly
/// zero-size) region instead of an arithmetic overflow.
#[must_use]
pub fn compute_chrome(screen_size: Size) -> ChromeLayout {
    compute_chrome_for(screen_size, PANEL)
}

/// [`compute_chrome`], parameterised over orientation — exists for tests
/// that must observe both orientations (the mirror test) without
/// depending on which one [`PANEL`] currently holds.
#[must_use]
pub fn compute_chrome_for(screen_size: Size, orientation: PanelOrientation) -> ChromeLayout {
    let width = screen_size.width;
    let height = screen_size.height;

    let title_height = TITLE_BAR_HEIGHT.min(height);
    let remaining = height.saturating_sub(title_height);

    let title = Rectangle::new(Point::new(0, 0), Size::new(width, title_height));
    let band = Rectangle::new(Point::new(0, title_height as i32), Size::new(width, remaining));

    let (rail, content) = carve_edge(band, orientation.button_edge(), RAIL_WIDTH);

    ChromeLayout {
        title,
        content,
        rail,
        orientation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_240x240_buttons_right_match_the_design_exactly() {
        let chrome = compute_chrome_for(Size::new(240, 240), PanelOrientation::ButtonsRight);
        assert_eq!(chrome.title, Rectangle::new(Point::new(0, 0), Size::new(240, 16)));
        assert_eq!(chrome.rail, Rectangle::new(Point::new(206, 16), Size::new(34, 224)));
        assert_eq!(chrome.content, Rectangle::new(Point::new(0, 16), Size::new(206, 224)));
    }

    #[test]
    fn regions_240x240_buttons_left_match_the_design_exactly() {
        let chrome = compute_chrome_for(Size::new(240, 240), PanelOrientation::ButtonsLeft);
        assert_eq!(chrome.title, Rectangle::new(Point::new(0, 0), Size::new(240, 16)));
        assert_eq!(chrome.rail, Rectangle::new(Point::new(0, 16), Size::new(34, 224)));
        assert_eq!(chrome.content, Rectangle::new(Point::new(34, 16), Size::new(206, 224)));
    }

    #[test]
    fn title_full_width_is_preserved_the_rail_and_content_split_the_remainder() {
        let chrome = compute_chrome(Size::new(240, 240));
        // Title bar spans the full width, untouched by the rail carve —
        // deliberate per the design (`carve order: title off the top ...
        // rail off one side of the remaining band`), so the shield/
        // readout/link-glyph/status-dot arithmetic in `Screen::render`
        // (all relative to the title bar) is byte-for-byte unaffected by
        // this bead.
        assert_eq!(chrome.title.size.width, 240);
        assert_eq!(chrome.title.size.height, chrome.title.size.height.min(240));

        // Invariants replacing the old three-region full-width test: the
        // rail and content no longer share the title's full width (that
        // was the whole point), but together they still exactly fill the
        // band beneath the title.
        assert_eq!(chrome.title.size.height + chrome.content.size.height, 240);
        assert_eq!(chrome.rail.top_left.y, chrome.content.top_left.y);
        assert_eq!(chrome.rail.size.height, chrome.content.size.height);
        assert_eq!(chrome.rail.size.width + chrome.content.size.width, chrome.title.size.width);
    }

    #[test]
    fn rail_and_content_never_overlap() {
        for orientation in [PanelOrientation::ButtonsRight, PanelOrientation::ButtonsLeft] {
            let chrome = compute_chrome_for(Size::new(240, 240), orientation);
            let rail_right = chrome.rail.top_left.x + chrome.rail.size.width as i32;
            let content_right = chrome.content.top_left.x + chrome.content.size.width as i32;
            // Either the rail is entirely left of the content, or entirely
            // right of it -- never interleaved.
            let disjoint = rail_right <= chrome.content.top_left.x || content_right <= chrome.rail.top_left.x;
            assert!(disjoint, "rail {:?} and content {:?} overlap under {orientation:?}", chrome.rail, chrome.content);
        }
    }

    #[test]
    fn no_literal_resolution_is_baked_in_128x32_also_lays_out_cleanly() {
        let chrome = compute_chrome(Size::new(128, 32));
        assert_eq!(chrome.title, Rectangle::new(Point::new(0, 0), Size::new(128, 16)));
        assert_eq!(chrome.rail, Rectangle::new(Point::new(94, 16), Size::new(34, 16)));
        assert_eq!(chrome.content, Rectangle::new(Point::new(0, 16), Size::new(94, 16)));
    }

    #[test]
    fn tiny_screen_does_not_panic_or_underflow() {
        let chrome = compute_chrome(Size::new(10, 5));
        assert_eq!(chrome.title, Rectangle::new(Point::new(0, 0), Size::new(10, 5)));
        assert_eq!(chrome.rail, Rectangle::new(Point::new(0, 5), Size::new(10, 0)));
        assert_eq!(chrome.content, Rectangle::new(Point::new(0, 5), Size::new(0, 0)));
    }

    // --- carve_edge: all four edges, not just the two PanelOrientation ships today ---

    #[test]
    fn carve_edge_left() {
        let band = Rectangle::new(Point::new(0, 16), Size::new(240, 224));
        let (strip, rest) = carve_edge(band, Edge::Left, 34);
        assert_eq!(strip, Rectangle::new(Point::new(0, 16), Size::new(34, 224)));
        assert_eq!(rest, Rectangle::new(Point::new(34, 16), Size::new(206, 224)));
    }

    #[test]
    fn carve_edge_right() {
        let band = Rectangle::new(Point::new(0, 16), Size::new(240, 224));
        let (strip, rest) = carve_edge(band, Edge::Right, 34);
        assert_eq!(strip, Rectangle::new(Point::new(206, 16), Size::new(34, 224)));
        assert_eq!(rest, Rectangle::new(Point::new(0, 16), Size::new(206, 224)));
    }

    #[test]
    fn carve_edge_top() {
        let band = Rectangle::new(Point::new(0, 16), Size::new(240, 224));
        let (strip, rest) = carve_edge(band, Edge::Top, 18);
        assert_eq!(strip, Rectangle::new(Point::new(0, 16), Size::new(240, 18)));
        assert_eq!(rest, Rectangle::new(Point::new(0, 34), Size::new(240, 206)));
    }

    #[test]
    fn carve_edge_bottom() {
        let band = Rectangle::new(Point::new(0, 16), Size::new(240, 224));
        let (strip, rest) = carve_edge(band, Edge::Bottom, 18);
        assert_eq!(strip, Rectangle::new(Point::new(0, 222), Size::new(240, 18)));
        assert_eq!(rest, Rectangle::new(Point::new(0, 16), Size::new(240, 206)));
    }

    #[test]
    fn carve_edge_saturates_instead_of_underflowing() {
        let band = Rectangle::new(Point::new(0, 0), Size::new(10, 10));
        let (strip, rest) = carve_edge(band, Edge::Right, 34);
        assert_eq!(strip, Rectangle::new(Point::new(0, 0), Size::new(10, 10)));
        assert_eq!(rest, Rectangle::new(Point::new(0, 0), Size::new(0, 10)));
    }
}
