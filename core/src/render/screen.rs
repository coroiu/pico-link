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
    geometry::OriginDimensions,
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
use super::paint_key::PaintKey;
use super::rail::{draw_rail, ButtonLabel, ButtonLabels};
use super::theme::{font, icon, palette};
use super::widget::{Action, ChromeContribution, ChromeStatus, FocusEvent, Verb, Widget};

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

/// One entry in [`Screen::paint_cache`]: what a chrome pseudo-region or
/// widget looked like at the end of the last *painted* frame, per the
/// damage pass design (`.planning/design/2026-09-06-damage-rect-render-
/// and-partial-blit.md` section 3.3). Slot order is fixed and matches
/// [`Screen::render`]'s own build order every frame: index 0 is the title
/// bar pseudo-region, index 1 is the button rail pseudo-region, and index
/// `2 + i` is `self.widgets[i]`. `damage_hint` is deliberately not part of
/// this cache -- unlike `key`/`area`, it is cheap to recompute from the
/// still-live widget the moment a slot turns out to be dirty (section
/// 3.3 step 4), so there is nothing stale here to compare against.
#[derive(Debug, Clone, Copy)]
struct PaintSlot {
    key: PaintKey,
    area: Rectangle,
}

/// Seeds only need to differ from each other and from a widget's own seed
/// space (widgets choose their own in bead `pico-link-7h5.5`) -- the exact
/// values carry no meaning.
const TITLE_PAINT_KEY_SEED: u64 = 1;
const RAIL_PAINT_KEY_SEED: u64 = 2;

/// Folds `text` into `key` if present, or a distinguishable "absent" tag if
/// not -- so `Some("")` (an empty but present string) can never collide
/// with `None`.
fn fold_opt_str(key: PaintKey, text: Option<&str>) -> PaintKey {
    match text {
        None => key.fold(0),
        Some(text) => key.fold(1).fold_str(text),
    }
}

fn fold_button_label(key: PaintKey, label: &ButtonLabel) -> PaintKey {
    match label {
        ButtonLabel::Inert => key.fold(0),
        ButtonLabel::Live(text) => key.fold(1).fold_str(text),
    }
}

/// The title bar's own paint key -- everything [`Screen::render`]'s title
/// bar drawing actually reads: the resolved title text, the readout, the
/// status dot, and the link glyph. See the damage design's section 3.5:
/// chrome is not exempt from paint keys just because it isn't a [`Widget`].
fn title_paint_key(title_text: &str, readout_text: Option<&str>, status: Option<ChromeStatus>, link: Option<LinkState>) -> PaintKey {
    let key = PaintKey::of(TITLE_PAINT_KEY_SEED).fold_str(title_text);
    let key = fold_opt_str(key, readout_text);
    let key = key.fold(match status {
        None => 0,
        Some(ChromeStatus::Success) => 1,
        Some(ChromeStatus::Error) => 2,
        Some(ChromeStatus::Neutral) => 3,
    });
    key.fold(match link {
        None => 0,
        Some(LinkState::Idle) => 1,
        Some(LinkState::Scanning) => 2,
        Some(LinkState::Connecting) => 3,
        Some(LinkState::Connected) => 4,
    })
}

/// The button rail's own paint key -- the four already-resolved
/// [`ButtonLabel`]s, which already fold in the focused widget's
/// [`ChromeContribution`], `can_go_back`, and the screen's static labels
/// (see [`Screen::resolve_a`]/[`Screen::resolve_button`]), so nothing that
/// can change the rail's pixels is missing from this key.
fn rail_paint_key(labels: &ButtonLabels) -> PaintKey {
    let key = PaintKey::of(RAIL_PAINT_KEY_SEED);
    let key = fold_button_label(key, &labels.a);
    let key = fold_button_label(key, &labels.b);
    let key = fold_button_label(key, &labels.x);
    fold_button_label(key, &labels.y)
}

