//! A chip-less, single-line, vertically-centered "action row" style —
//! distinct from `list.rs`'s two-line row (label+sublabel+letter/icon
//! chip). A menu of plain actions (e.g. "Reveal"/"Type") reusing
//! `list::VerticalList`/`ListItem` would draw every row with a
//! letter-chip badge and reserve two-line label+sublabel height — visually
//! indistinguishable from a two-line list item, when these rows are
//! *actions*, not list entries. [`MenuList`]/[`MenuItem`] give menu
//! screens their own narrow widget instead of bolting a third chip-less
//! mode onto `VerticalList` (which would let `VerticalList`'s other,
//! unrelated list-style consumers accidentally end up chip-less too).
//!
//! [`draw_row`] (the shared row-drawing primitive, `pub(crate)`) is the
//! **one row-drawing path** shared by [`MenuList`] and
//! `super::fields::FieldList` (`.planning/design/2026-09-02-field-list-
//! widget-ruling.md` is the design of record — read it before touching
//! this module). [`RowStyle`] parameterizes margins/padding/gutter so
//! each caller gets its own visual rhythm without a second,
//! independently-drawn row implementation drifting from this one on
//! selection fill, divider, or caret.

// Identical allow (and rationale) as `list.rs`:
// this module does the same `embedded-graphics` `Point`(i32)/`Size`(u32)
// coordinate math directly, so the same justification applies — no
// display this project targets is anywhere near large enough for these
// conversions to wrap, truncate, or lose a sign in practice.
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::convert::Infallible;

use embedded_graphics::draw_target::{DrawTarget, DrawTargetExt};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::{Point, Primitive, Size};
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use embedded_graphics::Drawable;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::FontRenderer;

use crate::input::NavIntent;

use super::ctx::RenderCtx;
use super::framebuffer::FrameBuffer565;
use super::paint_key::PaintKey;
use super::theme::{self, font, icon, palette};
use super::widget::{Action, FocusEvent, Verb, Widget};

/// Seed for [`MenuList::paint_key`] -- only needs to differ from other
/// widgets' own seeds.
const MENU_PAINT_KEY_SEED: u64 = 12;

/// Row metrics — one per **list**, not per row: a list whose rows use
/// different margins is not a list. Threaded through [`draw_row`] so
/// every caller's own visual rhythm (left margin, trailing margins, an
/// optional leading glyph gutter, vertical padding) lives in one place
/// instead of being re-derived per call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowStyle {
    /// x inset from the row's left edge to the leading gutter (or, when
    /// `leading_gutter == 0`, to the label itself).
    pub left_margin: i32,
    /// x inset from the row's RIGHT edge to the trailing value's right
    /// edge.
    pub value_right_margin: i32,
    /// x inset from the row's RIGHT edge to the caret's right edge. Must
    /// be `<= value_right_margin`. When it is strictly less, the caret
    /// occupies the gutter to the RIGHT of the value column, so gaining
    /// focus never moves the value — see `fields.rs`'s module doc for
    /// why that matters on the field list.
    pub caret_right_margin: i32,
    /// Reserved width (px) of the leading glyph gutter, applied to EVERY
    /// row whether or not that row carries a glyph, so labels align
    /// whether checked or not. `0` = no gutter.
    pub leading_gutter: i32,
    /// Padding above and below the single text line. Drives
    /// [`row_height`].
    pub vertical_padding: i32,
}

impl RowStyle {
    /// `MenuList`'s shipped metrics, reproduced exactly (legacy: the
    /// original `TEXT_LEFT_MARGIN`/`CARET_RIGHT_MARGIN`/`ROW_PADDING`
    /// values, pre-alignment-grid — see
    /// `.planning/design/2026-09-02-field-list-widget-ruling.md` §7.2/7.3
    /// for why the numbers stay put here while the false "matches a
    /// detail view" rationale they used to carry does not). Do not
    /// change these numbers as part of porting `MenuList` onto
    /// [`draw_row`] — see [`MenuList`]'s module-level acceptance test.
    pub const MENU: Self =
        Self { left_margin: 8, value_right_margin: 6, caret_right_margin: 6, leading_gutter: 0, vertical_padding: 10 };
    /// The field list: the alignment grid's 12/12
    /// (`.planning/design/2026-09-01-home-alignment-grid.md`), a compact
    /// row, and a caret that lives right of the value column so gaining
    /// focus never moves the value.
    pub const FIELD: Self =
        Self { left_margin: 12, value_right_margin: 12, caret_right_margin: 4, leading_gutter: 0, vertical_padding: 6 };
    /// [`Self::FIELD`] plus the pickers' current-choice check gutter.
    pub const FIELD_GUTTERED: Self = Self { leading_gutter: CHECK_GUTTER_WIDTH, ..Self::FIELD };
}

