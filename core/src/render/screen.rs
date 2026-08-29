//! `Screen`: one entry in the `Navigator`'s stack — a title, a set of
//! content widgets stacked vertically in the chrome's content region, and
//! this screen's own focus memory. Salvaged from
//! `simple_gui::document::Document`'s `ViewStackEntry` (title + components
//! + `focused_index`), reimplemented on `embedded-graphics`.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::convert::Infallible;

use embedded_graphics::{
    draw_target::DrawTargetExt,
    prelude::{Point, Primitive, Size},
    primitives::{Circle, PrimitiveStyle, Rectangle},
    Drawable,
};
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::FontRenderer;

use crate::app::LinkState;
use crate::input::NavIntent;

use super::chrome::ChromeLayout;
use super::framebuffer::FrameBuffer565;
use super::theme::{font, icon, palette};
use super::widget::{Action, ChromeContribution, ChromeStatus, FocusEvent, Widget};

/// Margin (px) from the title bar's left/right edges to its shield mark /
/// status dot — the "more air" half of Andreas's title-bar tweak (the
/// other half, using `font::icon_1x` instead of `icon_2x` for the mark
/// itself, lives in `theme.rs`).
const TITLE_SIDE_MARGIN: i32 = 6;
/// Gap (px) between adjacent title-bar elements (shield -> title text,
/// readout -> status dot).
const TITLE_ELEMENT_GAP: i32 = 6;
/// Diameter (px) of the title bar's sync-status dot.
const STATUS_DOT_DIAMETER: u32 = 6;
/// Left margin (px) for hint-bar text — bumped from the original `4` per
/// a "more padding" design-review tweak; the hint font is already the
/// smallest on screen, so it can afford a wider margin than body text
/// without feeling squeezed against the edge.
const HINT_SIDE_MARGIN: i32 = 8;