/// The smallest rect containing both `a` and `b`. `embedded-graphics`'s
/// `Rectangle` ships `intersection` but no `union`, so the damage pass
/// grows its own. A zero-sized operand is the identity element (returns
/// the other rect unchanged) -- load-bearing for a widget whose *old*
/// cached area is `Rectangle::zero()` (never rendered last frame, per
/// `Screen::render`'s layout pass): unioning that in must not corrupt the
/// result with a spurious corner at the origin.
fn union_rect(a: Rectangle, b: Rectangle) -> Rectangle {
    if a.is_zero_sized() {
        return b;
    }
    if b.is_zero_sized() {
        return a;
    }
    let a_end = Point::new(a.top_left.x + a.size.width as i32, a.top_left.y + a.size.height as i32);
    let b_end = Point::new(b.top_left.x + b.size.width as i32, b.top_left.y + b.size.height as i32);
    let top_left = Point::new(a.top_left.x.min(b.top_left.x), a.top_left.y.min(b.top_left.y));
    let end = Point::new(a_end.x.max(b_end.x), a_end.y.max(b_end.y));
    Rectangle::new(top_left, Size::new((end.x - top_left.x).max(0) as u32, (end.y - top_left.y).max(0) as u32))
}

/// Whether `a` and `b` share any pixels -- the SKIP test itself (design
/// section 1/3.3 step 7): a chrome pseudo-region or widget is drawn this
/// frame exactly when its own area intersects the frame damage rect,
/// regardless of whether *it* is the thing that changed.
fn rects_intersect(a: Rectangle, b: Rectangle) -> bool {
    !a.intersection(&b).is_zero_sized()
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
    widgets: Vec<Box<dyn Widget>>,
    focused_index: Option<usize>,
    /// What every chrome pseudo-region and widget looked like at the end
    /// of the last frame this screen actually painted -- the damage pass's
    /// diff target (design section 3.3 step 3). Empty on a freshly built
    /// `Screen`, which is itself a cache miss and therefore forces full
    /// damage on this screen's first render -- no separate "first frame"
    /// flag needed here (`Navigator` still forces it too, since a *pop*
    /// reveals an already-primed `Screen` whose own cache would otherwise
    /// see nothing dirty -- see `Navigator`'s `force_full_damage`).
    paint_cache: Vec<PaintSlot>,
}

impl Screen {
    #[must_use]
    pub fn new(title: impl Into<String>, widgets: Vec<Box<dyn Widget>>) -> Self {
        Self {
            title: title.into(),
            buttons: ButtonLabels::default(),
            widgets,
            focused_index: None,
            paint_cache: Vec::new(),
        }
    }

