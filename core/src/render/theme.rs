//! The M1 visual design language: a semantic color palette, per-role
//! `u8g2-fonts` accessors, `open_iconic` icon codepoints, and two shared
//! drawing primitives (a chip and a full-width selection block).
//!
//! Approved originally for the T-Embed's 320x170 ST7789 color panel;
//! carried over unchanged onto the 240x240 Pico Plus 2 W panel (Epic B2)
//! — this module's palette/font choices are resolution-independent, only
//! layout (`list.rs`'s row budget, `screen.rs`'s chrome) needed
//! recomputing for the new panel. This module is the *foundation* only —
//! it swaps the render core's
//! fonts/colors and provides primitives for later views (list chrome,
//! detail views, and similar) to compose; it does not itself restyle
//! spacing/layout.
//!
//! Reuses validated findings from the throwaway design-review spike
//! (`design-review-m1-mockups` branch, never merged):
//! `core/examples/design_review_m1.rs` for the palette's exact `Rgb565`
//! values and the font choice per role, and `core/examples/icon_probe.rs`
//! for the `shield`/`lock-locked`/`lock-unlocked`/`eye` `open_iconic_all`
//! codepoints (empirically probed there, since upstream u8g2 doesn't
//! document the mapping). `caret-right`'s codepoint was not covered by
//! that spike and was derived + probed fresh separately (see
//! `icon::CARET_RIGHT`'s doc comment).

use core::convert::Infallible;

use embedded_graphics::draw_target::DrawTarget;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::{Point, Primitive, Size};
use embedded_graphics::primitives::{
    CornerRadiiBuilder, PrimitiveStyle, Rectangle, RoundedRectangle, StyledDrawable,
};
use embedded_graphics::Drawable;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::FontRenderer;

/// The semantic color palette. Every color used by the render core should
/// come from here, by role, rather than an ad-hoc `embedded_graphics`
/// `WebColors` constant — that's what "centralizing the theme" means: a
/// future palette tweak (or a light-mode variant, if one is ever wanted)
/// is a one-file change instead of a codebase-wide grep.
pub mod palette {
    use embedded_graphics::pixelcolor::Rgb565;

    /// Screen base fill (`#0B1120`): whatever isn't covered by a bar,
    /// card, or row — the "void" behind all content.
    pub const BACKGROUND: Rgb565 = Rgb565::new(1, 4, 4);
    /// Chrome bars (title/hint) and an unselected row's implicit
    /// backdrop (`#16213A`) — one step brighter than `BACKGROUND`.
    pub const SURFACE: Rgb565 = Rgb565::new(3, 8, 7);
    /// A selected row or focused field's backdrop (`#1E2A47`) — one step
    /// brighter than `SURFACE`, giving focus a visible lift without a
    /// literal raised/rounded card.
    pub const SURFACE_ELEVATED: Rgb565 = Rgb565::new(4, 10, 9);
    /// The brand fill color (`#175DDC`) — used for solid brand-colored
    /// shapes (e.g. the initial chip's background), not text or icons.
    pub const BRAND: Rgb565 = Rgb565::new(3, 23, 27);
    /// The brighter brand accent (`#3B82F6`) — selection accent bars,
    /// active/positive icon glyphs; reserved for things that should read
    /// as "brand, but louder" than a plain `BRAND` fill.
    pub const BRAND_BRIGHT: Rgb565 = Rgb565::new(7, 32, 30);
    /// Primary text and glyph color (`#F5F7FA`) — list row names,
    /// field values, chip initials.
    pub const TEXT_PRIMARY: Rgb565 = Rgb565::new(30, 61, 30);
    /// Secondary/muted text (`#9AA6BF`) — usernames, hints, field
    /// labels, the sync counter.
    pub const TEXT_SECONDARY: Rgb565 = Rgb565::new(19, 41, 23);
    /// Hairline separators between chrome and content, or between rows
    /// (`#22304F`).
    pub const DIVIDER: Rgb565 = Rgb565::new(4, 12, 10);
    /// A successful/positive status indicator (`#3DDC84`) — e.g. a
    /// sync-ok dot.
    pub const STATUS_SUCCESS: Rgb565 = Rgb565::new(7, 54, 16);
    /// An error/negative status indicator (`#FF5964`).
    pub const STATUS_ERROR: Rgb565 = Rgb565::new(31, 22, 12);
    /// A caution/attention status indicator (`#FFB020`) — e.g. "secret
    /// currently revealed."
    pub const STATUS_WARNING: Rgb565 = Rgb565::new(31, 44, 4);
}

/// Per-role `u8g2-fonts` accessors. Each returns a fresh, independently
/// configured [`FontRenderer`] (cheap — it's a thin wrapper over a static
/// font table, not an allocation) with
/// [`with_ignore_unknown_chars`](FontRenderer::with_ignore_unknown_chars)
/// set: list row names/usernames are arbitrary user data (could contain
/// glyphs `helv`/`profont` don't cover), and this render core must never
/// panic on unusual input — better to silently skip an unrenderable
/// character than crash the render loop over it.
pub mod font {
    use u8g2_fonts::fonts;
    use u8g2_fonts::FontRenderer;

    /// Screen/chrome titles (the title bar).
    #[must_use]
    pub const fn title() -> FontRenderer {
        FontRenderer::new::<fonts::u8g2_font_helvB10_tf>().with_ignore_unknown_chars(true)
    }

    /// A list row's name — the bold, primary line of a list row (and
    /// the detail screen's title-bar text, via [`title`] instead — `name`
    /// is specifically the *list row* weight/size).
    #[must_use]
    pub const fn name() -> FontRenderer {
        FontRenderer::new::<fonts::u8g2_font_helvB12_tf>().with_ignore_unknown_chars(true)
    }

    /// A list row's secondary/subtitle line (e.g. a username).
    #[must_use]
    pub const fn username() -> FontRenderer {
        FontRenderer::new::<fonts::u8g2_font_helvR10_tf>().with_ignore_unknown_chars(true)
    }

    /// A plain detail-field value (e.g. website/URI) — not the secret
    /// field, which uses [`secret`] instead.
    #[must_use]
    pub const fn value() -> FontRenderer {
        FontRenderer::new::<fonts::u8g2_font_helvR12_tf>().with_ignore_unknown_chars(true)
    }

    /// The password/secret field's value. Monospaced (`profont`) so
    /// every masked `*` and every revealed character occupies the same
    /// width — a proportional font would make a masked secret's length
    /// visually leak information a monospaced mask doesn't.
    #[must_use]
    pub const fn secret() -> FontRenderer {
        FontRenderer::new::<fonts::u8g2_font_profont17_mf>().with_ignore_unknown_chars(true)
    }

    /// A detail field's label (e.g. "Username"). Callers render the
    /// label text in uppercase themselves — this accessor only provides
    /// the font/weight, per Uma's spec (`helvB08`, small caps-style
    /// label above each field).
    #[must_use]
    pub const fn label() -> FontRenderer {
        FontRenderer::new::<fonts::u8g2_font_helvB08_tf>().with_ignore_unknown_chars(true)
    }

