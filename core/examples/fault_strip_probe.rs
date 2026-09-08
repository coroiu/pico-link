//! Verifiable deliverable for the Home fault strip (design
//! `.planning/design/2026-09-07-home-fault-strip.md`, bead
//! `pico-link-9eq2.3.3`):
//!
//! 1. Measures `theme::font::label()` (`helvB08`)'s real `line_height`
//!    against `FAULT_ROW_HEIGHT`'s assumed 12px, and the real pixel width
//!    of every `FaultKey::name()` (uppercase, the actual strings shipped —
//!    not Uma's 6px/char estimate) against the ~105px name-field budget
//!    (design §4/§12 Fern item 7: "confirm ... do not truncate silently").
//! 2. Renders `HeroStatusView` for the fixture set the bead's report
//!    requires, on the real 240x240 `FrameBuffer565` inside the real
//!    chrome content area (same scaffolding as `hero_widget_probe.rs`),
//!    dumping a 4x-upscaled PNG per state for zoomed inspection — 1x
//!    inspection is explicitly insufficient on this project (CLAUDE.md's
//!    rendering-verification rule; this project has shipped both a
//!    sub-row text overflow and a fully blank window that a 1x check
//!    missed).
//! 3. Renders a swatch strip comparing `STATUS_ERROR_DIM`,
//!    `STATUS_WARNING_DIM` and `TEXT_SECONDARY` side by side at 8px text
//!    size, for the "genuinely distinguishable" claim design §12 Fern
//!    item 3 requires be verified on a zoomed capture rather than
//!    asserted from hex values.
//!
//! Run with: `cargo run -p pico-link-core --example fault_strip_probe`
//! Writes `fault_strip_probe_*.png` (4x nearest-neighbor upscaled) to the
//! current directory.

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::{Point, Primitive, RgbColor, Size};
use embedded_graphics::primitives::{PrimitiveStyleBuilder, Rectangle};
use embedded_graphics::Drawable;
use pico_link_core::app::{FaultKey, FaultLog};
use pico_link_core::platform::Instant;
use pico_link_core::render::hero::{BitrateStatus, CodecStatus, HeroStatusView};
use pico_link_core::render::theme::{font, palette};
use pico_link_core::render::{compute_chrome, FrameBuffer565, RenderCtx, Widget};
use u8g2_fonts::types::{HorizontalAlignment, VerticalPosition};

const SCALE: u32 = 4;

fn dump_png_scaled(framebuffer: &FrameBuffer565, path: &str, scale: u32) {
    let width = framebuffer.width();
    let height = framebuffer.height();
    let mut image = image::RgbImage::new(width, height);
    for pixel in framebuffer.pixels() {
        let point = pixel.0;
        let color: Rgb565 = pixel.1;
        #[allow(clippy::cast_sign_loss)]
        image.put_pixel(point.x as u32, point.y as u32, image::Rgb([color.r() << 3, color.g() << 2, color.b() << 3]));
    }
    let scaled = image::imageops::resize(&image, width * scale, height * scale, image::imageops::FilterType::Nearest);
    scaled.save(path).expect("failed to write PNG");
}

fn render_state(name: &str, view: &HeroStatusView, now_us: u64) {
    let mut fb = FrameBuffer565::new(240, 240);
    let chrome = compute_chrome(Size::new(240, 240));
    Rectangle::new(Point::zero(), Size::new(240, 240))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(palette::BACKGROUND).build())
        .draw(&mut fb)
        .expect("core DrawTarget is Infallible");

    let ctx = RenderCtx::at(Instant::from_micros(now_us));
    view.render(chrome.content, &ctx, &mut fb).expect("core DrawTarget is Infallible");

    let path = format!("fault_strip_probe_{name}.png");
    dump_png_scaled(&fb, &path, SCALE);
    println!("wrote {path} ({}x{} at {SCALE}x)", fb.width() * SCALE, fb.height() * SCALE);
}

fn nominal_hero() -> HeroStatusView {
    HeroStatusView::new(
        "Sony WH-1000XM5",
        CodecStatus::Connected { word: "LDAC".into(), fallback: None, bitrate: BitrateStatus::Kbps { kbps: 909, adaptive: false } },
    )
    .with_stat_line("USB 48k 24-bit")
}