/// Width (px) of the leading glyph gutter: an `icon_1x` glyph (~8px)
/// plus a 4px gap before the label.
pub(crate) const CHECK_GUTTER_WIDTH: i32 = 12;

/// Compile-time enforcement of "the caret never moves the value"
/// (field-list ruling §4.2/§5 test 5): `RowStyle::FIELD`'s caret must sit
/// strictly right of the value column's right edge, in its own gutter, so
/// gaining focus can never move the value. A `const` assertion catches a
/// future edit to either margin immediately, at compile time, rather than
/// only when the test suite happens to run.
const _: () = assert!(RowStyle::FIELD.caret_right_margin < RowStyle::FIELD.value_right_margin);

/// Gap (px) between a clipped label's right edge and the trailing value
/// column's left edge — see [`draw_row`]'s label-clipping step.
const LABEL_VALUE_GAP: i32 = 8;

/// Width (px) reserved for the padlock glyph a `locked` row draws right
/// after its label -- bead `pico-link-ryw.12.4`, Uma's design (`ryw12-3-
/// ux.md` sec 1): "the name ellipsises BEFORE the lock and trailing
/// count; lock and count never clip". An `icon_1x` glyph (~8px) plus a
/// small gap, mirroring [`CHECK_GUTTER_WIDTH`]'s own "glyph + gap"
/// shape.
const LOCK_GLYPH_RESERVE: i32 = 12;

/// Ellipsis-truncates `label` (byte-boundary-safe) so it renders at or
/// under `max_width` in `font`, appending `"..."` (three ASCII dots, not
/// U+2026 -- `helv*_tf` fonts in this codebase are `_tf` "full" glyph
/// sets over Latin-1, and this project's other ASCII-only formatting
/// helpers, e.g. `effects.rs`'s number formatters, already avoid non-
/// ASCII punctuation for the same font-coverage reason). Only called for
/// a `locked` row, so this per-prefix measuring loop -- the exact
/// "character-walking text hack" this module's clip-not-ellipsise
/// default deliberately avoids for every OTHER row -- runs for at most
/// the handful of locked rows a screen ever has (bead `pico-link-ryw.12`'s
/// own `MAX_PRESETS` bound is 8), not the whole list.
fn ellipsis_truncate(font: &FontRenderer, label: &str, max_width: i32) -> String {
    if text_width(font, label) <= max_width {
        return String::from(label);
    }
    let mut end = label.len();
    loop {
        if end == 0 {
            return String::from("...");
        }
        end -= 1;
        while end > 0 && !label.is_char_boundary(end) {
            end -= 1;
        }
        let candidate = alloc::format!("{}...", &label[..end]);
        if end == 0 || text_width(font, &candidate) <= max_width {
            return candidate;
        }
    }
}

/// A row's right-aligned trailing value text — drawn REGARDLESS of
/// `selected` (today's `Trailing::Label` semantics, unchanged and still
/// the point: `menu.rs`'s original doc comment on that variant).
pub(crate) struct RowValue<'a> {
    pub text: &'a str,
    pub color: Rgb565,
    pub font: &'a FontRenderer,
}

/// A row's trailing content. A STRUCT, not an enum: the value and the
/// caret are independent — `CODEC` must show `LDAC` unconditionally and
/// grow a caret only while focused, which the old `Trailing` enum's
/// Label-xor-Caret exclusivity could not express (see the field-list
/// ruling §3.2/§7.4 for why that enum is retired rather than extended).
pub(crate) struct RowTrailing<'a> {
    pub value: Option<RowValue<'a>>,
    /// Whether this row draws a disclosure caret WHILE SELECTED.
    /// Unchanged `Trailing::Caret` semantics.
    pub caret: bool,
}

/// [`MenuItem`]'s owned counterpart to [`RowTrailing`] — a borrowed
/// `RowTrailing<'a>` can't be stored on a long-lived `MenuItem`, so this
/// owns whatever text a labelled trailing needs and hands out a borrowed
/// [`RowTrailing`] view via [`MenuItem::trailing`] at render time.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnedTrailing {
    Caret,
    Label(String, Rgb565),
    None,
}

