//! Throwaway probe for `pico-link-znb.12`: renders a labelled grid of
//! candidate `open_iconic_all` codepoints so the actual glyph shapes can
//! be read off a PNG at zoom, rather than assumed from the alphabetical
//! codepoint formula alone (`theme.rs`'s icon module doc comment: codepoint
//! = `0x40 + alphabetical index` in `iconic/open-iconic`'s `svg/`
//! directory).
//!
//! Candidates were selected by fetching the current alphabetical listing
//! of `github.com/iconic/open-iconic/tree/master/svg` and applying that
//! formula, then cross-checked against every codepoint already in
//! `theme::icon` (SHIELD, LOCK_LOCKED, LOCK_UNLOCKED, EYE, CARET_RIGHT,
//! BLUETOOTH, COG) — all seven matched their existing doc comments exactly,
//! which is why this probe renders one candidate per target glyph rather
//! than a wide speculative sweep. There is no `usb.svg` in that directory
//! at all (confirmed: 223 files, alphabetically account-login..zoom-out,
//! no "usb" entry) so several USB-adjacent candidates are included for
//! visual comparison instead of one certain answer.
//!
//! Run with: `cargo run -p pico-link-core --example icon_probe_znb12`
//! Writes `icon_probe_znb12.png` to the current directory.
#![allow(clippy::pedantic, clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

use pico_link_core::render::theme::font;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};
use u8g2_fonts::FontRenderer;

use embedded_graphics::draw_target::DrawTarget;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::{Point, Primitive, RgbColor, Size};
use embedded_graphics::primitives::Rectangle;
use embedded_graphics::{Drawable, Pixel};

/// A minimal in-RAM RGB565 canvas, independent of `FrameBuffer565`, so this
/// throwaway probe doesn't need to route through the render-core's list/
/// screen machinery at all -- just draw glyphs and dump pixels.
struct Canvas {
    width: u32,
    height: u32,
    pixels: Vec<Rgb565>,
}

impl Canvas {
    fn new(width: u32, height: u32) -> Self {
        Self { width, height, pixels: vec![Rgb565::BLACK; (width * height) as usize] }
    }
}

impl embedded_graphics::geometry::OriginDimensions for Canvas {
    fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }
}

impl DrawTarget for Canvas {
    type Color = Rgb565;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(point, color) in pixels {
            if point.x >= 0 && point.y >= 0 && (point.x as u32) < self.width && (point.y as u32) < self.height {
                self.pixels[(point.y as u32 * self.width + point.x as u32) as usize] = color;
            }
        }
        Ok(())
    }
}

/// One candidate cell: a label (what we're testing / hoping for) plus the
/// codepoint to render at 2x and 4x.
struct Candidate {
    label: &'static str,
    codepoint: u32,
}

fn main() {
    // Codepoints = 0x40 + alphabetical index in iconic/open-iconic's svg/
    // directory (fetched live 2026-08-31; index confirmed against the
    // seven pre-existing theme::icon constants, all of which matched).
    let candidates = [
        Candidate { label: "headphones (idx118)", codepoint: 0x40 + 118 },
        Candidate { label: "check (idx51)", codepoint: 0x40 + 51 },
        Candidate { label: "plus (idx170)", codepoint: 0x40 + 170 },
        Candidate { label: "warning (idx216)", codepoint: 0x40 + 216 },
        // USB candidates: open-iconic has no "usb.svg" at all (verified
        // against the full 223-file listing), so these are the plausible
        // stand-ins for a "wired/USB audio in" title-bar mark, to compare
        // by eye against the existing BLUETOOTH mark's visual weight.
        Candidate { label: "USB? hard-drive (idx116)", codepoint: 0x40 + 116 },
        Candidate { label: "USB? data-xfer-dl (idx78)", codepoint: 0x40 + 78 },
        Candidate { label: "USB? data-xfer-ul (idx79)", codepoint: 0x40 + 79 },
        Candidate { label: "USB? cloud-download (idx60)", codepoint: 0x40 + 60 },
        Candidate { label: "USB? signal (idx189)", codepoint: 0x40 + 189 },
    ];

    let cell_w = 240u32;
    let cell_h = 64u32;
    let cols = 2u32;
    let rows = (candidates.len() as u32).div_ceil(cols);
    let mut canvas = Canvas::new(cell_w * cols, cell_h * rows);

    let label_font = font::hint();
    let icon2x: FontRenderer = font::icon_2x();
    let icon4x: FontRenderer = font::icon_4x();

    for (i, c) in candidates.iter().enumerate() {
        let col = (i as u32) % cols;
        let row = (i as u32) / cols;
        let ox = (col * cell_w) as i32;
        let oy = (row * cell_h) as i32;

        // Cell border so glyph footprints are unambiguous at zoom.
        let _ = Rectangle::new(Point::new(ox, oy), Size::new(cell_w, cell_h))
            .into_styled(embedded_graphics::primitives::PrimitiveStyle::with_stroke(Rgb565::new(4, 12, 10), 1))
            .draw(&mut canvas);

        let label = format!("{} U+{:04X}", c.label, c.codepoint);
        let _ = label_font.render_aligned(
            label.as_str(),
            Point::new(ox + 4, oy + 4),
            VerticalPosition::Top,
            HorizontalAlignment::Left,
            FontColor::Transparent(Rgb565::WHITE),
            &mut canvas,
        );

        let ch = char::from_u32(c.codepoint).expect("valid scalar value");
        let mut buf = [0u8; 4];
        let s: &str = ch.encode_utf8(&mut buf);

        // 2x glyph
        let _ = icon2x.render_aligned(
            s,
            Point::new(ox + 20, oy + 44),
            VerticalPosition::Bottom,
            HorizontalAlignment::Left,
            FontColor::Transparent(Rgb565::new(7, 32, 30)),
            &mut canvas,
        );
        // 4x glyph
        let _ = icon4x.render_aligned(
            s,
            Point::new(ox + 60, oy + 60),
            VerticalPosition::Bottom,
            HorizontalAlignment::Left,
            FontColor::Transparent(Rgb565::new(7, 32, 30)),
            &mut canvas,
        );
    }

    let path = "icon_probe_znb12.png";
    dump_png(&canvas, path);
    println!("wrote {path} ({}x{})", canvas.width, canvas.height);
}

fn dump_png(canvas: &Canvas, path: &str) {
    let mut image = image::RgbImage::new(canvas.width, canvas.height);
    for y in 0..canvas.height {
        for x in 0..canvas.width {
            let color = canvas.pixels[(y * canvas.width + x) as usize];
            image.put_pixel(x, y, image::Rgb([color.r() << 3, color.g() << 2, color.b() << 3]));
        }
    }
    image.save(path).expect("failed to write PNG");
}
