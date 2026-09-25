//! Bead `pico-link-t26` step 1: measures where the ~25ms
//! `PL_LOOP_PHASE_UI_RENDER` figure (bead `pico-link-7h5`) actually goes,
//! *before* touching `FrameBuffer565`'s `DrawTarget` impl. The bead's own
//! arithmetic (~65 cycles/pixel implied by that figure) is a hypothesis
//! from reading the fallback code path, not a measurement -- this example
//! is the measurement.
//!
//! Three things are timed, host-side, release build:
//!
//! 1. A full, forced-full-damage production render of the busiest real
//!    screen we have (`App`'s Home status face with a live LDAC link and
//!    a running stereo OUT level meter -- `home_fixtures::generate`'s
//!    `04_connected_with_out_level` scenario) -- the actual end-to-end
//!    number this bead's fix is trying to move.
//! 2. A synthetic rectangle-fill-only workload sized to roughly the same
//!    total fill area a full Home repaint pushes through `fill_solid`
//!    (full-screen background clear plus the meter/rail/divider rects) --
//!    isolates the `draw_iter` fallback this bead's fix targets.
//! 3. A synthetic text-only workload using the same `u8g2-fonts` faces and
//!    representative strings the real screen draws (title, hero codec
//!    word, hero bitrate line, rail glyphs) -- isolates glyph
//!    rasterization, which this bead's fix does NOT change (glyphs stay
//!    on `draw_iter`; only solid/contiguous rectangle fills move off it).
//!
//! Run with: `cargo run --release -p pico-link-core --example fill_bench`
//!
//! Report reads: full render ns/frame, rects-only ns/frame, text-only
//! ns/frame, and how (2)+(3) compares to (1) (the remainder is layout,
//! widget-tree walk, and any overdraw not captured by either synthetic
//! workload).

use std::time::Instant as StdInstant;

use embedded_graphics::draw_target::DrawTarget;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::{Point, Primitive, RgbColor, Size, WebColors};
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use embedded_graphics::Drawable;
use pico_link_core::{App, ConnectedCodec, Event, LinkState, PairedDevice};
use u8g2_fonts::fonts;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::FontRenderer;

const ITERATIONS: u32 = 500;

fn main() {
    println!("iterations per measurement: {ITERATIONS}");
    println!();

    bench_full_render();
    println!();
    bench_rects_only();
    println!();
    bench_text_only();
}

/// Times `App::render()` on the busiest real Home scenario, forcing full
/// damage every frame via `mark_dirty()` so the damage-rect pass
/// (`pico-link-7h5.4`) never gets to skip pixels -- this measures the same
/// "repaint everything" cost the original ~25ms figure was measured
/// against, not whatever a smaller damage rect would cost today.
fn bench_full_render() {
    let mut app = App::new(240, 240);
    let addr = [0xDD; 6];
    app.handle_event(Event::PairedDeviceUpserted(PairedDevice {
        addr,
        name: String::from("Sony WH-1000XM5"),
        mru_seq: 1,
        ldac_quality: 0,
        preset_id: 0,
    }));
    app.handle_event(Event::LinkStateChanged(LinkState::Connected));
    app.handle_event(Event::CodecChanged(ConnectedCodec {
        addr,
        word: String::from("LDAC"),
        nominal_bitrate_bps: 990_000,
    }));
    app.tick(1);
    app.handle_event(Event::LevelsChanged { peak_l: 90, peak_r: 45, rms_l: 26, rms_r: 40 });

    // Warm up (first render allocates/caches inside the widget tree).
    app.mark_dirty();
    let _ = app.render();

    let start = StdInstant::now();
    for _ in 0..ITERATIONS {
        app.mark_dirty();
        let _ = app.render();
    }
    let elapsed = start.elapsed();
    report("1. FULL RENDER (App::render, Home + live LDAC + OUT meter, forced full damage)", elapsed, ITERATIONS);
}