    /// The Home status face's hero codec word (design section 6: "LDAC",
    /// "AAC", "SBC", ...) — the design's central bet that *"is it actually
    /// LDAC, or did it quietly fall back"* is answerable at a glance across
    /// a desk (~30-50cm), which `helvB12` (the previous ceiling in this
    /// module, via [`name`]) cannot do at that distance.
    ///
    /// Face: `u8g2_font_helvB24_tf` — bold Helvetica, 24px nominal, measured
    /// 25px cap height for an all-caps codec word (via
    /// `cargo run -p pico-link-core --example hero_font_probe`, checked
    /// against `get_rendered_dimensions_aligned`'s bounding box for "LDAC",
    /// "SBC", "AAC"), inside the ~20-26px band the design specifies.
    /// `helvB18_tf` (the next face down) was rejected: at the same viewing
    /// distance driving the ~20-26px requirement, 18px reads closer to
    /// `name()`'s weight than to a hero. The `_tf` glyph set ("full") covers
    /// ASCII digits as well as uppercase letters, so a caller pairing this
    /// with a bitrate figure (section 6's "909 kbps" line) or any future
    /// all-digits use is covered by this face directly, though the current
    /// design routes the bitrate line through [`value`], not this accessor.
    ///
    /// Flash cost: `u8g2_font_helvB24_tf.u8g2font`'s glyph table is 6566
    /// bytes (vs. 3275 bytes for `helvB12_tf`, already linked in via
    /// [`name`]) — a new font entirely, so this is +6566 bytes of flash
    /// (measured against the crate's on-disk font blob; the linked
    /// `.rodata` cost tracks that figure closely since `u8g2-fonts` embeds
    /// each font as one static byte table with no per-glyph code
    /// generation).
    #[must_use]
    pub const fn hero() -> FontRenderer {
        FontRenderer::new::<fonts::u8g2_font_helvB24_tf>().with_ignore_unknown_chars(true)
    }

    /// The hint bar's control-legend text. One size down from
    /// `username`/the design-review mockup's `helvR10` (Andreas's
    /// feedback: the hint bar reads as decoration, not primary content,
    /// so it can afford to be the smallest text on screen) — `helvR08`,
    /// the next `helv` size down that's still legible on this panel.
    #[must_use]
    pub const fn hint() -> FontRenderer {
        FontRenderer::new::<fonts::u8g2_font_helvR08_tf>().with_ignore_unknown_chars(true)
    }

    /// `open_iconic` glyphs at 1x scale (roughly 8x8px) — for compact
    /// inline icons (e.g. a row's focus caret), and deliberately also the
    /// title bar's brand mark: the design-review mockup used `icon_2x`
    /// there, which — inside the fixed `TITLE_BAR_HEIGHT`-px bar — left
    /// the mark touching the bar's top/bottom edges with no breathing
    /// room; design review asked for a smaller mark with more air around
    /// it, and `icon_1x` is what leaves that air without shrinking
    /// `TITLE_BAR_HEIGHT` itself.
    #[must_use]
    pub const fn icon_1x() -> FontRenderer {
        FontRenderer::new::<fonts::u8g2_font_open_iconic_all_1x_t>()
    }

    /// `open_iconic` glyphs at 2x scale (roughly 16x16px) — for
    /// field-level status icons (e.g. the lock in a password field).
    #[must_use]
    pub const fn icon_2x() -> FontRenderer {
        FontRenderer::new::<fonts::u8g2_font_open_iconic_all_2x_t>()
    }

    /// `open_iconic` glyphs at 4x scale (roughly 32x32px) — for a large,
    /// centered decorative mark (e.g. an empty-content state's icon),
    /// never for chrome (too big for the fixed-height title/hint bars).
    #[must_use]
    pub const fn icon_4x() -> FontRenderer {
        FontRenderer::new::<fonts::u8g2_font_open_iconic_all_4x_t>()
    }
}

/// `open_iconic_all_*` codepoints. Not documented upstream — u8g2's
/// `do_iconic.sh` build script assigns codepoints in the alphabetical
/// order of `github.com/iconic/open-iconic`'s `svg/` directory, starting
/// at `U+0040` (`-e 64`), which has to be derived (or probed) rather than
/// looked up.
pub mod icon {
    /// A right-pointing solid caret/triangle — a row/field's "this is
    /// selected, activate to go further" disclosure indicator.
    ///
    /// Not covered by the design-review spike (it drew this shape as a
    /// raw `Triangle` primitive instead). Derived from
    /// `iconic/open-iconic`'s `svg/` directory listing: `caret-right.svg`
    /// is the 48th file alphabetically (1-indexed) -> 0-based index 47
    /// -> codepoint `64 + 47 = 111 = 0x6F`. Cross-checked two ways before
    /// trusting it: (1) the same formula applied to `eye.svg` (102nd
    /// file -> codepoint `0xA5`) reproduced the design-review spike's
    /// independently probed codepoint for that glyph exactly (the `eye`
    /// constant that validated this has since been removed as a
    /// Bitwarden-hardware-key leftover with no consumer — see
    /// `pico-link-znb.12`); (2) a throwaway `core/examples/caret_probe.rs`
    /// rendered `0x6D..=0x71` to a PNG and visually confirmed
    /// `0x6D`/`0x6E`/`0x6F`/`0x70` are down/left/right/up-pointing carets
    /// respectively, with `0x6C` (camera-slr) and `0x71` (cart) as sane
    /// neighbors either side.
    pub const CARET_RIGHT: char = '\u{6F}';
    /// A Bluetooth glyph — e.g. a Home menu's "Pair device" row icon.
    /// Probed via a throwaway grid probe
    /// (`core/examples/icon_probe_home.rs`, deleted after use per the
    /// `caret_probe.rs` precedent) and logged there as rendering cleanly
    /// at `icon_2x`; re-confirmed at `icon_4x` (the chip size) via
    /// `core/examples/home_menu_probe.rs`'s zoomed PNG output.
    pub const BLUETOOTH: char = '\u{5E}';
    /// A cog/gear glyph — e.g. a Home menu's "Settings" row icon. Probed
    /// alongside [`BLUETOOTH`]; see its doc comment.
    pub const COG: char = '\u{81}';

    // -- pico-link-znb.12: E10 icon probe (2026-08-31) --
    //
    // Fetched the live alphabetical listing of
    // `github.com/iconic/open-iconic/tree/master/svg` (223 files,
    // `account-login.svg`..`zoom-out.svg`) and applied this module's
    // `codepoint = 0x40 + alphabetical index` formula. Cross-checked
    // against every codepoint already in this module before trusting the
    // formula for new ones: SHIELD (index 188), CARET_RIGHT (47),
    // BLUETOOTH (30) and COG (65) all reproduced their existing,
    // independently-probed values exactly. All five glyphs below were
    // then rendered in a labelled grid at `icon_2x`/`icon_4x`
    // (`core/examples/icon_probe_znb12.rs`, run via `cargo run -p
    // pico-link-core --example icon_probe_znb12`, output
    // `icon_probe_znb12.png`) and confirmed by eye at 3x nearest-neighbor
    // zoom — each one names the glyph it actually rendered, not just the
    // formula's prediction.

    /// A headphones glyph — e.g. the Devices/scan list's per-row device
    /// icon. `headphones.svg` is alphabetical index 118 -> codepoint
    /// `0x40 + 118 = 0xB6`. Confirmed by eye: a clean over-ear headphones
    /// silhouette at both `icon_2x` and `icon_4x` in
    /// `icon_probe_znb12.png`.
    pub const HEADPHONES: char = '\u{B6}';

    /// A checkmark glyph — e.g. the pairing wizard's success outcome.
    /// `check.svg` is alphabetical index 51 -> codepoint
    /// `0x40 + 51 = 0x73`. Confirmed by eye: a clean single checkmark
    /// (not a circled check — that's the separate `circle-check.svg`,
    /// index 56) at both `icon_2x` and `icon_4x` in
    /// `icon_probe_znb12.png`.
    pub const CHECK: char = '\u{73}';