    /// Sets this screen's static X/Y rail labels. Neither A nor B is a
    /// parameter here: **A never has a screen-level static label**
    /// (design rule 4 — A's liveness and label are always the focused
    /// widget's own [`Widget::activation`], because "activate the focused
    /// thing" is meaningless without a focused thing), and B's text is
    /// always [`BACK_LABEL`] with its liveness a navigator fact (stack
    /// depth) OR'd with the focused widget's own [`Widget::handles_back`]
    /// — see [`Screen::resolve_button`].
    #[must_use]
    pub fn with_button_labels(mut self, x: ButtonLabel, y: ButtonLabel) -> Self {
        self.buttons = ButtonLabels { a: ButtonLabel::Inert, b: ButtonLabel::Inert, x, y };
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

    /// The focused widget's own scroll-top row index, if any — see
    /// `Widget::scroll_top`'s doc comment. The scroll-position
    /// counterpart to [`Screen::selected_index`]/[`Screen::selected_key`],
    /// for the same "carry live-rebuilt state forward" purpose.
    #[must_use]
    pub fn scroll_top(&self) -> Option<usize> {
        self.focused_index.and_then(|index| self.widgets[index].scroll_top())
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
    ///
    /// Refuses to dispatch when the focused widget's [`Widget::activation`]
    /// is `None` — design rule 4: "A's liveness and A's label are the same
    /// fact." [`Screen::resolve_a`] reads this exact same accessor to
    /// decide what the rail *shows*, so the two cannot disagree; "A acts
    /// but renders dim" is a state this program has no way to represent.
    pub(super) fn activate_focused(&mut self) -> Action {
        match self.focused_index {
            Some(index) if self.widgets[index].activation().is_some() => self.widgets[index].on_focus(FocusEvent::Activated),
            _ => Action::None,
        }
    }

    /// Resolves A's rail label from the focused widget's own
    /// [`Widget::activation`] — the one and only source of A's liveness
    /// and text (design rule 4). A never falls back to a screen-level
    /// static label (see [`Screen::with_button_labels`]) and never reads
    /// [`ChromeContribution`] — see [`Screen::activate_focused`] for the
    /// matching gate this must never disagree with.
    ///
    /// `pub(crate)`, not private: this is also the accessor
    /// `app::tests::a_rail_liveness_matches_activation_for_every_screen`
    /// reads to observe what the rail actually renders, independently of
    /// [`Screen::focused_activation`] — see that method's doc comment for
    /// why the two are kept as separate call sites in the test even though
    /// they are, today, one expression apart.
    pub(crate) fn resolve_a(&self) -> ButtonLabel {
        let verb = self.focused_activation();
        match verb {
            Some(verb) => ButtonLabel::Live(String::from(verb.as_str())),
            None => ButtonLabel::Inert,
        }
    }

    /// The focused widget's [`Widget::activation`], or `None` if nothing is
    /// focused — the same expression [`Screen::activate_focused`] gates
    /// dispatch on and [`Screen::resolve_a`] renders from (design rule 4:
    /// "A's liveness and A's label are the same fact"). `pub(crate)` so
    /// `app`'s central regression test
    /// (`a_rail_liveness_matches_activation_for_every_screen`) can assert
    /// the invariant from outside this module, as a tripwire on the
    /// mechanism rather than a proof of per-screen correctness — see the
    /// design doc §5(d)'s "what is NOT enforceable" note.
    pub(crate) fn focused_activation(&self) -> Option<Verb> {
        self.focused_index.and_then(|index| self.widgets[index].activation())
    }

    /// Whether the focused widget wants B treated as live even at
    /// navigator depth 1 (nothing to pop to) — see
    /// [`Widget::handles_back`]'s doc comment (e.g. Home's menu face
    /// folding back to its status face).
    fn widget_handles_back(&self) -> bool {
        self.focused_index.is_some_and(|index| self.widgets[index].handles_back())
    }

    /// Resolves one of B/X/Y's rail label: the focused widget's
    /// [`ChromeContribution`] wins when it has an opinion (`Some(_)`,
    /// including `Some(Inert)`), otherwise this screen's static
    /// [`Screen::buttons`] label is used. **Never called for A** — see
    /// [`Screen::resolve_a`]. B is further special-cased: its text is
    /// always [`BACK_LABEL`] and its liveness is `can_go_back` OR'd with
    /// [`Screen::widget_handles_back`] rather than anything either the
    /// widget or the screen authored for it.
    fn resolve_button(&self, button: Button, contribution: Option<&ChromeContribution>, can_go_back: bool) -> ButtonLabel {
        if button == Button::B {
            return if can_go_back || self.widget_handles_back() { ButtonLabel::Live(String::from(BACK_LABEL)) } else { ButtonLabel::Inert };
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
    /// The frame damage pass (`.planning/design/2026-09-06-damage-rect-
    /// render-and-partial-blit.md` section 3.3): lays out every widget
    /// plus the two chrome pseudo-regions, diffs their [`PaintKey`]s and
    /// areas against [`Self::paint_cache`], fills only the resulting
    /// damage rect (replacing the old whole-framebuffer clear), redraws
    /// every chrome region/widget whose area *intersects* that rect (not
    /// just the ones that changed -- section 3.3's correctness rule, so an
    /// unrelated widget overlapping a repainted region never goes stale),
    /// commits the fresh cache, and returns the damage rect actually
    /// painted -- `Rectangle::zero()` if nothing needed repainting.
    ///
    /// `force_full_damage` short-circuits the diff and damages the whole
    /// framebuffer -- the caller ([`super::navigator::Navigator::render`])
    /// sets this on the enumerated full-damage triggers (first frame,
    /// push/pop/replace, resize, wake); an empty-or-mismatched
    /// [`Self::paint_cache`] (this screen's own first render, or a
    /// screen whose widget count somehow changed) forces it independently.
    ///
    /// `can_go_back` is a navigator fact (`Navigator::depth() > 1`), not a
    /// screen fact — see [`Screen::resolve_button`] and [`BACK_LABEL`]'s
    /// doc comments for why B is resolved outside the normal
    /// widget-then-screen fallback chain.
    #[allow(clippy::too_many_lines)] // the damage pass is one linear sequence of numbered steps (section 3.3) -- splitting it up would scatter, not shrink, the logic
    pub(super) fn render(
        &mut self,
        chrome: &ChromeLayout,
        can_go_back: bool,
        ctx: &RenderCtx,
        force_full_damage: bool,
        target: &mut FrameBuffer565,
    ) -> Result<Rectangle, Infallible> {
        let contribution = self.chrome_contribution(ctx);
        let title_text = contribution.as_ref().and_then(|c| c.title.as_deref()).unwrap_or(self.title.as_str());
        let readout_text = contribution.as_ref().and_then(|c| c.readout.as_deref());
        let status = contribution.as_ref().and_then(|c| c.status);
        let link = contribution.as_ref().and_then(|c| c.link);

        let labels = ButtonLabels {
            a: self.resolve_a(),
            b: self.resolve_button(Button::B, contribution.as_ref(), can_go_back),
            x: self.resolve_button(Button::X, contribution.as_ref(), can_go_back),
            y: self.resolve_button(Button::Y, contribution.as_ref(), can_go_back),
        };

        // --- Step 1: layout. One area per widget -- `Rectangle::zero()`
        // for any widget the content region has already run out of
        // vertical space for (the same early-exit the old single-pass
        // loop used; those widgets are never measured or rendered,
        // exactly as before this bead). ---
        let mut widget_areas: Vec<Rectangle> = Vec::with_capacity(self.widgets.len());
        let mut y = chrome.content.top_left.y;
        let bottom = chrome.content.top_left.y + chrome.content.size.height as i32;
        for widget in &self.widgets {
            if y >= bottom {
                widget_areas.push(Rectangle::zero());
                continue;
            }
            let available = Size::new(chrome.content.size.width, (bottom - y) as u32);
            let requested = widget.measure(available, ctx);
            let height = requested.height.min(available.height);
            let area = Rectangle::new(Point::new(chrome.content.top_left.x, y), Size::new(chrome.content.size.width, height));
            widget_areas.push(area);
            y += height as i32;
        }

        // --- Step 2: collect keys. Chrome pseudo-regions first (indices
        // 0/1) to match `Self::paint_cache`'s fixed slot order. A widget
        // past the content region's bottom (zero-sized area, never
        // rendered) gets `PaintKey::ALWAYS` rather than a real
        // `paint_key()` call -- it contributes nothing to the damage rect
        // either way (its area is zero-sized), so there is nothing to
        // gain from actually invoking it, matching the "never measured"
        // treatment above. ---
        let title_key = title_paint_key(title_text, readout_text, status, link);
        let rail_key = rail_paint_key(&labels);

        let mut new_slots: Vec<PaintSlot> = Vec::with_capacity(2 + self.widgets.len());
        new_slots.push(PaintSlot { key: title_key, area: chrome.title });
        new_slots.push(PaintSlot { key: rail_key, area: chrome.rail });
        for (widget, &area) in self.widgets.iter().zip(widget_areas.iter()) {
            let key = if area.is_zero_sized() { PaintKey::ALWAYS } else { widget.paint_key(ctx) };
            new_slots.push(PaintSlot { key, area });
        }

        // --- Steps 3-5: diff against the last painted frame, narrow via
        // `damage_hint`, union into one frame damage rect. ---
        let whole_frame = Rectangle::new(Point::zero(), target.size());
        let cache_miss = self.paint_cache.len() != new_slots.len();
        let mut damage = Rectangle::zero();

        if force_full_damage || cache_miss {
            damage = whole_frame;
        } else {
            for (index, new_slot) in new_slots.iter().enumerate() {
                let old_slot = self.paint_cache[index];
                let moved = old_slot.area != new_slot.area;
                let key_changed = old_slot.key != new_slot.key;
                if !moved && !key_changed {
                    continue;
                }
                let region = if moved {
                    // The slot itself relocated: the whole old-and-new
                    // footprint is damaged (its old pixels must be
                    // cleared too), not just a `damage_hint` narrowing of
                    // the new position.
                    union_rect(old_slot.area, new_slot.area)
                } else if index >= 2 {
                    // Only a real widget can narrow its own dirty area;
                    // the two chrome pseudo-regions (index 0/1) have no
                    // `damage_hint` of their own.
                    match self.widgets[index - 2].damage_hint(new_slot.area, ctx) {
                        Some(hint) => hint.intersection(&new_slot.area),
                        None => new_slot.area,
                    }
                } else {
                    new_slot.area
                };
                damage = union_rect(damage, region);
            }
        }

        // Clamp to the framebuffer -- defensive against a stale cache
        // entry from a since-shrunk screen size handing back a rect past
        // its edge (`force_full_damage`/resize already covers the normal
        // resize path; this is the belt).
        damage = damage.intersection(&whole_frame);

        // --- Step 8 (part 1): commit the cache for next frame, before any
        // early return below -- a frame that painted nothing still needs
        // its keys/areas recorded so the *next* frame's diff is correct.
        self.paint_cache = new_slots;

        if damage.is_zero_sized() {
            return Ok(damage);
        }

        // --- Step 6: fill only the damage rect -- replaces
        // `Navigator::render`'s old whole-framebuffer `target.clear(...)`.
        damage.into_styled(PrimitiveStyle::with_fill(palette::BACKGROUND)).draw(target)?;

        let frame_ctx = ctx.with_damage(damage);

        // --- Step 7: redraw every chrome pseudo-region/widget whose area
        // intersects the damage rect -- a SKIP (the drawing code below is
        // not called at all when it doesn't), never a clip. Title bar
        // first, matching the original paint order. ---
        if rects_intersect(chrome.title, damage) {
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
        }

        for (widget, &area) in self.widgets.iter().zip(widget_areas.iter()) {
            if rects_intersect(area, damage) {
                widget.render(area, &frame_ctx, target)?;
            }
        }

        if chrome.rail.size.width > 0 && rects_intersect(chrome.rail, damage) {
            draw_rail(chrome.rail, chrome.orientation, &labels, target)?;
        }

        Ok(damage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::widget::Verb;
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
        screen.render(&chrome, false, &test_ctx(), true, &mut fb).unwrap();
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
    fn any_pixel_near_the_right_title_edge(screen: &mut Screen, color: embedded_graphics::pixelcolor::Rgb565) -> bool {
        let chrome = super::super::chrome::compute_chrome(Size::new(240, 240));
        let mut fb = FrameBuffer565::new(240, 240);
        screen.render(&chrome, false, &test_ctx(), true, &mut fb).unwrap();
        (220..240).any(|x| (0..super::super::chrome::TITLE_BAR_HEIGHT as i32).any(|y| fb.pixel(Point::new(x, y)) == color))
    }

    #[test]
    fn connected_link_state_paints_the_glyph_in_the_brand_bright_color() {
        let mut screen = link_screen(Some(LinkState::Connected));
        assert!(
            any_pixel_near_the_right_title_edge(&mut screen, palette::BRAND_BRIGHT),
            "a Connected link should paint the glyph in BRAND_BRIGHT near the title bar's right edge"
        );
    }

    #[test]
    fn scanning_link_state_paints_the_glyph_in_the_warning_color() {
        let mut screen = link_screen(Some(LinkState::Scanning));
        assert!(
            any_pixel_near_the_right_title_edge(&mut screen, palette::STATUS_WARNING),
            "a Scanning link should paint the glyph in STATUS_WARNING (shared with Connecting -- both are 'in progress')"
        );
    }

    #[test]
    fn idle_link_state_paints_the_glyph_in_the_muted_secondary_color() {
        let mut screen = link_screen(Some(LinkState::Idle));
        assert!(
            any_pixel_near_the_right_title_edge(&mut screen, palette::TEXT_SECONDARY),
            "an Idle link should paint the glyph in TEXT_SECONDARY"
        );
    }

    #[test]
    fn no_link_state_omits_the_glyph_entirely() {
        let mut connected = link_screen(Some(LinkState::Connected));
        let mut none = link_screen(None);

        assert!(
            any_pixel_near_the_right_title_edge(&mut connected, palette::BRAND_BRIGHT),
            "sanity check: Connected does paint something to compare against"
        );
        assert!(
            !any_pixel_near_the_right_title_edge(&mut none, palette::BRAND_BRIGHT)
                && !any_pixel_near_the_right_title_edge(&mut none, palette::STATUS_WARNING)
                && !any_pixel_near_the_right_title_edge(&mut none, palette::TEXT_SECONDARY),
            "link: None must omit the glyph -- no link-glyph color anywhere near the right edge"
        );
    }

    // --- Button rail (pico-link-znb.5 / E2) ---

    use crate::panel::{Button, PanelOrientation};

    /// A single-purpose focusable widget whose only job is reporting a
    /// caller-fixed set of button overrides -- same shape as `LinkOnlyWidget`
    /// above, one field per rail slot. `a` is a [`Verb`] (routed through
    /// [`Widget::activation`], the only source of A's label since design
    /// rule 4), not a `ButtonLabel` like the other three (still routed
    /// through [`ChromeContribution`]).
    struct ButtonsOnlyWidget {
        a: Option<Verb>,
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
        fn activation(&self) -> Option<Verb> {
            self.a
        }
        fn chrome_contribution(&self, _ctx: &RenderCtx) -> Option<ChromeContribution> {
            Some(ChromeContribution {
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
        screen.render(&chrome, false, &test_ctx(), true, &mut fb).unwrap();
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
                a: Some(Verb::Exception("devs")),
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
            ButtonLabel::Live(String::from("link")),
            ButtonLabel::Inert,
        );
        screen.initialize_focus();
        let chrome = super::super::chrome::compute_chrome(Size::new(240, 240));
        let mut fb = FrameBuffer565::new(240, 240);
        screen.render(&chrome, false, &test_ctx(), true, &mut fb).unwrap();

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
            ButtonLabel::Live(String::from("link")),
            ButtonLabel::Inert,
        );
        screen.initialize_focus();
        let chrome = super::super::chrome::compute_chrome(Size::new(240, 240));
        let mut fb = FrameBuffer565::new(240, 240);
        screen.render(&chrome, false, &test_ctx(), true, &mut fb).unwrap();

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
            a: Some(Verb::Exception("abcdefghijklmnop")),
            b: Some(ButtonLabel::Inert),
            x: Some(ButtonLabel::Inert),
            y: Some(ButtonLabel::Inert),
        };
        let (chrome, fb) = render_buttons_screen(PanelOrientation::ButtonsRight, widget);

        // `palette::BACKGROUND`, not `Rgb565::default()` (pure black): as
        // of this bead, `Screen::render` fills its own damage rect with
        // the real background color (design section 3.3 step 6), which
        // replaces the whole-framebuffer `Navigator::render` clear this
        // test used to rely on implicitly by calling `Screen::render`
        // directly on a freshly black `FrameBuffer565`.
        let background = palette::BACKGROUND;
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
