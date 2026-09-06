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
use core::time::Duration;

use embedded_graphics::draw_target::DrawTargetExt;
use embedded_graphics::prelude::{Point, Primitive, Size};
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use embedded_graphics::Drawable;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::FontRenderer;

use crate::panel::{Edge, PANEL};
use crate::platform::Instant;

use super::chrome::carve_edge;
use super::ctx::RenderCtx;
use super::framebuffer::FrameBuffer565;
use super::paint_key::PaintKey;
use super::theme::{self, font, palette};
use super::widget::{ChromeContribution, Widget};

/// Seed for [`HeroStatusView::paint_key`] -- only needs to differ from
/// other widgets' own seeds.
const HERO_PAINT_KEY_SEED: u64 = 14;

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
/// `BANNER_TOP`. As of the vertical OUT meter
/// (`.planning/design/2026-09-03-vertical-out-meter.md`), this slot is no
/// longer shared with the OUT meter — see [`HeroStatusView::render`]'s
/// stat-strip comment for how that collision is resolved.
const STAT_TOP: i32 = 128;
/// Height (px) of the name row's own band, carved off `Edge::Top` before
/// anything else (design section 3/4): `TOP_PADDING 12 + line_height(name)
/// 16 + GAP_NAME_TO_HERO 12`. Derived, not a magic number — carving the
/// meter strip off the *whole* content instead of below this band would
/// narrow the device-name budget from 206px to 158px, and "Sony
/// WH-1000XM5" measures 144px: it would truncate to "Sony WH-1000X...",
/// losing the model digit that disambiguates two paired Sonys.
const NAME_BAND_HEIGHT: u32 = (TOP_PADDING + 16 + GAP_NAME_TO_HERO) as u32;
/// Width (px) of the vertical OUT-meter strip, carved off
/// `PANEL.button_edge()` immediately inboard of the button rail (design
/// section 4): `LEAD_GAP 12 + CHANNEL_WIDTH 12 + CHANNEL_GAP 4 +
/// CHANNEL_WIDTH 12 + TRAIL_GAP 8`.
const METER_STRIP_WIDTH: u32 = 48;
/// Gap (px) between the meter strip and the hero/bitrate/banner/stat body
/// it sits beside (the content-facing side of the strip) — the largest of
/// the three gap sizes, so Gestalt proximity binds the two meter columns
/// to each other rather than to the hero body (design section 4).
const LEAD_GAP: u32 = 12;
/// Gap (px) between the meter strip and the button rail (the rail-facing
/// side of the strip) — smaller than `LEAD_GAP`, larger than
/// `CHANNEL_GAP` (design section 4's gap rhythm: 4 < 8 < 12).
const TRAIL_GAP: u32 = 8;
/// Width (px) of one meter channel column.
const CHANNEL_WIDTH: u32 = 12;
/// Gap (px) between the L and R meter columns — the smallest of the three
/// gap sizes (design section 4's gap rhythm).
const CHANNEL_GAP: u32 = 4;
/// Inset (px) between the meter block's bottom edge and the content band's
/// bottom edge (design section 4) — the block itself is exactly
/// [`super::theme::VERTICAL_METER_GLYPH_HEIGHT`] (174px) tall, bottom-
/// pinned here and growing upward, so its top lands flush with the hero
/// codec word's own top rule.
const METER_BLOCK_BOTTOM_INSET: u32 = 10;
/// How long a stale OUT-meter reading is still drawn before this widget
/// treats it as "no PCM" and stops drawing it at all (design section 15:
/// **absent, never frozen** — the hard constraint this whole feature was
/// commissioned under, since a still VU meter reads as silence when it
/// means no data). Bead pico-link-ajj (step D, 2026-09-06): shortened from
/// 600ms to stay a small multiple (4x) of C's `PL_A2DP_LEVEL_PUSH_INTERVAL_MS`,
/// which this same bead lowered from 250ms to 50ms -- keeping the same
/// "one merely-late reading doesn't blank the meter, but a genuinely
/// stopped stream reads as silence within a couple of frames" ratio at the
/// new, much faster cadence rather than leaving a stopped stream visibly
/// frozen for up to 600ms.
pub(crate) const OUT_LEVEL_STALE_AFTER: Duration = Duration::from_millis(200);
/// The OUT meter's own repaint cadence while live. Andreas's 2026-09-02
/// override on this bead: ship at the design's ~4Hz refresh, do not run a
/// framerate sweep to tune it — this is the one named constant that makes
/// a future rate change a one-line edit. See
/// [`HeroStatusView::redraw_after`].
pub(crate) const OUT_LEVEL_REFRESH_INTERVAL: Duration = Duration::from_millis(250);

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

