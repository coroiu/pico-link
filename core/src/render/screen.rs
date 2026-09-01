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
use crate::panel::Button;

use super::chrome::ChromeLayout;
use super::ctx::RenderCtx;
use super::framebuffer::FrameBuffer565;
use super::rail::{draw_rail, ButtonLabel, ButtonLabels};
use super::theme::{font, icon, palette};
use super::widget::{Action, ChromeContribution, ChromeStatus, FocusEvent, Widget};

/// Margin (px) from the title bar's left/right edges to its content —
/// now the same left rule `L = 12` the body content uses (design doc
/// `.planning/design/2026-09-01-home-alignment-grid.md` section 3), so
/// the title text and the device name directly beneath it finally stack
/// on one shared edge.
const TITLE_SIDE_MARGIN: i32 = 12;
/// Gap (px) between adjacent title-bar elements (readout -> status dot,
/// status dot -> link glyph).
const TITLE_ELEMENT_GAP: i32 = 6;
/// Diameter (px) of the title bar's sync-status dot.
const STATUS_DOT_DIAMETER: u32 = 6;

/// B's rail text is always this constant — never per-screen authorable.
/// Only B's *liveness* varies (see `Screen::render`'s `can_go_back`
/// parameter); its text does not, because a per-screen B label is exactly
/// how B stops meaning Back. See design section 4/rule 3.
const BACK_LABEL: &str = "back";