    /// A plus glyph — e.g. the Devices list's "Pair new headphones" row.
    /// `plus.svg` is alphabetical index 170 -> codepoint
    /// `0x40 + 170 = 0xEA`. Confirmed by eye: a clean plus/cross shape at
    /// both `icon_2x` and `icon_4x` in `icon_probe_znb12.png`.
    pub const PLUS: char = '\u{EA}';

    /// A warning-triangle glyph (a triangle containing "!") — e.g. the
    /// FALLBACK/MUTED status banners. `warning.svg` is alphabetical index
    /// 216 -> codepoint `0x40 + 216 = 0x118`. Confirmed by eye: a clean
    /// filled triangle with an exclamation mark at both `icon_2x` and
    /// `icon_4x` in `icon_probe_znb12.png`. Codepoint exceeds `0xFF`
    /// (unlike every other constant in this module) because index 216
    /// pushes past the single-byte range this alphabet happens to fit
    /// for earlier glyphs — still a single valid `char`, just not a
    /// single UTF-8 byte when encoded.
    pub const WARNING: char = '\u{118}';

    /// A "USB" title-bar glyph, paired with [`BLUETOOTH`] to indicate the
    /// wired audio-in link (design section 3's title-bar layout).
    ///
    /// **There is no dedicated USB icon in `open-iconic`** — verified
    /// against the full, current 223-file `svg/` directory listing: no
    /// `usb.svg` or equivalent exists at all. This constant is therefore
    /// a stand-in, not a literal USB glyph: `data-transfer-download.svg`
    /// (alphabetical index 78 -> codepoint `0x40 + 78 = 0x8E`), a
    /// downward arrow into a tray, chosen over the other candidates
    /// rendered side-by-side in `icon_probe_znb12.png`
    /// (`hard-drive`/`data-transfer-upload`/`cloud-download`/`signal`)
    /// because it reads as "data coming in" without visually resembling
    /// [`BLUETOOTH`]'s glyph. **Flagged for design confirmation** (Uma or
    /// Andreas) before it ships on-screen — swap this codepoint rather
    /// than treating the name as settled.
    pub const USB: char = '\u{8E}';
}

/// Corner radius, in pixels, [`draw_chip`] draws its background with.
pub const CHIP_CORNER_RADIUS: u32 = 4;

/// Draws a brand-colored rounded-square "chip" filling `rect`, with
/// `initial` centered in it on *both* axes.
///
/// Uses [`font::name`] for the glyph (matching the design-review mockup),
/// but centers it by measuring the glyph's actual rendered ink
/// bounding box via
/// [`get_rendered_dimensions_aligned`](FontRenderer::get_rendered_dimensions_aligned)
/// and aligning *that* box's center to `rect`'s center — not by asking
/// `render_aligned` for `VerticalPosition::Center`/`HorizontalAlignment::Center`
/// directly. That naive approach is what the design-review mockup used,
/// and Andreas flagged the resulting chip letters as visibly off-center:
/// `u8g2-fonts`' `Center` positioning centers on the font's *line metrics*
/// (ascent/descent from the baseline), not a specific glyph's ink — for a
/// single flat-topped capital like "G" sitting inside a font whose descent
/// budget accounts for glyphs like "g"/"y" that this one character doesn't
/// use, that mismatch reads as "pushed up" from the box's true center.
/// Measuring this specific glyph's own ink box and centering *that*
/// sidesteps the whole line-metrics-vs-ink distinction.
///
/// Falls back to `rect`'s geometric center (no visible ink to align) if
/// `initial` has no glyph in [`font::name`] — this can't happen for the
/// ASCII/Latin-1 initials `char::to_ascii_uppercase` produces, but
/// `font::name` is configured to ignore unknown glyphs rather than error,
/// so this stays a graceful no-glyph-drawn case instead of a panic for a
/// non-Latin initial.
///
/// # Errors
///
/// Returns `Infallible`'s uninhabited variant in practice — see
/// [`super::widget::Widget::render`]'s doc comment for why the `Result`
/// return exists at all.
pub fn draw_chip<D>(target: &mut D, rect: Rectangle, initial: char) -> Result<(), Infallible>
where
    D: DrawTarget<Color = Rgb565, Error = Infallible>,
{
    let radii = CornerRadiiBuilder::new().all(Size::new_equal(CHIP_CORNER_RADIUS)).build();
    RoundedRectangle::new(rect, radii)
        .draw_styled(&PrimitiveStyle::with_fill(palette::BRAND), target)?;

    let glyph_font: FontRenderer = font::name();
    let mut buf = [0_u8; 4];
    let text: &str = initial.encode_utf8(&mut buf);

    let ink_bbox = glyph_font
        .get_rendered_dimensions_aligned(
            text,
            Point::zero(),
            VerticalPosition::Top,
            HorizontalAlignment::Left,
        )
        .unwrap_or(None);

    let render_pos = match ink_bbox {
        Some(ink) => {
            let ink_center = ink.center();
            let target_center = rect.center();
            Point::new(target_center.x - ink_center.x, target_center.y - ink_center.y)
        }
        None => return Ok(()),
    };

    let _ = glyph_font.render_aligned(
        text,
        render_pos,
        VerticalPosition::Top,
        HorizontalAlignment::Left,
        FontColor::Transparent(palette::TEXT_PRIMARY),
        target,
    );

    Ok(())
}

/// Draws an `open_iconic` glyph centered in `rect`, with **no** background
/// fill — a Home menu's differentiator from [`draw_chip`]'s filled letter
/// chip: same chip slot, but "icon, no blue background" instead of "no
/// chip at all".
///
/// Uses [`font::icon_4x`] (`~32px`, close to a row's `chip_size()`,
/// currently `34px`) and the same ink-bounding-box centering [`draw_chip`]
/// uses for its letter glyph, for the same reason: naive
/// `VerticalPosition::Center`/`HorizontalAlignment::Center` centers on the
/// font's line metrics, not this specific glyph's ink, which can read as
/// visibly off-center inside a small fixed-size slot.
///
/// Renders in [`palette::BRAND_BRIGHT`] (not [`palette::TEXT_PRIMARY`],
/// [`draw_chip`]'s letter color) — matching [`super::message::MessageView`]'s
/// default icon color and the selection accent bar's color, so an
/// unfilled icon still reads as a deliberate brand-colored mark rather
/// than plain body text.
///
/// Falls back to drawing nothing (no icon, no background) if `icon` has
/// no glyph in [`font::icon_4x`] — mirrors [`draw_chip`]'s no-ink
/// fallback; every codepoint this module actually calls this with
/// ([`icon::BLUETOOTH`], [`icon::COG`]) has been probed to render
/// cleanly, so this path is a defensive no-panic guard, not an expected
/// case.
///
/// # Errors
///
/// Returns `Infallible`'s uninhabited variant in practice — see
/// [`super::widget::Widget::render`]'s doc comment for why the `Result`
/// return exists at all.
pub fn draw_icon_chip<D>(target: &mut D, rect: Rectangle, icon: char) -> Result<(), Infallible>
where
    D: DrawTarget<Color = Rgb565, Error = Infallible>,
{
    let glyph_font: FontRenderer = font::icon_4x();
    let mut buf = [0_u8; 4];
    let text: &str = icon.encode_utf8(&mut buf);

    let ink_bbox = glyph_font
        .get_rendered_dimensions_aligned(
            text,
            Point::zero(),
            VerticalPosition::Top,
            HorizontalAlignment::Left,
        )
        .unwrap_or(None);

    let render_pos = match ink_bbox {
        Some(ink) => {
            let ink_center = ink.center();
            let target_center = rect.center();
            Point::new(target_center.x - ink_center.x, target_center.y - ink_center.y)
        }
        None => return Ok(()),
    };

    let _ = glyph_font.render_aligned(
        text,
        render_pos,
        VerticalPosition::Top,
        HorizontalAlignment::Left,
        FontColor::Transparent(palette::BRAND_BRIGHT),
        target,
    );

    Ok(())
}