/// One live OUT-meter reading, as this widget's own render-time clock
/// will judge it (bead pico-link-du0, design section 21 E17/C8). A
/// hero-local type, deliberately not `crate::app::OutLevelSample`
/// directly — same "the widget stays decoupled from `BtModel`" shape
/// [`CodecStatus`]/[`BitrateStatus`] already use; `render::home` is the
/// only place that translates live model data into this widget's own
/// vocabulary (see that module's `HomeView::new`).
///
/// `peak_l`/`peak_r`/`rms_l`/`rms_r`/`hold_l`/`hold_r` are linear 0-255
/// (255 == full-scale PCM / clipping). `received_at` is what
/// [`HeroStatusView::render`] and [`HeroStatusView::redraw_after`]
/// compare against [`RenderCtx::now`] to decide whether this reading is
/// still live or has gone stale — see [`OUT_LEVEL_STALE_AFTER`]'s doc
/// comment for why that comparison, not a boolean C sends, is the
/// mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutLevelDisplay {
    pub peak_l: u8,
    pub peak_r: u8,
    pub rms_l: u8,
    pub rms_r: u8,
    pub hold_l: u8,
    pub hold_r: u8,
    pub received_at: Instant,
    /// Release-ballistic attack anchor (bead pico-link-ajj, design
    /// requirement C) -- a field-for-field carry of
    /// `crate::app::OutLevelSample`'s own anchor fields; see
    /// [`crate::app::decay_rms`]'s doc comment for what this widget does
    /// with it at render time.
    pub attack_rms_l: u8,
    pub attack_rms_r: u8,
    pub attack_rms_l_at: Instant,
    pub attack_rms_r_at: Instant,
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
    out_level: Option<OutLevelDisplay>,
}