/// The horizontal pixel footprint `u8g2-fonts`' `render_aligned` would give
/// `text` in `font` — used to right-align/clip chrome elements without
/// hardcoding a per-character pixel width, so a later `u8g2-fonts` font
/// swap doesn't require recomputing these layouts by hand.
fn text_width(font: &FontRenderer, text: &str) -> u32 {
    font.get_rendered_dimensions_aligned(text, Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
        .unwrap_or(None)
        .map_or(0, |bbox| bbox.size.width)
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

    // `icon_1x`, not `icon_2x` -- fits the fixed `TITLE_BAR_HEIGHT`-px bar
    // with room to spare. Centered on its own *ink* bounding box, not
    // `VerticalPosition::Center` (which centers on the font's line
    // metrics/ascent-descent budget, not this specific glyph's ink) --
    // the same mismatch `theme::draw_chip`'s doc comment describes for
    // the chip-letter fix.
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
    /// Static button-rail labels, e.g. control legends. Not a `Widget` —
    /// chrome furniture is intentionally simpler than the content widget
    /// model. B is included in this struct's shape for symmetry, but per
    /// [`BACK_LABEL`] its *text* is never actually read from here — only
    /// its slot's presence/absence-of-override matters, and even that is
    /// moot since B's liveness comes from `Navigator`, not `Screen`.
    pub buttons: ButtonLabels,
    /// Whether this screen handles `NavIntent::Back` internally (e.g. a
    /// menu/status face toggle) rather than deferring to the navigator's
    /// stack-depth pop. ORs into the B slot's liveness passed to
    /// [`Screen::render`] — see `can_go_back`.
    handles_back: bool,
    widgets: Vec<Box<dyn Widget>>,
    focused_index: Option<usize>,
}

impl Screen {
    #[must_use]
    pub fn new(title: impl Into<String>, widgets: Vec<Box<dyn Widget>>) -> Self {
        Self {
            title: title.into(),
            buttons: ButtonLabels::default(),
            handles_back: false,
            widgets,
            focused_index: None,
        }
    }

    /// Sets this screen's static A/X/Y rail labels. B is deliberately not
    /// a parameter here: its text is always [`BACK_LABEL`] and its
    /// liveness is a navigator fact (stack depth), not a screen fact — see
    /// the design's rule that B must not be per-screen overridable, and
    /// [`Screen::handles_back`] for the one thing a screen *can* say about
    /// B (that it wants to be treated as live even at depth 1).
    #[must_use]
    pub fn with_button_labels(mut self, a: ButtonLabel, x: ButtonLabel, y: ButtonLabel) -> Self {
        self.buttons = ButtonLabels { a, b: ButtonLabel::Inert, x, y };
        self
    }

    /// Declares that this screen handles `NavIntent::Back` internally
    /// (e.g. Home's menu<->status face toggle) — its B slot should read as
    /// live even at navigator depth 1, where there is otherwise nothing to
    /// pop back to.
    #[must_use]
    pub fn handles_back(mut self, handles: bool) -> Self {
        self.handles_back = handles;
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
    pub(super) fn chrome_contribution(&self, ctx: &RenderCtx) -> Option<ChromeContribution> {
        self.focused_index.and_then(|index| self.widgets[index].chrome_contribution(ctx))
    }

    /// The soonest any of this screen's widgets say their own appearance
    /// could next change purely from elapsed time -- the min over every
    /// widget's [`Widget::redraw_after`], not just the focused one (unlike
    /// [`Screen::chrome_contribution`]): an unfocused widget's on-screen
    /// pixels still need to stay live (e.g. a background status widget
    /// ticking a live duration while a list has focus). `None` if no
    /// widget on this screen has a time-driven opinion.
    pub(super) fn redraw_after(&self, ctx: &RenderCtx) -> Option<core::time::Duration> {
        self.widgets.iter().filter_map(|widget| widget.redraw_after(ctx)).min()
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

    /// Resolves one button's rail label: the focused widget's
    /// [`ChromeContribution`] wins when it has an opinion (`Some(_)`,
    /// including `Some(Inert)`), otherwise this screen's static
    /// [`Screen::buttons`] label is used. B is special-cased: its text is
    /// always [`BACK_LABEL`] and its liveness is `can_go_back` (already
    /// `OR`ed with [`Screen::handles_back`] by the caller) rather than
    /// anything either the widget or the screen authored for it.
    fn resolve_button(&self, button: Button, contribution: Option<&ChromeContribution>, can_go_back: bool) -> ButtonLabel {
        if button == Button::B {
            return if can_go_back || self.handles_back { ButtonLabel::Live(String::from(BACK_LABEL)) } else { ButtonLabel::Inert };
        }
        match contribution.and_then(|c| c.button(button)) {
            Some(label) => label.clone(),
            None => self.buttons.get(button).clone(),
        }
    }

    /// Draws the title bar (title text, position readout, the
    /// keyboard-output-link Bluetooth glyph, sync status dot), the content
    /// widgets (stacked vertically, sized via
    /// `Widget::measure`), and the button rail — pulling live overrides
    /// from the focused widget's [`ChromeContribution`] (see
    /// [`Self::chrome_contribution`]) over this screen's static
    /// `title`/button labels wherever the widget supplies one.
    ///
    /// `can_go_back` is a navigator fact (`Navigator::depth() > 1`), not a
    /// screen fact — see [`Screen::resolve_button`] and [`BACK_LABEL`]'s
    /// doc comments for why B is resolved outside the normal
    /// widget-then-screen fallback chain.
    pub(super) fn render(
        &self,
        chrome: &ChromeLayout,
        can_go_back: bool,
        ctx: &RenderCtx,
        target: &mut FrameBuffer565,
    ) -> Result<(), Infallible> {
        chrome.title.into_styled(PrimitiveStyle::with_fill(palette::SURFACE)).draw(target)?;

        // Hairline divider along the title bar's bottom edge — the same
        // `palette::DIVIDER` hairline the list rows use between unfocused
        // rows, so chrome and content read as one consistent visual
        // language rather than content borrowing a rule chrome doesn't
        // also follow. The rail draws its own hairlines (inner edge +
        // between slots) in `draw_rail`.
        if chrome.title.size.height > 0 {
            let divider = Rectangle::new(
                Point::new(chrome.title.top_left.x, chrome.title.top_left.y + chrome.title.size.height as i32 - 1),
                Size::new(chrome.title.size.width, 1),
            );
            divider.into_styled(PrimitiveStyle::with_fill(palette::DIVIDER)).draw(target)?;
        }

        let contribution = self.chrome_contribution(ctx);
        let title_text = contribution.as_ref().and_then(|c| c.title.as_deref()).unwrap_or(self.title.as_str());
        let readout_text = contribution.as_ref().and_then(|c| c.readout.as_deref());
        let status = contribution.as_ref().and_then(|c| c.status);
        let link = contribution.as_ref().and_then(|c| c.link);

        // Vertically centered in the title bar via `VerticalPosition::Center`
        // rather than a hand-picked baseline offset (the "+11" this
        // retires) — `u8g2-fonts` derives the correct baseline from the
        // font's own ascent/descent metrics for us.
        let title_mid_y = chrome.title.top_left.y + chrome.title.size.height as i32 / 2;

        // Title text starts directly on the left rule -- no shield mark
        // (pico-link-d9y: the Bitwarden-era brand glyph is deleted, not
        // replaced; see the design doc's section 5 for why nothing takes
        // its place). This is also what fixes the 12px title/body
        // misalignment (pico-link-nvj's D2): the title text and the
        // device name directly beneath it now share one left edge.
        let title_text_x = chrome.title.top_left.x + TITLE_SIDE_MARGIN;

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
            let requested = widget.measure(available, ctx);
            let height = requested.height.min(available.height);

            let area = Rectangle::new(Point::new(chrome.content.top_left.x, y), Size::new(chrome.content.size.width, height));
            widget.render(area, ctx, target)?;
            y += height as i32;
        }

        if chrome.rail.size.width > 0 {
            let labels = ButtonLabels {
                a: self.resolve_button(Button::A, contribution.as_ref(), can_go_back),
                b: self.resolve_button(Button::B, contribution.as_ref(), can_go_back),
                x: self.resolve_button(Button::X, contribution.as_ref(), can_go_back),
                y: self.resolve_button(Button::Y, contribution.as_ref(), can_go_back),
            };
            draw_rail(chrome.rail, chrome.orientation, &labels, target)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Instant;
    use crate::render::list::{ListItem, VerticalList};

    fn test_ctx() -> RenderCtx {
        RenderCtx::at(Instant::from_micros(0))
    }

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
        screen.render(&chrome, false, &test_ctx(), &mut fb).unwrap();
        // Title bar was filled with its background color.
        assert_eq!(fb.pixel(Point::new(0, 0)), palette::SURFACE);
    }

    // --- Link glyph ---

    /// A single-purpose focusable widget whose only job is reporting a
    /// caller-settable `ChromeContribution::link` -- everything else is
    /// `None`/default, so a test only ever samples pixels this specific
    /// glyph rendering could plausibly have painted (no status dot, no
    /// readout, a one-character title to keep the title text away from
    /// the right edge this glyph draws into).
    struct LinkOnlyWidget(core::cell::Cell<Option<LinkState>>);

    impl Widget for LinkOnlyWidget {
        fn measure(&self, _constraints: Size, _ctx: &RenderCtx) -> Size {
            Size::zero()
        }
        fn render(&self, _area: Rectangle, _ctx: &RenderCtx, _target: &mut FrameBuffer565) -> Result<(), Infallible> {
            Ok(())
        }
        fn is_focusable(&self) -> bool {
            true
        }
        fn chrome_contribution(&self, _ctx: &RenderCtx) -> Option<ChromeContribution> {
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
    /// "T" title text, drawn from the left edge.
    fn any_pixel_near_the_right_title_edge(screen: &Screen, color: embedded_graphics::pixelcolor::Rgb565) -> bool {
        let chrome = super::super::chrome::compute_chrome(Size::new(240, 240));
        let mut fb = FrameBuffer565::new(240, 240);
        screen.render(&chrome, false, &test_ctx(), &mut fb).unwrap();
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

    // --- Button rail (pico-link-znb.5 / E2) ---

    use crate::panel::{Button, PanelOrientation};

    /// A single-purpose focusable widget whose only job is reporting a
    /// caller-fixed set of `ChromeContribution` button overrides -- same
    /// shape as `LinkOnlyWidget` above, one field per rail slot.
    struct ButtonsOnlyWidget {
        a: Option<ButtonLabel>,
        b: Option<ButtonLabel>,
        x: Option<ButtonLabel>,
        y: Option<ButtonLabel>,
    }

    impl Widget for ButtonsOnlyWidget {
        fn measure(&self, _constraints: Size, _ctx: &RenderCtx) -> Size {
            Size::zero()
        }
        fn render(&self, _area: Rectangle, _ctx: &RenderCtx, _target: &mut FrameBuffer565) -> Result<(), Infallible> {
            Ok(())
        }
        fn is_focusable(&self) -> bool {
            true
        }
        fn chrome_contribution(&self, _ctx: &RenderCtx) -> Option<ChromeContribution> {
            Some(ChromeContribution {
                a: self.a.clone(),
                b: self.b.clone(),
                x: self.x.clone(),
                y: self.y.clone(),
                ..Default::default()
            })
        }
    }

    /// Computes the exact slot rect for `button` under `chrome`'s
    /// orientation -- the same arithmetic `render::rail::draw_rail` uses
    /// internally (rail height / 4, `slot_order` for the physical index),
    /// duplicated here deliberately so the test asserts against the
    /// *design's* geometry, not against whatever `draw_rail` happens to
    /// compute.
    fn slot_rect(chrome: &ChromeLayout, button: Button) -> Rectangle {
        let order = chrome.orientation.slot_order();
        let index = order.iter().position(|&b| b == button).expect("every Button is in slot_order");
        let slot_height = chrome.rail.size.height / 4;
        Rectangle::new(
            Point::new(chrome.rail.top_left.x, chrome.rail.top_left.y + (index as u32 * slot_height) as i32),
            Size::new(chrome.rail.size.width, slot_height),
        )
    }

    fn any_pixel_of_color_in_rect(fb: &FrameBuffer565, rect: Rectangle, color: embedded_graphics::pixelcolor::Rgb565) -> bool {
        (rect.top_left.y..rect.top_left.y + rect.size.height as i32)
            .any(|y| (rect.top_left.x..rect.top_left.x + rect.size.width as i32).any(|x| fb.pixel(Point::new(x, y)) == color))
    }

    fn render_buttons_screen(orientation: PanelOrientation, widget: ButtonsOnlyWidget) -> (ChromeLayout, FrameBuffer565) {
        let mut screen = Screen::new("T", vec![Box::new(widget)]);
        screen.initialize_focus();
        let chrome = super::super::chrome::compute_chrome_for(Size::new(240, 240), orientation);
        let mut fb = FrameBuffer565::new(240, 240);
        screen.render(&chrome, false, &test_ctx(), &mut fb).unwrap();
        (chrome, fb)
    }

    /// The proof that the rail's parameterisation is real, not claimed:
    /// under both orientations, a live A label's `TEXT_SECONDARY` pixels
    /// land inside A's own slot rect and in **none** of the other three
    /// slot rects. The negative half is what makes this a proof rather
    /// than a "some pixel exists somewhere" smoke test.
    #[test]
    fn mirroring_the_orientation_moves_both_the_edge_and_the_slot_order() {
        for orientation in [PanelOrientation::ButtonsRight, PanelOrientation::ButtonsLeft] {
            let widget = ButtonsOnlyWidget {
                a: Some(ButtonLabel::Live(String::from("devs"))),
                b: Some(ButtonLabel::Inert),
                x: Some(ButtonLabel::Inert),
                y: Some(ButtonLabel::Inert),
            };
            let (chrome, fb) = render_buttons_screen(orientation, widget);

            let a_rect = slot_rect(&chrome, Button::A);
            assert!(
                any_pixel_of_color_in_rect(&fb, a_rect, palette::TEXT_SECONDARY),
                "A's own slot must contain TEXT_SECONDARY pixels under {orientation:?}"
            );
            for other in [Button::B, Button::X, Button::Y] {
                let other_rect = slot_rect(&chrome, other);
                assert!(
                    !any_pixel_of_color_in_rect(&fb, other_rect, palette::TEXT_SECONDARY),
                    "slot for {other:?} must contain no TEXT_SECONDARY pixels under {orientation:?} -- \
                     a mirror bug would leak A's label into the wrong slot"
                );
            }
        }
    }

    #[test]
    fn buttons_right_a_slot_is_top_right_buttons_left_a_slot_is_bottom_left() {
        // Exact rects from the design doc section 5, restated here as the
        // load-bearing numbers rather than an inequality.
        let right = super::super::chrome::compute_chrome_for(Size::new(240, 240), PanelOrientation::ButtonsRight);
        assert_eq!(slot_rect(&right, Button::A), Rectangle::new(Point::new(206, 16), Size::new(34, 56)));

        let left = super::super::chrome::compute_chrome_for(Size::new(240, 240), PanelOrientation::ButtonsLeft);
        assert_eq!(slot_rect(&left, Button::A), Rectangle::new(Point::new(0, 184), Size::new(34, 56)));
    }

    #[test]
    fn a_live_button_paints_text_secondary_inside_its_own_slot() {
        let widget = ButtonsOnlyWidget { a: None, b: Some(ButtonLabel::Inert), x: Some(ButtonLabel::Live(String::from("link"))), y: Some(ButtonLabel::Inert) };
        let (chrome, fb) = render_buttons_screen(PanelOrientation::ButtonsRight, widget);
        let x_rect = slot_rect(&chrome, Button::X);
        assert!(any_pixel_of_color_in_rect(&fb, x_rect, palette::TEXT_SECONDARY));
    }

    #[test]
    fn an_inert_button_paints_divider_and_never_text_secondary_in_its_own_slot() {
        let widget = ButtonsOnlyWidget { a: None, b: Some(ButtonLabel::Inert), x: Some(ButtonLabel::Inert), y: Some(ButtonLabel::Inert) };
        let (chrome, fb) = render_buttons_screen(PanelOrientation::ButtonsRight, widget);
        let x_rect = slot_rect(&chrome, Button::X);
        assert!(
            any_pixel_of_color_in_rect(&fb, x_rect, palette::DIVIDER),
            "an inert slot still draws its dim letter in DIVIDER"
        );
        assert!(
            !any_pixel_of_color_in_rect(&fb, x_rect, palette::TEXT_SECONDARY),
            "an inert slot must never contain TEXT_SECONDARY -- that would be indistinguishable from live"
        );
    }

    #[test]
    fn widget_none_defers_to_the_screens_static_label() {
        // No widget opinion (`x: None`) + a screen static Live("link") ->
        // the screen's label wins and its text paints.
        let widget = ButtonsOnlyWidget { a: None, b: None, x: None, y: None };
        let mut screen = Screen::new("T", vec![Box::new(widget)]).with_button_labels(
            ButtonLabel::Inert,
            ButtonLabel::Live(String::from("link")),
            ButtonLabel::Inert,
        );
        screen.initialize_focus();
        let chrome = super::super::chrome::compute_chrome(Size::new(240, 240));
        let mut fb = FrameBuffer565::new(240, 240);
        screen.render(&chrome, false, &test_ctx(), &mut fb).unwrap();

        let x_rect = slot_rect(&chrome, Button::X);
        assert!(
            any_pixel_of_color_in_rect(&fb, x_rect, palette::TEXT_SECONDARY),
            "widget x: None must defer to the screen's static Live(\"link\") label"
        );
    }

    #[test]
    fn widget_some_inert_overrides_the_screens_static_live_label() {
        // Same screen static label as above, but the widget actively
        // overrides X to Inert -- proving Option-of-Option isn't
        // decoration: without this override the pixels from the previous
        // test would still be there.
        let widget = ButtonsOnlyWidget { a: None, b: None, x: Some(ButtonLabel::Inert), y: None };
        let mut screen = Screen::new("T", vec![Box::new(widget)]).with_button_labels(
            ButtonLabel::Inert,
            ButtonLabel::Live(String::from("link")),
            ButtonLabel::Inert,
        );
        screen.initialize_focus();
        let chrome = super::super::chrome::compute_chrome(Size::new(240, 240));
        let mut fb = FrameBuffer565::new(240, 240);
        screen.render(&chrome, false, &test_ctx(), &mut fb).unwrap();

        let x_rect = slot_rect(&chrome, Button::X);
        assert!(
            !any_pixel_of_color_in_rect(&fb, x_rect, palette::TEXT_SECONDARY),
            "widget x: Some(Inert) must override the screen's static Live label -- its text must not paint"
        );
        assert!(
            any_pixel_of_color_in_rect(&fb, x_rect, palette::DIVIDER),
            "the overridden slot renders as inert (dim letter)"
        );
    }

    #[test]
    fn a_16_character_label_never_paints_left_of_the_rails_own_left_edge() {
        let widget = ButtonsOnlyWidget {
            a: Some(ButtonLabel::Live(String::from("abcdefghijklmnop"))),
            b: Some(ButtonLabel::Inert),
            x: Some(ButtonLabel::Inert),
            y: Some(ButtonLabel::Inert),
        };
        let (chrome, fb) = render_buttons_screen(PanelOrientation::ButtonsRight, widget);

        let background = embedded_graphics::pixelcolor::Rgb565::default();
        let rail_left = chrome.rail.top_left.x;
        for y in chrome.rail.top_left.y..(chrome.rail.top_left.y + chrome.rail.size.height as i32) {
            for x in 0..rail_left {
                assert_eq!(
                    fb.pixel(Point::new(x, y)),
                    background,
                    "an oversized label bled a non-background pixel to ({x}, {y}), left of the rail's own edge at x={rail_left}"
                );
            }
        }
    }
}
