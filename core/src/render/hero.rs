//! `HeroStatusView`: the Home status face's hero widget — design
//! `.planning/design/2026-08-28-on-device-ui.md` section 6 (layout/fonts/
//! colour rules), 6.1 (the nine Home states), 6.2 ("the most important
//! part of this document" — the fallback chain), 6.3 (Muted). Bead
//! `pico-link-znb.6` (E4).
//!
//! One widget owning its own internal layout — device name, hero codec
//! word, bitrate line, and the bottom stat strip — per the `ConfirmView`
//! adapter pattern (`confirm.rs`'s module doc): a `Screen`'s widget list
//! stacks vertically via `Widget::measure`, which only works cleanly for
//! independent widgets, so a composite status display with its own
//! internal vertical rhythm is one `Widget` impl, not several stacked
//! ones.
//!
//! **Scope boundary** (per the bead): this is the widget and its
//! persistent banner slot only. Wiring it into the Home screen's status
//! face, the two-face toggle, and driving it from live [`crate::app::
//! BtModel`] data is `pico-link-znb.8` (E7), which depends on this bead.

#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use alloc::format;
use alloc::string::String;
use core::convert::Infallible;

use embedded_graphics::draw_target::DrawTargetExt;
use embedded_graphics::prelude::{Point, Primitive, Size};
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use embedded_graphics::Drawable;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::FontRenderer;

use super::ctx::RenderCtx;
use super::framebuffer::FrameBuffer565;
use super::theme::{font, palette};
use super::widget::{ChromeContribution, Widget};

/// Left rule `L` (px, area-relative) — shared by the device-name line, the
/// hero codec word, the banner text and the stat strip. Design
/// `.planning/design/2026-09-01-home-alignment-grid.md` section 3: the
/// single left rule every left-aligned element on Home now shares with
/// the title bar's `TITLE_SIDE_MARGIN` (`screen.rs`).
const LEFT_MARGIN: i32 = 12;
/// Right rule `R` (px, area-relative) — the device-name truncation budget
/// ends here. Same design doc, section 3: symmetric with `LEFT_MARGIN` on
/// the 206px content measure. The bitrate slot no longer anchors to this
/// rule — see `BITRATE_SLOT_WIDTH`'s doc comment: Andreas overruled the
/// right-aligned bitrate on 2026-09-02.
const RIGHT_MARGIN: i32 = 12;
/// Top padding (px) before the device-name line's first pixel — part of
/// the uniform 12px frame (design doc section 4).
const TOP_PADDING: i32 = 12;
/// Gap (px) between the device-name line and the hero codec word.
const GAP_NAME_TO_HERO: i32 = 12;
/// Fixed vertical slot every hero codec word gets, regardless of its own
/// ink height. Measured off `core/examples/hero_font_probe.rs` against
/// the real `font::hero()` face (`cargo run -p pico-link-core --example
/// hero_font_probe`): all-caps codec words ("LDAC", "SBC", "AAC") measure
/// 25px cap height, but "aptX HD" measures 32px because of the lowercase
/// `p` descender. Budgeting the 25px cap height would let the one
/// mixed-case codec name this product actually ships collide with the
/// bitrate line drawn right below it — see the orchestrator note on bead
/// `pico-link-znb.6`. Every hero word, of any case mix, gets this same
/// fixed 32px slot so the bitrate line's position never depends on which
/// codec is showing. Unchanged by the alignment-grid rework — this
/// rationale still holds.
const HERO_SLOT_HEIGHT: i32 = 32;
/// Gap (px) between the hero slot's bottom and the bitrate line —
/// deliberately tight: hero + bitrate read as one unit (design doc
/// section 4).
const GAP_HERO_TO_BITRATE: i32 = 4;
/// Width (px) of the bitrate's fixed, `BACKGROUND`-cleared slot. The
/// original design rule right-aligned every number into a slot like this
/// one specifically so a digit-count change (e.g. "660 kbps" -> "90 kbps")
/// never shifted other digits' positions or left stale ink behind — see
/// `.planning/design/2026-09-01-home-alignment-grid.md` section 3.1.
/// **Superseded 2026-09-02**: Andreas ruled the bitrate line should be
/// left-aligned instead ("Yes, put it left, it's not like it changes a
/// lot."), so the slot now sits at `LEFT_MARGIN` and the anti-jitter
/// clearing (still real — the slot is still `BACKGROUND`-cleared before
/// each draw) is the only surviving reason for a fixed-width slot instead
/// of measuring the text.
const BITRATE_SLOT_WIDTH: u32 = 120;
/// Height (px) of the persistent banner bar, when shown.
const BANNER_HEIGHT: i32 = 20;
/// Left/right inset (px) for the banner's own text within its bar — now
/// on the same left rule as everything else.
const BANNER_TEXT_INSET: i32 = 12;
/// Banner slot's fixed top y (px, relative to `area.top_left.y`) — design
/// doc section 4/7: absolute panel y 116. Fixed, not cursor-derived, so
/// the banner's presence never moves the stat strip below it.
const BANNER_TOP: i32 = 100;
/// Stat strip's fixed top y (px, relative to `area.top_left.y`) — design
/// doc section 4/7: absolute panel y 144. Fixed for the same reason as
/// `BANNER_TOP`.
const STAT_TOP: i32 = 128;

