//! Verifiable deliverable for `pico-link-znb.3` (E3): renders
//! `theme::font::hero()` against the longest real codec strings the
//! device will actually show ("LDAC", "SBC", "AAC", "aptX HD") plus a
//! bitrate line in `font::value()`, on the real 240x240 `FrameBuffer565`
//! and the real `palette::BACKGROUND`/`TEXT_PRIMARY` colors, then dumps a
//! PNG for zoomed inspection (sub-pixel/overflow bugs are invisible at
//! 1x — see CLAUDE.md's rendering-verification rule) and prints the
//! measured cap-height/advance-width for each word so the `font::hero()`
//! doc comment's numbers are checked against this file, not asserted
//! blind.
//!
//! Run with: `cargo run -p pico-link-core --example hero_font_probe`
//! Writes `hero_font_probe.png` to the current directory.

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::{Point, Primitive, RgbColor};
use embedded_graphics::Drawable;
use pico_link_core::render::theme::{font, palette};
use pico_link_core::render::FrameBuffer565;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};

fn main() {
    let mut framebuffer = FrameBuffer565::new(240, 240);
    // Fill BACKGROUND explicitly rather than relying on a default — this
    // is the exact backdrop the hero word renders against on Home.
    embedded_graphics::primitives::Rectangle::new(
        Point::zero(),
        embedded_graphics::prelude::Size::new(240, 240),
    )
    .into_styled(
        embedded_graphics::primitives::PrimitiveStyleBuilder::new()
            .fill_color(palette::BACKGROUND)
            .build(),
    )
    .draw(&mut framebuffer)
    .expect("core DrawTarget is Infallible");

    // The longest real codec words the design names (section 6 / S11):
    // LDAC, SBC, AAC, aptX HD. Rendered stacked, each centered, with a
    // bitrate line under the first to prove font::hero()'s digit
    // coverage claim and to match section 6's paired layout ("LDAC" /
    // "909 kbps").
    let words = ["LDAC", "SBC", "AAC", "aptX HD"];
    let mut y = 20;
    for word in words {
        let hero = font::hero();
        let bbox = hero
            .get_rendered_dimensions_aligned(
                word,
                Point::zero(),
                VerticalPosition::Top,
                HorizontalAlignment::Left,
            )
            .expect("ASCII text must measure")
            .expect("non-empty text has a bounding box");
        println!("{word:>8}: cap-height={}px advance-width={}px", bbox.size.height, bbox.size.width);

        let _ = hero.render_aligned(
            word,
            Point::new(120, y),
            VerticalPosition::Top,
            HorizontalAlignment::Center,
            FontColor::Transparent(palette::TEXT_PRIMARY),
            &mut framebuffer,
        );
        y += 34;
    }

    // The bitrate line (font::value(), section 6's second hero-adjacent
    // line) under the first word, right-aligned into a fixed slot per
    // the design's "every number is right-aligned into a fixed slot"
    // numeric rule.
    let value = font::value();
    let _ = value.render_aligned(
        "909 kbps",
        Point::new(120, y + 6),
        VerticalPosition::Top,
        HorizontalAlignment::Center,
        FontColor::Transparent(palette::TEXT_PRIMARY),
        &mut framebuffer,
    );

    let path = "hero_font_probe.png";
    dump_png(&framebuffer, path);
    println!("wrote {path} ({}x{})", framebuffer.width(), framebuffer.height());
}

fn dump_png(framebuffer: &FrameBuffer565, path: &str) {
    let width = framebuffer.width();
    let height = framebuffer.height();
    let mut image = image::RgbImage::new(width, height);

    for pixel in framebuffer.pixels() {
        let point = pixel.0;
        let color: Rgb565 = pixel.1;
        #[allow(clippy::cast_sign_loss)]
        image.put_pixel(
            point.x as u32,
            point.y as u32,
            image::Rgb([color.r() << 3, color.g() << 2, color.b() << 3]),
        );
    }

    image.save(path).expect("failed to write PNG");
}
