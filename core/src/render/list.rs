//! A scrolling vertical list widget — the core content widget proving the
//! render core end-to-end (`VerticalMenu` is the salvaged concept,
//! reimplemented on `embedded-graphics` instead of the retired RGBA
//! rasterizer).
//!
//! [`ListItem`] is a display-only row shape, deliberately decoupled from
//! any application domain model — the widget layer shouldn't know about
//! whatever data a call site's list actually represents. A domain-specific
//! view maps its own model into `ListItem`s before handing them here.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use core::convert::Infallible;

use embedded_graphics::{
    draw_target::{DrawTarget, DrawTargetExt},
    pixelcolor::Rgb565,
    prelude::{Point, Primitive, Size},
    primitives::{PrimitiveStyle, Rectangle},
    Drawable,
};
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::FontRenderer;

use crate::input::NavIntent;

use super::framebuffer::FrameBuffer565;
use super::theme::{self, font, icon, palette};
use super::widget::{Action, FocusEvent, Widget};

/// An opaque row-identity key, supplied by the call site — e.g. a
/// Bluetooth device's 6-byte address. `Copy`/allocation-free by design:
/// `core` is `no_std` + `alloc`, and identity needs to be cheap to carry
/// around and compare on every rebuild of a live-data-backed list, so this
/// is a fixed-size byte key rather than a `String`/`Vec`-backed one.
///
/// This is what lets [`VerticalList::with_selected_identity`] resolve a
/// row's *position* across a rebuild instead of trusting a caller-supplied
/// index, which is the actual bug this type exists to close — see that
/// method's doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ListItemKey([u8; 8]);

impl ListItemKey {
    /// Builds a key from a raw 8-byte value. `const fn` so callers can
    /// define sentinel keys (e.g. a fixed row that isn't backed by any
    /// domain entity) as `const`s.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 8]) -> Self {
        Self(bytes)
    }

    /// Builds a key from a `u64` id, little-endian.
    #[must_use]
    pub const fn from_u64(id: u64) -> Self {
        Self(id.to_le_bytes())
    }
}

impl From<[u8; 6]> for ListItemKey {
    /// Widens a 6-byte Bluetooth device address into a key by
    /// zero-padding the top two bytes. Those two bytes are never `0xFF`
    /// for a real device address wrapped this way (a real address can be
    /// anything in its own 6 bytes, but this conversion always leaves
    /// bytes `[6]`/`[7]` at `0`), so a key built via
    /// [`ListItemKey::from_bytes`] with those two bytes non-zero (e.g.
    /// `[0xFF; 8]`, used by `pico_link_core::app` for its non-device
    /// "Scan" row) can never collide with a real device's key.
    fn from(addr: [u8; 6]) -> Self {
        let mut bytes = [0_u8; 8];
        bytes[..6].copy_from_slice(&addr);
        Self(bytes)
    }
}

/// A single displayable row. Display-only: no domain fields beyond an
/// optional identity key — just what a `VerticalList` needs to draw a row
/// and (if the caller supplies one) resolve its position by identity
/// rather than index across a rebuild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    pub label: String,
    pub sublabel: Option<String>,
    /// Overrides the row's chip content with an `open_iconic` glyph drawn
    /// with **no** background fill, instead of the default filled
    /// brand-colored chip with `label`'s initial letter — for a widget
    /// that wants "icon, no colored background" instead of "no chip at
    /// all". `None` (the default) keeps the letter chip. See [`RowChip`].
    pub icon: Option<char>,
    /// Overrides the row's second (sublabel) line with a graphical 4-bar
    /// signal-strength glyph ([`theme::draw_signal_bars`]) instead of
    /// `sublabel`'s plain text — design section 14, F10, unconditional
    /// for the pairing wizard's scan list (pico-link-0r3). `0..=4`; a
    /// caller passing both this and `sublabel` gets the glyph, since a
    /// row has exactly one second-line slot and the glyph is the more
    /// specific request. `None` (the default) leaves the slot to
    /// `sublabel`, unchanged from before this field existed.
    pub signal_bars: Option<u8>,
    /// This row's stable identity, if the caller has one (e.g. a
    /// Bluetooth device address). `None` for rows with no natural
    /// identity (a static menu row, a transient placeholder) — such rows
    /// keep falling back to index-based selection carry-forward, see
    /// [`VerticalList::with_selected_identity`].
    pub key: Option<ListItemKey>,
}

impl ListItem {
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            sublabel: None,
            icon: None,
            signal_bars: None,
            key: None,
        }
    }

    #[must_use]
    pub fn with_sublabel(mut self, sublabel: impl Into<String>) -> Self {
        self.sublabel = Some(sublabel.into());
        self
    }

    /// Draws this row's second line as a 4-bar signal glyph instead of
    /// text — see [`Self::signal_bars`]'s doc comment. `level` is not
    /// clamped here; [`theme::draw_signal_bars`] clamps at draw time.
    #[must_use]
    pub fn with_signal_bars(mut self, level: u8) -> Self {
        self.signal_bars = Some(level);
        self
    }

    /// Draws this row's chip slot as a background-less icon glyph instead
    /// of the default filled letter chip — see [`Self::icon`]'s doc
    /// comment.
    #[must_use]
    pub fn with_icon(mut self, icon: char) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Tags this row with a stable identity key — see [`ListItem::key`].
    #[must_use]
    pub fn with_key(mut self, key: ListItemKey) -> Self {
        self.key = Some(key);
        self
    }
}

/// Vertical padding above the name line and below the username line, and
/// the gap between the two lines.
///
/// Bumped from the original `1`/`1` (tuned only to fit the theme fonts
/// without overflow) per an explicit design-review tweak: the approved
/// mockup read as
/// cramped, both between a row's own name/username lines and between
/// consecutive rows (the bottom padding of row N plus the top padding of
/// row N+1 is what a user perceives as "space between rows"). Chosen to
/// visibly loosen both without shrinking the list to fewer than ~3 full
/// rows + a scroll peek — see `ROW_HEIGHT`'s doc comment for the
/// resulting row budget. (Epic B2 recomputed that budget for the 240x240
/// Pico Plus 2 W panel: unchanged padding still lands 5 full rows + a
/// 6px peek in the taller content area — see `ROW_HEIGHT`'s doc comment
/// for the arithmetic and the reasoning behind leaving these constants
/// alone rather than loosening them.)
const ROW_PADDING: i32 = 3;
const LINE_GAP: i32 = 2;

/// Worst-case pixel footprint (leading + ink height, including
/// descenders) of a single line rendered with [`font::name`]/
/// [`font::username`] from a `VerticalPosition::Top`-anchored position —
/// i.e. how much vertical room a `render_aligned(.., VerticalPosition::Top,
/// ..)` call at row-relative y=0 actually occupies in the worst case.
///
/// These are measured constants, not something `u8g2-fonts` can compute at
/// compile time (`FontRenderer::get_rendered_dimensions*` take `&self` and
/// aren't `const fn`) — derived once via a throwaway probe
/// (`core/examples/dim_probe.rs`, run manually, not part of the build)
/// against `"gjpqy"` (all five ASCII descenders) rendered in each font,
/// then hardcoded here the same way the retired `FONT_ASCENT`/
/// `FONT_DESCENT` constants were hardcoded from `FONT_6X10`'s metrics.
///
/// Using the worst case (not the specific name/username string being
/// rendered) is deliberate: this guards every possible name/username the
/// same way `ROW_HEIGHT`'s previous, font-metric-derived formula did,
/// rather than only the non-descender demo strings the design-review
/// spike's mockup happened to use (its "+2/+15" row offsets were tuned
/// against short, no-descender-style names, which real row data can't
/// guarantee).
const NAME_LINE_FOOTPRINT: i32 = 17;
const USERNAME_LINE_FOOTPRINT: i32 = 15;