/// Number of bars in [`draw_signal_bars`]'s glyph — fixed at the
/// conventional "signal strength" count (design section 14, F10), not a
/// parameter: a caller wanting a different resolution would also need a
/// different `level` scale, which doesn't exist anywhere in this crate.
const SIGNAL_BAR_COUNT: i32 = 4;
/// Width (px) of a single bar.
const SIGNAL_BAR_WIDTH: i32 = 4;
/// Gap (px) between adjacent bars.
const SIGNAL_BAR_GAP: i32 = 2;
/// Total footprint (px) of the whole glyph — `SIGNAL_BAR_COUNT` bars plus
/// the gaps between them, no gap after the last bar. Exposed so callers
/// (e.g. `list.rs`'s row layout) can reserve exactly this much width
/// without duplicating the arithmetic.
pub const SIGNAL_GLYPH_WIDTH: u32 =
    (SIGNAL_BAR_COUNT * SIGNAL_BAR_WIDTH + (SIGNAL_BAR_COUNT - 1) * SIGNAL_BAR_GAP) as u32;

/// Draws a 4-bar signal-strength glyph, growing left to right and
/// bottom-aligned within `rect` — the conventional "signal bars" shape
/// (design section 14, F10: "4-bar signal glyph...unconditional for the
/// scan list"). This replaces the ASCII `#`/`.` stand-in
/// `wizard.rs`'s `signal_bars` used to build as a list row's plain-text
/// sublabel (pico-link-0r3) — a deliberate stand-in for the design rule's
/// *behavior*, never meant to ship as the pixel form.
///
/// `level` (clamped to `0..=SIGNAL_BAR_COUNT`) is the number of *filled*
/// bars, counted from the left/shortest bar — i.e. the same "more bars
/// lit = stronger signal" reading as every phone/wifi status icon, not a
/// right-to-left or tallest-first count. Filled bars draw in
/// [`palette::BRAND_BRIGHT`] (matching [`draw_icon_chip`]'s icon color, so
/// this reads as the same family of themed glyph); unfilled bars draw in
/// [`palette::DIVIDER`] (the theme's existing "present but inactive" tone,
/// also used for disabled affordances) rather than being left unpainted —
/// an all-bars-drawn glyph is what makes "zero bars" read as "no signal"
/// instead of "nothing rendered here, is this a bug".
///
/// `rect`'s height sets each bar's height step (`rect.size.height *
/// (index + 1) / SIGNAL_BAR_COUNT`); its width is expected to be at least
/// [`SIGNAL_GLYPH_WIDTH`]. A narrower `rect` is not honoured: this
/// function ignores `rect.size.width` entirely and always lays out all
/// `SIGNAL_BAR_COUNT` bars, so the overhanging bars are dropped only if
/// they fall outside the *framebuffer*, which bounds-checks and discards
/// out-of-range pixels rather than panicking. There is no rect-relative
/// clipping.
///
/// # Errors
///
/// Returns `Infallible`'s uninhabited variant in practice — see
/// [`super::widget::Widget::render`]'s doc comment for why the `Result`
/// return exists at all.
pub fn draw_signal_bars<D>(target: &mut D, rect: Rectangle, level: u8) -> Result<(), Infallible>
where
    D: DrawTarget<Color = Rgb565, Error = Infallible>,
{
    let level = i32::from(level.min(SIGNAL_BAR_COUNT as u8));
    let bottom = rect.top_left.y + rect.size.height as i32;

    for i in 0..SIGNAL_BAR_COUNT {
        let bar_height = (rect.size.height as i32 * (i + 1) / SIGNAL_BAR_COUNT).max(1);
        let x = rect.top_left.x + i * (SIGNAL_BAR_WIDTH + SIGNAL_BAR_GAP);
        let y = bottom - bar_height;
        let color = if i < level { palette::BRAND_BRIGHT } else { palette::DIVIDER };
        Rectangle::new(Point::new(x, y), Size::new(SIGNAL_BAR_WIDTH as u32, bar_height as u32))
            .into_styled(PrimitiveStyle::with_fill(color))
            .draw(target)?;
    }

    Ok(())
}

/// Number of segments in one [`draw_level_meter`] channel row — the
/// design's own "6-8 segments per channel" (section 6/21 E17), pinned to
/// the top of that range: 8 divides the 0-255 linear scale evenly (32 per
/// segment) with no remainder bucket to special-case.
const METER_SEGMENT_COUNT: i32 = 8;
/// Width (px) of a single segment.
const METER_SEGMENT_WIDTH: i32 = 6;
/// Gap (px) between adjacent segments.
const METER_SEGMENT_GAP: i32 = 2;
/// Total footprint (px) of one full meter row — exposed for the same
/// "caller can reserve exact width" reason as [`SIGNAL_GLYPH_WIDTH`].
pub const METER_GLYPH_WIDTH: u32 =
    (METER_SEGMENT_COUNT * METER_SEGMENT_WIDTH + (METER_SEGMENT_COUNT - 1) * METER_SEGMENT_GAP) as u32;

/// Which colour segment `index` (0-based, quietest first) draws in when
/// filled — green for the bottom 5/8, amber for the next 2/8, red only
/// for the top 1/8 (design section 6's "OUT meter ... peak-hold and a
/// `STATUS_ERROR` cap" — the cap is a colour a peak can reach, not a
/// separate glyph).
fn level_segment_color(index: i32) -> Rgb565 {
    if index >= METER_SEGMENT_COUNT - 1 {
        palette::STATUS_ERROR
    } else if index >= METER_SEGMENT_COUNT - 3 {
        palette::STATUS_WARNING
    } else {
        palette::TEXT_PRIMARY
    }
}

/// Draws one stereo OUT-meter channel row (design section 6/21 E17/C8,
/// bead pico-link-du0): [`METER_SEGMENT_COUNT`] segments, left to right,
/// bottom-aligned within `rect` — same left-to-right growth convention as
/// [`draw_signal_bars`]. `level` (0-255 linear, 255 == full-scale/
/// clipping) sets how many segments are filled; `hold` (also 0-255) draws
/// a single highlighted segment at the peak-hold cap's position, on top
/// of whichever fill colour would otherwise be there — [`palette::
/// STATUS_ERROR`] if the hold segment is the top (clip) one, [`palette::
/// BRAND_BRIGHT`] otherwise, so the cap always reads as "the peak", never
/// blends into an ordinary filled segment. `hold == 0` draws no cap at
/// all (nothing has peaked yet).
///
/// Every segment draws regardless of fill state (unfilled segments use
/// [`palette::DIVIDER`]) for the same "zero-bars must still read as
/// present, not as nothing rendered" reason [`draw_signal_bars`]'s doc
/// comment gives.
///
/// `rect.size.width` is ignored, same caveat as [`draw_signal_bars`]:
/// this always lays out all `METER_SEGMENT_COUNT` segments starting at
/// `rect.top_left`, relying on the framebuffer's own out-of-range
/// discard rather than rect-relative clipping.
///
/// # Errors
///
/// Returns `Infallible`'s uninhabited variant in practice — see
/// [`super::widget::Widget::render`]'s doc comment for why the `Result`
/// return exists at all.
pub fn draw_level_meter<D>(target: &mut D, rect: Rectangle, level: u8, hold: u8) -> Result<(), Infallible>
where
    D: DrawTarget<Color = Rgb565, Error = Infallible>,
{
    // Round-to-nearest segment count from a 0-255 linear level -- e.g.
    // level=255 (full scale) must fill all 8 segments, not 7 from a floor
    // division that leaves the top segment looking un-driven at max input.
    let filled = (i32::from(level) * METER_SEGMENT_COUNT + 127) / 256;
    // Hold segment index: which segment the peak-hold cap sits on.
    // `hold == 0` is "no peak recorded yet" (a fresh channel with no
    // reading above silence), drawn as no cap rather than a cap pinned to
    // segment 0 -- segment 0 already reads as "quietest filled segment"
    // on its own, so a permanent cap there for genuine silence would be
    // visual noise, not information.
    let hold_index = if hold == 0 { None } else { Some((i32::from(hold) * METER_SEGMENT_COUNT / 256).min(METER_SEGMENT_COUNT - 1)) };

    for i in 0..METER_SEGMENT_COUNT {
        let x = rect.top_left.x + i * (METER_SEGMENT_WIDTH + METER_SEGMENT_GAP);
        let is_hold = hold_index == Some(i);
        let color = if is_hold {
            if i >= METER_SEGMENT_COUNT - 1 { palette::STATUS_ERROR } else { palette::BRAND_BRIGHT }
        } else if i < filled {
            level_segment_color(i)
        } else {
            palette::DIVIDER
        };
        Rectangle::new(Point::new(x, rect.top_left.y), Size::new(METER_SEGMENT_WIDTH as u32, rect.size.height))
            .into_styled(PrimitiveStyle::with_fill(color))
            .draw(target)?;
    }

    Ok(())
}