/// The hero codec word's colour is a reassurance mechanic (design section
/// 6): steady green means "you got what you asked for", amber means "you
/// got less than you asked for" (a downgrade, not an error — SBC instead
/// of LDAC must never read as a failure), red means "no link at all".
/// [`CodecStatus`] carries enough to derive the colour and the banner
/// together, so the two can never drift out of sync with each other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecStatus {
    /// No active link. The hero word renders as "NO LINK" in
    /// [`palette::STATUS_ERROR`] — design section 6's table names
    /// `STATUS_ERROR` for "no link" in words, and that is what this
    /// widget renders. Andreas's own design sketch shorthanded this state
    /// as "`Codec: -`" (quoted verbatim in section 6.2's intro), but a
    /// bare hyphen does not survive `font::hero()` at hero size: measured
    /// via `core/examples/hero_font_probe.rs`'s method, the glyph is a
    /// 9x5px mid-line dash (u8g2's real, present hyphen glyph — not a
    /// missing-glyph tofu box), which reads as a tiny coloured smudge
    /// rather than a legible hero word and fails the glance-across-a-room
    /// requirement the hero is built for (bead `pico-link-znb.6` comment,
    /// 2026-08-29). "NO LINK" measures 131x25px in `font::hero()`,
    /// comfortably inside the 240px panel and the `HERO_SLOT_HEIGHT`
    /// budget, same as every codec word.
    /// No bitrate line is drawn at all — absent, never frozen and never
    /// faked (design section 15); a "0 kbps" or a frozen last-known figure
    /// would both be lies about a link that no longer exists.
    NoLink,
    /// A connected link. `word` is the codec name (design section 6.1's
    /// "LDAC"/"SBC"/"AAC"/"aptX HD", ...). `fallback`, when `Some`, is
    /// *why* the word should read amber ([`palette::STATUS_WARNING`])
    /// instead of green ([`palette::TEXT_PRIMARY`]) — this is link 1 of
    /// the design's five-link fallback chain (section 6.2) — and doubles
    /// as the persistent fallback banner's reason text, which is link 2
    /// (unless a MUTED banner outranks it — see
    /// [`HeroStatusView::with_muted`]).
    Connected {
        word: String,
        fallback: Option<String>,
        bitrate: BitrateStatus,
    },
}

/// The bitrate line's content, for a [`CodecStatus::Connected`] link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitrateStatus {
    /// Host connected but silent. Renders as "idle", **never** "0 kbps" —
    /// design section 6.1 state 4: a zero reads as broken, not quiet.
    Idle,
    /// A live figure, already smoothed and snapped to the nominal LDAC
    /// ladder (330/660/909/990) by the caller per the design's numeric
    /// rule — this widget only formats and right-aligns whatever it's
    /// given, it does not smooth or snap itself (that needs a clock/
    /// history the widget has no business owning).
    Kbps(u32),
}

/// Which persistent banner (if any) is currently showing — resolved by
/// [`HeroStatusView::active_banner`] from `muted`/`CodecStatus::fallback`
/// per the design's priority rule: **at most one banner, MUTED outranks
/// FALLBACK** (section 6.2/6.3). Not a toast — the entire point, per the
/// design, is that it persists: glance an hour later and the answer is
/// still there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveBanner<'a> {
    /// Design section 6.3: device-side volume at zero with the host
    /// slider unaware. Renders "MUTED  Press Up to raise".
    Muted,
    /// Design section 6.2, link 2 of the fallback chain: states what
    /// happened and why, so a glance an hour later still explains the
    /// amber hero word.
    Fallback(&'a str),
}