/// The horizontal pixel footprint `u8g2-fonts`' `render_aligned` would give
/// `text` in `font` — used to right-align/clip chrome elements without
/// hardcoding a per-character pixel width, so a later `u8g2-fonts` font
/// swap doesn't require recomputing these layouts by hand.
fn text_width(font: &FontRenderer, text: &str) -> u32 {
    font.get_rendered_dimensions_aligned(text, Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
        .unwrap_or(None)
        .map_or(0, |bbox| bbox.size.width)
}

/// Draws the title bar's shield mark, left-anchored at `title_left_x +
/// `[`TITLE_SIDE_MARGIN`], and returns the x position at which the title
/// text itself should start (the shield's right edge plus
/// [`TITLE_ELEMENT_GAP`]).
///
/// `icon_1x`, not `icon_2x` — see that accessor's doc comment for why
/// (Andreas's "smaller, more air" tweak). Centered on its own *ink*
/// bounding box, not `VerticalPosition::Center` (which centers on the
/// font's line metrics/ascent-descent budget, not this specific glyph's
/// ink) — the same mismatch `theme::draw_chip`'s doc comment describes for
/// the chip-letter fix, and the reason the shield reads visibly
/// off-center at `icon_1x`'s small size even though it looked fine at
/// `icon_2x`.
///
/// Split out of `render` (alongside [`draw_link_glyph`]) purely to keep
/// that function's line count in check.
fn draw_shield_mark(title_left_x: i32, title_mid_y: i32, target: &mut FrameBuffer565) -> i32 {
    let shield_font = font::icon_1x();
    let mut shield_buf = [0_u8; 4];
    let shield_str: &str = icon::SHIELD.encode_utf8(&mut shield_buf);
    let shield_x = title_left_x + TITLE_SIDE_MARGIN;
    let shield_ink = shield_font
        .get_rendered_dimensions_aligned(shield_str, Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
        .unwrap_or(None);
    let shield_y = shield_ink.map_or(title_mid_y, |ink| title_mid_y - (ink.top_left.y + ink.size.height as i32 / 2));
    let _ = shield_font.render_aligned(
        shield_str,
        Point::new(shield_x, shield_y),
        VerticalPosition::Top,
        HorizontalAlignment::Left,
        FontColor::Transparent(palette::BRAND_BRIGHT),
        target,
    );
    shield_x + text_width(&shield_font, shield_str) as i32 + TITLE_ELEMENT_GAP
}

/// Draws the A2DP/Bluetooth link glyph immediately left of
/// `right_cursor`, in a
/// color derived from `link_state`, and returns the updated `right_cursor`
/// after reserving this glyph's width plus [`TITLE_ELEMENT_GAP`] -- the
/// same "each title-bar element returns the next available cursor" shape
/// `Screen::render`'s status-dot/readout blocks use inline. Split out of
/// `render` itself (rather than inlined alongside those) purely to keep
/// that function's line count in check; there is nothing else this helper
/// needs to be independently reusable for.
fn draw_link_glyph(link_state: LinkState, right_cursor: i32, title_mid_y: i32, target: &mut FrameBuffer565) -> i32 {
    let link_color = match link_state {
        LinkState::Connected => palette::BRAND_BRIGHT,
        LinkState::Scanning | LinkState::Connecting => palette::STATUS_WARNING,
        LinkState::Idle => palette::TEXT_SECONDARY,
    };

    // Same `icon_1x` + ink-bounding-box-centered-on-the-title-bar technique
    // the shield mark uses in `render` (see its own doc comment for why
    // `icon_1x`, not `icon_2x`, fits the fixed `TITLE_BAR_HEIGHT`-px bar
    // with room to spare).
    let link_font = font::icon_1x();
    let mut link_buf = [0_u8; 4];
    let link_str: &str = icon::BLUETOOTH.encode_utf8(&mut link_buf);
    let link_width = text_width(&link_font, link_str) as i32;
    let link_x = right_cursor - link_width;
    let link_ink = link_font
        .get_rendered_dimensions_aligned(link_str, Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
        .unwrap_or(None);
    let link_y = link_ink.map_or(title_mid_y, |ink| title_mid_y - (ink.top_left.y + ink.size.height as i32 / 2));
    let _ = link_font.render_aligned(
        link_str,
        Point::new(link_x, link_y),
        VerticalPosition::Top,
        HorizontalAlignment::Left,
        FontColor::Transparent(link_color),
        target,
    );

    right_cursor - link_width - TITLE_ELEMENT_GAP
}

pub struct Screen {
    pub title: String,
    /// Static hint text drawn in the hint bar, e.g. control legends. Not a
    /// `Widget` — chrome furniture is intentionally simpler than the
    /// content widget model.
    pub hint: String,
    widgets: Vec<Box<dyn Widget>>,
    focused_index: Option<usize>,
}

impl Screen {
    #[must_use]
    pub fn new(title: impl Into<String>, widgets: Vec<Box<dyn Widget>>) -> Self {
        Self {
            title: title.into(),
            hint: String::new(),
            widgets,
            focused_index: None,
        }
    }

    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = hint.into();
        self
    }

    #[must_use]
    pub fn focused_index(&self) -> Option<usize> {
        self.focused_index
    }

    #[must_use]
    pub fn widgets(&self) -> &[Box<dyn Widget>] {
        &self.widgets
    }

    /// The focused widget's own internal selection index, if any — see
    /// `Widget::selected_index`'s doc comment for why this exists (letting
    /// a rebuilt-from-model screen carry the user's selection forward
    /// instead of resetting it).
    #[must_use]
    pub fn selected_index(&self) -> Option<usize> {
        self.focused_index.and_then(|index| self.widgets[index].selected_index())
    }

    /// The focused widget's own currently selected row's identity key, if
    /// any — see `Widget::selected_key`'s doc comment. The key-based
    /// counterpart to [`Screen::selected_index`], for the same
    /// carry-forward purpose but robust to the underlying list reordering,
    /// growing, or shrinking between rebuilds.
    #[must_use]
    pub fn selected_key(&self) -> Option<super::list::ListItemKey> {
        self.focused_index.and_then(|index| self.widgets[index].selected_key())
    }

    /// The currently focused widget's [`ChromeContribution`], if any.
    /// Consulting *only* the focused widget (not e.g. merging every
    /// widget's contribution) is deliberate: on every screen this bead
    /// builds there's exactly one widget anyway, and for a future
    /// multi-widget screen "whichever thing has the user's attention
    /// decides what the chrome says" is the same rule per-screen focus
    /// memory already uses for input.
    pub(super) fn chrome_contribution(&self) -> Option<ChromeContribution> {
        self.focused_index.and_then(|index| self.widgets[index].chrome_contribution())
    }

    /// Focuses the first focusable widget, if none is focused yet. Called
    /// when a screen is first pushed onto the stack. A no-op if focus was
    /// already established (which is how per-screen focus memory works:
    /// a screen re-visited via `pop` still has its old `focused_index`,
    /// so this does nothing and the old focus is preserved).
    pub(super) fn initialize_focus(&mut self) {
        if self.focused_index.is_some() {
            return;
        }
        for index in 0..self.widgets.len() {
            if self.widgets[index].is_focusable() {
                self.set_focus(Some(index));
                return;
            }
        }
    }

    fn set_focus(&mut self, new_index: Option<usize>) {
        if self.focused_index == new_index {
            return;
        }
        if let Some(old) = self.focused_index {
            self.widgets[old].on_focus(FocusEvent::Lost);
        }
        if let Some(new) = new_index {
            self.widgets[new].on_focus(FocusEvent::Gained);
        }
        self.focused_index = new_index;
    }

    /// Moves top-level focus to the next focusable widget, wrapping
    /// around. Salvaged from `Document::focus_next`.
    pub(super) fn focus_next(&mut self) {
        if self.widgets.is_empty() {
            return;
        }
        let len = self.widgets.len();
        let start = self.focused_index.map_or(0, |i| (i + 1) % len);
        for step in 0..len {
            let index = (start + step) % len;
            if self.widgets[index].is_focusable() {
                self.set_focus(Some(index));
                return;
            }
        }
    }

    /// Moves top-level focus to the previous focusable widget, wrapping
    /// around. Salvaged from `Document::focus_previous`.
    pub(super) fn focus_previous(&mut self) {
        if self.widgets.is_empty() {
            return;
        }
        let len = self.widgets.len();
        let start = self.focused_index.unwrap_or(0);
        for step in 0..len {
            let index = (start + len - 1 - step) % len;
            if self.widgets[index].is_focusable() {
                self.set_focus(Some(index));
                return;
            }
        }
    }

    /// Forwards a navigation intent to the currently focused widget (if
    /// any), for `Up`/`Down`/`JumpBy` (and `Left`/`Right`/`ShortcutX`/
    /// `ShortcutY`, forwarded but with no top-level effect) — see
    /// `Navigator::dispatch` for how this interacts with
    /// `focus_next`/`focus_previous`.
    pub(super) fn forward_to_focused(&mut self, intent: NavIntent) -> Action {
        match self.focused_index {
            Some(index) => self.widgets[index].on_intent(intent),
            None => Action::None,
        }
    }

    /// Activates the currently focused widget (`NavIntent::Select`).
    pub(super) fn activate_focused(&mut self) -> Action {
        match self.focused_index {
            Some(index) => self.widgets[index].on_focus(FocusEvent::Activated),
            None => Action::None,
        }
    }

    /// Draws the title bar (shield mark, title, position readout, the
    /// keyboard-output-link Bluetooth glyph, sync status dot), the content
    /// widgets (stacked vertically, sized via
    /// `Widget::measure`), and the hint bar — pulling live overrides from
    /// the focused widget's [`ChromeContribution`] (see
    /// [`Self::chrome_contribution`]) over this screen's static
    /// `title`/`hint` wherever the widget supplies one.
    pub(super) fn render(
        &self,
        chrome: &ChromeLayout,
        target: &mut FrameBuffer565,
    ) -> Result<(), Infallible> {
        chrome.title.into_styled(PrimitiveStyle::with_fill(palette::SURFACE)).draw(target)?;

        // Hairline dividers along the title bar's bottom edge and the hint
        // bar's top edge — the same `palette::DIVIDER` hairline the list
        // rows use between unfocused rows, so chrome and content read as
        // one consistent visual language rather than content borrowing a
        // rule chrome doesn't also follow.
        if chrome.title.size.height > 0 {
            let divider = Rectangle::new(
                Point::new(chrome.title.top_left.x, chrome.title.top_left.y + chrome.title.size.height as i32 - 1),
                Size::new(chrome.title.size.width, 1),
            );
            divider.into_styled(PrimitiveStyle::with_fill(palette::DIVIDER)).draw(target)?;
        }
        if chrome.hint.size.height > 0 {
            let divider = Rectangle::new(chrome.hint.top_left, Size::new(chrome.hint.size.width, 1));
            divider.into_styled(PrimitiveStyle::with_fill(palette::DIVIDER)).draw(target)?;
        }

        let contribution = self.chrome_contribution();
        let title_text = contribution.as_ref().and_then(|c| c.title.as_deref()).unwrap_or(self.title.as_str());
        let readout_text = contribution.as_ref().and_then(|c| c.readout.as_deref());
        let status = contribution.as_ref().and_then(|c| c.status);
        let link = contribution.as_ref().and_then(|c| c.link);
        let hint_text = contribution.as_ref().and_then(|c| c.hint.as_deref()).unwrap_or(self.hint.as_str());

        // Vertically centered in the title bar via `VerticalPosition::Center`
        // rather than a hand-picked baseline offset (the "+11" this
        // retires) — `u8g2-fonts` derives the correct baseline from the
        // font's own ascent/descent metrics for us.
        let title_mid_y = chrome.title.top_left.y + chrome.title.size.height as i32 / 2;

        let title_text_x = draw_shield_mark(chrome.title.top_left.x, title_mid_y, target);

        // Right side, built right-to-left so the status dot and readout
        // can each be omitted independently: status dot first (rightmost),
        // then the readout to its left.
        let mut right_cursor = chrome.title.top_left.x + chrome.title.size.width as i32 - TITLE_SIDE_MARGIN;

        if let Some(status) = status {
            let dot_color = match status {
                ChromeStatus::Success => palette::STATUS_SUCCESS,
                ChromeStatus::Error => palette::STATUS_ERROR,
                ChromeStatus::Neutral => palette::TEXT_SECONDARY,
            };
            let dot_center = Point::new(right_cursor - STATUS_DOT_DIAMETER as i32 / 2, title_mid_y);
            Circle::with_center(dot_center, STATUS_DOT_DIAMETER)
                .into_styled(PrimitiveStyle::with_fill(dot_color))
                .draw(target)?;
            right_cursor -= STATUS_DOT_DIAMETER as i32 + TITLE_ELEMENT_GAP;
        }

        // Bluetooth glyph, immediately left of the status dot, per design
        // spec. `link` being `None` omits the glyph entirely, rather than
        // drawing it in some "definitely not connected" color: a widget
        // with no link-state opinion at all has nothing meaningful to
        // report here.
        if let Some(link_state) = link {
            right_cursor = draw_link_glyph(link_state, right_cursor, title_mid_y, target);
        }

        let readout_font = font::title();
        if let Some(readout) = readout_text {
            let _ = readout_font.render_aligned(
                readout,
                Point::new(right_cursor, title_mid_y),
                VerticalPosition::Center,
                HorizontalAlignment::Right,
                FontColor::Transparent(palette::TEXT_SECONDARY),
                target,
            );
            right_cursor -= text_width(&readout_font, readout) as i32 + TITLE_ELEMENT_GAP;
        }

        // Title text: clipped to `[title_text_x, right_cursor)` so a long
        // title can never bleed into the readout/status dot — retiring the
        // old un-clipped single-blob title draw this bead's description
        // calls out.
        let title_clip_width = (right_cursor - title_text_x).max(0) as u32;
        let title_rect = Rectangle::new(
            Point::new(title_text_x, chrome.title.top_left.y),
            Size::new(title_clip_width, chrome.title.size.height),
        );
        let mut title_target = target.clipped(&title_rect);
        let _ = font::title().render_aligned(
            title_text,
            Point::new(title_text_x, title_mid_y),
            VerticalPosition::Center,
            HorizontalAlignment::Left,
            FontColor::Transparent(palette::TEXT_PRIMARY),
            &mut title_target,
        );

        let mut y = chrome.content.top_left.y;
        let bottom = chrome.content.top_left.y + chrome.content.size.height as i32;
        for widget in &self.widgets {
            if y >= bottom {
                break;
            }
            let available = Size::new(chrome.content.size.width, (bottom - y) as u32);
            let requested = widget.measure(available);
            let height = requested.height.min(available.height);

            let area = Rectangle::new(Point::new(chrome.content.top_left.x, y), Size::new(chrome.content.size.width, height));
            widget.render(area, target)?;
            y += height as i32;
        }

        if chrome.hint.size.height > 0 {
            let hint_mid_y = chrome.hint.top_left.y + chrome.hint.size.height as i32 / 2;
            let _ = font::hint().render_aligned(
                hint_text,
                Point::new(chrome.hint.top_left.x + HINT_SIDE_MARGIN, hint_mid_y),
                VerticalPosition::Center,
                HorizontalAlignment::Left,
                FontColor::Transparent(palette::TEXT_SECONDARY),
                target,
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::list::{ListItem, VerticalList};

    fn list_screen(n: usize) -> Screen {
        let items = (0..n).map(|i| ListItem::new(format!("item-{i}"))).collect();
        Screen::new("Test", vec![Box::new(VerticalList::new(items))])
    }

    #[test]
    fn initialize_focus_focuses_the_first_focusable_widget() {
        let mut screen = list_screen(3);
        assert_eq!(screen.focused_index(), None);
        screen.initialize_focus();
        assert_eq!(screen.focused_index(), Some(0));
    }

    #[test]
    fn initialize_focus_on_a_screen_with_no_focusable_widgets_is_a_noop() {
        let mut screen = list_screen(0);
        screen.initialize_focus();
        assert_eq!(screen.focused_index(), None);
    }

    #[test]
    fn focus_next_wraps_around_a_single_widget() {
        let mut screen = list_screen(3);
        screen.initialize_focus();
        screen.focus_next();
        // Only one focusable widget on this screen: wraps back to itself.
        assert_eq!(screen.focused_index(), Some(0));
    }

    #[test]
    fn render_does_not_panic_and_writes_into_the_provided_chrome_regions() {
        let mut screen = list_screen(5);
        screen.initialize_focus();
        let chrome = super::super::chrome::compute_chrome(Size::new(240, 240));
        let mut fb = FrameBuffer565::new(240, 240);
        screen.render(&chrome, &mut fb).unwrap();
        // Title bar was filled with its background color.
        assert_eq!(fb.pixel(Point::new(0, 0)), palette::SURFACE);
    }

    // --- Link glyph ---

    /// A single-purpose focusable widget whose only job is reporting a
    /// caller-settable `ChromeContribution::link` -- everything else is
    /// `None`/default, so a test only ever samples pixels this specific
    /// glyph rendering could plausibly have painted (no status dot, no
    /// readout, a one-character title to keep the shield/title text away
    /// from the right edge this glyph draws into).
    struct LinkOnlyWidget(core::cell::Cell<Option<LinkState>>);

    impl Widget for LinkOnlyWidget {
        fn measure(&self, _constraints: Size) -> Size {
            Size::zero()
        }
        fn render(&self, _area: Rectangle, _target: &mut FrameBuffer565) -> Result<(), Infallible> {
            Ok(())
        }
        fn is_focusable(&self) -> bool {
            true
        }
        fn chrome_contribution(&self) -> Option<ChromeContribution> {
            Some(ChromeContribution { link: self.0.get(), ..Default::default() })
        }
    }

    fn link_screen(state: Option<LinkState>) -> Screen {
        let mut screen = Screen::new("T", vec![Box::new(LinkOnlyWidget(core::cell::Cell::new(state)))]);
        screen.initialize_focus();
        screen
    }

    /// Renders `screen` into a fresh 240x240 framebuffer (the Pico Plus 2
    /// W panel, Epic B2) and reports whether `color` appears anywhere in
    /// the rightmost 20 columns of the title bar -- the glyph's drawing
    /// region when (per `link_screen`) there is no status dot/readout
    /// ahead of it, so it lands flush against the title bar's right
    /// margin. Narrow and right-aligned enough to never collide with the
    /// "T" title text or the shield mark, both drawn from the left edge.
    fn any_pixel_near_the_right_title_edge(screen: &Screen, color: embedded_graphics::pixelcolor::Rgb565) -> bool {
        let chrome = super::super::chrome::compute_chrome(Size::new(240, 240));
        let mut fb = FrameBuffer565::new(240, 240);
        screen.render(&chrome, &mut fb).unwrap();
        (220..240).any(|x| (0..super::super::chrome::TITLE_BAR_HEIGHT as i32).any(|y| fb.pixel(Point::new(x, y)) == color))
    }

    #[test]
    fn connected_link_state_paints_the_glyph_in_the_brand_bright_color() {
        let screen = link_screen(Some(LinkState::Connected));
        assert!(
            any_pixel_near_the_right_title_edge(&screen, palette::BRAND_BRIGHT),
            "a Connected link should paint the glyph in BRAND_BRIGHT near the title bar's right edge"
        );
    }

    #[test]
    fn scanning_link_state_paints_the_glyph_in_the_warning_color() {
        let screen = link_screen(Some(LinkState::Scanning));
        assert!(
            any_pixel_near_the_right_title_edge(&screen, palette::STATUS_WARNING),
            "a Scanning link should paint the glyph in STATUS_WARNING (shared with Connecting -- both are 'in progress')"
        );
    }

    #[test]
    fn idle_link_state_paints_the_glyph_in_the_muted_secondary_color() {
        let screen = link_screen(Some(LinkState::Idle));
        assert!(
            any_pixel_near_the_right_title_edge(&screen, palette::TEXT_SECONDARY),
            "an Idle link should paint the glyph in TEXT_SECONDARY"
        );
    }

    #[test]
    fn no_link_state_omits_the_glyph_entirely() {
        let connected = link_screen(Some(LinkState::Connected));
        let none = link_screen(None);

        assert!(
            any_pixel_near_the_right_title_edge(&connected, palette::BRAND_BRIGHT),
            "sanity check: Connected does paint something to compare against"
        );
        assert!(
            !any_pixel_near_the_right_title_edge(&none, palette::BRAND_BRIGHT)
                && !any_pixel_near_the_right_title_edge(&none, palette::STATUS_WARNING)
                && !any_pixel_near_the_right_title_edge(&none, palette::TEXT_SECONDARY),
            "link: None must omit the glyph -- no link-glyph color anywhere near the right edge"
        );
    }
}