/// Number of segments in one [`draw_vertical_level_meter`] channel column —
/// `.planning/design/2026-09-03-vertical-out-meter.md` section 5: 16, up
/// from [`METER_SEGMENT_COUNT`]'s 8, doubled again to 32 by bead
/// pico-link-53c once the smoothed damage pass made the meter worth
/// judging at finer resolution. The horizontal meter's 8 segments in a
/// 174px-tall column would read as a crude three-state light; 32 stay
/// individually resolvable at 30-50cm and halve the dBFS step to 1.5dB
/// (see [`VERTICAL_METER_DBFS_THRESHOLDS`]).
const VERTICAL_METER_SEGMENT_COUNT: i32 = 32;
/// Height (px) of a single vertical-meter segment. 32*9 + 31*2 would no
/// longer fit the fixed 174px footprint the 16-segment geometry used, so
/// bead pico-link-53c shrank this (and [`VERTICAL_METER_SEGMENT_GAP`])
/// instead of changing [`VERTICAL_METER_GLYPH_HEIGHT`], which the
/// surrounding layout depends on. 32*4 + 31*1 = 159px, 15px short of the
/// 174px footprint; the drawn column is centred inside it via
/// [`VERTICAL_METER_TOP_PAD`] rather than flush to the top or bottom edge.
const VERTICAL_METER_SEGMENT_HEIGHT: i32 = 4;
/// Gap (px) between adjacent vertical-meter segments — see
/// [`VERTICAL_METER_SEGMENT_HEIGHT`]'s doc comment for how this and the
/// segment height were chosen to fit inside the fixed footprint.
const VERTICAL_METER_SEGMENT_GAP: i32 = 1;
/// Height (px) of the actually-drawn column: 32 segments of
/// [`VERTICAL_METER_SEGMENT_HEIGHT`] separated by 31 gaps of
/// [`VERTICAL_METER_SEGMENT_GAP`]. Strictly less than
/// [`VERTICAL_METER_GLYPH_HEIGHT`] as of bead pico-link-53c — see that
/// constant's doc comment for why the two are no longer equal.
const VERTICAL_METER_DRAWN_HEIGHT: i32 =
    VERTICAL_METER_SEGMENT_COUNT * VERTICAL_METER_SEGMENT_HEIGHT + (VERTICAL_METER_SEGMENT_COUNT - 1) * VERTICAL_METER_SEGMENT_GAP;
/// Total footprint (px) of one full vertical meter column — fixed at 174,
/// exactly the design's meter-block height (section 4). Held fixed rather
/// than recomputed from the segment geometry (as it was before the
/// 16-to-32 segment bump, bead pico-link-53c) because the surrounding
/// layout is built against this exact number; 32 segments do not divide
/// 174 evenly, so [`VERTICAL_METER_DRAWN_HEIGHT`] (159px) is centred inside
/// this footprint instead via [`VERTICAL_METER_TOP_PAD`].
pub const VERTICAL_METER_GLYPH_HEIGHT: u32 = 174;
/// Padding (px) above the drawn column within [`VERTICAL_METER_GLYPH_HEIGHT`]'s
/// footprint: half of the `174 - `[`VERTICAL_METER_DRAWN_HEIGHT`]` = 15`px
/// of slack, so the column sits centred rather than flush to the top edge
/// (bead pico-link-53c). Integer division rounds down, so any odd leftover
/// pixel goes to the bottom pad instead — arbitrary but deterministic.
const VERTICAL_METER_TOP_PAD: i32 = (VERTICAL_METER_GLYPH_HEIGHT as i32 - VERTICAL_METER_DRAWN_HEIGHT) / 2;

/// dBFS threshold, per vertical-meter segment, that `level` (linear 0-255,
/// see [`draw_vertical_level_meter`]'s doc comment) must meet or exceed for
/// that segment to be considered "lit" — bead pico-link-ajj.
///
/// A linear `level*16/256` mapping (the previous behaviour) put typical
/// music RMS (-10..-20 dBFS, i.e. 0.1-0.3 linear) at only 1-3 of 16
/// segments: a level meter reads amplitude on a log scale, not a linear
/// one, so a linear segment count is wrong on any real program material,
/// not just quiet ones.
///
/// `core` is `no_std` with no `libm`, so this is a precomputed const table
/// rather than a `log10` call at render time. Each entry is
/// `round(255 * 10^((-48 + 1.5*(i+1)) / 20))` for `i` in `0..32` — a 1.5
/// dB-per-segment scale (halved from 3dB by bead pico-link-53c, doubling
/// the segment count from 16 to 32) spanning -48 dBFS (segment 1 lit) to
/// 0 dBFS (full scale, all 32 lit). Segment `i`'s threshold is this array's
/// `i`-th entry (0-indexed, quietest/bottom-most first, same convention as
/// [`vertical_level_segment_color`]).
///
/// Several of the lowest entries round to the same `u8` (`1, 1, 2, 2, 2, 3,
/// 3, ...`) — unavoidable once a 1.5dB step is quantised onto a linear 0-255
/// input at the quiet end of the scale, where consecutive dB steps are only
/// a couple of linear units apart. The practical effect is that the bottom
/// few segments light together rather than strictly one at a time; this was
/// called out and accepted when the segment count was doubled, not an
/// oversight.
const VERTICAL_METER_DBFS_THRESHOLDS: [u8; VERTICAL_METER_SEGMENT_COUNT as usize] = [
    1, 1, 2, 2, 2, 3, 3, 4, 5, 6, 7, 8, 10, 11, 14, 16, 19, 23, 27, 32, 38, 45, 54, 64, 76, 90, 108, 128, 152, 181, 215, 255,
];