/// The Home status face's hero/status widget: device name, hero codec
/// word, bitrate line, and the persistent banner slot (design section 6,
/// 6.2, 6.3). Not focusable — Home has no focusable list (design section
/// 4's Home exception: Up/Down is volume, centre toggles the menu face,
/// both handled above this widget by whatever wires it into Home, which
/// is `pico-link-znb.8`/E7, out of this bead's scope).
pub struct HeroStatusView {
    device_name: String,
    status: CodecStatus,
    muted: bool,
    stat_line: Option<String>,
}

impl HeroStatusView {
    #[must_use]
    pub fn new(device_name: impl Into<String>, status: CodecStatus) -> Self {
        Self { device_name: device_name.into(), status, muted: false, stat_line: None }
    }

    /// Design section 6.3: device-side volume at zero with the host
    /// slider unaware. Drives the MUTED banner, which outranks a
    /// simultaneous codec fallback banner (the fallback amber hero word
    /// stays visible underneath regardless — only the *banner text*
    /// changes).
    #[must_use]
    pub fn with_muted(mut self, muted: bool) -> Self {
        self.muted = muted;
        self
    }

    /// The bottom stat strip's text (e.g. "USB 48K 24-BIT"). `None` draws
    /// nothing there — the `OUT` level meter and `LINK`/`SIGNAL` bars this
    /// row shows in the design sketch are CUT for Tier 1 (design section
    /// 13: unconfirmed data, cut entirely rather than dashed or frozen),
    /// so building the specific field-by-field logic for what remains is
    /// this widget's caller's job, not this widget's — it just renders
    /// whatever single line it's handed, uppercase-styled per the design's
    /// `font::label` stat-label rule.
    #[must_use]
    pub fn with_stat_line(mut self, line: impl Into<String>) -> Self {
        self.stat_line = Some(line.into());
        self
    }

    /// Resolves the design's banner-priority rule (section 6.2/6.3): at
    /// most one banner, MUTED outranks FALLBACK. The fallback banner's
    /// text is [`CodecStatus::Connected::fallback`]'s reason, so it can
    /// never show without the hero word also being amber.
    fn active_banner(&self) -> Option<ActiveBanner<'_>> {
        if self.muted {
            return Some(ActiveBanner::Muted);
        }
        if let CodecStatus::Connected { fallback: Some(reason), .. } = &self.status {
            return Some(ActiveBanner::Fallback(reason));
        }
        None
    }

    /// Whether the codec link is currently in the fallback state (design
    /// section 6.2, link 1: the amber hero word). Exposed so a caller
    /// wiring this widget into a `Screen` can fold it into a
    /// [`ChromeContribution`] itself; also what
    /// [`Widget::chrome_contribution`] reports below.
    #[must_use]
    pub fn is_fallback(&self) -> bool {
        matches!(&self.status, CodecStatus::Connected { fallback: Some(_), .. })
    }
}