/// Pixel height of a single row (padding + name line + gap + username
/// line + padding). Fixed, like the chrome bar heights in `chrome.rs` — a
/// pixel budget, not a screen-resolution assumption.
///
/// Grew from `FONT_6X10`'s 23px to accommodate the larger, proportional
/// `helvB12`/`helvR10` theme fonts, then grew again (35px -> 40px) for
/// `ROW_PADDING`/`LINE_GAP`'s "more vertical air" bump.
///
/// **Epic B2 (240x240 retarget) row budget, recomputed:** the content
/// area (screen height minus `chrome::TITLE_BAR_HEIGHT` and
/// `chrome::HINT_BAR_HEIGHT`) was 136px on the retired 320x170 panel,
/// giving 3 full rows (120px) plus a 16px partial-row "scroll peek". On
/// the 240x240 panel the same subtraction gives 206px of content — `206 /
/// 40 = 5` full rows (200px) plus a 6px peek. That is a deliberate
/// choice, not an accident of leaving the formula alone: the panel lost
/// 80px of *width* (a 25% cut to the already-tight chip+text+caret row
/// layout) while gaining 70px of *height*, so the extra vertical room
/// goes to showing more rows outright — cheaper scrolling through
/// device/codec lists on a screen that's already asking a user to read
/// more compressed row text — rather than to loosening rows further,
/// which would only spend the gain on whitespace the wider panel already
/// had enough of. `ROW_PADDING`/`LINE_GAP` (and therefore this constant's
/// formula) are unchanged from the design-reviewed 320x170 values; only
/// the resulting row *count* for the new panel is documented here. The
/// smaller 6px peek is still a real partial row (clipped, not hidden —
/// see `VerticalList::render`), just a less generous one than the old
/// panel's 16px. Still derived so the two lines' worst-case ink always
/// fits entirely inside `[0, ROW_HEIGHT)`, the same guarantee the
/// original `ROW_HEIGHT` doc comment describes and `render_png_dump.rs`'s
/// `text_never_bleeds_past_a_rows_bottom_padding` test still enforces.
pub const ROW_HEIGHT: u32 =
    (ROW_PADDING + NAME_LINE_FOOTPRINT + LINE_GAP + USERNAME_LINE_FOOTPRINT + ROW_PADDING) as u32;

/// Left margin (px) from a row's left edge to its chip's left edge.
const CHIP_LEFT_MARGIN: i32 = 6;
/// Gap (px) between a chip's right edge and the row's text block.
const CHIP_TEXT_GAP: i32 = 8;
/// Right margin (px) reserved for the focused row's disclosure caret.
const CARET_RIGHT_MARGIN: i32 = 6;

/// Side length (px) of a row's chip: sized to the two-line name+username
/// text block's own height (`NAME_LINE_FOOTPRINT + LINE_GAP +
/// USERNAME_LINE_FOOTPRINT`, i.e. `ROW_HEIGHT` minus its top/bottom
/// `ROW_PADDING`) so the chip's vertical extent lines up with the text
/// beside it instead of bleeding into the row's padding — the same
/// "vertical padding is sacred" invariant `text_never_bleeds_past_a_rows_
/// bottom_padding` enforces for text.
pub(crate) fn chip_size() -> u32 {
    (NAME_LINE_FOOTPRINT + LINE_GAP + USERNAME_LINE_FOOTPRINT) as u32
}

/// Row-relative X offset where a row's text block (name/username) starts —
/// past the chip's left margin, its own width, and the chip-to-text gap.
/// `pub(crate)`: see [`name_top_offset`]'s doc comment for why other
/// modules need to reuse layout constants like this one rather than
/// recomputing them independently.
pub(crate) fn text_left_offset() -> i32 {
    CHIP_LEFT_MARGIN + chip_size() as i32 + CHIP_TEXT_GAP
}

/// Row-relative Y offset for the name line's
/// `render_aligned(.., VerticalPosition::Top, ..)` call.
///
/// `pub(crate)`: shared with other views (e.g. `message.rs`) that draw
/// their own rows rather than delegating to `VerticalList`, but must
/// reuse this exact offset to avoid reintroducing the row-overflow bug
/// `ROW_HEIGHT`'s doc comment describes. Plain
/// layout constants now, not baseline-derived math — `u8g2-fonts`'
/// `VerticalPosition::Top` does the ascent/descent arithmetic internally,
/// which is the whole point of retiring the old `FONT_ASCENT`/
/// `FONT_DESCENT`/baseline-offset math this replaces.
pub(crate) const fn name_top_offset() -> i32 {
    ROW_PADDING
}

/// Row-relative Y offset for the username line's `render_aligned` call —
/// directly below the name line's worst-case footprint, plus `LINE_GAP`.
/// `pub(crate)`: see [`name_top_offset`]'s doc comment.
pub(crate) const fn username_top_offset() -> i32 {
    name_top_offset() + NAME_LINE_FOOTPRINT + LINE_GAP
}

/// Reconciles a list's scroll-top **row index** against a newly resolved
/// selection — the "only scroll at the viewport edges" rule, replacing
/// the retired `scroll_offset_for_selection`/
/// `VerticalList::scroll_for_viewport` (see their old doc comments' git
/// history for why this file used to pin the selected row to the
/// viewport's bottom edge on every render — a `selected`-only pure
/// function had no way to know the list *hadn't* scrolled off past that
/// row, only where it currently is).
///
/// Index-based (rows, not pixels) so it's resolution-independent — the
/// caller multiplies by whatever its row height is (`ROW_HEIGHT` here, a
/// pixel-range variant for other views' per-field scrolling needs).
///
/// # Why this runs at render time, not in `on_intent`
///
/// `on_intent` only knows the selection *delta* (`NavIntent::Down`/
/// `Up`/`JumpBy`) — it has no idea how many rows the viewport can
/// currently show (`area.size.height` is a render-time input, passed to
/// `Widget::render`, never to `Widget::on_intent`). This function is
/// therefore called from `render`, fed whatever `top_index` was
/// persisted from the *previous* render — **not** a design smell to
/// "fix" by threading `visible_rows` through `on_intent` instead: the
/// rule below is an idempotent clamp (calling it twice in a row with
/// the same inputs returns the same `top`), so re-running it every
/// render is exactly as correct as running it once per intent would be,
/// just simpler (one call site, no risk of `on_intent` and `render`
/// disagreeing about `visible_rows` if the viewport is ever resized).
/// **Do not** move this into `on_intent` — that would reintroduce the
/// "no viewport dimensions available" problem this design sidesteps.
///
/// # The rule
///
/// - No items: top is always `0`.
/// - `prev_top` is first clamped to `max_top` (`item_count -
///   visible_rows`, floored at `0`) — handles the list having shrunk
///   since the last render (e.g. a deletion), so a stale `top` can't
///   leave blank space below the last row.
/// - If `selected` is above the current window (`selected < top`):
///   scroll up exactly enough to make it the *first* visible row.
/// - If `selected` is below the current window (`selected >= top +
///   visible_rows`): scroll down exactly enough to make it the *last*
///   visible row.
/// - Otherwise (`selected` is already somewhere inside `[top, top +
///   visible_rows)`): **`top` is left unchanged.** This is the actual
///   fix — the old pin-to-bottom formula recomputed a fresh scroll
///   position from `selected` alone on every call, so moving the
///   selection *up* while it was still fully visible re-pinned it to
///   the viewport's bottom edge anyway (repro: move down, then back up
///   one row — the row above was already on screen, yet the old code
///   scrolled the list to redraw it at the bottom).
#[must_use]
pub(crate) fn reconcile_top_index(prev_top: usize, selected: usize, visible_rows: usize, item_count: usize) -> usize {
    if item_count == 0 {
        return 0;
    }
    let visible_rows = visible_rows.max(1);
    let max_top = item_count.saturating_sub(visible_rows);
    let mut top = prev_top.min(max_top);

    if selected < top {
        top = selected;
    } else if selected >= top + visible_rows {
        top = selected - visible_rows + 1;
    }
    // else: selected is already visible within the current window --
    // leave `top` unchanged. THE FIX.

    top.min(max_top)
}