/// Maps a linear 0-255 level to a segment *count* (0..=32) via
/// [`VERTICAL_METER_DBFS_THRESHOLDS`]: the number of thresholds `level`
/// meets or exceeds. Shared by the moving bar (`filled`) and the peak-hold
/// cap (`hold_index`) in [`draw_vertical_level_meter`] so both read off one
/// scale — see bead pico-link-ajj, which found the previous code split them
/// (log-shaped intent, linear-shaped bar).
///
/// `pub(crate)`, not private: [`super::hero::HeroStatusView::paint_key`]
/// (bead pico-link-7h5.5) needs the exact same quantisation this function
/// applies to the bar, to fold the *segment count* the release ballistic
/// will actually paint rather than the raw continuously-decaying `u8` --
/// exporting this one function keeps that one threshold table the single
/// source of truth instead of a second copy drifting out of sync with it.
pub(crate) fn vertical_level_dbfs_segment_count(level: u8) -> i32 {
    VERTICAL_METER_DBFS_THRESHOLDS.iter().filter(|&&threshold| level >= threshold).count() as i32
}

/// Which colour segment `index` (0-based, quietest/bottom-most first) draws
/// in when filled — design section 5: the colour zones keep the *same
/// proportions* as the horizontal meter (green bottom 5/8, amber next 2/8,
/// red top 1/8), not the literal indices, since porting the indices from
/// [`level_segment_color`] verbatim would halve the red zone at 16
/// segments. At 16 (before bead pico-link-53c doubled the segment count):
/// green bottom 10/16 (indices 0-9), amber next 4/16 (indices 10-13), red
/// top 2/16 (indices 14-15). At 32, the same proportions become green
/// bottom 20/32 (indices 0-19), amber next 8/32 (indices 20-27), red top
/// 4/32 (indices 28-31) — this is exactly the trap this doc comment already
/// warned about: porting the *indices* (`-2`/`-6`) instead of recomputing
/// them from the proportions would have halved the red zone again.
fn vertical_level_segment_color(index: i32) -> Rgb565 {
    if index >= VERTICAL_METER_SEGMENT_COUNT - 4 {
        palette::STATUS_ERROR
    } else if index >= VERTICAL_METER_SEGMENT_COUNT - 12 {
        palette::STATUS_WARNING
    } else {
        palette::TEXT_PRIMARY
    }
}

/// Draws one stereo OUT-meter channel *column* (design section 5, the
/// vertical placement beside the button rail supersedes [`draw_level_meter`]'s
/// horizontal row for this purpose — that function is unchanged and kept
/// for now, but Home no longer calls it): [`VERTICAL_METER_SEGMENT_COUNT`]
/// segments, bottom to top, growing *upward* from `rect`'s bottom edge —
/// the bottom is the datum because that is where the level grows from
/// (design section 4). `level` (0-255 linear) sets how many segments are
/// filled; `hold` (also 0-255) draws a single highlighted segment at the
/// peak-hold cap's position, [`palette::STATUS_ERROR`] if it lands on the
/// top (clip) segment, [`palette::BRAND_BRIGHT`] otherwise. `hold == 0`
/// draws no cap (nothing has peaked yet).
///
/// Every segment draws regardless of fill state (unfilled segments use
/// [`palette::DIVIDER`]) while this reading is live — same "zero must still
/// read as present" reasoning as [`draw_level_meter`]. Callers implement
/// "absent, never frozen" themselves by not calling this function at all
/// once a reading has gone stale (see `hero.rs`'s staleness check) — this
/// function has no concept of staleness and will happily draw a fully
/// unfilled column forever if asked to, which is exactly the frozen-ghost
/// outline the design explicitly forbids leaving on screen.
///
/// `rect.size.width` sets the column's width (the design's 12px channel
/// width); `rect.size.height` is ignored in favor of the fixed
/// [`VERTICAL_METER_GLYPH_HEIGHT`] footprint, same
/// ignore-caller-supplied-extent convention [`draw_level_meter`] and
/// [`draw_signal_bars`] already use.
///
/// # Errors
///
/// Returns `Infallible`'s uninhabited variant in practice — see
/// [`super::widget::Widget::render`]'s doc comment for why the `Result`
/// return exists at all.
pub fn draw_vertical_level_meter<D>(target: &mut D, rect: Rectangle, level: u8, hold: u8) -> Result<(), Infallible>
where
    D: DrawTarget<Color = Rgb565, Error = Infallible>,
{
    // dBFS-scaled segment count (bead pico-link-ajj) -- NOT a linear
    // level*32/256 mapping. See `vertical_level_dbfs_segment_count`'s doc
    // comment for why: linear made typical music RMS light only a handful
    // of segments.
    let filled = vertical_level_dbfs_segment_count(level);
    let hold_index = if hold == 0 {
        None
    } else {
        // Same dBFS scale as `filled`, converted from a 0..=32 count to a
        // 0-based index -- a nonzero hold always shows at least segment 0,
        // and the cap lands on the same segment the bar would if it were
        // at this level (one shared scale, not one log and one linear).
        Some((vertical_level_dbfs_segment_count(hold) - 1).clamp(0, VERTICAL_METER_SEGMENT_COUNT - 1))
    };

    // The drawn column (`VERTICAL_METER_DRAWN_HEIGHT`, 159px) is shorter
    // than the fixed footprint (`VERTICAL_METER_GLYPH_HEIGHT`, 174px) as of
    // bead pico-link-53c, so its bottom edge sits `VERTICAL_METER_TOP_PAD`
    // px up from the footprint's own bottom edge, centring the column
    // rather than anchoring it flush to the bottom.
    let bottom = rect.top_left.y + VERTICAL_METER_TOP_PAD + VERTICAL_METER_DRAWN_HEIGHT;

    for i in 0..VERTICAL_METER_SEGMENT_COUNT {
        // i=0 is the bottom-most (quietest) segment; growth is upward, so
        // higher i means smaller y.
        let y = bottom - (i + 1) * VERTICAL_METER_SEGMENT_HEIGHT - i * VERTICAL_METER_SEGMENT_GAP;
        let is_hold = hold_index == Some(i);
        let color = if is_hold {
            if i >= VERTICAL_METER_SEGMENT_COUNT - 1 { palette::STATUS_ERROR } else { palette::BRAND_BRIGHT }
        } else if i < filled {
            vertical_level_segment_color(i)
        } else {
            palette::DIVIDER
        };
        Rectangle::new(Point::new(rect.top_left.x, y), Size::new(rect.size.width, VERTICAL_METER_SEGMENT_HEIGHT as u32))
            .into_styled(PrimitiveStyle::with_fill(color))
            .draw(target)?;
    }

    Ok(())
}

/// Width, in pixels, of the left accent bar [`draw_selection`] draws.
pub const SELECTION_ACCENT_WIDTH: u32 = 4;