fn measure_fonts() {
    let label_font = font::label();
    let probe_bbox = label_font
        .get_rendered_dimensions_aligned("Agjpqy", Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
        .expect("ASCII text must measure")
        .expect("non-empty text has a bounding box");
    println!("font::label() worst-case line_height = {}px (FAULT_ROW_HEIGHT assumes 12px)", probe_bbox.size.height);

    println!("Fault key name widths in font::label(), uppercase (name field budget ~105px, abs x24..133):");
    let mut worst = 0u32;
    for key in FaultKey::ALL {
        let name = key.name();
        let bbox = label_font
            .get_rendered_dimensions_aligned(name, Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
            .expect("ASCII text must measure")
            .expect("non-empty text has a bounding box");
        println!("  {name:>16} ({:>2} chars): {}px", name.len(), bbox.size.width);
        worst = worst.max(bbox.size.width);
    }
    println!("  worst-case name width = {worst}px");
    println!(
        "  16-char cap check: {}",
        if worst <= 105 { "FITS within the ~105px budget" } else { "OVERFLOWS the ~105px budget -- report the real cap, do not truncate" }
    );

    // "+3 MORE - PRESS X" is the widest overflow string (max 6 keys, cap 3
    // shown -> at most 3 overflow).
    let overflow_text = "+3 MORE - PRESS X";
    let bbox = label_font
        .get_rendered_dimensions_aligned(overflow_text, Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
        .expect("ASCII text must measure")
        .expect("non-empty text has a bounding box");
    println!("overflow text {overflow_text:?}: {}px (name+count budget is 145-24=121px)", bbox.size.width);
}

fn render_dim_swatch() {
    let mut fb = FrameBuffer565::new(240, 80);
    Rectangle::new(Point::zero(), Size::new(240, 80))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(palette::BACKGROUND).build())
        .draw(&mut fb)
        .expect("core DrawTarget is Infallible");

    let label_font = font::label();
    let rows: [(&str, Rgb565); 5] = [
        ("STATUS_ERROR (bright)", palette::STATUS_ERROR),
        ("STATUS_ERROR_DIM", palette::STATUS_ERROR_DIM),
        ("STATUS_WARNING (bright)", palette::STATUS_WARNING),
        ("STATUS_WARNING_DIM", palette::STATUS_WARNING_DIM),
        ("TEXT_SECONDARY", palette::TEXT_SECONDARY),
    ];
    for (i, (label, color)) in rows.iter().enumerate() {
        let y = 4 + i as i32 * 15;
        let _ = label_font.render_aligned(
            *label,
            Point::new(4, y),
            VerticalPosition::Top,
            HorizontalAlignment::Left,
            u8g2_fonts::types::FontColor::Transparent(*color),
            &mut fb,
        );
    }
    dump_png_scaled(&fb, "fault_strip_probe_dim_swatch.png", 6);
    println!("wrote fault_strip_probe_dim_swatch.png (6x)");
}

fn measure_why_page_consequence_texts() {
    let value_font = font::value(); // the why? page's readonly-row font (RowStyle::FIELD)
    let budget: i32 = 206 - 12; // row width - left_margin, no value/caret on these rows
    println!("why? page line-3 consequence texts (font::value(), budget ~{budget}px, no ellipsis -- FieldList clips, not truncates):");
    // These strings must match app.rs's `fault_consequence_text` exactly --
    // duplicated here (not calling the private fn) purely to measure width
    // against the row budget before shipping the wording.
    let samples = ["ring dry, min fill 0ms", "ring full, 140 dropped", "supply 0.62x nominal", "air busy, x3 deferred", "link dropped x2", "trim dropped x22"];
    for sample in samples {
        let w = value_font
            .get_rendered_dimensions_aligned(sample, Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
            .unwrap()
            .unwrap()
            .size
            .width;
        println!("  {sample:?}: {w}px -- {}", if (w as i32) <= budget { "FITS" } else { "OVERFLOWS, will be hard-clipped" });
    }
}

fn main() {
    measure_fonts();
    measure_why_page_consequence_texts();
    render_dim_swatch();

    let now = 0u64;

    // 1. Empty -- must be pixel-identical to today's Home (compare against
    // hero_widget_probe_1_connected_nominal.png, same nominal state, no
    // fault_log call at all).
    render_state("1_empty", &nominal_hero(), now);

    // 2. One live Filled row (BufOverflow -- up triangle, red, bright).
    let mut log = FaultLog::default();
    log.record(FaultKey::BufOverflow, Instant::from_micros(now), None, 1);
    render_state("2_live_filled", &nominal_hero().with_fault_log(log), now);

    // 3. One live Starved row (BufStarved -- down triangle, red, bright).
    let mut log = FaultLog::default();
    log.record(FaultKey::BufStarved, Instant::from_micros(now), None, 1);
    render_state("3_live_starved", &nominal_hero().with_fault_log(log), now);

    // 4. Three keys spanning both directions and both tiers: BufStarved
    // Recent (down, dim red, oldest/bottom row), AirCongested Live
    // (square, amber bright), BufOverflow Live (up, red bright,
    // newest/top row).
    let live_window = pico_link_core::run::FAULT_LIVE_WINDOW.as_micros() as u64;
    let render_now_4 = 30_000_000u64;
    let mut log = FaultLog::default();
    log.record(FaultKey::BufStarved, Instant::from_micros(0), None, 9); // 30s ago -> Recent
    log.record(FaultKey::AirCongested, Instant::from_micros(render_now_4 - 5_000_000), None, 3); // 5s ago -> Live
    log.record(FaultKey::BufOverflow, Instant::from_micros(render_now_4 - 2_000_000), None, 14); // 2s ago -> Live
    render_state("4_three_keys_mixed", &nominal_hero().with_fault_log(log), render_now_4);

    // 5. Overflow at 5+ keys -- rows 1-3 (oldest three, first-seen order)
    // plus a "+2 MORE - PRESS X" row.
    let mut log = FaultLog::default();
    log.record(FaultKey::EncResync, Instant::from_micros(1_000_000), None, 22);
    log.record(FaultKey::BufOverflow, Instant::from_micros(2_000_000), None, 14);
    log.record(FaultKey::AirCongested, Instant::from_micros(3_000_000), None, 3);
    log.record(FaultKey::AirLinkLost, Instant::from_micros(4_000_000), None, 2);
    log.record(FaultKey::UsbSupplyLow, Instant::from_micros(5_000_000), None, 5);
    render_state("5_overflow_five_keys", &nominal_hero().with_fault_log(log), 5_000_000);

    // 6. Banner + full 4-row strip + stat line, all present together.
    let mut log = FaultLog::default();
    log.record(FaultKey::EncResync, Instant::from_micros(1_000_000), None, 22);
    log.record(FaultKey::BufOverflow, Instant::from_micros(2_000_000), None, 14);
    log.record(FaultKey::AirCongested, Instant::from_micros(3_000_000), None, 3);
    log.record(FaultKey::AirLinkLost, Instant::from_micros(4_000_000), None, 1); // x1 must render as nothing
    let banner_view = HeroStatusView::new(
        "Sony WH-1000XM5",
        CodecStatus::Connected {
            word: "SBC".into(),
            fallback: Some("Headphones don't support LDAC".into()),
            bitrate: BitrateStatus::Kbps { kbps: 328, adaptive: false },
        },
    )
    .with_stat_line("USB 48k 24-bit")
    .with_fault_log(log);
    render_state("6_banner_full_strip_stat_line", &banner_view, 4_000_000);

    // 7. A key transitioning Live -> Recent -> Retired, three frames.
    let mut log = FaultLog::default();
    log.record(FaultKey::BufStarved, Instant::from_micros(0), None, 1);
    render_state("7a_live", &nominal_hero().with_fault_log(log), 0);
    let recent_at = live_window + 5_000_000; // well inside 20..120s Recent
    render_state("7b_recent", &nominal_hero().with_fault_log(log), recent_at);
    let retired_at = pico_link_core::run::FAULT_RETIRE.as_micros() as u64 + 5_000_000;
    render_state("7c_retired", &nominal_hero().with_fault_log(log), retired_at);

    render_why_page_with_scroll();
}

/// The `why?` page fixture (design §10.7), via the real `App` +
/// `Navigator` round trip (`ShortcutX` on Home), scrolled -- not the
/// `HeroStatusView` in isolation like the fixtures above, since the page
/// is a separate `Screen`, not part of the hero composite.
fn render_why_page_with_scroll() {
    use pico_link_core::app::{App, FaultValue};
    use pico_link_core::input::NavIntent;

    let mut app = App::new(240, 240);
    app.tick(0);
    // All 6 keys, so the page has more blocks than fit on screen at once.
    app.on_fault_raised(FaultKey::BufStarved, Some(FaultValue::Millis(0)), 9);
    app.on_fault_raised(FaultKey::BufOverflow, Some(FaultValue::Count(14)), 14);
    app.on_fault_raised(FaultKey::UsbSupplyLow, Some(FaultValue::Ratio(159)), 5);
    app.on_fault_raised(FaultKey::AirCongested, Some(FaultValue::Count(3)), 3);
    app.on_fault_raised(FaultKey::AirLinkLost, None, 2);
    app.on_fault_raised(FaultKey::EncResync, Some(FaultValue::Count(22)), 22);
    app.tick(1_000_000);

    app.handle_input(vec![NavIntent::ShortcutX]); // Home -> why? page
    app.handle_input(vec![NavIntent::Down, NavIntent::Down, NavIntent::Down, NavIntent::Down]); // scroll down

    let output = app.render();
    let path = "fault_strip_probe_8_why_page_scrolled.png";
    dump_png_scaled(&output, path, SCALE);
    println!("wrote {path} ({}x{} at {SCALE}x)", output.width() * SCALE, output.height() * SCALE);
}