/// Synthetic rectangle-only workload: a full-screen background clear plus
/// a handful of smaller rects (list rows, meter segments, dividers,
/// selection blocks) roughly matching the fill area a real Home repaint
/// pushes through `fill_solid`/`clear`. Every one of these currently goes
/// through `FrameBuffer565`'s inherited `draw_iter` fallback -- this is
/// exactly what step 2 of the bead replaces with row-wise slice writes.
fn bench_rects_only() {
    use pico_link_core::render::FrameBuffer565;

    let mut fb = FrameBuffer565::new(240, 240);

    let start = StdInstant::now();
    for _ in 0..ITERATIONS {
        // Full-screen background clear -- the single largest fill any
        // Home repaint does.
        fb.clear(Rgb565::BLACK).unwrap();

        // A representative handful of smaller rects: two 16-segment
        // meter columns (roughly 8x12 each), a button rail background,
        // a divider line, and a selection block -- sized off `hero.rs`/
        // `rail.rs`'s real geometry, not exact pixel-for-pixel, but the
        // same order of magnitude and count.
        for col in 0..2 {
            for seg in 0..16 {
                Rectangle::new(Point::new(200 + col * 12, 20 + seg * 13), Size::new(8, 12))
                    .into_styled(PrimitiveStyle::with_fill(Rgb565::GREEN))
                    .draw(&mut fb)
                    .unwrap();
            }
        }
        Rectangle::new(Point::new(190, 0), Size::new(50, 240))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::CSS_DARK_SLATE_GRAY))
            .draw(&mut fb)
            .unwrap();
        Rectangle::new(Point::new(0, 40), Size::new(190, 2))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::CSS_DIM_GRAY))
            .draw(&mut fb)
            .unwrap();
        Rectangle::new(Point::new(10, 200), Size::new(170, 32))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::CSS_MIDNIGHT_BLUE))
            .draw(&mut fb)
            .unwrap();
    }
    let elapsed = start.elapsed();
    report("2. RECTS ONLY (full-screen clear + meter/rail/divider/selection rects)", elapsed, ITERATIONS);
}

/// Synthetic text-only workload: the same `u8g2-fonts` faces and roughly
/// the same strings the real Home screen draws (title, hero codec word,
/// hero bitrate line, a rail glyph, a list row's name/username pair).
/// This is genuine per-pixel rasterization work (glyph bitmaps aren't
/// solid rects) and is NOT changed by this bead's fix -- it's measured
/// here purely so the full-render number in (1) can be attributed.
fn bench_text_only() {
    use pico_link_core::render::FrameBuffer565;

    let mut fb = FrameBuffer565::new(240, 240);

    let title_font: FontRenderer = FontRenderer::new::<fonts::u8g2_font_helvB10_tf>().with_ignore_unknown_chars(true);
    let hero_font: FontRenderer = FontRenderer::new::<fonts::u8g2_font_helvB24_tf>().with_ignore_unknown_chars(true);
    let value_font: FontRenderer = FontRenderer::new::<fonts::u8g2_font_helvR12_tf>().with_ignore_unknown_chars(true);
    let name_font: FontRenderer = FontRenderer::new::<fonts::u8g2_font_helvB12_tf>().with_ignore_unknown_chars(true);
    let username_font: FontRenderer = FontRenderer::new::<fonts::u8g2_font_helvR10_tf>().with_ignore_unknown_chars(true);
    let label_font: FontRenderer = FontRenderer::new::<fonts::u8g2_font_helvB08_tf>().with_ignore_unknown_chars(true);

    let start = StdInstant::now();
    for _ in 0..ITERATIONS {
        let _ = title_font.render_aligned(
            "Pico Link",
            Point::new(120, 4),
            VerticalPosition::Top,
            HorizontalAlignment::Center,
            FontColor::Transparent(Rgb565::WHITE),
            &mut fb,
        );
        let _ = hero_font.render_aligned(
            "LDAC",
            Point::new(95, 100),
            VerticalPosition::Top,
            HorizontalAlignment::Center,
            FontColor::Transparent(Rgb565::WHITE),
            &mut fb,
        );
        let _ = value_font.render_aligned(
            "990 kbps",
            Point::new(95, 140),
            VerticalPosition::Top,
            HorizontalAlignment::Center,
            FontColor::Transparent(Rgb565::CSS_LIGHT_GRAY),
            &mut fb,
        );
        let _ = name_font.render_aligned(
            "Sony WH-1000XM5",
            Point::new(10, 60),
            VerticalPosition::Top,
            HorizontalAlignment::Left,
            FontColor::Transparent(Rgb565::WHITE),
            &mut fb,
        );
        let _ = username_font.render_aligned(
            "Connected",
            Point::new(10, 80),
            VerticalPosition::Top,
            HorizontalAlignment::Left,
            FontColor::Transparent(Rgb565::CSS_LIGHT_GRAY),
            &mut fb,
        );
        let _ = label_font.render_aligned(
            "OUT",
            Point::new(215, 6),
            VerticalPosition::Top,
            HorizontalAlignment::Center,
            FontColor::Transparent(Rgb565::CSS_LIGHT_GRAY),
            &mut fb,
        );
    }
    let elapsed = start.elapsed();
    report("3. TEXT ONLY (title + hero word + hero value + name/username + rail label)", elapsed, ITERATIONS);
}

fn report(label: &str, elapsed: std::time::Duration, iterations: u32) {
    let per_iter = elapsed / iterations;
    println!("{label}");
    println!(
        "  total: {:.3}ms over {} iters -> {:.4}ms/iter ({} ns/iter)",
        elapsed.as_secs_f64() * 1000.0,
        iterations,
        per_iter.as_secs_f64() * 1000.0,
        per_iter.as_nanos()
    );
}