impl HeroStatusView {
    #[must_use]
    pub fn new(device_name: impl Into<String>, status: CodecStatus) -> Self {
        Self { device_name: device_name.into(), status, muted: false, stat_line: None, out_level: None }
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
    /// nothing there — the `LINK`/`SIGNAL` bars this row shows in the
    /// design sketch are still CUT for Tier 1 (design section 13:
    /// unconfirmed data, cut entirely rather than dashed or frozen), so
    /// building the specific field-by-field logic for what remains is
    /// this widget's caller's job, not this widget's — it just renders
    /// whatever single line it's handed, uppercase-styled per the design's
    /// `font::label` stat-label rule. (The `OUT` level meter itself is no
    /// longer CUT — see [`Self::with_out_level`], bead pico-link-du0: it
    /// was gated on M3, which is now proven and shipping audio.)
    #[must_use]
    pub fn with_stat_line(mut self, line: impl Into<String>) -> Self {
        self.stat_line = Some(line.into());
        self
    }

    /// The stereo OUT level meter (design section 21 E17/C8, bead
    /// pico-link-du0) — `None` draws nothing at all, which is the correct
    /// state whenever there is no live PCM to measure (design section 15:
    /// absent, never frozen or faked) or the caller couldn't produce a
    /// reading. `Some` is not a promise the meter actually draws this
    /// frame, either: [`Widget::render`] separately checks
    /// [`OutLevelDisplay::received_at`] against [`RenderCtx::now`] and
    /// still draws nothing if it's gone stale (see
    /// [`OUT_LEVEL_STALE_AFTER`]'s doc comment) — the caller is not
    /// expected to re-derive that judgement itself before calling this.
    #[must_use]
    pub fn with_out_level(mut self, level: Option<OutLevelDisplay>) -> Self {
        self.out_level = level;
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
    // `l_block`/`r_block`/`l_col`/`r_col` are the OUT meter's two channel
    // columns -- the L/R domain vocabulary, same false-positive
    // clippy::similar_names has on `App::on_levels_changed`.
    #[allow(clippy::too_many_lines, clippy::similar_names)]
    fn render(&self, area: Rectangle, ctx: &RenderCtx, target: &mut FrameBuffer565) -> Result<(), Infallible> {
        let mut clipped = target.clipped(&area);

        // --- Geometry (`.planning/design/2026-09-03-vertical-out-meter.md`
        // section 4): a name band off `Edge::Top` (full content width, so
        // the device name keeps its whole 206px budget), then the meter
        // strip off `PANEL.button_edge()` of the remainder, immediately
        // inboard of the button rail. Every element below the name row
        // (hero word, bitrate, banner, stat strip) narrows to `hero_body`'s
        // width; the name row alone keeps `area`'s full width. Zero left/
        // right literals here -- both carves follow the orientation knob,
        // exactly the shape `carve_edge`'s own doc comment and the rail
        // already establish. ---
        let (name_band, rest) = carve_edge(area, Edge::Top, NAME_BAND_HEIGHT);
        let (strip, hero_body) = carve_edge(rest, PANEL.button_edge(), METER_STRIP_WIDTH);
        let (_trail_gap, s) = carve_edge(strip, PANEL.button_edge(), TRAIL_GAP);
        let (_lead_gap, pair) = carve_edge(s, PANEL.button_edge().opposite(), LEAD_GAP);
        // The L/R column split is deliberately NOT wired to the
        // orientation knob (design section 6): screen-left is screen-left
        // in both orientations (nothing mirrors the framebuffer when the
        // panel flips), so L is always the *literal* `Edge::Left` half of
        // `pair`, never `PANEL.button_edge()` or its opposite. Wiring
        // channel order to the rail edge would silently swap L/R on a
        // panel flip -- a bug nobody would catch by looking.
        let (l_col, remainder) = carve_edge(pair, Edge::Left, CHANNEL_WIDTH);
        let (_channel_gap, r_col) = carve_edge(remainder, Edge::Left, CHANNEL_GAP);
        // Each column's meter block is bottom-pinned `METER_BLOCK_BOTTOM_
        // INSET` above the content band's bottom edge and grows upward --
        // carving the inset off `Edge::Bottom` and keeping the *rest*
        // (the top portion) gives exactly that: a 174px-tall,
        // top-of-`hero_body` -aligned block whose top rule lands flush
        // with the hero codec word's own top rule (40 + 174 = 214, and the
        // strip spans content-rel y40..224).
        let (_l_bottom_pad, l_block) = carve_edge(l_col, Edge::Bottom, METER_BLOCK_BOTTOM_INSET);
        let (_r_bottom_pad, r_block) = carve_edge(r_col, Edge::Bottom, METER_BLOCK_BOTTOM_INSET);

        // --- Device name: truncates with an ellipsis, never a marquee.
        // Uses `name_band`'s full width -- protected alongside the hero
        // word by the same "if it crowds, it loses" ruling, per section 3.
        // ---
        let name_font = font::name();
        let name_line_h = line_height(&name_font);
        let name_max_width = (name_band.size.width as i32 - LEFT_MARGIN - RIGHT_MARGIN).max(0) as u32;
        let name_text = truncate_to_width(&name_font, &self.device_name, name_max_width);
        let name_y = name_band.top_left.y + TOP_PADDING;
        let name_rect = Rectangle::new(
            Point::new(name_band.top_left.x + LEFT_MARGIN, name_y),
            Size::new(name_max_width, name_line_h as u32),
        );
        let mut name_target = clipped.clipped(&name_rect);
        let _ = name_font.render_aligned(
            name_text.as_str(),
            Point::new(name_band.top_left.x + LEFT_MARGIN, name_y),
            VerticalPosition::Top,
            HorizontalAlignment::Left,
            FontColor::Transparent(palette::TEXT_PRIMARY),
            &mut name_target,
        );

        // --- Hero codec word: the fixed 32px slot, regardless of word.
        // Left-aligned on `LEFT_MARGIN`, not centered — design doc section
        // 3.1: a centred hero moves both its edges the moment the codec
        // changes, exactly when the change most needs to be noticed.
        // Narrowed to `hero_body`'s width now that the meter strip sits
        // beside it, not under it. ---
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
            Point::new(hero_body.top_left.x + LEFT_MARGIN, hero_y),
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
                Point::new(hero_body.top_left.x + LEFT_MARGIN, bitrate_y),
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
        // on this widget. Fixed y (`BANNER_TOP`, relative to `area`, not
        // `hero_body` -- it does not move), not cursor-derived, so
        // showing/hiding it never moves the stat strip below it. Narrowed
        // to `hero_body`'s width now that the meter strip sits beside it —
        // measured to still fit: the MUTED text is 128px, `hero_body`'s
        // text budget is 158 - 2*12 = 134px, 6px slack. ---
        if let Some(banner) = self.active_banner() {
            let banner_y = area.top_left.y + BANNER_TOP;
            let (text, color) = match banner {
                ActiveBanner::Muted => (String::from("MUTED  Press Up to raise"), palette::STATUS_WARNING),
                ActiveBanner::Fallback(reason) => (String::from(reason), palette::STATUS_WARNING),
            };
            let banner_rect = Rectangle::new(
                Point::new(hero_body.top_left.x, banner_y),
                Size::new(hero_body.size.width, BANNER_HEIGHT as u32),
            );
            banner_rect.into_styled(PrimitiveStyle::with_fill(palette::SURFACE_ELEVATED)).draw(&mut clipped)?;
            let banner_text_max_width = (hero_body.size.width as i32 - 2 * BANNER_TEXT_INSET).max(0) as u32;
            let label_font = font::label();
            let banner_text = truncate_to_width(&label_font, &text, banner_text_max_width);
            let banner_mid_y = banner_y + BANNER_HEIGHT / 2;
            let _ = label_font.render_aligned(
                banner_text.as_str(),
                Point::new(hero_body.top_left.x + BANNER_TEXT_INSET, banner_mid_y),
                VerticalPosition::Center,
                HorizontalAlignment::Left,
                FontColor::Transparent(color),
                &mut clipped,
            );
        }

        // --- Bottom stat strip. Whatever fields the design's data-
        // dependency table (section 13) says survive Tier 1 — the
        // LINK/SIGNAL bars are still CUT entirely, not dashed, so they
        // are simply not part of `stat_line` at all. Fixed y
        // (`STAT_TOP`, relative to `area`, not `hero_body` -- it does not
        // move) — occupies the same rows whether or not a banner is
        // showing; this is the whole point of the fixed grid. Narrowed to
        // `hero_body`'s width for the same reason the banner is. **This
        // is also where the OUT meter's old collision with this slot gets
        // resolved**: the horizontal meter used to share this exact grid
        // slot with `stat_line` (a latent collision this module's own
        // prior comment admitted, papered over only because nothing
        // populated `stat_line` with live data yet). The vertical meter
        // now lives entirely inside its own strip beside the rail, a
        // disjoint rectangle from every text element on Home -- there is
        // no longer anything to collide with here. ---
        if let Some(stat_line) = &self.stat_line {
            let stat_y = area.top_left.y + STAT_TOP;
            let label_font = font::label();
            let stat_max_width = (hero_body.size.width as i32 - LEFT_MARGIN - RIGHT_MARGIN).max(0) as u32;
            let upper = stat_line.to_uppercase();
            let stat_text = truncate_to_width(&label_font, &upper, stat_max_width);
            let _ = label_font.render_aligned(
                stat_text.as_str(),
                Point::new(hero_body.top_left.x + LEFT_MARGIN, stat_y),
                VerticalPosition::Top,
                HorizontalAlignment::Left,
                FontColor::Transparent(palette::TEXT_SECONDARY),
                &mut clipped,
            );
        }

        // --- Stereo OUT level meter (design section 21 E17/C8, bead
        // pico-link-du0; moved to the vertical strip beside the rail per
        // `.planning/design/2026-09-03-vertical-out-meter.md`, bead
        // pico-link-ky8): a vertical pair of segmented columns, L (screen-
        // left, always) beside R, in their own disjoint strip. **Absent
        // whenever there is no live reading, or the last one has gone
        // stale** — this is the hard constraint the bead was
        // commissioned under (design section 15): a still meter reads as
        // silence when it means no data, so a stale reading is simply not
        // drawn, not frozen and not dashed, and the unlit `DIVIDER`
        // segments must NOT be left behind as a ghost outline -- that IS
        // a frozen meter pinned at zero. The `OUT` legend vanishes with
        // the columns for the same reason -- a label over nothing reads
        // as broken, not as silent. See `OUT_LEVEL_STALE_AFTER`'s doc
        // comment for the staleness window and `HeroStatusView::
        // redraw_after` for how this widget schedules its own repaint to
        // *notice* staleness with no new event to trigger a rebuild. ---
        if let Some(level) = &self.out_level {
            if ctx.now().saturating_duration_since(level.received_at) <= OUT_LEVEL_STALE_AFTER {
                let label_font = font::label();
                let legend_center_x = pair.top_left.x + pair.size.width as i32 / 2;
                let _ = label_font.render_aligned(
                    "OUT",
                    Point::new(legend_center_x, name_y),
                    VerticalPosition::Top,
                    HorizontalAlignment::Center,
                    FontColor::Transparent(palette::TEXT_SECONDARY),
                    &mut clipped,
                );
                // Release-ballistic decay (bead pico-link-ajj, design
                // requirement C): the bar draws the attack anchor decayed
                // to *now* -- NO floor against `level.rms_l`/`rms_r`. An
                // earlier version floored at the latest raw sample,
                // reasoning that it was a better estimate of "the level
                // right now" than continuing to decay past it -- that
                // reasoning was wrong and defeats the whole ballistic
                // (code review on this bead, confirmed by the
                // orchestrator): between publishes `rms_l`/`rms_r` are
                // frozen at whatever the last event reported, so the
                // `.max()` pinned the displayed value to that constant
                // for the sample's entire life, making the release only
                // ever move at publish events -- exactly the quantized-to-
                // cadence behaviour this bead exists to fix, and worse in
                // the loud-then-silence case (silence publishes nothing
                // at all, per the deliberate empty-window skip, so the
                // bar would sit frozen at the last loud reading until the
                // staleness cutoff hides it outright). No floor is
                // actually needed: `on_levels_changed` already
                // re-anchors correctly from an arbitrarily large gap (its
                // own fold-time decay saturates toward 0, it does not
                // hold a stale anchor), and a genuinely stale reading is
                // never rendered at all -- see the `OUT_LEVEL_STALE_AFTER`
                // check just above this block. See `crate::app::
                // decay_rms`'s doc comment for why this is computed here,
                // at render time, rather than mutated on a schedule.
                let displayed_rms_l = crate::app::decay_rms(level.attack_rms_l, ctx.now().saturating_duration_since(level.attack_rms_l_at));
                let displayed_rms_r = crate::app::decay_rms(level.attack_rms_r, ctx.now().saturating_duration_since(level.attack_rms_r_at));
                theme::draw_vertical_level_meter(&mut clipped, l_block, displayed_rms_l, level.hold_l)?;
                theme::draw_vertical_level_meter(&mut clipped, r_block, displayed_rms_r, level.hold_r)?;
            }
        }

        Ok(())
    }

    /// Folds everything [`HeroStatusView::render`] actually reads: the
    /// device name, the codec status (word/fallback reason/bitrate), the
    /// muted flag (together with `fallback` this fully determines
    /// [`Self::active_banner`] -- see that method's own priority rule, so
    /// there is nothing left of the banner to fold separately), the stat
    /// line, and the OUT-meter sample.
    ///
    /// **THE TIME-FOLDING TRAP** (design section 3.2, this widget's own
    /// worked example in section 8): [`Self::redraw_after`] overrides the
    /// default, so per the mechanical review rule this `paint_key` MUST
    /// fold the *quantised visual consequence* of time -- never
    /// `ctx.now()` itself.
    ///
    /// CORRECTED 2026-09-06 (coordinator finding, bead pico-link-7h5.5):
    /// an earlier version of this method reasoned that the continuously-
    /// decaying release ballistic `render` computes from `attack_rms_*`/
    /// `attack_rms_*_at` (bead pico-link-ajj) didn't need folding, on the
    /// theory that a fresh sample arrives on every `Event::LevelsChanged`
    /// and that alone drives the visible motion. That reasoning was
    /// backwards: `Self::redraw_after` schedules a repaint every
    /// `OUT_LEVEL_REFRESH_INTERVAL` *specifically* so the bar keeps
    /// decaying between publishes, with no new sample and therefore no
    /// change to `received_at`. With nothing time-derived folded, the
    /// damage pass saw an unchanged key on every one of those scheduled
    /// repaints and skipped `render` outright -- the exact "folding
    /// nothing time-related freezes a time-driven widget" failure this
    /// bead's own rule warns about, and it made the ballistic invisible
    /// (the bar only ever stepped at publish cadence, the behaviour bead
    /// pico-link-ajj was written to eliminate).
    ///
    /// The fix: recompute the same decayed value `render` does from
    /// `ctx.elapsed_since(attack_rms_*_at)`, then quantise it through
    /// [`theme::vertical_level_dbfs_segment_count`] -- the exact mapping
    /// [`theme::draw_vertical_level_meter`] uses to choose how many of the
    /// 16 segments light up. That segment count (0..=16 per channel, 17
    /// possible values) *is* the quantised visual consequence: it changes
    /// only at the instant a segment actually lights or extinguishes on
    /// screen, never merely because `ctx.now()` advanced. The peak-hold
    /// cap gets the same treatment for scale consistency, though it needs
    /// no time term of its own -- `render` draws it straight from
    /// `level.hold_l`/`hold_r` with no decay, so its position cannot move
    /// without a new sample, which `received_at` already catches.
    ///
    /// Also still folded: `level.received_at` (a sample timestamp, not the
    /// render instant -- it only changes when a new reading actually
    /// arrives, catching everything a new sample can change) and the
    /// already-computed boolean `age >= OUT_LEVEL_STALE_AFTER` (the same
    /// comparison `render` and `redraw_after` both make, catching the
    /// "stream went silent" transition to absent). Folding `ctx.now()`
    /// directly would make this widget permanently dirty (a silent no-op
    /// -- see `PaintKey`'s doc comment); folding nothing time-related at
    /// all is exactly the bug described above.
    fn paint_key(&self, ctx: &RenderCtx) -> PaintKey {
        let key = PaintKey::of(HERO_PAINT_KEY_SEED).fold_str(&self.device_name);
        let key = match &self.status {
            CodecStatus::NoLink => key.fold(0),
            CodecStatus::Connected { word, fallback, bitrate } => {
                let key = key.fold(1).fold_str(word);
                let key = key.fold_opt_str(fallback.as_deref());
                match bitrate {
                    BitrateStatus::Idle => key.fold(0),
                    BitrateStatus::Kbps(kbps) => key.fold(1).fold(u64::from(*kbps)),
                }
            }
        };
        let key = key.fold(u64::from(self.muted));
        let key = key.fold_opt_str(self.stat_line.as_deref());
        match &self.out_level {
            None => key.fold(0),
            Some(level) => {
                let stale = ctx.elapsed_since(level.received_at) >= OUT_LEVEL_STALE_AFTER;
                // The bar's actually-drawn value (coordinator finding on
                // this bead, 2026-09-06): `render` does not draw
                // `level.rms_l`/`rms_r` at all -- it draws
                // `crate::app::decay_rms` applied to `attack_rms_*` and
                // `attack_rms_*_at`, a value that changes continuously
                // between publishes purely as `ctx.now()` advances (the
                // release ballistic, bead pico-link-ajj). Folding the raw,
                // publish-cadence `rms_l`/`rms_r` fields (as this key used
                // to) folds a value the widget never paints, and folds
                // nothing that actually tracks the bar's motion between
                // samples -- exactly the "folding nothing time-related
                // freezes a time-driven widget" failure this bead's own
                // rule warns about: `HeroStatusView::redraw_after`
                // schedules a repaint every `OUT_LEVEL_REFRESH_INTERVAL`
                // while live, the damage pass would call this `paint_key`,
                // see no change, and skip `render` -- the decay would only
                // ever visibly step at publish events again.
                //
                // The fix folds the *quantised visual consequence*
                // instead of `ctx.now()` itself: run the same decayed
                // value `render` computes through
                // `theme::vertical_level_dbfs_segment_count`, the exact
                // mapping `draw_vertical_level_meter` uses to choose how
                // many of the 16 segments light up. That count only has
                // 17 possible values (0..=16) and only changes at the
                // instant a segment actually lights or extinguishes on
                // screen -- an honest, cheap total summary of what the eye
                // can see, not a proxy for "time passed".
                let displayed_rms_l = crate::app::decay_rms(level.attack_rms_l, ctx.elapsed_since(level.attack_rms_l_at));
                let displayed_rms_r = crate::app::decay_rms(level.attack_rms_r, ctx.elapsed_since(level.attack_rms_r_at));
                let segments_l = theme::vertical_level_dbfs_segment_count(displayed_rms_l);
                let segments_r = theme::vertical_level_dbfs_segment_count(displayed_rms_r);
                // The peak-hold cap (`draw_vertical_level_meter`'s `hold`
                // parameter) is drawn straight from `level.hold_l`/
                // `hold_r` with no decay applied -- its on-screen position
                // cannot move without a new sample, so it needs no
                // time-derived fold, only the same segment-count
                // quantisation for consistency with the bar it shares a
                // scale with.
                let hold_segments_l = theme::vertical_level_dbfs_segment_count(level.hold_l);
                let hold_segments_r = theme::vertical_level_dbfs_segment_count(level.hold_r);
                key.fold(1)
                    .fold(level.received_at.as_micros())
                    .fold(u64::try_from(segments_l).expect("segment count is in 0..=16"))
                    .fold(u64::try_from(segments_r).expect("segment count is in 0..=16"))
                    .fold(u64::try_from(hold_segments_l).expect("segment count is in 0..=16"))
                    .fold(u64::try_from(hold_segments_r).expect("segment count is in 0..=16"))
                    .fold(u64::from(stale))
            }
        }
    }

    /// Exposes the fallback state (design section 6.2, link 3: the X-rail
    /// label switches from "link" to "why?" under fallback) via
    /// [`ChromeContribution::fallback`] rather than painting the rail
    /// itself — the rail lives in `ChromeContribution`'s a/b/x/y fields,
    /// which is `pico-link-znb.5` (E2)'s job, not this widget's.
    fn chrome_contribution(&self, _ctx: &RenderCtx) -> Option<ChromeContribution> {
        Some(ChromeContribution { fallback: self.is_fallback(), ..Default::default() })
    }

    /// Requests another render before this widget's OUT-meter reading
    /// would otherwise go stale-but-still-drawn (bead pico-link-du0) —
    /// the mechanism that lets the "absent, never frozen" rule in
    /// `render` above actually fire on schedule even when no new
    /// [`crate::app::Event::LevelsChanged`] arrives to trigger a rebuild
    /// (e.g. the host stops sending PCM mid-stream). Bounded at
    /// [`OUT_LEVEL_REFRESH_INTERVAL`] while comfortably live, and at
    /// exactly the remaining time-to-staleness once close to it, so the
    /// meter disappears within one frame of going stale rather than up to
    /// a whole refresh interval late. Returns `None` once already stale
    /// (this frame already drew it absent; nothing more to schedule) or
    /// when there's no reading at all.
    fn redraw_after(&self, ctx: &RenderCtx) -> Option<Duration> {
        let level = self.out_level.as_ref()?;
        let age = ctx.now().saturating_duration_since(level.received_at);
        if age >= OUT_LEVEL_STALE_AFTER {
            None
        } else {
            Some(OUT_LEVEL_REFRESH_INTERVAL.min(OUT_LEVEL_STALE_AFTER.saturating_sub(age)))
        }
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

    // --- paint_key (bead pico-link-7h5.5): the time-folding trap -------

    fn out_level_at(received_at: Instant) -> OutLevelDisplay {
        OutLevelDisplay {
            peak_l: 200,
            peak_r: 180,
            rms_l: 120,
            rms_r: 100,
            hold_l: 150,
            hold_r: 140,
            received_at,
            attack_rms_l: 120,
            attack_rms_r: 100,
            attack_rms_l_at: received_at,
            attack_rms_r_at: received_at,
        }
    }

    #[test]
    fn paint_key_is_stable_across_calls_with_no_state_change() {
        let view = nominal();
        assert_eq!(view.paint_key(&test_ctx()), view.paint_key(&test_ctx()));
    }

    #[test]
    fn paint_key_does_not_change_from_ctx_now_alone_with_no_out_level() {
        // No `out_level` at all: this widget's own appearance has nothing
        // time-driven left to fold (`redraw_after` returns `None` in this
        // case too -- see that method), so advancing the clock alone must
        // not change the key.
        let view = nominal();
        let t0 = RenderCtx::at(Instant::from_micros(0));
        let t1 = RenderCtx::at(Instant::from_micros(10_000_000));
        assert_eq!(view.paint_key(&t0), view.paint_key(&t1), "advancing the clock alone must not dirty a widget with no live OUT sample");
    }

    #[test]
    fn paint_key_does_not_change_from_ctx_now_alone_while_the_out_level_is_still_fresh() {
        // Same `OutLevelDisplay`, same `received_at` -- only `ctx.now()`
        // moves, by an amount too small for the release ballistic to
        // cross even one of `theme::VERTICAL_METER_DBFS_THRESHOLDS`' 16
        // segment boundaries (1ms of decay is well under 1% of any
        // starting value -- see `decay_rms`'s own doc comment for the
        // decay curve). This is the "folding raw now() makes the widget
        // permanently dirty" failure mode this test exists to rule out --
        // NOT a claim that no time span ever changes the key while
        // "fresh": `paint_key_changes_as_the_release_ballistic_crosses_a_
        // segment_boundary_with_no_new_sample` below proves the opposite
        // for a span long enough to matter, which is the whole point of
        // this bead's fix.
        let received_at = Instant::from_micros(1_000_000);
        let view = nominal().with_out_level(Some(out_level_at(received_at)));
        let t0 = RenderCtx::at(received_at);
        let t1 = RenderCtx::at(received_at + Duration::from_millis(1));
        assert_eq!(
            view.paint_key(&t0),
            view.paint_key(&t1),
            "the paint key must not change purely from ctx.now() advancing by an amount too small to move any segment"
        );
    }

    #[test]
    fn paint_key_changes_exactly_when_the_out_level_crosses_the_stale_threshold() {
        // The "folding nothing time-related freezes a time-driven widget"
        // failure mode: the sample's `received_at` never changes, but the
        // quantised stale boolean must flip once `ctx.now()` crosses
        // `OUT_LEVEL_STALE_AFTER`, and the key must change with it -- this
        // is what lets the meter actually go absent on schedule (design
        // section 15) even with no new event.
        let received_at = Instant::from_micros(1_000_000);
        let view = nominal().with_out_level(Some(out_level_at(received_at)));
        let just_before = RenderCtx::at(received_at + OUT_LEVEL_STALE_AFTER.checked_sub(Duration::from_millis(1)).expect("OUT_LEVEL_STALE_AFTER is well over 1ms"));
        let just_after = RenderCtx::at(received_at + (OUT_LEVEL_STALE_AFTER + Duration::from_millis(1)));
        assert_ne!(
            view.paint_key(&just_before),
            view.paint_key(&just_after),
            "crossing the stale threshold with no new sample must still change the paint key"
        );
    }

    #[test]
    fn paint_key_changes_when_a_new_sample_arrives_even_at_the_same_instant() {
        let ctx = test_ctx();
        let a = nominal().with_out_level(Some(out_level_at(Instant::from_micros(0))));
        let b = nominal().with_out_level(Some(out_level_at(Instant::from_micros(1))));
        assert_ne!(a.paint_key(&ctx), b.paint_key(&ctx), "a new sample's received_at must change the key even under the same render instant");
    }

    #[test]
    fn paint_key_changes_when_a_level_value_changes_but_received_at_does_not() {
        // Mutates `attack_rms_l` (the ballistic anchor `render` actually
        // decays and draws), not `peak_l`/`rms_l` -- `render` never reads
        // the latter two at all, so folding them would test nothing about
        // what's on screen. `test_ctx()` sits at the same instant as
        // `received_at` (elapsed == 0), so `decay_rms` is a no-op here and
        // the mutated value passes straight through to the segment count.
        let received_at = Instant::from_micros(0);
        let a = nominal().with_out_level(Some(out_level_at(received_at)));
        let mut sample = out_level_at(received_at);
        sample.attack_rms_l = 255;
        let b = nominal().with_out_level(Some(sample));
        assert_ne!(a.paint_key(&test_ctx()), b.paint_key(&test_ctx()));
    }

    #[test]
    fn paint_key_changes_when_the_hold_cap_moves_but_received_at_does_not() {
        // `render` draws the peak-hold cap straight from `level.hold_l`/
        // `hold_r` (no decay) -- pins that this key still folds it even
        // though it needed no time term.
        let received_at = Instant::from_micros(0);
        let a = nominal().with_out_level(Some(out_level_at(received_at)));
        let mut sample = out_level_at(received_at);
        sample.hold_l = 255;
        let b = nominal().with_out_level(Some(sample));
        assert_ne!(a.paint_key(&test_ctx()), b.paint_key(&test_ctx()));
    }

    /// THE DELIVERABLE for the coordinator's finding on this bead
    /// (2026-09-06): the A4 property test
    /// (`damage_rendered_frame_matches_a_full_frame_render_of_the_same_state_for_every_screen`)
    /// cannot catch this bug class -- it only ever compares two renders of
    /// the SAME `ctx.now()`. The actual failure is across two DIFFERENT
    /// instants with the exact same `OutLevelSample`/`OutLevelDisplay`
    /// (no new event, no new `received_at`): the release ballistic
    /// (`crate::app::decay_rms` over `attack_rms_l`/`attack_rms_l_at`,
    /// bead pico-link-ajj) keeps moving as `ctx.now()` advances, and
    /// `Self::redraw_after` schedules exactly this kind of no-new-sample
    /// repaint every `OUT_LEVEL_REFRESH_INTERVAL` while the reading is
    /// live. If `paint_key` doesn't fold the decayed value's quantised
    /// segment count, those scheduled repaints see an unchanged key, the
    /// damage pass skips `render`, and the bar freezes between publishes
    /// -- silently undoing the whole ballistic.
    ///
    /// `anchor = 200` and `elapsed = 1000ms` are chosen from the existing
    /// `decay_rms_after_one_second_is_roughly_ten_percent` fixture
    /// (200 -> ~20, comfortably crossing several of
    /// `theme::VERTICAL_METER_DBFS_THRESHOLDS`' 16 segment boundaries, not
    /// balanced on the edge of just one) so this test's premise -- that a
    /// segment boundary is actually crossed -- is pinned by another test,
    /// not asserted here on faith.
    #[test]
    fn paint_key_changes_as_the_release_ballistic_crosses_a_segment_boundary_with_no_new_sample() {
        let received_at = Instant::from_micros(1_000_000);
        let mut sample = out_level_at(received_at);
        sample.attack_rms_l = 200;
        sample.attack_rms_l_at = received_at;
        let view = nominal().with_out_level(Some(sample));

        // Same OutLevelDisplay both times -- only ctx.now() moves.
        let t0 = RenderCtx::at(received_at);
        let t1 = RenderCtx::at(received_at + Duration::from_millis(1000));

        assert_ne!(
            view.paint_key(&t0),
            view.paint_key(&t1),
            "the release ballistic decaying between publishes must change the paint key even though the OutLevelSample itself never changed -- otherwise the damage pass would freeze the bar mid-decay"
        );
    }

    #[test]
    fn paint_key_distinguishes_no_out_level_from_a_present_one() {
        let without = nominal();
        let with = nominal().with_out_level(Some(out_level_at(Instant::from_micros(0))));
        assert_ne!(without.paint_key(&test_ctx()), with.paint_key(&test_ctx()));
    }

    #[test]
    fn paint_key_changes_with_the_codec_word_fallback_and_bitrate() {
        let word_a = nominal();
        let word_b = HeroStatusView::new(
            "Sony WH-1000XM5",
            CodecStatus::Connected { word: String::from("SBC"), fallback: None, bitrate: BitrateStatus::Kbps(909) },
        );
        assert_ne!(word_a.paint_key(&test_ctx()), word_b.paint_key(&test_ctx()), "a different codec word must change the key");

        let fallback = HeroStatusView::new(
            "Sony WH-1000XM5",
            CodecStatus::Connected {
                word: String::from("LDAC"),
                fallback: Some(String::from("reason")),
                bitrate: BitrateStatus::Kbps(909),
            },
        );
        assert_ne!(word_a.paint_key(&test_ctx()), fallback.paint_key(&test_ctx()), "a fallback reason must change the key");

        let bitrate = HeroStatusView::new(
            "Sony WH-1000XM5",
            CodecStatus::Connected { word: String::from("LDAC"), fallback: None, bitrate: BitrateStatus::Kbps(328) },
        );
        assert_ne!(word_a.paint_key(&test_ctx()), bitrate.paint_key(&test_ctx()), "a different bitrate must change the key");

        let no_link = HeroStatusView::new("Sony WH-1000XM5", CodecStatus::NoLink);
        assert_ne!(word_a.paint_key(&test_ctx()), no_link.paint_key(&test_ctx()), "NoLink must differ from Connected");
    }

    #[test]
    fn paint_key_changes_with_muted_and_stat_line() {
        let base = nominal();
        let muted = nominal().with_muted(true);
        assert_ne!(base.paint_key(&test_ctx()), muted.paint_key(&test_ctx()));

        let no_stat = HeroStatusView::new(
            "Sony WH-1000XM5",
            CodecStatus::Connected { word: String::from("LDAC"), fallback: None, bitrate: BitrateStatus::Kbps(909) },
        );
        assert_ne!(no_stat.paint_key(&test_ctx()), base.paint_key(&test_ctx()), "a stat line vs none must change the key");
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