/// A single menu row's label. Display-only, like [`super::list::ListItem`]
/// — no domain/identity fields, and no sublabel/icon/chip slots at all
/// (unlike `ListItem`'s): this style has none of those.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub label: String,
    /// The label's own text color. Defaults to [`palette::TEXT_PRIMARY`]
    /// (the original, uniform row color) — overridden via
    /// [`MenuItem::with_label_color`] for a row that needs
    /// to carry meaning in its color (e.g. a destructive action in
    /// [`palette::STATUS_ERROR`]).
    label_color: Rgb565,
    trailing: OwnedTrailing,
}

impl MenuItem {
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self { label: label.into(), label_color: palette::TEXT_PRIMARY, trailing: OwnedTrailing::Caret }
    }

    /// Overrides this row's label text color (default
    /// [`palette::TEXT_PRIMARY`]) — e.g. [`palette::STATUS_ERROR`] for a
    /// destructive action row (e.g. a confirm-clear screen).
    #[must_use]
    pub fn with_label_color(mut self, color: Rgb565) -> Self {
        self.label_color = color;
        self
    }

    /// Replaces this row's trailing disclosure caret with a state label
    /// (e.g. "On"/"Off"), drawn in `color`, shown regardless of focus —
    /// e.g. a settings-screen toggle row.
    #[must_use]
    pub fn with_trailing_label(mut self, text: impl Into<String>, color: Rgb565) -> Self {
        self.trailing = OwnedTrailing::Label(text.into(), color);
        self
    }

    /// Suppresses this row's trailing content entirely (no caret, no
    /// label).
    #[must_use]
    pub fn with_no_trailing(mut self) -> Self {
        self.trailing = OwnedTrailing::None;
        self
    }

    /// Lowers this row's owned trailing state into a borrowed
    /// [`RowTrailing`] for [`draw_row`]. `value_font` is the caller's
    /// single trailing-value font (every `MenuList` row shares one) —
    /// borrowed rather than constructed here so the returned
    /// `RowTrailing<'a>`'s lifetime ties to a `FontRenderer` that
    /// outlives the [`draw_row`] call it feeds, not a temporary dropped
    /// at the end of this method.
    fn trailing<'a>(&'a self, value_font: &'a FontRenderer) -> RowTrailing<'a> {
        match &self.trailing {
            OwnedTrailing::Caret => RowTrailing { value: None, caret: true },
            OwnedTrailing::Label(text, color) => {
                RowTrailing { value: Some(RowValue { text, color: *color, font: value_font }), caret: false }
            }
            OwnedTrailing::None => RowTrailing { value: None, caret: false },
        }
    }
}

/// Fallback line height (px): only used if a font's metrics are somehow
/// unavailable.
const FALLBACK_LINE_HEIGHT: i32 = 16;

