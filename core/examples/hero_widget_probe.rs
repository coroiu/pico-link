//! Verifiable deliverable for `pico-link-znb.6` (E4): renders
//! `HeroStatusView` for the five states named in the bead's DONE
//! criteria — connected-nominal, connected-idle, connected-fell-back-to-
//! SBC, MUTED, and no-link — on the real 240x240 `FrameBuffer565` inside
//! the real chrome content area, then dumps a 4x-upscaled PNG per state
//! for zoomed inspection (sub-pixel/overflow bugs are invisible at 1x —
//! see CLAUDE.md's rendering-verification rule).
//!
//! A sixth state renders the `aptX HD` descender case specifically, the
//! one this bead's orchestrator note flagged: budgeting the 25px cap
//! height (rather than the real 32px total ink height) would let this
//! mixed-case codec name collide with the bitrate line below it.
//!
//! Run with: `cargo run -p pico-link-core --example hero_widget_probe`
//! Writes `hero_widget_probe_<state>.png` (4x nearest-neighbor upscaled)
//! to the current directory.

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::{Point, RgbColor, Size};
use embedded_graphics::primitives::Rectangle;
use pico_link_core::render::hero::{BitrateStatus, CodecStatus, HeroStatusView};
use pico_link_core::render::{compute_chrome, FrameBuffer565, Widget};

const SCALE: u32 = 4;

fn render_state(name: &str, view: HeroStatusView) {
    let mut fb = FrameBuffer565::new(240, 240);
    let chrome = compute_chrome(Size::new(240, 240));
    // Fill the whole panel BACKGROUND first, matching the real screen
    // render path (Screen::render fills the title bar itself but leaves
    // untouched content/hint background to whatever the widget draws --
    // this probe fills the full panel so any area the widget *doesn't*
    // touch reads as background, same as production).
    use embedded_graphics::prelude::Primitive;
    use embedded_graphics::primitives::PrimitiveStyleBuilder;
    use embedded_graphics::Drawable;
    Rectangle::new(Point::zero(), Size::new(240, 240))
        .into_styled(PrimitiveStyleBuilder::new().fill_color(pico_link_core::render::theme::palette::BACKGROUND).build())
        .draw(&mut fb)
        .expect("core DrawTarget is Infallible");

    view.render(chrome.content, &mut fb).expect("core DrawTarget is Infallible");

    let path = format!("hero_widget_probe_{name}.png");
    dump_png_scaled(&fb, &path, SCALE);
    println!("wrote {path} ({}x{} at {SCALE}x)", fb.width() * SCALE, fb.height() * SCALE);
}

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

fn main() {
    // 1. connected-nominal
    render_state(
        "1_connected_nominal",
        HeroStatusView::new(
            "Sony WH-1000XM5",
            CodecStatus::Connected { word: "LDAC".into(), fallback: None, bitrate: BitrateStatus::Kbps(909) },
        )
        .with_stat_line("USB 48k 24-bit"),
    );

    // 2. connected-idle (host silent -- must read "idle", never "0 kbps")
    render_state(
        "2_connected_idle",
        HeroStatusView::new(
            "Sony WH-1000XM5",
            CodecStatus::Connected { word: "LDAC".into(), fallback: None, bitrate: BitrateStatus::Idle },
        )
        .with_stat_line("USB 48k 24-bit"),
    );

    // 3. connected-fell-back-to-SBC (amber word + FALLBACK banner)
    render_state(
        "3_fell_back_to_sbc",
        HeroStatusView::new(
            "Sony WH-1000XM5",
            CodecStatus::Connected {
                word: "SBC".into(),
                fallback: Some("Headphones don't support LDAC".into()),
                bitrate: BitrateStatus::Kbps(328),
            },
        )
        .with_stat_line("USB 48k 24-bit"),
    );

    // 4. MUTED (banner outranks FALLBACK; word stays amber underneath)
    render_state(
        "4_muted_over_fallback",
        HeroStatusView::new(
            "Sony WH-1000XM5",
            CodecStatus::Connected {
                word: "SBC".into(),
                fallback: Some("Headphones don't support LDAC".into()),
                bitrate: BitrateStatus::Kbps(328),
            },
        )
        .with_muted(true)
        .with_stat_line("USB 48k 24-bit"),
    );

    // 5. no-link
    render_state("5_no_link", HeroStatusView::new("Sony WH-1000XM5", CodecStatus::NoLink));

    // 6. aptX HD -- the descender case. Longest advance width (125px)
    // *and* the 32px-tall mixed-case word this bead's whole vertical
    // budget is built around.
    render_state(
        "6_aptx_hd_descender",
        HeroStatusView::new(
            "Sony WH-1000XM5",
            CodecStatus::Connected { word: "aptX HD".into(), fallback: None, bitrate: BitrateStatus::Kbps(576) },
        )
        .with_stat_line("USB 48k 24-bit"),
    );

    // 7. aptX HD + fallback banner together -- the worst-case stack:
    // tallest hero word AND a banner both present, nothing else changed.
    render_state(
        "7_aptx_hd_with_banner",
        HeroStatusView::new(
            "Sony WH-1000XM5",
            CodecStatus::Connected {
                word: "aptX HD".into(),
                fallback: Some("Headphones don't support aptX Adaptive".into()),
                bitrate: BitrateStatus::Kbps(276),
            },
        )
        .with_stat_line("USB 48k 24-bit"),
    );

    // 8. long device name -- ellipsis truncation, not overflow.
    render_state(
        "8_long_device_name",
        HeroStatusView::new(
            "Sennheiser Momentum 4 Wireless Over-Ear Headphones",
            CodecStatus::Connected { word: "LDAC".into(), fallback: None, bitrate: BitrateStatus::Kbps(990) },
        )
        .with_stat_line("USB 48k 24-bit"),
    );
}