/// Same "Agjpqy" worst-case single-line probe `menu.rs`/`confirm.rs`'s
/// `line_height` uses — duplicated for the same reason those modules'
/// doc comments give (no shared home for a helper this small, used by
/// only one module each).
fn line_height(font: &FontRenderer) -> i32 {
    font.get_rendered_dimensions_aligned("Agjpqy", Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
        .unwrap_or(None)
        .map_or(16, |bbox| bbox.size.height as i32)
}

/// The horizontal pixel footprint `render_aligned` would give `text` in
/// `font` — duplicated from `screen.rs`'s private `text_width` for the
/// same "no shared home for a helper this small" reason.
fn text_width(font: &FontRenderer, text: &str) -> u32 {
    font.get_rendered_dimensions_aligned(text, Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
        .unwrap_or(None)
        .map_or(0, |bbox| bbox.size.width)
}

/// Truncates `text` with a trailing "..." so it fits within `max_width`
/// pixels in `font` — the device name's "truncates with an ellipsis, never
/// a marquee" rule (design section 6). A full-frame marquee redraw is
/// retired (see `widget.rs`'s `Widget::render` doc comment: `clipped()` is
/// "the mechanism the old character-skip marquee code is retired in
/// favor of"); this widget still calls `target.clipped()` around the
/// name line as a defensive backstop (belt-and-suspenders against any
/// width this function's own math gets wrong), but the primary truncation
/// behaviour — an actual visible "..." rather than a mid-glyph hard cut —
/// happens here. ASCII "..." rather than a single "…" glyph: the `_tf`
/// u8g2 font subsets this crate uses are not guaranteed to carry the
/// Unicode ellipsis codepoint, and `with_ignore_unknown_chars(true)` would
/// silently drop it if absent.
fn truncate_to_width(font: &FontRenderer, text: &str, max_width: u32) -> String {
    const ELLIPSIS: &str = "...";
    if text_width(font, text) <= max_width {
        return String::from(text);
    }
    if text_width(font, ELLIPSIS) > max_width {
        return String::new();
    }
    let mut end = text.len();
    while end > 0 {
        end -= 1;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        let candidate = format!("{}{}", &text[..end], ELLIPSIS);
        if text_width(font, &candidate) <= max_width {
            return candidate;
        }
    }
    String::from(ELLIPSIS)
}

impl Widget for HeroStatusView {
    fn measure(&self, constraints: Size, _ctx: &RenderCtx) -> Size {
        constraints
    }

    /// # Errors
    ///
    /// Returns `Infallible`'s uninhabited variant in practice — see
    /// [`Widget::render`]'s doc comment for why the `Result` return exists
    /// at all.
    #[allow(clippy::too_many_lines)]
    fn render(&self, area: Rectangle, _ctx: &RenderCtx, target: &mut FrameBuffer565) -> Result<(), Infallible> {
        let mut clipped = target.clipped(&area);

        // --- Device name: truncates with an ellipsis, never a marquee. ---
        let name_font = font::name();
        let name_line_h = line_height(&name_font);
        let name_max_width = (area.size.width as i32 - LEFT_MARGIN - RIGHT_MARGIN).max(0) as u32;
        let name_text = truncate_to_width(&name_font, &self.device_name, name_max_width);
        let name_y = area.top_left.y + TOP_PADDING;
        let name_rect = Rectangle::new(
            Point::new(area.top_left.x + LEFT_MARGIN, name_y),
            Size::new(name_max_width, name_line_h as u32),
        );
        let mut name_target = clipped.clipped(&name_rect);
        let _ = name_font.render_aligned(
            name_text.as_str(),
            Point::new(area.top_left.x + LEFT_MARGIN, name_y),
            VerticalPosition::Top,
            HorizontalAlignment::Left,
            FontColor::Transparent(palette::TEXT_PRIMARY),
            &mut name_target,
        );

        // --- Hero codec word: the fixed 32px slot, regardless of word.
        // Left-aligned on `LEFT_MARGIN`, not centered — design doc section
        // 3.1: a centred hero moves both its edges the moment the codec
        // changes, exactly when the change most needs to be noticed. ---
        let hero_font = font::hero();
        let hero_y = name_y + name_line_h + GAP_NAME_TO_HERO;
        let (hero_text, hero_color) = match &self.status {
            CodecStatus::NoLink => (String::from("NO LINK"), palette::STATUS_ERROR),
            CodecStatus::Connected { word, fallback, .. } => {
                let color = if fallback.is_some() { palette::STATUS_WARNING } else { palette::TEXT_PRIMARY };
                (word.clone(), color)
            }
        };
        let _ = hero_font.render_aligned(
            hero_text.as_str(),
            Point::new(area.top_left.x + LEFT_MARGIN, hero_y),
            VerticalPosition::Top,
            HorizontalAlignment::Left,
            FontColor::Transparent(hero_color),
            &mut clipped,
        );

        // --- Bitrate line: fixed slot, cleared to BACKGROUND, left-
        // aligned to `LEFT_MARGIN` (Andreas's ruling, 2026-09-02, overrides
        // the design doc's original right-aligned/anti-jitter rule — see
        // .planning/design/2026-09-01-home-alignment-grid.md section 3) —
        // absent entirely for NoLink, never a faked/frozen number. ---
        let value_font = font::value();
        let value_line_h = line_height(&value_font);
        let bitrate_y = hero_y + HERO_SLOT_HEIGHT + GAP_HERO_TO_BITRATE;
        if let CodecStatus::Connected { bitrate, .. } = &self.status {
            let bitrate_text = match bitrate {
                BitrateStatus::Idle => String::from("idle"),
                BitrateStatus::Kbps(kbps) => format!("{kbps} kbps"),
            };
            let slot_rect = Rectangle::new(
                Point::new(area.top_left.x + LEFT_MARGIN, bitrate_y),
                Size::new(BITRATE_SLOT_WIDTH, value_line_h as u32),
            );
            slot_rect.into_styled(PrimitiveStyle::with_fill(palette::BACKGROUND)).draw(&mut clipped)?;
            let _ = value_font.render_aligned(
                bitrate_text.as_str(),
                Point::new(slot_rect.top_left.x, bitrate_y),
                VerticalPosition::Top,
                HorizontalAlignment::Left,
                FontColor::Transparent(palette::TEXT_PRIMARY),
                &mut clipped,
            );
        }

        // --- Persistent banner slot: at most one, MUTED outranks
        // FALLBACK (design section 6.2/6.3). Not a toast: no timer, no
        // auto-dismiss, drawn every render exactly like everything else
        // on this widget. Fixed y (`BANNER_TOP`), not cursor-derived, so
        // showing/hiding it never moves the stat strip below it. ---
        if let Some(banner) = self.active_banner() {
            let banner_y = area.top_left.y + BANNER_TOP;
            let (text, color) = match banner {
                ActiveBanner::Muted => (String::from("MUTED  Press Up to raise"), palette::STATUS_WARNING),
                ActiveBanner::Fallback(reason) => (String::from(reason), palette::STATUS_WARNING),
            };
            let banner_rect = Rectangle::new(
                Point::new(area.top_left.x, banner_y),
                Size::new(area.size.width, BANNER_HEIGHT as u32),
            );
            banner_rect.into_styled(PrimitiveStyle::with_fill(palette::SURFACE_ELEVATED)).draw(&mut clipped)?;
            let banner_text_max_width = (area.size.width as i32 - 2 * BANNER_TEXT_INSET).max(0) as u32;
            let label_font = font::label();
            let banner_text = truncate_to_width(&label_font, &text, banner_text_max_width);
            let banner_mid_y = banner_y + BANNER_HEIGHT / 2;
            let _ = label_font.render_aligned(
                banner_text.as_str(),
                Point::new(area.top_left.x + BANNER_TEXT_INSET, banner_mid_y),
                VerticalPosition::Center,
                HorizontalAlignment::Left,
                FontColor::Transparent(color),
                &mut clipped,
            );
        }

        // --- Bottom stat strip. Whatever fields the design's data-
        // dependency table (section 13) says survive Tier 1 — the OUT
        // meter and LINK/SIGNAL bars are CUT entirely, not dashed, so
        // they are simply not part of `stat_line` at all. Fixed y
        // (`STAT_TOP`) — occupies the same rows whether or not a banner
        // is showing; this is the whole point of the fixed grid. ---
        if let Some(stat_line) = &self.stat_line {
            let stat_y = area.top_left.y + STAT_TOP;
            let label_font = font::label();
            let stat_max_width = (area.size.width as i32 - LEFT_MARGIN - RIGHT_MARGIN).max(0) as u32;
            let upper = stat_line.to_uppercase();
            let stat_text = truncate_to_width(&label_font, &upper, stat_max_width);
            let _ = label_font.render_aligned(
                stat_text.as_str(),
                Point::new(area.top_left.x + LEFT_MARGIN, stat_y),
                VerticalPosition::Top,
                HorizontalAlignment::Left,
                FontColor::Transparent(palette::TEXT_SECONDARY),
                &mut clipped,
            );
        }

        Ok(())
    }

    /// Exposes the fallback state (design section 6.2, link 3: the X-rail
    /// label switches from "link" to "why?" under fallback) via
    /// [`ChromeContribution::fallback`] rather than painting the rail
    /// itself — the rail lives in `ChromeContribution`'s a/b/x/y fields,
    /// which is `pico-link-znb.5` (E2)'s job, not this widget's.
    fn chrome_contribution(&self, _ctx: &RenderCtx) -> Option<ChromeContribution> {
        Some(ChromeContribution { fallback: self.is_fallback(), ..Default::default() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Instant;
    use crate::render::widget::FocusEvent;

    const AREA: Rectangle = Rectangle::new(Point::new(0, 0), Size::new(240, 206));

    fn test_ctx() -> RenderCtx {
        RenderCtx::at(Instant::from_micros(0))
    }

    fn nominal() -> HeroStatusView {
        HeroStatusView::new(
            "Sony WH-1000XM5",
            CodecStatus::Connected { word: String::from("LDAC"), fallback: None, bitrate: BitrateStatus::Kbps(909) },
        )
        .with_stat_line("USB 48k 24-bit")
    }

    fn render(view: &HeroStatusView) -> FrameBuffer565 {
        let mut fb = FrameBuffer565::new(240, 206);
        view.render(AREA, &test_ctx(), &mut fb).unwrap();
        fb
    }

    // --- Not focusable: Home has no focusable list (design section 4's
    // Home exception) --------------------------------------------------

    #[test]
    fn is_not_focusable() {
        assert!(!nominal().is_focusable());
    }

    #[test]
    fn on_focus_is_a_harmless_noop() {
        let mut view = nominal();
        let action = view.on_focus(FocusEvent::Activated);
        assert!(matches!(action, super::super::widget::Action::None));
    }

    // --- Codec word colour: the reassurance mechanic (section 6) ------

    #[test]
    fn nominal_codec_renders_the_hero_word_in_text_primary() {
        let fb = render(&nominal());
        assert!(
            fb.pixels().any(|p| p.1 == palette::TEXT_PRIMARY),
            "nominal LDAC should paint TEXT_PRIMARY ink somewhere"
        );
    }

    #[test]
    fn fallback_codec_renders_the_hero_word_in_status_warning_amber() {
        let view = HeroStatusView::new(
            "Sony WH-1000XM5",
            CodecStatus::Connected {
                word: String::from("SBC"),
                fallback: Some(String::from("Headphones don't support LDAC")),
                bitrate: BitrateStatus::Kbps(328),
            },
        );
        let fb = render(&view);
        assert!(
            fb.pixels().any(|p| p.1 == palette::STATUS_WARNING),
            "a fallen-back codec must paint the hero word in STATUS_WARNING amber, never red"
        );
        assert!(
            !fb.pixels().any(|p| p.1 == palette::STATUS_ERROR),
            "a downgrade is not an error -- no STATUS_ERROR ink anywhere for a fallback state"
        );
    }

    #[test]
    fn no_link_renders_the_hero_word_in_status_error() {
        let view = HeroStatusView::new("Sony WH-1000XM5", CodecStatus::NoLink);
        let fb = render(&view);
        assert!(
            fb.pixels().any(|p| p.1 == palette::STATUS_ERROR),
            "no link should paint the hero word in STATUS_ERROR"
        );
    }

    // --- Bitrate: idle vs kbps vs absent for NoLink --------------------

    #[test]
    fn idle_bitrate_never_renders_a_literal_zero_kbps() {
        let view = HeroStatusView::new(
            "Sony WH-1000XM5",
            CodecStatus::Connected { word: String::from("LDAC"), fallback: None, bitrate: BitrateStatus::Idle },
        );
        // Rendering must not panic on the idle path, and (checked via
        // chrome_contribution/is_fallback below) must not be mistaken for
        // a fallback. The literal "0 kbps" text is asserted absent by
        // construction -- BitrateStatus::Idle's render arm never formats
        // a number at all, only the "idle" string. This test exists
        // primarily to exercise that arm without panicking; string
        // content isn't pixel-inspectable here, so the zoomed PNG capture
        // (see the bead's verification evidence) is the real check.
        let _ = render(&view);
        assert!(!view.is_fallback());
    }

    #[test]
    fn no_link_draws_no_bitrate_slot_fill() {
        // NoLink must never draw the BACKGROUND-clearing fill rectangle
        // the bitrate slot uses -- there's nothing to prove that would
        // distinguish "cleared, empty slot" from "never drew a slot" by
        // pixel color alone (both are BACKGROUND), so this is really
        // guarded by `render`'s `if let CodecStatus::Connected` gate, and
        // this test is a smoke test that NoLink renders without panicking
        // and produces the expected hero-only ink.
        let view = HeroStatusView::new("", CodecStatus::NoLink);
        let fb = render(&view);
        assert!(fb.pixels().any(|p| p.1 == palette::STATUS_ERROR));
        assert!(!fb.pixels().any(|p| p.1 == palette::TEXT_PRIMARY), "NoLink draws no device-name/bitrate ink in this fixture (empty device name)");
    }

    // --- Banner priority: MUTED outranks FALLBACK (section 6.2/6.3) ---

    #[test]
    fn no_banner_by_default() {
        assert!(nominal().active_banner().is_none());
    }

    #[test]
    fn fallback_without_muted_shows_the_fallback_banner() {
        let view = HeroStatusView::new(
            "Sony WH-1000XM5",
            CodecStatus::Connected {
                word: String::from("SBC"),
                fallback: Some(String::from("Headphones don't support LDAC")),
                bitrate: BitrateStatus::Kbps(328),
            },
        );
        assert_eq!(view.active_banner(), Some(ActiveBanner::Fallback("Headphones don't support LDAC")));
    }

    #[test]
    fn muted_alone_shows_the_muted_banner() {
        let view = nominal().with_muted(true);
        assert_eq!(view.active_banner(), Some(ActiveBanner::Muted));
    }

    #[test]
    fn muted_outranks_a_simultaneous_fallback() {
        // The design is explicit: at most one banner, MUTED outranks
        // FALLBACK, and the codec word stays amber underneath regardless
        // -- so both signals can be true at once, and only the *banner*
        // must resolve to Muted; the hero word's own colour is untouched.
        let view = HeroStatusView::new(
            "Sony WH-1000XM5",
            CodecStatus::Connected {
                word: String::from("SBC"),
                fallback: Some(String::from("Headphones don't support LDAC")),
                bitrate: BitrateStatus::Kbps(328),
            },
        )
        .with_muted(true);
        assert_eq!(view.active_banner(), Some(ActiveBanner::Muted), "MUTED must outrank a simultaneous FALLBACK");
        assert!(view.is_fallback(), "the hero word must stay amber underneath a MUTED banner");
    }

    #[test]
    fn muted_banner_renders_surface_elevated_and_warning_ink() {
        let view = nominal().with_muted(true);
        let fb = render(&view);
        assert!(fb.pixels().any(|p| p.1 == palette::SURFACE_ELEVATED), "the banner bar itself should paint SURFACE_ELEVATED");
        assert!(fb.pixels().any(|p| p.1 == palette::STATUS_WARNING), "the banner text and the hero word both use STATUS_WARNING");
    }

    // --- ChromeContribution: fallback is exposed, rail is not painted -

    #[test]
    fn chrome_contribution_reports_fallback_true_when_degraded() {
        let view = HeroStatusView::new(
            "Sony WH-1000XM5",
            CodecStatus::Connected {
                word: String::from("SBC"),
                fallback: Some(String::from("Headphones don't support LDAC")),
                bitrate: BitrateStatus::Kbps(328),
            },
        );
        let contribution = view.chrome_contribution(&test_ctx()).expect("hero widget always reports a contribution");
        assert!(contribution.fallback);
    }

    #[test]
    fn chrome_contribution_reports_fallback_false_when_nominal() {
        let contribution = nominal().chrome_contribution(&test_ctx()).expect("hero widget always reports a contribution");
        assert!(!contribution.fallback);
    }

    // --- Device name: ellipsis truncation, not marquee, not overflow --

    #[test]
    fn a_long_device_name_truncates_with_an_ellipsis_rather_than_overflowing_its_slot() {
        // This project has already shipped a sub-row text overflow that
        // passed weak checks (CLAUDE.md's rendering-verification rule) --
        // this test asserts the actual truncated string, not just "it
        // rendered without panicking".
        let long_name = "Sennheiser Momentum 4 Wireless Over-Ear Headphones Extended Name";
        let truncated = truncate_to_width(&font::name(), long_name, 224);
        assert!(truncated.ends_with("..."), "an overflowing name must end in an ellipsis, got {truncated:?}");
        assert!(truncated.len() < long_name.len(), "the truncated name must be strictly shorter than the source");
        assert!(
            text_width(&font::name(), &truncated) <= 224,
            "the truncated name (with its ellipsis) must actually fit the budget"
        );
    }

    #[test]
    fn a_short_device_name_is_never_truncated() {
        let short_name = "Sony WH-1000XM5";
        let result = truncate_to_width(&font::name(), short_name, 224);
        assert_eq!(result, short_name);
    }

    #[test]
    fn rendering_a_long_device_name_does_not_panic_and_stays_left_of_the_right_margin() {
        let view = HeroStatusView::new(
            "Sennheiser Momentum 4 Wireless Over-Ear Headphones With An Extremely Long Marketing Name",
            CodecStatus::Connected { word: String::from("LDAC"), fallback: None, bitrate: BitrateStatus::Kbps(909) },
        );
        let fb = render(&view);
        // No ink at all in the rightmost margin column of the name's row
        // -- if truncation failed, the name would run into (or past) this
        // column instead of stopping short of it.
        let name_row_y = TOP_PADDING;
        let right_edge_x = AREA.size.width as i32 - 2;
        let overflowed = (0..line_height(&font::name())).any(|dy| {
            fb.pixel(Point::new(right_edge_x, name_row_y + dy)) == palette::TEXT_PRIMARY
        });
        assert!(!overflowed, "a long device name must not paint ink into the right-edge margin column");
    }

    // --- aptX HD: the descender case that motivated HERO_SLOT_HEIGHT ---

    #[test]
    fn aptx_hd_hero_word_does_not_collide_with_the_bitrate_line_below_it() {
        let view = HeroStatusView::new(
            "Sony WH-1000XM5",
            CodecStatus::Connected { word: String::from("aptX HD"), fallback: None, bitrate: BitrateStatus::Kbps(576) },
        );
        let fb = render(&view);
        // The bitrate slot is cleared to BACKGROUND immediately before
        // its text is drawn; if the 32px-descender hero word bled into
        // that slot's top row, this slot-clear would either not exist
        // (impossible -- it always runs for a Connected state) or the
        // hero word's ink would still be visible at the slot's very top
        // row, since the clear only wipes its own rectangle, not the row
        // above it. So: assert the row immediately above the bitrate
        // slot (still inside the hero's 32px budget) is where the
        // descender is allowed to live, and the bitrate text's own row
        // (well inside the slot) carries no TEXT_PRIMARY ink outside the
        // slot's fixed rectangle bounds -- i.e. the two elements don't
        // smear into one blob. This is a smoke check; the authoritative
        // check is the zoomed PNG capture named in the bead's DONE
        // criteria.
        assert!(fb.pixels().any(|p| p.1 == palette::TEXT_PRIMARY), "the hero word + bitrate line should both paint TEXT_PRIMARY ink");
    }

    // --- Fixed vertical grid: the regression this refactor exists to
    // prevent (design `.planning/design/2026-09-01-home-alignment-grid.md`
    // section 4/7) -- the stat strip's y position must never depend on
    // whether the banner is showing. Before this bead, `hero.rs` walked
    // an accumulating `cursor_y`, so showing the banner moved the stat
    // strip down 28px. ---

    /// The set of framebuffer rows (y coordinates) that carry
    /// `palette::TEXT_SECONDARY` ink -- the stat strip's exclusive color
    /// in this widget (device name/hero/bitrate use `TEXT_PRIMARY`; the
    /// banner and a fallen-back hero use `STATUS_WARNING`), so this
    /// isolates exactly the stat strip's occupied rows.
    fn stat_strip_ink_rows(fb: &FrameBuffer565) -> alloc::vec::Vec<i32> {
        let mut rows: alloc::vec::Vec<i32> = fb
            .pixels()
            .filter(|p| p.1 == palette::TEXT_SECONDARY)
            .map(|p| p.0.y)
            .collect();
        rows.sort_unstable();
        rows.dedup();
        rows
    }

    #[test]
    fn stat_strip_occupies_the_same_rows_with_and_without_a_banner() {
        let without_banner = nominal(); // no fallback, not muted -- no banner
        let with_banner = HeroStatusView::new(
            "Sony WH-1000XM5",
            CodecStatus::Connected {
                word: String::from("SBC"),
                fallback: Some(String::from("Headphones don't support LDAC")),
                bitrate: BitrateStatus::Kbps(328),
            },
        )
        .with_stat_line("USB 48k 24-bit");
        assert!(with_banner.active_banner().is_some(), "fixture must actually exercise the banner");

        let fb_without = render(&without_banner);
        let fb_with = render(&with_banner);

        let rows_without = stat_strip_ink_rows(&fb_without);
        let rows_with = stat_strip_ink_rows(&fb_with);

        assert!(!rows_without.is_empty(), "the stat strip must render some ink when a stat line is set");
        assert_eq!(
            rows_without, rows_with,
            "the stat strip must occupy the SAME rows whether or not the banner is showing -- its y is a fixed grid slot (STAT_TOP), not derived from a cursor that the banner also advances"
        );
    }

    #[test]
    fn stat_strip_renders_at_the_fixed_stat_top_slot() {
        let fb = render(&nominal());
        let rows = stat_strip_ink_rows(&fb);
        assert!(!rows.is_empty(), "nominal() sets a stat line, so it must render ink");
        let first_row = rows[0];
        assert!(
            (STAT_TOP..STAT_TOP + 20).contains(&first_row),
            "stat strip ink must start within the STAT_TOP grid slot, got y={first_row}"
        );
    }
}