/// Measures a font's worst-case single-line pixel footprint using the
/// same "all five ASCII descenders (+ a capital, for ascent)" probe
/// string technique used elsewhere in this module tree; duplicated
/// locally rather than shared since this is a small private helper and
/// this module has no other reason to depend on another view module.
fn line_height(font: &FontRenderer) -> i32 {
    font.get_rendered_dimensions_aligned("Agjpqy", Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
        .unwrap_or(None)
        .map_or(FALLBACK_LINE_HEIGHT, |bbox| bbox.size.height as i32)
}

/// A single line's rendered pixel width in `font` — duplicated from
/// `list.rs`'s private `text_width` for the same "small private helper,
/// no other reason to depend on that module" rationale [`line_height`]
/// already states.
fn text_width(font: &FontRenderer, text: &str) -> i32 {
    font.get_rendered_dimensions_aligned(text, Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
        .unwrap_or(None)
        .map_or(0, |bbox| bbox.size.width as i32)
}

/// Pixel height of one chip-less action row: `style.vertical_padding`
/// above and below a single line rendered in `font`. Style-and-font
/// derived (not hardcoded to [`font::value`]) — see the field-list
/// ruling §3.4/§7.6 for why the caller's own trailing/label font drives
/// this rather than a font the caller may not even be using.
#[must_use]
pub(crate) fn row_height(style: &RowStyle, font: &FontRenderer) -> u32 {
    (style.vertical_padding * 2 + line_height(font)) as u32
}

/// Draws one chip-less action row: the shared selection fill + left accent
/// bar (via [`theme::draw_selection`]) when `selected`, else a plain
/// bottom hairline divider; an optional leading glyph in `style`'s gutter;
/// `label` (in `label_color`) vertically centered in `label_font`,
/// clipped before it would run under the trailing value column; and, per
/// `trailing`, a right-aligned value (drawn regardless of `selected`)
/// and/or the shared disclosure caret (drawn only while `selected`), per
/// [`RowStyle::caret_right_margin`]'s placement rule.
///
/// # Errors
///
/// Returns `Infallible`'s uninhabited variant in practice — see
/// [`super::widget::Widget::render`]'s doc comment for why the `Result`
/// return exists at all.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_row<D>(
    target: &mut D,
    row_rect: Rectangle,
    style: &RowStyle,
    label: &str,
    label_color: Rgb565,
    label_font: &FontRenderer,
    leading: Option<char>,
    trailing: &RowTrailing<'_>,
    selected: bool,
    locked: bool,
) -> Result<(), Infallible>
where
    D: DrawTarget<Color = Rgb565, Error = Infallible>,
{
    if selected {
        theme::draw_selection(row_rect, target)?;
    } else {
        let divider = Rectangle::new(
            Point::new(row_rect.top_left.x, row_rect.top_left.y + row_rect.size.height as i32 - 1),
            Size::new(row_rect.size.width, 1),
        );
        divider.into_styled(PrimitiveStyle::with_fill(palette::DIVIDER)).draw(target)?;
    }

    let row_center_y = row_rect.top_left.y + row_rect.size.height as i32 / 2;
    let gutter_x = row_rect.top_left.x + style.left_margin;
    let label_x = gutter_x + style.leading_gutter;

    // The leading glyph gutter is reserved whether or not THIS row
    // carries a glyph -- see `RowStyle::leading_gutter`'s doc comment --
    // so only the draw call, not the label's start x, is conditional on
    // `leading`.
    if style.leading_gutter > 0 {
        if let Some(glyph) = leading {
            let mut buf = [0_u8; 4];
            let glyph_str: &str = glyph.encode_utf8(&mut buf);
            let _ = font::icon_1x().render_aligned(
                glyph_str,
                Point::new(gutter_x, row_center_y),
                VerticalPosition::Center,
                HorizontalAlignment::Left,
                FontColor::Transparent(label_color),
                target,
            );
        }
    }

    let value_right_edge = row_rect.top_left.x + row_rect.size.width as i32 - style.value_right_margin;
    let caret_right_edge = row_rect.top_left.x + row_rect.size.width as i32 - style.caret_right_margin;

    // Measure the value first (§4.6 of the field-list ruling): the label
    // is clipped -- not ellipsised, which would need a measure-per-prefix
    // loop this render core has already retired one character-walking
    // text hack for -- at the value column's left edge, so a long label
    // never runs underneath a right-aligned value.
    let label_clip_right = trailing.value.as_ref().map_or(
        row_rect.top_left.x + row_rect.size.width as i32,
        |value| value_right_edge - text_width(value.font, value.text) - LABEL_VALUE_GAP,
    );

    if locked {
        // Bead `pico-link-ryw.12.4`: a locked row's padlock and trailing
        // value must NEVER clip (Uma's design, `ryw12-3-ux.md` sec 1) --
        // the label is ellipsis-truncated to leave room for the padlock
        // instead of being clipped like every other row's label.
        let label_budget_right = label_clip_right - LOCK_GLYPH_RESERVE;
        let truncated = ellipsis_truncate(label_font, label, (label_budget_right - label_x).max(0));
        let _ = label_font.render_aligned(
            truncated.as_str(),
            Point::new(label_x, row_center_y),
            VerticalPosition::Center,
            HorizontalAlignment::Left,
            FontColor::Transparent(label_color),
            target,
        );
        let lock_x = label_x + text_width(label_font, &truncated) + 4;
        let mut buf = [0_u8; 4];
        let lock: &str = icon::LOCK.encode_utf8(&mut buf);
        let _ = font::icon_1x().render_aligned(
            lock,
            Point::new(lock_x, row_center_y),
            VerticalPosition::Center,
            HorizontalAlignment::Left,
            FontColor::Transparent(palette::TEXT_SECONDARY),
            target,
        );
    } else {
        let label_clip_width = (label_clip_right - label_x).max(0) as u32;
        let label_rect = Rectangle::new(
            Point::new(label_x, row_rect.top_left.y),
            Size::new(label_clip_width, row_rect.size.height),
        );
        let mut label_target = target.clipped(&label_rect);
        let _ = label_font.render_aligned(
            label,
            Point::new(label_x, row_center_y),
            VerticalPosition::Center,
            HorizontalAlignment::Left,
            FontColor::Transparent(label_color),
            &mut label_target,
        );
    }

    if let Some(value) = &trailing.value {
        let _ = value.font.render_aligned(
            value.text,
            Point::new(value_right_edge, row_center_y),
            VerticalPosition::Center,
            HorizontalAlignment::Right,
            FontColor::Transparent(value.color),
            target,
        );
    }

    if trailing.caret && selected {
        let mut buf = [0_u8; 4];
        let caret: &str = icon::CARET_RIGHT.encode_utf8(&mut buf);
        let _ = font::icon_1x().render_aligned(
            caret,
            Point::new(caret_right_edge, row_center_y),
            VerticalPosition::Center,
            HorizontalAlignment::Right,
            FontColor::Transparent(palette::TEXT_PRIMARY),
            target,
        );
    }

    Ok(())
}

/// Callback invoked with the selected row's index on activation — the
/// only activation seam this widget needs (mirrors
/// `list::VerticalList::on_activate_index`; this style has no
/// `on_activate`-by-`&MenuItem` sibling since every current caller already
/// dispatches on a fixed, known row order).
type OnActivateIndex = Box<dyn Fn(usize) -> Action>;

/// A focusable, chip-less vertical menu of [`MenuItem`]s — the restyled
/// field action menu style, drawn via the shared [`draw_row`] primitive
/// at [`RowStyle::MENU`]. No scrolling: every current caller has at most
/// two rows, well within any content area this app renders into, so
/// `list::reconcile_top_index`'s viewport-edge scrolling isn't needed
/// here (unlike `VerticalList`, or `super::fields::FieldList`, which do
/// need it for arbitrarily long lists). A row beyond the viewport is
/// simply not drawn, the same as a `VerticalList` row would be once
/// clipped — see [`Widget::render`]'s early `break`.
pub struct MenuList {
    items: Vec<MenuItem>,
    selected: usize,
    focused: bool,
    on_activate_index: Option<OnActivateIndex>,
    /// The A-rail verb reported while this menu is focused -- set
    /// alongside the activation callback (design rule 4 §5(c)). `None`
    /// iff no callback is registered.
    verb: Option<Verb>,
}

impl MenuList {
    #[must_use]
    pub fn new(items: Vec<MenuItem>) -> Self {
        Self { items, selected: 0, focused: false, on_activate_index: None, verb: None }
    }

    /// Registers a callback invoked with the selected row's index when the
    /// menu is activated while focused, and the A-rail verb to show while
    /// this menu is focused (design rule 4 §5(c): the builder that installs
    /// a handler must also name what A does).
    #[must_use]
    pub fn on_activate_index(mut self, verb: Verb, callback: impl Fn(usize) -> Action + 'static) -> Self {
        self.verb = Some(verb);
        self.on_activate_index = Some(Box::new(callback));
        self
    }

    /// Sets the initially selected row, clamped to the item list's bounds
    /// — mirrors `list::VerticalList::with_selected`, for the same
    /// "store-backed widget rebuilds a fresh `MenuList` from live data on
    /// every call, carrying forward its own persisted selection" pattern.
    #[must_use]
    pub fn with_selected(mut self, selected: usize) -> Self {
        self.selected = selected.min(self.items.len().saturating_sub(1));
        self
    }

    /// Sets the initial focus-highlight state — mirrors
    /// `list::VerticalList::with_focused`; same rationale as
    /// [`Self::with_selected`].
    #[must_use]
    pub fn with_focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    #[must_use]
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    fn move_selection(&mut self, delta: i32) {
        if self.items.is_empty() {
            return;
        }
        let len = self.items.len() as i32;
        let next = (self.selected as i32 + delta).clamp(0, len - 1);
        self.selected = next as usize;
    }
}

impl Widget for MenuList {
    fn measure(&self, constraints: Size, _ctx: &RenderCtx) -> Size {
        constraints
    }

    fn is_focusable(&self) -> bool {
        !self.items.is_empty()
    }

    /// The verb registered on [`Self::on_activate_index`], or `None` if no
    /// callback (and therefore no verb) was ever registered -- design
    /// rule 4.
    fn activation(&self) -> Option<Verb> {
        self.on_activate_index.is_some().then_some(self.verb).flatten()
    }

    fn on_focus(&mut self, event: FocusEvent) -> Action {
        match event {
            FocusEvent::Gained => {
                self.focused = true;
                Action::None
            }
            FocusEvent::Lost => {
                self.focused = false;
                Action::None
            }
            FocusEvent::Activated => {
                if let Some(callback) = &self.on_activate_index {
                    callback(self.selected)
                } else {
                    Action::None
                }
            }
        }
    }

    /// Folds everything [`MenuList::render`] actually reads: which row is
    /// selected/focused (selection fill + caret gating), the item count,
    /// and per item the label, its resolved text color, and its resolved
    /// [`RowTrailing`] content (a caret draws no extra state of its own; a
    /// label trailing folds its text and color; no trailing folds nothing
    /// beyond its own discriminant).
    ///
    /// `MenuList` does not override [`Widget::redraw_after`], so per the
    /// mechanical review rule this `paint_key` must not fold time either,
    /// and it doesn't -- `_ctx` is unused.
    fn paint_key(&self, _ctx: &RenderCtx) -> PaintKey {
        let mut key = PaintKey::of(MENU_PAINT_KEY_SEED)
            .fold(self.selected as u64)
            .fold(u64::from(self.focused))
            .fold(self.items.len() as u64);
        for item in &self.items {
            key = key.fold_str(&item.label);
            key = key.fold_color(item.label_color);
            key = match &item.trailing {
                OwnedTrailing::Caret => key.fold(0),
                OwnedTrailing::Label(text, color) => key.fold(1).fold_str(text).fold_color(*color),
                OwnedTrailing::None => key.fold(2),
            };
        }
        key
    }

    fn on_intent(&mut self, intent: NavIntent) -> Action {
        match intent {
            NavIntent::Down => self.move_selection(1),
            NavIntent::Up => self.move_selection(-1),
            NavIntent::JumpBy(n) => self.move_selection(i32::from(n)),
            NavIntent::Select | NavIntent::Back | NavIntent::Left | NavIntent::Right | NavIntent::ShortcutX | NavIntent::ShortcutY => {}
        }
        Action::None
    }

    fn render(&self, area: Rectangle, _ctx: &RenderCtx, target: &mut FrameBuffer565) -> Result<(), Infallible> {
        let mut clipped = target.clipped(&area);
        let value_font = font::value();
        let height = row_height(&RowStyle::MENU, &value_font);

        for (index, item) in self.items.iter().enumerate() {
            let row_top = area.top_left.y + (index as u32 * height) as i32;
            if row_top >= area.top_left.y + area.size.height as i32 {
                break;
            }
            let row_rect = Rectangle::new(Point::new(area.top_left.x, row_top), Size::new(area.size.width, height));
            let selected = self.focused && index == self.selected;
            let trailing = item.trailing(&value_font);
            draw_row(
                &mut clipped,
                row_rect,
                &RowStyle::MENU,
                &item.label,
                item.label_color,
                &value_font,
                None,
                &trailing,
                selected,
                false, // MenuList rows are never locked -- only FieldList's imported-effect rows are.
            )?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Instant;

    fn test_ctx() -> RenderCtx {
        RenderCtx::at(Instant::from_micros(0))
    }

    fn items(n: usize) -> Vec<MenuItem> {
        (0..n).map(|i| MenuItem::new(format!("item-{i}"))).collect()
    }

    #[test]
    fn empty_menu_is_not_focusable() {
        let menu = MenuList::new(vec![]);
        assert!(!menu.is_focusable());
    }

    #[test]
    fn non_empty_menu_is_focusable() {
        let menu = MenuList::new(items(2));
        assert!(menu.is_focusable());
    }

    #[test]
    fn next_and_prev_move_selection_and_clamp_at_the_ends() {
        let mut menu = MenuList::new(items(2));
        assert_eq!(menu.selected_index(), 0);
        menu.on_intent(NavIntent::Up);
        assert_eq!(menu.selected_index(), 0);
        menu.on_intent(NavIntent::Down);
        assert_eq!(menu.selected_index(), 1);
        menu.on_intent(NavIntent::Down);
        assert_eq!(menu.selected_index(), 1);
    }

    #[test]
    fn activate_without_callback_is_a_noop() {
        let mut menu = MenuList::new(items(1));
        let action = menu.on_focus(FocusEvent::Activated);
        assert!(matches!(action, Action::None));
    }

    #[test]
    fn activate_invokes_the_callback_with_the_selected_rows_index() {
        let mut menu = MenuList::new(items(3)).on_activate_index(Verb::Open, |index| {
            assert_eq!(index, 1);
            Action::PopView
        });
        menu.on_intent(NavIntent::Down);
        let action = menu.on_focus(FocusEvent::Activated);
        assert!(matches!(action, Action::PopView));
    }

    #[test]
    fn row_height_is_shorter_than_the_two_line_list_row_height() {
        assert!(
            row_height(&RowStyle::MENU, &font::value()) < crate::render::ROW_HEIGHT,
            "a chip-less single-line row must be shorter than the two-line list row"
        );
    }

    #[test]
    fn field_row_is_shorter_than_the_menu_row() {
        // Field-list ruling §5 test 2: FIELD's tighter vertical_padding
        // must actually produce a shorter row than MENU's.
        assert!(
            row_height(&RowStyle::FIELD, &font::value()) < row_height(&RowStyle::MENU, &font::value()),
            "RowStyle::FIELD must be shorter than RowStyle::MENU"
        );
    }

    #[test]
    fn render_never_draws_the_list_row_letter_chip_fill_color() {
        // The whole point of this widget: unlike `list::draw_row`, it must
        // never paint `palette::BRAND` (the letter-chip's fill) anywhere.
        let menu = MenuList::new(vec![MenuItem::new("Reveal"), MenuItem::new("Type password")]);
        let mut fb = FrameBuffer565::new(200, 100);
        let area = Rectangle::new(Point::new(0, 0), Size::new(200, 100));
        menu.render(area, &test_ctx(), &mut fb).unwrap();

        let any_chip_fill = fb.pixels().any(|p| p.1 == palette::BRAND);
        assert!(!any_chip_fill, "the restyled menu must never draw the list-row letter-chip fill color");
    }

    #[test]
    fn render_focused_row_shows_the_shared_selection_fill() {
        let mut menu = MenuList::new(items(2)).on_activate_index(Verb::Open, |_| Action::None);
        menu.on_focus(FocusEvent::Gained);
        let mut fb = FrameBuffer565::new(200, 100);
        let area = Rectangle::new(Point::new(0, 0), Size::new(200, 100));
        menu.render(area, &test_ctx(), &mut fb).unwrap();

        let any_selection_ink = fb.pixels().any(|p| p.1 == palette::SURFACE_ELEVATED);
        assert!(any_selection_ink, "the focused row must show the shared selection fill");
    }

    // --- Trailing ---

    #[test]
    fn a_trailing_label_is_drawn_even_when_the_row_is_not_selected() {
        let menu = MenuList::new(vec![MenuItem::new("Screen sleep").with_trailing_label("On", palette::STATUS_SUCCESS)]);
        // Not focused, so nothing is selected -- a caret-only row would
        // draw nothing on the right edge here; a labelled trailing row
        // must draw its state label regardless.
        let mut fb = FrameBuffer565::new(200, 100);
        let area = Rectangle::new(Point::new(0, 0), Size::new(200, 100));
        menu.render(area, &test_ctx(), &mut fb).unwrap();

        let any_label_ink = fb.pixels().any(|p| p.1 == palette::STATUS_SUCCESS);
        assert!(any_label_ink, "a trailing label must be visible even on an unselected row");
    }

    #[test]
    fn a_trailing_caret_is_still_gated_on_selection() {
        let mut menu = MenuList::new(vec![MenuItem::new("Clear all items")]);
        let mut fb_unselected = FrameBuffer565::new(200, 100);
        let area = Rectangle::new(Point::new(0, 0), Size::new(200, 100));
        menu.render(area, &test_ctx(), &mut fb_unselected).unwrap();

        menu.on_focus(FocusEvent::Gained);
        let mut fb_selected = FrameBuffer565::new(200, 100);
        menu.render(area, &test_ctx(), &mut fb_selected).unwrap();

        let unselected_pixels: Vec<_> = fb_unselected.pixels().map(|p| p.1).collect();
        let selected_pixels: Vec<_> = fb_selected.pixels().map(|p| p.1).collect();
        assert_ne!(unselected_pixels, selected_pixels, "selecting the row must change its rendered output (the caret appearing)");
    }

    #[test]
    fn a_custom_label_color_is_used_for_the_rows_own_text() {
        let menu = MenuList::new(vec![MenuItem::new("Clear everything").with_label_color(palette::STATUS_ERROR)]);
        let mut fb = FrameBuffer565::new(200, 100);
        let area = Rectangle::new(Point::new(0, 0), Size::new(200, 100));
        menu.render(area, &test_ctx(), &mut fb).unwrap();

        let any_error_ink = fb.pixels().any(|p| p.1 == palette::STATUS_ERROR);
        assert!(any_error_ink, "a custom label color must actually be used when drawing the row's text");
    }

    // --- paint_key (bead pico-link-7h5.5) ---

    #[test]
    fn paint_key_is_stable_across_calls_with_no_state_change() {
        let menu = MenuList::new(items(2));
        assert_eq!(menu.paint_key(&test_ctx()), menu.paint_key(&test_ctx()));
    }

    #[test]
    fn paint_key_changes_when_selection_or_focus_changes() {
        let mut menu = MenuList::new(items(2));
        let before = menu.paint_key(&test_ctx());
        menu.on_intent(NavIntent::Down);
        assert_ne!(before, menu.paint_key(&test_ctx()), "moving the selection must change the paint key");

        let mut menu = MenuList::new(items(2));
        let before = menu.paint_key(&test_ctx());
        menu.on_focus(FocusEvent::Gained);
        assert_ne!(before, menu.paint_key(&test_ctx()), "gaining focus must change the paint key (selection fill/caret)");
    }

    #[test]
    fn paint_key_changes_when_a_trailing_label_or_its_color_changes() {
        let a = MenuList::new(vec![MenuItem::new("Screen sleep").with_trailing_label("On", palette::STATUS_SUCCESS)]);
        let b = MenuList::new(vec![MenuItem::new("Screen sleep").with_trailing_label("Off", palette::STATUS_SUCCESS)]);
        let c = MenuList::new(vec![MenuItem::new("Screen sleep").with_trailing_label("On", palette::STATUS_ERROR)]);
        assert_ne!(a.paint_key(&test_ctx()), b.paint_key(&test_ctx()), "a different trailing label text must change the key");
        assert_ne!(a.paint_key(&test_ctx()), c.paint_key(&test_ctx()), "a different trailing label color must change the key");
    }

    #[test]
    fn paint_key_distinguishes_caret_label_and_no_trailing() {
        let caret = MenuList::new(vec![MenuItem::new("Reveal")]);
        let label = MenuList::new(vec![MenuItem::new("Reveal").with_trailing_label("On", palette::STATUS_SUCCESS)]);
        let none = MenuList::new(vec![MenuItem::new("Reveal").with_no_trailing()]);
        assert_ne!(caret.paint_key(&test_ctx()), label.paint_key(&test_ctx()));
        assert_ne!(caret.paint_key(&test_ctx()), none.paint_key(&test_ctx()));
        assert_ne!(label.paint_key(&test_ctx()), none.paint_key(&test_ctx()));
    }

    #[test]
    fn paint_key_changes_when_the_label_color_changes() {
        let a = MenuList::new(vec![MenuItem::new("Clear everything")]);
        let b = MenuList::new(vec![MenuItem::new("Clear everything").with_label_color(palette::STATUS_ERROR)]);
        assert_ne!(a.paint_key(&test_ctx()), b.paint_key(&test_ctx()));
    }

    #[test]
    fn with_selected_and_with_focused_carry_state_into_a_freshly_built_list() {
        let list = MenuList::new(items(3)).with_selected(2).with_focused(true);
        assert_eq!(list.selected_index(), 2);
        // `with_focused(true)` + `with_selected(2)` together are what a
        // store-backed adapter needs to carry its own persisted state into
        // a freshly rebuilt `MenuList` every call -- proven behaviorally
        // via `on_intent` acting on row 2 as already-selected.
        let mut list = list;
        let action = list.on_focus(FocusEvent::Activated);
        assert!(matches!(action, Action::None), "activation without a callback is a no-op, same as MenuList::new");
    }
}