/// What [`draw_row`] draws inside a row's fixed chip slot — the slot's
/// position/size ([`CHIP_LEFT_MARGIN`]/[`chip_size`]) and the text block's
/// offset ([`text_left_offset`]) never change; only the slot's *content*
/// does — a plain boolean "show a chip or don't" would be the wrong shape
/// for a widget that wants "icon, no colored background" as its
/// differentiator from the default letter chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowChip {
    /// The default style: a solid [`palette::BRAND`]-filled chip with
    /// `name`'s uppercased first character centered in it, via
    /// [`theme::draw_chip`].
    Letter,
    /// The icon style: no background fill, just an `open_iconic` glyph,
    /// via [`theme::draw_icon_chip`].
    Icon(char),
}

/// What [`draw_row`] draws in a row's second (sublabel) line slot — a
/// row has exactly one such slot, so this is an enum (mutually exclusive
/// content) rather than the two separate `Option`s [`ListItem`] itself
/// carries (`sublabel`/`signal_bars`), which `draw_row`'s caller resolves
/// down to one of these three per [`ListItem::signal_bars`]'s
/// priority-over-`sublabel` doc comment. Folding two parameters into one
/// also keeps `draw_row`'s argument count under `clippy::pedantic`'s
/// `too_many_arguments` threshold.
#[derive(Clone, Copy)]
pub(crate) enum RowSecondLine<'a> {
    /// No second line at all.
    None,
    /// Plain sublabel text, rendered with [`font::username`].
    Text(&'a str),
    /// A graphical 4-bar signal glyph (pico-link-0r3, design section 14
    /// F10) via [`theme::draw_signal_bars`], instead of text.
    SignalBars(u8),
}

/// Draws one list row's shared visual language: an optional hairline
/// bottom divider, the chip, the bold name line, the muted username line,
/// and — for the focused/selected row — the full-width selection fill
/// (via [`theme::draw_selection`]) plus a right-edge disclosure caret.
///
/// Extracted as a free function (rather than duplicated inside both
/// `VerticalList::render` and other views' hand-rolled row loops, per
/// [`name_top_offset`]'s doc comment on why that duplication exists at
/// all) so the independent row-rendering call sites cannot visually
/// drift apart — a design tweak here lands in all of them by
/// construction, not by remembering to update each one.
///
/// `draw_divider` is the caller's decision, not derived here: a caller
/// iterating its own rows top-to-bottom knows whether *this* row is
/// selected, which is the only input the divider rule needs (see the call
/// sites) — drawn only below an *unselected* row, since a following
/// selected row's own full-width fill (drawn on top, after, when that next
/// row is rendered) already paints over/replaces it, and a divider
/// directly below a *selected* row would fight the selection block's own
/// bottom edge instead of reading as a plain row separator.
///
/// `chip` ([`RowChip`]) selects what's drawn in the row's chip slot — a
/// call site can pass `RowChip::Letter` (the default) or
/// `RowChip::Icon(..)` per [`ListItem::icon`]. The slot's geometry is
/// identical either way, so the text block's `text_left_offset` never
/// needs to change.
///
/// Generic over `D: DrawTarget<Color = Rgb565, Error = Infallible>` for
/// the same reason [`theme::draw_selection`] is — callers pass a
/// `DrawTargetExt::clipped()` sub-region directly.
///
/// # Errors
///
/// Returns `Infallible`'s uninhabited variant in practice — see
/// [`super::widget::Widget::render`]'s doc comment for why the `Result`
/// return exists at all.
/// Ellipsis appended by [`truncate_label_to_width`] when a label is cut.
///
/// Three ASCII periods, not the Unicode `…` (U+2026), for the same reason
/// [`hero.rs`](super::hero)'s own `truncate_to_width` uses ASCII: every
/// `FontRenderer` in [`theme::font`] is built `with_ignore_unknown_chars(true)`,
/// which silently *drops* glyphs missing from a `u8g2` font's embedded
/// glyph set rather than erroring, and the `_tf` font subsets used here
/// aren't guaranteed to carry the Unicode ellipsis codepoint. An ellipsis
/// glyph that silently vanished would make a truncated label look merely
/// short, not truncated; every font here embeds ASCII `.`, so three of
/// them are guaranteed to render.
const ELLIPSIS: &str = "...";