/// Draws the shared "this is the selected/focused thing" visual across
/// `area`: a full-`area`, edge-to-edge fill in [`palette::SURFACE_ELEVATED`],
/// then a [`SELECTION_ACCENT_WIDTH`]-px accent bar in
/// [`palette::BRAND_BRIGHT`] along `area`'s left edge.
///
/// Replaces `render::widget::draw_focus_block` (retired in favor of this):
/// same shape (full-area fill + left accent bar, no inset/rounding/fake
/// elevation), but the colors are no longer caller-supplied parameters —
/// they're always the theme's selection colors, so every selected row or
/// focused field in the app looks identical by construction instead of by
/// convention. Per Andreas's explicit direction, this draws *only* the
/// fill + accent bar; a caret (see [`icon::CARET_RIGHT`]) or any other
/// per-row/per-field decoration is the caller's job, drawn on top of (or
/// clipped within) the same `area` after this returns.
///
/// Generic over `D: DrawTarget<Color = Rgb565, Error = Infallible>`
/// (rather than the concrete `FrameBuffer565`) so a caller drawing into a
/// `DrawTargetExt::clipped()` sub-region can pass that clipped target
/// directly — same rationale as the function this replaces.
///
/// # Errors
///
/// Returns `Infallible`'s uninhabited variant in practice — see
/// [`super::widget::Widget::render`]'s doc comment for why the `Result`
/// return exists at all.
pub fn draw_selection<D>(area: Rectangle, target: &mut D) -> Result<(), Infallible>
where
    D: DrawTarget<Color = Rgb565, Error = Infallible>,
{
    area.into_styled(PrimitiveStyle::with_fill(palette::SURFACE_ELEVATED)).draw(target)?;

    let accent_width = SELECTION_ACCENT_WIDTH.min(area.size.width);
    let accent = Rectangle::new(area.top_left, Size::new(accent_width, area.size.height));
    accent.into_styled(PrimitiveStyle::with_fill(palette::BRAND_BRIGHT)).draw(target)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::FrameBuffer565;

    #[test]
    fn draw_selection_fills_the_full_area_and_paints_a_left_accent_bar() {
        let mut fb = FrameBuffer565::new(40, 20);
        let area = Rectangle::new(Point::new(0, 0), Size::new(40, 20));
        draw_selection(area, &mut fb).unwrap();

        // Left edge: the accent bar.
        assert_eq!(fb.pixel(Point::new(0, 10)), palette::BRAND_BRIGHT);
        assert_eq!(
            fb.pixel(Point::new(SELECTION_ACCENT_WIDTH as i32 - 1, 10)),
            palette::BRAND_BRIGHT
        );
        // Just past the accent bar, and the far right edge: the fill —
        // full width, no inset.
        assert_eq!(fb.pixel(Point::new(SELECTION_ACCENT_WIDTH as i32, 10)), palette::SURFACE_ELEVATED);
        assert_eq!(fb.pixel(Point::new(39, 10)), palette::SURFACE_ELEVATED);
        // Full height, no vertical inset either.
        assert_eq!(fb.pixel(Point::new(20, 0)), palette::SURFACE_ELEVATED);
        assert_eq!(fb.pixel(Point::new(20, 19)), palette::SURFACE_ELEVATED);
    }

    #[test]
    fn draw_selection_clamps_the_accent_bar_to_a_narrower_area() {
        let mut fb = FrameBuffer565::new(2, 10);
        let area = Rectangle::new(Point::new(0, 0), Size::new(2, 10));
        // Must not panic even though `area` is narrower than
        // `SELECTION_ACCENT_WIDTH`.
        draw_selection(area, &mut fb).unwrap();
        assert_eq!(fb.pixel(Point::new(0, 5)), palette::BRAND_BRIGHT);
        assert_eq!(fb.pixel(Point::new(1, 5)), palette::BRAND_BRIGHT);
    }

    #[test]
    fn draw_chip_paints_brand_background_and_a_centered_glyph() {
        let mut fb = FrameBuffer565::new(30, 30);
        let rect = Rectangle::new(Point::new(4, 4), Size::new(22, 22));
        draw_chip(&mut fb, rect, 'G').unwrap();

        // A corner (outside the rounded radius, outside the glyph) is the
        // brand fill.
        assert_eq!(fb.pixel(Point::new(5, 5)), palette::BRAND);
        // Somewhere in the chip drew primary-text-colored ink (the glyph)
        // — proves a glyph was actually rasterized, not just the
        // background.
        let any_glyph_ink =
            (rect.top_left.x..rect.top_left.x + rect.size.width as i32).any(|x| {
                (rect.top_left.y..rect.top_left.y + rect.size.height as i32)
                    .any(|y| fb.pixel(Point::new(x, y)) == palette::TEXT_PRIMARY)
            });
        assert!(any_glyph_ink, "draw_chip should have rasterized the initial's glyph ink");
    }

    #[test]
    fn draw_chip_centers_the_glyphs_ink_bounding_box_on_the_rects_center() {
        // Render 'G' via draw_chip, then independently recompute where
        // font::name()'s *ink bounding box* for "G" landed, and assert its
        // center matches rect.center() exactly. This is the specific
        // Andreas-flagged bug (naive Center/Center alignment centers on
        // font line-metrics, not glyph ink) this primitive exists to fix.
        let mut fb = FrameBuffer565::new(30, 30);
        let rect = Rectangle::new(Point::new(4, 4), Size::new(22, 22));
        draw_chip(&mut fb, rect, 'G').unwrap();

        let mut min = Point::new(i32::MAX, i32::MAX);
        let mut max = Point::new(i32::MIN, i32::MIN);
        let mut found_any = false;
        for x in rect.top_left.x..rect.top_left.x + rect.size.width as i32 {
            for y in rect.top_left.y..rect.top_left.y + rect.size.height as i32 {
                if fb.pixel(Point::new(x, y)) == palette::TEXT_PRIMARY {
                    found_any = true;
                    min.x = min.x.min(x);
                    min.y = min.y.min(y);
                    max.x = max.x.max(x);
                    max.y = max.y.max(y);
                }
            }
        }
        assert!(found_any);
        let ink_center = Point::new((min.x + max.x) / 2, (min.y + max.y) / 2);
        let rect_center = rect.center();
        // Within 1px on each axis: the ink box's own width/height parity
        // (odd vs even pixel count) can make an exact-integer center land
        // a half-pixel either side of rect_center depending on rounding.
        assert!(
            (ink_center.x - rect_center.x).abs() <= 1,
            "chip glyph should be horizontally centered: ink_center={ink_center:?} rect_center={rect_center:?}"
        );
        assert!(
            (ink_center.y - rect_center.y).abs() <= 1,
            "chip glyph should be vertically centered: ink_center={ink_center:?} rect_center={rect_center:?}"
        );
    }

    /// Samples the top-center pixel of each of the four bar columns in a
    /// `draw_signal_bars` glyph, for a `rect` tall enough that even the
    /// shortest (leftmost) bar's top row is distinguishable from the
    /// background -- the fill color at that single point is enough to
    /// tell filled from unfilled without re-deriving the per-bar height
    /// arithmetic here.
    fn sample_bar_tops(fb: &FrameBuffer565, rect: Rectangle) -> [Rgb565; 4] {
        let mut colors = [palette::BACKGROUND; 4];
        for (i, color) in colors.iter_mut().enumerate() {
            let bar_height = rect.size.height as i32 * (i as i32 + 1) / SIGNAL_BAR_COUNT;
            let x = rect.top_left.x + i as i32 * (SIGNAL_BAR_WIDTH + SIGNAL_BAR_GAP);
            let y = rect.top_left.y + rect.size.height as i32 - bar_height;
            *color = fb.pixel(Point::new(x, y));
        }
        colors
    }

    #[test]
    fn draw_signal_bars_zero_level_leaves_every_bar_unfilled() {
        let mut fb = FrameBuffer565::new(30, 20);
        let rect = Rectangle::new(Point::new(0, 0), Size::new(SIGNAL_GLYPH_WIDTH, 16));
        draw_signal_bars(&mut fb, rect, 0).unwrap();
        assert_eq!(sample_bar_tops(&fb, rect), [palette::DIVIDER; 4]);
    }

    #[test]
    fn draw_signal_bars_full_level_fills_every_bar() {
        let mut fb = FrameBuffer565::new(30, 20);
        let rect = Rectangle::new(Point::new(0, 0), Size::new(SIGNAL_GLYPH_WIDTH, 16));
        draw_signal_bars(&mut fb, rect, 4).unwrap();
        assert_eq!(sample_bar_tops(&fb, rect), [palette::BRAND_BRIGHT; 4]);
    }

    #[test]
    fn draw_signal_bars_partial_level_fills_only_the_low_bars() {
        let mut fb = FrameBuffer565::new(30, 20);
        let rect = Rectangle::new(Point::new(0, 0), Size::new(SIGNAL_GLYPH_WIDTH, 16));
        draw_signal_bars(&mut fb, rect, 2).unwrap();
        assert_eq!(
            sample_bar_tops(&fb, rect),
            [palette::BRAND_BRIGHT, palette::BRAND_BRIGHT, palette::DIVIDER, palette::DIVIDER]
        );
    }

    #[test]
    fn draw_signal_bars_clamps_a_level_above_the_bar_count() {
        let mut fb = FrameBuffer565::new(30, 20);
        let rect = Rectangle::new(Point::new(0, 0), Size::new(SIGNAL_GLYPH_WIDTH, 16));
        // Must not panic on an out-of-range level -- `signal_bars`'
        // caller derives this from raw RSSI, not a value this module
        // controls.
        draw_signal_bars(&mut fb, rect, 200).unwrap();
        assert_eq!(sample_bar_tops(&fb, rect), [palette::BRAND_BRIGHT; 4]);
    }

    // --- draw_vertical_level_meter: bottom-up growth, proportional colour
    // zones (design section 5 -- porting the horizontal meter's literal
    // indices instead of its proportions would halve the red zone) -------

    fn vertical_meter_segment_color_at(fb: &FrameBuffer565, rect: Rectangle, index_from_bottom: i32) -> Rgb565 {
        let bottom = rect.top_left.y + VERTICAL_METER_TOP_PAD + VERTICAL_METER_DRAWN_HEIGHT;
        let y = bottom
            - (index_from_bottom + 1) * VERTICAL_METER_SEGMENT_HEIGHT
            - index_from_bottom * VERTICAL_METER_SEGMENT_GAP
            + 1;
        fb.pixel(Point::new(rect.top_left.x + 1, y))
    }

    #[test]
    fn vertical_meter_glyph_height_is_174() {
        assert_eq!(VERTICAL_METER_GLYPH_HEIGHT, 174);
    }

    #[test]
    fn draw_vertical_level_meter_zero_level_draws_every_segment_unfilled() {
        let mut fb = FrameBuffer565::new(20, 180);
        let rect = Rectangle::new(Point::new(0, 0), Size::new(12, VERTICAL_METER_GLYPH_HEIGHT));
        draw_vertical_level_meter(&mut fb, rect, 0, 0).unwrap();
        for i in 0..VERTICAL_METER_SEGMENT_COUNT {
            assert_eq!(
                vertical_meter_segment_color_at(&fb, rect, i),
                palette::DIVIDER,
                "segment {i} from the bottom should be unfilled DIVIDER at level=0"
            );
        }
    }

    #[test]
    fn draw_vertical_level_meter_full_scale_fills_bottom_up_with_proportional_colour_zones() {
        let mut fb = FrameBuffer565::new(20, 180);
        let rect = Rectangle::new(Point::new(0, 0), Size::new(12, VERTICAL_METER_GLYPH_HEIGHT));
        draw_vertical_level_meter(&mut fb, rect, 255, 0).unwrap();
        // Bottom 20/32 segments (indices 0-19) are the green zone -- same
        // 5/8 proportion as the 16-segment geometry, doubled by bead
        // pico-link-53c.
        for i in 0..20 {
            assert_eq!(
                vertical_meter_segment_color_at(&fb, rect, i),
                palette::TEXT_PRIMARY,
                "segment {i} from the bottom must be in the green (TEXT_PRIMARY) zone"
            );
        }
        // Next 8/32 (indices 20-27) are the amber zone.
        for i in 20..28 {
            assert_eq!(
                vertical_meter_segment_color_at(&fb, rect, i),
                palette::STATUS_WARNING,
                "segment {i} from the bottom must be in the amber (STATUS_WARNING) zone"
            );
        }
        // Top 4/32 (indices 28-31) are the red zone -- porting the
        // horizontal meter's literal indices (top 1/8) instead of the
        // proportion would wrongly leave index 28 amber.
        for i in 28..32 {
            assert_eq!(
                vertical_meter_segment_color_at(&fb, rect, i),
                palette::STATUS_ERROR,
                "segment {i} from the bottom must be in the red (STATUS_ERROR) zone"
            );
        }
    }

    #[test]
    fn draw_vertical_level_meter_half_linear_scale_fills_28_of_32_on_the_dbfs_scale() {
        // 128/255 linear is -6 dBFS, not -infinity-to-0's midpoint -- on a
        // log scale that is loud, not "half". This replaces a pre-pico-
        // link-ajj test that expected a linear split; the dBFS mapping is
        // the point of this bead, so the old expectation would be testing
        // the bug. 28/32 (bead pico-link-53c's 1.5dB-per-segment scale)
        // lands -6 dBFS's 28-segment fill exactly at the green/amber/red
        // boundary the top 4-segment red zone starts at.
        let mut fb = FrameBuffer565::new(20, 180);
        let rect = Rectangle::new(Point::new(0, 0), Size::new(12, VERTICAL_METER_GLYPH_HEIGHT));
        draw_vertical_level_meter(&mut fb, rect, 128, 0).unwrap();
        for i in 0..28 {
            assert_ne!(
                vertical_meter_segment_color_at(&fb, rect, i),
                palette::DIVIDER,
                "segment {i} from the bottom should be filled at level=128 (-6 dBFS)"
            );
        }
        for i in 28..32 {
            assert_eq!(
                vertical_meter_segment_color_at(&fb, rect, i),
                palette::DIVIDER,
                "segment {i} from the bottom should still be unfilled at level=128 (-6 dBFS)"
            );
        }
    }

    #[test]
    fn vertical_level_dbfs_segment_count_full_scale_fills_32() {
        assert_eq!(vertical_level_dbfs_segment_count(255), 32);
    }

    #[test]
    fn vertical_level_dbfs_segment_count_silence_fills_0() {
        assert_eq!(vertical_level_dbfs_segment_count(0), 0);
    }

    #[test]
    fn vertical_level_dbfs_segment_count_typical_music_rms_lands_around_segment_18() {
        // 0.1 linear RMS (-20 dBFS) is squarely in typical-music territory
        // (bead pico-link-ajj) -- the linear mapping this replaces put it
        // at only a handful of segments; a dBFS mapping must land it well
        // up the column instead. At the 1.5dB-per-segment, 32-segment scale
        // (bead pico-link-53c doubled from 16 at 3dB/segment), -20 dBFS is
        // `(-20 - -48) / 1.5 = 18.67` segments up from the floor.
        let filled = vertical_level_dbfs_segment_count(26); // round(0.1 * 255)
        assert!((17..=19).contains(&filled), "expected ~segment 18, got {filled}");
    }

    #[test]
    fn draw_vertical_level_meter_hold_zero_draws_no_peak_cap() {
        // hold == 0 means "no peak recorded yet" -- same convention as
        // draw_level_meter -- so no segment should render BRAND_BRIGHT.
        let mut fb = FrameBuffer565::new(20, 180);
        let rect = Rectangle::new(Point::new(0, 0), Size::new(12, VERTICAL_METER_GLYPH_HEIGHT));
        draw_vertical_level_meter(&mut fb, rect, 0, 0).unwrap();
        assert!(!fb.pixels().any(|p| p.1 == palette::BRAND_BRIGHT));
    }

    #[test]
    fn draw_vertical_level_meter_hold_draws_a_peak_cap() {
        let mut fb = FrameBuffer565::new(20, 180);
        let rect = Rectangle::new(Point::new(0, 0), Size::new(12, VERTICAL_METER_GLYPH_HEIGHT));
        draw_vertical_level_meter(&mut fb, rect, 0, 128).unwrap();
        assert!(fb.pixels().any(|p| p.1 == palette::BRAND_BRIGHT), "a non-zero hold must draw a peak-hold cap");
    }
}