/// The horizontal pixel footprint `render_aligned` would give `text` in
/// `font` — duplicated from [`hero.rs`](super::hero)'s private
/// `text_width` (itself duplicated from `screen.rs`) for the same "no
/// shared home for a helper this small, used by only one module each"
/// reason those two give.
fn text_width(font: &FontRenderer, text: &str) -> u32 {
    font.get_rendered_dimensions_aligned(text, Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
        .unwrap_or(None)
        .map_or(0, |bbox| bbox.size.width)
}

/// Truncates `name` to fit within `max_width` px when set in `font`,
/// appending [`ELLIPSIS`] when truncation actually happens — the width
/// clamp `draw_row` was missing entirely (pico-link-ok1). Row labels come
/// from live Bluetooth scan results, so their length is never in this
/// widget's control; the previous code drew them left-aligned with no
/// clamp at all, so a name a few characters over budget silently
/// overflowed into the disclosure caret's slot.
///
/// Same shape as [`hero.rs`](super::hero)'s `truncate_to_width` (the
/// device name's own "truncates with an ellipsis, never a marquee" rule),
/// duplicated rather than shared for the reason [`text_width`]'s doc
/// comment gives. Widths are measured, not assumed from a fixed
/// characters-per-row count, because the row font is proportional: `i`
/// and `M` do not cost the same px, and a per-font "average character
/// width" would either under-truncate wide names or over-truncate narrow
/// ones.
///
/// Returns `name` itself (as an owned `String`, since the caller needs a
/// value it can render either way) when it already fits — the common
/// case, so no truncation search runs at all.
///
/// If `max_width` is too small to fit even [`ELLIPSIS`] alone, returns an
/// empty string rather than a misleading lone glyph or a half-drawn
/// ellipsis: there's no readable state smaller than the ellipsis itself,
/// and drawing nothing reads more clearly as "no room" than a fragment
/// that looks like its own rendering bug. This never happens on the
/// current 240px panel — `chip_size()` + `CHIP_TEXT_GAP` +
/// `CARET_RIGHT_MARGIN` + a caret glyph still leaves comfortably more
/// than three periods' worth of width — but a future narrower row
/// (a nested list, a smaller panel) could reach it, so the branch exists
/// deliberately rather than by omission.
pub(crate) fn truncate_label_to_width(font: &FontRenderer, name: &str, max_width: i32) -> String {
    if max_width <= 0 {
        return String::new();
    }
    let Ok(max_width) = u32::try_from(max_width) else {
        return String::new();
    };
    if text_width(font, name) <= max_width {
        return String::from(name);
    }
    if text_width(font, ELLIPSIS) > max_width {
        return String::new();
    }
    let mut end = name.len();
    while end > 0 {
        end -= 1;
        while end > 0 && !name.is_char_boundary(end) {
            end -= 1;
        }
        let mut candidate = String::from(&name[..end]);
        candidate.push_str(ELLIPSIS);
        if text_width(font, &candidate) <= max_width {
            return candidate;
        }
    }
    String::from(ELLIPSIS)
}

pub(crate) fn draw_row<D>(
    target: &mut D,
    row_rect: Rectangle,
    name: &str,
    second_line: RowSecondLine<'_>,
    selected: bool,
    draw_divider: bool,
    chip: RowChip,
) -> Result<(), Infallible>
where
    D: DrawTarget<Color = Rgb565, Error = Infallible>,
{
    if selected {
        theme::draw_selection(row_rect, target)?;
    } else if draw_divider {
        let divider = Rectangle::new(
            Point::new(row_rect.top_left.x, row_rect.top_left.y + row_rect.size.height as i32 - 1),
            Size::new(row_rect.size.width, 1),
        );
        divider.into_styled(PrimitiveStyle::with_fill(palette::DIVIDER)).draw(target)?;
    }

    let chip_rect = Rectangle::new(
        Point::new(row_rect.top_left.x + CHIP_LEFT_MARGIN, row_rect.top_left.y + ROW_PADDING),
        Size::new_equal(chip_size()),
    );
    match chip {
        RowChip::Letter => {
            let initial = name.chars().next().map_or('#', |c| c.to_ascii_uppercase());
            theme::draw_chip(target, chip_rect, initial)?;
        }
        RowChip::Icon(glyph) => {
            theme::draw_icon_chip(target, chip_rect, glyph)?;
        }
    }
    let text_x = row_rect.top_left.x + text_left_offset();

    // Reserved unconditionally, whether or not *this* row is selected —
    // reserving it only when `selected` would make a row's available
    // text width (and therefore its truncation point) change the moment
    // focus lands on or leaves it, which would read as the label itself
    // changing rather than the same label just losing/gaining a caret.
    let mut caret_buf = [0_u8; 4];
    let caret: &str = icon::CARET_RIGHT.encode_utf8(&mut caret_buf);
    let caret_reserved_width = text_width(&font::icon_1x(), caret) as i32;
    let available_text_width =
        row_rect.size.width as i32 - text_left_offset() - CARET_RIGHT_MARGIN - caret_reserved_width;

    let name_display = truncate_label_to_width(&font::name(), name, available_text_width);
    let _ = font::name().render_aligned(
        name_display.as_str(),
        Point::new(text_x, row_rect.top_left.y + name_top_offset()),
        VerticalPosition::Top,
        HorizontalAlignment::Left,
        FontColor::Transparent(palette::TEXT_PRIMARY),
        target,
    );

    match second_line {
        RowSecondLine::SignalBars(level) => {
            let bars_rect = Rectangle::new(
                Point::new(text_x, row_rect.top_left.y + username_top_offset()),
                Size::new(theme::SIGNAL_GLYPH_WIDTH, USERNAME_LINE_FOOTPRINT as u32),
            );
            theme::draw_signal_bars(target, bars_rect, level)?;
        }
        RowSecondLine::Text(username) => {
            let username_display = truncate_label_to_width(&font::username(), username, available_text_width);
            let _ = font::username().render_aligned(
                username_display.as_str(),
                Point::new(text_x, row_rect.top_left.y + username_top_offset()),
                VerticalPosition::Top,
                HorizontalAlignment::Left,
                FontColor::Transparent(palette::TEXT_SECONDARY),
                target,
            );
        }
        RowSecondLine::None => {}
    }

    if selected {
        let caret_x = row_rect.top_left.x + row_rect.size.width as i32 - CARET_RIGHT_MARGIN;
        let caret_y = row_rect.top_left.y + row_rect.size.height as i32 / 2;
        let _ = font::icon_1x().render_aligned(
            caret,
            Point::new(caret_x, caret_y),
            VerticalPosition::Center,
            HorizontalAlignment::Right,
            FontColor::Transparent(palette::TEXT_PRIMARY),
            target,
        );
    }

    Ok(())
}

/// Callback invoked with the selected item on activation. Named as a type
/// alias purely to keep `VerticalList`'s field type readable.
type OnActivate = Box<dyn Fn(&ListItem) -> Action>;

/// Callback invoked with the selected row's **index** on activation. See
/// [`VerticalList::on_activate_index`]'s doc comment for why this exists
/// alongside [`OnActivate`].
type OnActivateIndex = Box<dyn Fn(usize) -> Action>;

/// A focusable, scrollable vertical list of [`ListItem`]s. Moves its
/// internal selection in response to `NavIntent::{Up,Down,JumpBy}` via
/// `Widget::on_intent`, auto-scrolling to keep the selection visible (only
/// at the viewport edges — see [`reconcile_top_index`]); fires its
/// `on_activate`/`on_activate_index` callback (if any) when activated
/// while focused.
pub struct VerticalList {
    items: Vec<ListItem>,
    selected: usize,
    /// The row index currently scrolled to the top of the viewport.
    /// `Cell`, not a plain field: `Widget::render` takes `&self` but still
    /// needs to persist this across calls (see [`reconcile_top_index`]'s
    /// doc comment on why the reconciliation runs in `render`, not
    /// `on_intent`).
    top_index: Cell<usize>,
    focused: bool,
    on_activate: Option<OnActivate>,
    on_activate_index: Option<OnActivateIndex>,
}

impl VerticalList {
    #[must_use]
    pub fn new(items: Vec<ListItem>) -> Self {
        Self {
            items,
            selected: 0,
            top_index: Cell::new(0),
            focused: false,
            on_activate: None,
            on_activate_index: None,
        }
    }

    /// Registers a callback invoked with the selected `ListItem` when the
    /// list is activated (joystick press, button A / `NavIntent::Select`)
    /// while focused. Typically used to return `Action::PushView(...)`.
    #[must_use]
    pub fn on_activate(mut self, callback: impl Fn(&ListItem) -> Action + 'static) -> Self {
        self.on_activate = Some(Box::new(callback));
        self
    }

    /// Registers a callback invoked with the selected row's **index** (not
    /// its `ListItem`) when the list is activated while focused.
    ///
    /// An index-keyed alternative to [`Self::on_activate`], for callers
    /// with a fixed, known-shape menu (e.g. a home screen's static rows)
    /// that need to dispatch on *which row* was activated. The point of
    /// adding this
    /// rather than reusing `on_activate` is what it *prevents*: matching
    /// against `ListItem::label` (arbitrary, human-facing display text) to
    /// decide what a row does is fragile — a copy tweak to a row's wording
    /// would silently break its activation behavior with no compiler
    /// error. An index into a caller-known, fixed row order has no such
    /// failure mode.
    ///
    /// Takes precedence over `on_activate` if both happen to be set —
    /// activation only ever fires one callback, never both.
    #[must_use]
    pub fn on_activate_index(mut self, callback: impl Fn(usize) -> Action + 'static) -> Self {
        self.on_activate_index = Some(Box::new(callback));
        self
    }

    /// Sets the initially selected row, clamped to the item list's bounds.
    ///
    /// Used by store-backed widgets (e.g. a list view backed by live
    /// application data) that rebuild a fresh `VerticalList` from live
    /// data on every render call
    /// but need to carry forward the persistent selection they track
    /// themselves — `VerticalList::new` alone always starts at `0`.
    #[must_use]
    pub fn with_selected(mut self, selected: usize) -> Self {
        self.selected = selected.min(self.items.len().saturating_sub(1));
        self
    }

    /// Sets the initially selected row by **identity**, with an
    /// index-based fallback — the fix for the defect `with_selected`
    /// (index-only) has: a store-backed list rebuilt from live data on
    /// every model change (the devices screen driven by Bluetooth
    /// inquiry results, e.g.) can grow, shrink, or reorder between builds,
    /// and a plain index into the *new* list no longer points at the row
    /// the user was actually looking at.
    ///
    /// `prev_key` is the outgoing widget's [`Self::selected_key`] (`None`
    /// if it had no keyed row selected, e.g. the very first build).
    /// `prev_index` is the outgoing widget's plain [`Self::selected_index`],
    /// used only as the fallback below.
    ///
    /// # The rule
    ///
    /// - If `prev_key` is `Some` and some row in the freshly built `items`
    ///   carries that same key: select **that row**, wherever it now sits.
    ///   This is the actual point of this method — inserting or removing
    ///   *other* rows around the selected one must never move the
    ///   selection, and a rebuild (e.g. a late name arriving for an
    ///   existing device, replacing that row's label in place) that keeps
    ///   the same key at the same index is naturally a no-op here too.
    /// - Otherwise — `prev_key` is `None` (first build, or the caller has
    ///   no identity for this list), or the key it names is no longer
    ///   present (the previously selected row was removed / timed out) —
    ///   fall back to **clamping `prev_index` to the new list's bounds**
    ///   (`prev_index.min(items.len().saturating_sub(1))`). This is a
    ///   deliberate "clamp to the nearest surviving position" rule, not a
    ///   reset to row `0`: if the selected row was near the bottom of a
    ///   long list and vanished, landing back at row 0 would be just as
    ///   disorienting as an unrelated selection jump, so the fallback
    ///   keeps the cursor at roughly the same *place* in the list instead.
    #[must_use]
    pub fn with_selected_identity(mut self, prev_key: Option<ListItemKey>, prev_index: usize) -> Self {
        self.selected = prev_key
            .and_then(|key| self.items.iter().position(|item| item.key == Some(key)))
            .unwrap_or_else(|| prev_index.min(self.items.len().saturating_sub(1)));
        self
    }

    /// The currently selected row's identity key, if it has one — the
    /// `prev_key` a caller reads back before rebuilding this widget from
    /// scratch, to pass into the replacement's
    /// [`Self::with_selected_identity`]. `None` if the list is empty or
    /// the selected row was never tagged with [`ListItem::with_key`].
    #[must_use]
    pub fn selected_key(&self) -> Option<ListItemKey> {
        self.items.get(self.selected).and_then(|item| item.key)
    }

    /// Sets the initial focus-highlight state. Same rationale as
    /// `with_selected`: a caller that rebuilds this widget fresh per render
    /// still needs to carry forward focus state it tracks itself.
    #[must_use]
    pub fn with_focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    #[must_use]
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    #[must_use]
    pub fn is_focused(&self) -> bool {
        self.focused
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

impl Widget for VerticalList {
    fn measure(&self, constraints: Size) -> Size {
        // A list fills whatever vertical space its screen gives it; it
        // manages overflow itself via scrolling, not by requesting more
        // height than is on offer.
        constraints
    }

    fn is_focusable(&self) -> bool {
        !self.items.is_empty()
    }

    fn selected_index(&self) -> Option<usize> {
        Some(self.selected)
    }

    fn selected_key(&self) -> Option<ListItemKey> {
        VerticalList::selected_key(self)
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
                    return callback(self.selected);
                }
                if let (Some(callback), Some(item)) =
                    (&self.on_activate, self.items.get(self.selected))
                {
                    callback(item)
                } else {
                    Action::None
                }
            }
        }
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

    fn render(&self, area: Rectangle, target: &mut FrameBuffer565) -> Result<(), Infallible> {
        // Real clipping (DrawTargetExt::clipped), not the retired
        // character-skip marquee: anything a row draws outside `area` —
        // an over-long label, a row scrolled partway off the top/bottom —
        // is simply dropped by the clip, not manually truncated by the
        // widget.
        let mut clipped = target.clipped(&area);

        // "Only scroll at the viewport edges" -- see
        // `reconcile_top_index`'s doc comment for the full rule and why
        // this reconciliation happens here, in `render`, rather than in
        // `on_intent`.
        let visible_rows = (area.size.height / ROW_HEIGHT).max(1) as usize;
        let top = reconcile_top_index(self.top_index.get(), self.selected, visible_rows, self.items.len());
        self.top_index.set(top);
        let scroll = top as u32 * ROW_HEIGHT;

        for (index, item) in self.items.iter().enumerate() {
            let row_top =
                area.top_left.y + (index as u32 * ROW_HEIGHT) as i32 - scroll as i32;

            // Skip rows fully outside the viewport — a cheap early-out;
            // `clipped` would drop their pixels anyway, but there's no
            // point building draw commands for off-screen rows.
            if row_top + ROW_HEIGHT as i32 <= area.top_left.y
                || row_top >= area.top_left.y + area.size.height as i32
            {
                continue;
            }

            let row_rect = Rectangle::new(
                Point::new(area.top_left.x, row_top),
                Size::new(area.size.width, ROW_HEIGHT),
            );

            let selected = self.focused && index == self.selected;
            let chip = item.icon.map_or(RowChip::Letter, RowChip::Icon);
            // `signal_bars` wins over plain sublabel text when both are
            // set on one item — see `ListItem::signal_bars`'s doc
            // comment for why a row has exactly one second-line slot.
            let second_line = match (item.signal_bars, item.sublabel.as_deref()) {
                (Some(level), _) => RowSecondLine::SignalBars(level),
                (None, Some(text)) => RowSecondLine::Text(text),
                (None, None) => RowSecondLine::None,
            };
            draw_row(
                &mut clipped,
                row_rect,
                item.label.as_str(),
                second_line,
                selected,
                !selected,
                chip,
            )?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(n: usize) -> Vec<ListItem> {
        (0..n).map(|i| ListItem::new(format!("item-{i}"))).collect()
    }

    #[test]
    fn empty_list_is_not_focusable() {
        let list = VerticalList::new(vec![]);
        assert!(!list.is_focusable());
    }

    #[test]
    fn non_empty_list_is_focusable() {
        let list = VerticalList::new(items(3));
        assert!(list.is_focusable());
    }

    #[test]
    fn next_and_prev_move_selection_and_clamp_at_the_ends() {
        let mut list = VerticalList::new(items(3));
        assert_eq!(list.selected_index(), 0);

        list.on_intent(NavIntent::Up); // clamp below zero
        assert_eq!(list.selected_index(), 0);

        list.on_intent(NavIntent::Down);
        assert_eq!(list.selected_index(), 1);
        list.on_intent(NavIntent::Down);
        assert_eq!(list.selected_index(), 2);
        list.on_intent(NavIntent::Down); // clamp at the end
        assert_eq!(list.selected_index(), 2);

        list.on_intent(NavIntent::Up);
        assert_eq!(list.selected_index(), 1);
    }

    #[test]
    fn next_n_jumps_and_clamps() {
        let mut list = VerticalList::new(items(10));
        list.on_intent(NavIntent::JumpBy(4));
        assert_eq!(list.selected_index(), 4);
        list.on_intent(NavIntent::JumpBy(20));
        assert_eq!(list.selected_index(), 9);
    }

    #[test]
    fn intent_on_empty_list_does_not_panic() {
        let mut list = VerticalList::new(vec![]);
        let action = list.on_intent(NavIntent::Down);
        assert!(matches!(action, Action::None));
    }

    #[test]
    fn activate_without_callback_is_a_noop() {
        let mut list = VerticalList::new(items(2));
        let action = list.on_focus(FocusEvent::Activated);
        assert!(matches!(action, Action::None));
    }

    #[test]
    fn activate_with_callback_invokes_it_with_the_selected_item() {
        let mut list = VerticalList::new(items(3)).on_activate(|item| {
            assert_eq!(item.label, "item-1");
            Action::PopView
        });
        list.on_intent(NavIntent::Down); // select index 1
        let action = list.on_focus(FocusEvent::Activated);
        assert!(matches!(action, Action::PopView));
    }

    #[test]
    fn on_activate_index_receives_the_selected_rows_index_not_its_label() {
        // The index-activation seam: activation routes on the
        // row's *position*, not a string match against its label -- proven
        // here by using labels ("item-0"/"item-1"/"item-2") that carry no
        // hint of index 1 being special, only the callback's own assertion
        // on the numeric index does.
        let mut list = VerticalList::new(items(3)).on_activate_index(|index| {
            assert_eq!(index, 1, "the callback must receive the selected row's index");
            Action::PopView
        });
        list.on_intent(NavIntent::Down); // select index 1
        let action = list.on_focus(FocusEvent::Activated);
        assert!(matches!(action, Action::PopView));
    }

    #[test]
    fn on_activate_index_takes_precedence_when_both_callbacks_are_registered() {
        let mut list = VerticalList::new(items(3))
            .on_activate(|_item| Action::PopView)
            .on_activate_index(|index| {
                assert_eq!(index, 0);
                Action::Back
            });
        let action = list.on_focus(FocusEvent::Activated);
        assert!(matches!(action, Action::Back), "on_activate_index must win when both callbacks are set");
    }

    #[test]
    fn gained_and_lost_toggle_is_focused() {
        let mut list = VerticalList::new(items(1));
        assert!(!list.is_focused());
        list.on_focus(FocusEvent::Gained);
        assert!(list.is_focused());
        list.on_focus(FocusEvent::Lost);
        assert!(!list.is_focused());
    }

    #[test]
    fn scroll_for_viewport_keeps_the_selection_visible() {
        // Follows the "only scroll at the viewport edges" rule:
        // `VerticalList` now persists `top_index` across `render` calls
        // instead of recomputing a fresh scroll position from `selected`
        // alone every time (the retired `scroll_offset_for_selection`'s
        // pin-to-bottom behavior).
        let mut list = VerticalList::new(items(10));
        let visible_rows = 3;
        let viewport_height = visible_rows as u32 * ROW_HEIGHT; // fits 3 rows
        let area = Rectangle::new(Point::new(0, 0), Size::new(240, viewport_height));
        let mut fb = FrameBuffer565::new(240, viewport_height);

        // Selection within the first screenful: no scroll needed.
        list.render(area, &mut fb).unwrap();
        assert_eq!(list.top_index.get(), 0);

        // Selecting row 4 (0-indexed) means rows 0-3 no longer all fit;
        // top should advance just enough to make row 4 the last visible
        // row (top=2: rows 2,3,4 visible), not "selected's own bottom
        // pinned to the viewport bottom" (the old, buggy rule).
        for _ in 0..4 {
            list.on_intent(NavIntent::Down);
        }
        list.render(area, &mut fb).unwrap();
        assert_eq!(list.top_index.get(), 2);

        // The selected row's own top and bottom must both land inside
        // the visible window.
        let scroll = list.top_index.get() as u32 * ROW_HEIGHT;
        let selected_top = 4 * ROW_HEIGHT;
        let selected_bottom = selected_top + ROW_HEIGHT;
        assert!(selected_top >= scroll);
        assert!(selected_bottom <= scroll + viewport_height);
    }

    #[test]
    fn moving_up_while_still_visible_does_not_re_pin_to_the_viewport_edge() {
        // The original repro, exercised through the actual widget (not
        // just `reconcile_top_index` directly, below): move down enough
        // to scroll, then back up ONE row that was already visible --
        // the list must not scroll again.
        let mut list = VerticalList::new(items(10));
        let visible_rows = 3;
        let viewport_height = visible_rows as u32 * ROW_HEIGHT;
        let area = Rectangle::new(Point::new(0, 0), Size::new(240, viewport_height));
        let mut fb = FrameBuffer565::new(240, viewport_height);

        for _ in 0..4 {
            list.on_intent(NavIntent::Down);
        }
        list.render(area, &mut fb).unwrap(); // selected=4, top settles at 2
        let top_after_scrolling_down = list.top_index.get();
        assert_eq!(top_after_scrolling_down, 2);

        list.on_intent(NavIntent::Up); // selected=3, still within [2, 5)
        list.render(area, &mut fb).unwrap();
        assert_eq!(
            list.top_index.get(),
            top_after_scrolling_down,
            "moving up into an already-visible row must not scroll the list"
        );
    }

    mod reconcile_top_index_tests {
        use super::super::reconcile_top_index;

        // Design-spec traces, each checked independently
        // against the pure function rather than through the widget, so
        // the exact rule is pinned down unambiguously.

        #[test]
        fn a_big_jump_scrolls_down_just_enough_to_reveal_the_new_selection() {
            // JumpBy(20) from top=0, visible_rows=3, 10 items clamps
            // selection to the last item (9), and top should land at 7
            // (rows 7,8,9 visible -- 9 is the new last visible row).
            assert_eq!(reconcile_top_index(0, 9, 3, 10), 7);
        }

        #[test]
        fn moving_above_the_window_scrolls_up_to_make_it_the_first_row() {
            // Continuing from top=7: selecting row 6 (above the current
            // [7, 10) window) scrolls up so it becomes the top row.
            assert_eq!(reconcile_top_index(7, 6, 3, 10), 6);
        }

        #[test]
        fn moving_within_the_current_window_leaves_top_unchanged() {
            // THE FIX: with top=6 (window [6, 9)), selecting 6, 7, or 8
            // must never change top -- this is exactly what the old
            // pin-to-bottom formula got wrong (it would re-pin on every
            // move regardless of whether the row was already visible).
            assert_eq!(reconcile_top_index(6, 6, 3, 10), 6);
            assert_eq!(reconcile_top_index(6, 7, 3, 10), 6);
            assert_eq!(reconcile_top_index(6, 8, 3, 10), 6);
        }

        #[test]
        fn a_shrinking_list_clamps_top_so_there_is_no_blank_space_below_the_last_row() {
            // The list shrank to 4 items (visible_rows still 3) while top
            // was still 6 from a longer list -- max_top is now 4-3=1, so
            // top must clamp down to 1, not leave rows 6.. rendering
            // nothing while the viewport has unused space.
            assert_eq!(reconcile_top_index(6, 3, 3, 4), 1);
        }

        #[test]
        fn zero_items_always_reports_top_zero() {
            assert_eq!(reconcile_top_index(5, 0, 3, 0), 0);
        }

        #[test]
        fn visible_rows_of_zero_is_treated_as_one_not_a_division_by_zero() {
            // `visible_rows.max(1)` in the implementation -- a
            // zero-height viewport must not panic or produce a
            // nonsensical max_top.
            assert_eq!(reconcile_top_index(0, 0, 0, 5), 0);
        }

        #[test]
        fn regression_47g_moving_below_the_window_then_back_up_one_row_leaves_top_unchanged() {
            // The exact bug originally reported, reproduced against the
            // pure function directly (the widget-level equivalent is
            // `moving_up_while_still_visible_does_not_re_pin_to_the_
            // viewport_edge` above): the selection moves down far enough
            // that the window has to scroll (top advances from 0 to 2 as
            // selected goes 0->4, one `Down` at a time, mirroring what
            // `VerticalList::render` actually does on every frame), then
            // Up moves the selection back up ONE row that is still
            // inside that window -- top must stay exactly where it was,
            // not recompute a fresh position from the new `selected`
            // alone (the retired `scroll_offset_for_selection`'s bug:
            // it would have put top back at 1, not 2, because it never
            // looked at where the window currently was).
            let visible_rows = 3;
            let item_count = 10;

            let mut top = 0;
            for selected in 0..=4 {
                top = reconcile_top_index(top, selected, visible_rows, item_count);
            }
            assert_eq!(top, 2, "sanity check: the window should have scrolled to keep row 4 visible");

            let top_before_prev = top;
            let top_after_prev = reconcile_top_index(top, 3, visible_rows, item_count);
            assert_eq!(
                top_after_prev, top_before_prev,
                "moving back into an already-visible row must not scroll the list"
            );
        }
    }

    #[test]
    fn render_does_not_panic_for_a_small_viewport_with_more_rows_than_fit() {
        let list = VerticalList::new(items(50));
        let mut fb = FrameBuffer565::new(64, 40);
        let area = Rectangle::new(Point::new(0, 0), Size::new(64, 40));
        list.render(area, &mut fb).unwrap();
    }

    // --- pico-link-0r3: F10, the scan list's real 4-bar signal glyph ---

    /// Scans the row's second (sublabel/signal-bars) line for any pixel
    /// in `color` -- coarse ("is this color present at all in the slot",
    /// not "exactly where") on purpose, since the goal here is telling
    /// "a graphical glyph was drawn" apart from "plain text was drawn"
    /// (which never paints `palette::BRAND_BRIGHT`/`palette::DIVIDER`,
    /// only `palette::TEXT_SECONDARY` glyph ink), not pinning down the
    /// bar layout `theme::draw_signal_bars`'s own tests already cover.
    fn second_line_contains_color(fb: &FrameBuffer565, row_rect: Rectangle, color: embedded_graphics::pixelcolor::Rgb565) -> bool {
        let y_top = row_rect.top_left.y + username_top_offset();
        let y_bottom = y_top + USERNAME_LINE_FOOTPRINT;
        (row_rect.top_left.x..row_rect.top_left.x + row_rect.size.width as i32)
            .any(|x| (y_top..y_bottom).any(|y| fb.pixel(Point::new(x, y)) == color))
    }

    #[test]
    fn a_row_with_signal_bars_draws_the_graphical_glyph_not_sublabel_text() {
        let list = VerticalList::new(vec![ListItem::new("Cans").with_signal_bars(3)]);
        let mut fb = FrameBuffer565::new(240, ROW_HEIGHT);
        let area = Rectangle::new(Point::new(0, 0), Size::new(240, ROW_HEIGHT));
        list.render(area, &mut fb).unwrap();

        assert!(
            second_line_contains_color(&fb, area, theme::palette::BRAND_BRIGHT),
            "a filled bar should paint BRAND_BRIGHT into the sublabel row"
        );
        assert!(
            second_line_contains_color(&fb, area, theme::palette::DIVIDER),
            "an unfilled bar (level 3 of 4) should paint DIVIDER into the sublabel row"
        );
    }

    #[test]
    fn zero_signal_bars_still_draws_the_glyph_not_a_blank_row() {
        // The all-empty case matters on its own: this is exactly what a
        // weak nearby device looks like, and it must read as "no signal"
        // (four dim bars), not as "nothing rendered" (a rendering bug).
        let list = VerticalList::new(vec![ListItem::new("Cans").with_signal_bars(0)]);
        let mut fb = FrameBuffer565::new(240, ROW_HEIGHT);
        let area = Rectangle::new(Point::new(0, 0), Size::new(240, ROW_HEIGHT));
        list.render(area, &mut fb).unwrap();

        assert!(
            second_line_contains_color(&fb, area, theme::palette::DIVIDER),
            "zero bars must still paint four dim bars, not leave the slot blank"
        );
        assert!(
            !second_line_contains_color(&fb, area, theme::palette::BRAND_BRIGHT),
            "zero bars must have no filled bar"
        );
    }

    #[test]
    fn signal_bars_take_priority_over_a_sublabel_set_on_the_same_item() {
        let list = VerticalList::new(vec![ListItem::new("Cans").with_sublabel("-40 dBm").with_signal_bars(4)]);
        let mut fb = FrameBuffer565::new(240, ROW_HEIGHT);
        let area = Rectangle::new(Point::new(0, 0), Size::new(240, ROW_HEIGHT));
        list.render(area, &mut fb).unwrap();

        assert!(
            second_line_contains_color(&fb, area, theme::palette::BRAND_BRIGHT),
            "signal_bars must win over sublabel text when both are set on one item"
        );
    }

    // --- pico-link-znb.4: stable identity across rebuilds ---
    //
    // `VerticalList` is rebuilt from scratch on every model change (see
    // `pico_link_core::app::build_devices_screen`); these tests exercise
    // `with_selected_identity`/`selected_key` directly against the widget,
    // independent of the `App`-level device model, to pin down the exact
    // resolution rule the design's phase-2 stable-ordering requirement
    // depends on.

    fn keyed_item(key: u64, label: &str) -> ListItem {
        ListItem::new(label).with_key(ListItemKey::from_u64(key))
    }

    #[test]
    fn selection_follows_its_keyed_row_when_other_rows_are_inserted_around_it() {
        // First build: A, B, C -- select A (index 0).
        let list = VerticalList::new(vec![keyed_item(1, "A"), keyed_item(2, "B"), keyed_item(3, "C")]);
        assert_eq!(list.selected_key(), Some(ListItemKey::from_u64(1)));

        // Rebuild with two more rows inserted -- one *before* A, one
        // between A and B -- simulating discovery results arriving that
        // sort/land around the previously selected device. A must still
        // be the selection, now at index 1, not index 0.
        let rebuilt = VerticalList::new(vec![
            keyed_item(4, "D"), // inserted before A
            keyed_item(1, "A"),
            keyed_item(5, "E"), // inserted between A and B
            keyed_item(2, "B"),
            keyed_item(3, "C"),
        ])
        .with_selected_identity(list.selected_key(), list.selected_index());

        assert_eq!(rebuilt.selected_index(), 1, "selection must follow A to its new index");
        assert_eq!(rebuilt.selected_key(), Some(ListItemKey::from_u64(1)));
    }

    #[test]
    fn a_relabeled_row_with_the_same_key_keeps_the_selection_and_does_not_duplicate() {
        // A late name arriving for an already-listed row: same key, new
        // label, same position -- selection must resolve to it unchanged,
        // and the rebuilt list must still have exactly as many rows (no
        // second row appended for the "same" entity under a new label).
        let list = VerticalList::new(vec![keyed_item(1, "(unknown device)"), keyed_item(2, "B")]);
        assert_eq!(list.selected_index(), 0);

        let rebuilt = VerticalList::new(vec![keyed_item(1, "Sony WH-1000XM5"), keyed_item(2, "B")])
            .with_selected_identity(list.selected_key(), list.selected_index());

        assert_eq!(rebuilt.selected_index(), 0, "the relabeled row must keep the selection");
        assert_eq!(rebuilt.items.len(), 2, "a relabel must update in place, not append a second row");
        assert_eq!(rebuilt.items[0].label, "Sony WH-1000XM5");
    }

    #[test]
    fn the_selected_rows_key_disappearing_clamps_to_the_nearest_surviving_position_not_row_zero() {
        // Five rows, selection on the last one (index 4, key 5) -- the
        // "removed/timed out while selected" case. The rebuilt list drops
        // that row (down to 3 rows) and gains no replacement for it, so
        // no row in the new list carries key 5 -- the fallback must clamp
        // `prev_index` (4) to the new list's bounds (index 2, the new
        // last row), NOT reset to row 0.
        let list = VerticalList::new(vec![
            keyed_item(1, "A"),
            keyed_item(2, "B"),
            keyed_item(3, "C"),
            keyed_item(4, "D"),
            keyed_item(5, "E"),
        ]);
        let mut list = list;
        for _ in 0..4 {
            list.on_intent(NavIntent::Down);
        }
        assert_eq!(list.selected_key(), Some(ListItemKey::from_u64(5)));

        let rebuilt = VerticalList::new(vec![keyed_item(1, "A"), keyed_item(2, "B"), keyed_item(3, "C")])
            .with_selected_identity(list.selected_key(), list.selected_index());

        assert_eq!(
            rebuilt.selected_index(),
            2,
            "a vanished selection must clamp to the nearest surviving row, not jump back to row 0"
        );
    }

    #[test]
    fn with_selected_identity_falls_back_to_the_index_when_no_key_is_given() {
        // A caller with no identity concept for this list (e.g. `None`
        // for both the first build and any rebuild) behaves exactly like
        // the retired `with_selected`: plain index clamping.
        let rebuilt = VerticalList::new(items(3)).with_selected_identity(None, 5);
        assert_eq!(rebuilt.selected_index(), 2, "out-of-range prev_index must clamp to the last row");
    }

    #[test]
    fn selected_key_is_none_for_an_empty_list_or_an_unkeyed_row() {
        assert_eq!(VerticalList::new(vec![]).selected_key(), None);
        assert_eq!(VerticalList::new(items(3)).selected_key(), None, "plain ListItem::new rows carry no key");
    }

    // -- truncate_label_to_width (pico-link-ok1) --------------------------
    //
    // These exercise the pure function directly rather than rendering a
    // full row: the widths involved come straight from the real
    // `u8g2-fonts` glyph metrics (`text_width`), so a short/exact/
    // overflow/pathological label is expressed relative to a *measured*
    // budget rather than a hardcoded pixel number that would silently
    // drift out of sync with the font.

    #[test]
    fn a_short_label_that_fits_is_returned_unchanged() {
        let font = font::name();
        let width = text_width(&font, "Pixel Buds") as i32;
        // Generous headroom -- this is the "doesn't even need truncation"
        // case, not a boundary test.
        let result = truncate_label_to_width(&font, "Pixel Buds", width + 40);
        assert_eq!(result, "Pixel Buds", "a label with room to spare must render untouched, no ellipsis");
    }

    #[test]
    fn a_label_that_exactly_fills_the_width_is_returned_unchanged() {
        let font = font::name();
        let name = "Sony WH-1000XM5";
        let exact_width = text_width(&font, name) as i32;
        let result = truncate_label_to_width(&font, name, exact_width);
        assert_eq!(result, name, "a label whose measured width equals the budget exactly must not be truncated");
    }

    #[test]
    fn a_label_that_overflows_by_one_character_gets_truncated_with_an_ellipsis() {
        let font = font::name();
        let name = "Sony WH-1000XM5"; // fits at `exact_width`
        let exact_width = text_width(&font, name) as i32;
        // One pixel under "exactly fits" forces truncation even though
        // only the last glyph's width worth of ink is actually over
        // budget -- this is the "overflow by one character" case, not a
        // drastic cut.
        let budget = exact_width - 1;
        let result = truncate_label_to_width(&font, name, budget);
        assert_ne!(result, name, "a label one pixel over budget must be truncated");
        assert!(result.ends_with(ELLIPSIS), "a truncated label must end in the ellipsis: got {result:?}");
        assert!(name.starts_with(result.trim_end_matches(ELLIPSIS)), "the kept prefix must be a real prefix of the original label");
        assert!(text_width(&font, &result) as i32 <= budget, "the truncated-plus-ellipsis result must itself fit the budget");
    }

    #[test]
    fn a_pathological_long_label_still_fits_the_budget_and_keeps_the_ellipsis() {
        let font = font::name();
        let name = "Bang and Olufsen Beoplay H95 Wireless Over-Ear Headphones Extended Edition";
        // A budget representative of the real row layout: about what's
        // left after the chip, chip-text gap and caret reservation on a
        // 240px-wide panel (see `available_text_width` in `draw_row`).
        let budget = 150;
        let result = truncate_label_to_width(&font, name, budget);
        assert!(result.ends_with(ELLIPSIS), "a wildly-overflowing label must still end in the ellipsis: got {result:?}");
        assert!(text_width(&font, &result) as i32 <= budget, "the result must fit the given budget: {result:?} measured wider than {budget}px");
        assert!(result.len() < name.len(), "a pathologically long label must actually be shortened");
    }

    #[test]
    fn a_budget_too_narrow_for_even_the_ellipsis_renders_nothing() {
        let font = font::name();
        // 1px cannot fit three periods in any real font; this exercises
        // the documented "no readable state smaller than the ellipsis"
        // fallback rather than drawing a stray fragment.
        let result = truncate_label_to_width(&font, "Sennheiser Momentum 4 Wireless", 1);
        assert_eq!(result, "", "an unfittable budget must render nothing, not a partial ellipsis or a stray glyph");
    }

    #[test]
    fn a_non_positive_max_width_renders_nothing() {
        let font = font::name();
        assert_eq!(truncate_label_to_width(&font, "Anything", 0), "");
        assert_eq!(truncate_label_to_width(&font, "Anything", -5), "");
    }

    #[test]
    fn an_empty_label_stays_empty_regardless_of_width() {
        let font = font::name();
        assert_eq!(truncate_label_to_width(&font, "", 200), "");
    }
}
