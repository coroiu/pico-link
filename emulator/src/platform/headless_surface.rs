//! `HeadlessSurface`: a `DisplaySurface` with no window. It keeps the most
//! recently flushed framebuffer in RAM (as a flat `Vec<Rgb565>`, not a
//! `FrameBuffer565` clone — see the note below) and exposes PNG encoding
//! on demand, so an agent driving the emulator headlessly can request a
//! screenshot without a display server.
//!
//! `flush` itself can never fail (there is no real device I/O to fail
//! against — it's a RAM-to-RAM copy), so `Error = Infallible`.
//!
//! The pixel -> PNG conversion (`Rgb565::r() << 3`, etc.) is deliberately
//! identical to the one already proven in
//! `core/tests/render_png_dump.rs` and `core/examples/render_scene.rs`:
//! this is the same "expand each Rgb565 channel to 8 bits by left-shifting"
//! convention used everywhere else in this codebase that turns a
//! `FrameBuffer565` into an `image::RgbImage`. Reusing it here (rather than
//! inventing a second conversion) is exactly what makes the headless-vs-
//! windowed parity test in `emulator/tests/surface_parity.rs` meaningful.

// Same justification as `pico_link_core::render`'s identical `#![allow]`: pixel
// counts here never approach `u32::MAX` on any display this project
// targets, so `usize -> u32` here can't realistically truncate.
#![allow(clippy::cast_possible_truncation)]

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use pico_link_core::platform::{DisplayPower, DisplaySurface, FrameBuffer565};
use embedded_graphics::pixelcolor::{IntoStorage, Rgb565};
use embedded_graphics::Pixel;

/// Expands one packed Rgb565 raw value (`RRRRRGGGGGGBBBBB`, as produced by
/// `Rgb565::into_storage()` or decoded straight off the wire via
/// `u16::from_be_bytes` -- both are the *same* bit layout, just reached by
/// different paths) into 8-bit-per-channel RGB888 by left-shifting each
/// channel to fill its byte -- the "expand each Rgb565 channel to 8 bits"
/// convention already proven in `core/tests/render_png_dump.rs` and
/// `core/examples/render_scene.rs`.
///
/// Shared between this module's own [`HeadlessSurface::encode_png`]
/// (operating on in-memory `Rgb565` values, reached via `into_storage()`)
/// and `emulator::platform::remote_verify_png::framebuffer_bytes_to_png`
/// (operating on raw big-endian wire bytes captured over serial from a
/// real Pico Plus 2 W, reached via `u16::from_be_bytes`) so a screenshot taken
/// over HTTP (headless) and one taken over serial (the agent verify seam)
/// are pixel-identical for the same underlying color, not two
/// independently-derived conversions that could silently drift apart.
#[must_use]
pub(crate) fn rgb565_raw_to_rgb888(raw: u16) -> [u8; 3] {
    let r5 = ((raw >> 11) & 0x1F) as u8;
    let g6 = ((raw >> 5) & 0x3F) as u8;
    let b5 = (raw & 0x1F) as u8;
    [r5 << 3, g6 << 2, b5 << 3]
}

struct CapturedFrame {
    width: u32,
    height: u32,
    /// Row-major, per `FrameBuffer565::pixels()`'s documented iteration
    /// order.
    pixels: Vec<Rgb565>,
}

pub struct HeadlessSurface {
    last_frame: Option<CapturedFrame>,
    /// Idle-screensaver display power state.
    /// Defaults to `On` so existing callers that never touch `set_power`
    /// see unchanged behavior. `flush` keeps recording the real frame
    /// regardless of this field -- only `encode_png`'s *output* changes
    /// when dimmed/off, mirroring how a real display keeps receiving pixel
    /// data over SPI even with its backlight dimmed or off.
    power: DisplayPower,
}

impl Default for HeadlessSurface {
    fn default() -> Self {
        Self { last_frame: None, power: DisplayPower::On }
    }
}

impl HeadlessSurface {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The most recently requested display power level.
    #[must_use]
    pub fn power(&self) -> DisplayPower {
        self.power
    }

    /// Encodes the most recently flushed framebuffer as a PNG, returning
    /// `None` if `flush` has never been called. `On` renders the real
    /// frame; `Off` returns an all-black image of the correct dimensions
    /// (headless must observe the same blanking a real display would
    /// show, not silently keep exposing the last real pixels); `Dim`
    /// renders the real frame with each channel scaled by
    /// [`DisplayPower::backlight_permille`]'s value, gamma-corrected
    /// (`(permille/1000)^(1/2.2)`) so the PNG preview looks like what a
    /// perceptually-linear backlight dim actually looks like, not a flat
    /// linear scale-down.
    ///
    /// # Panics
    ///
    /// Panics if in-memory PNG encoding fails, which should not be
    /// possible for a buffer built directly from a `FrameBuffer565` (no
    /// filesystem or format-mismatch failure modes apply here).
    #[must_use]
    pub fn encode_png(&self) -> Option<Vec<u8>> {
        let frame = self.last_frame.as_ref()?;
        let mut image = image::RgbImage::new(frame.width, frame.height);

        match self.power {
            DisplayPower::On => {
                for (index, color) in frame.pixels.iter().enumerate() {
                    let index = index as u32;
                    let x = index % frame.width;
                    let y = index / frame.width;
                    image.put_pixel(x, y, image::Rgb(rgb565_raw_to_rgb888(color.into_storage())));
                }
            }
            DisplayPower::Dim => {
                let f = crate::platform::dim_factor();
                for (index, color) in frame.pixels.iter().enumerate() {
                    let index = index as u32;
                    let x = index % frame.width;
                    let y = index / frame.width;
                    let [r, g, b] = rgb565_raw_to_rgb888(color.into_storage());
                    image.put_pixel(
                        x,
                        y,
                        image::Rgb([
                            (f32::from(r) * f).round() as u8,
                            (f32::from(g) * f).round() as u8,
                            (f32::from(b) * f).round() as u8,
                        ]),
                    );
                }
            }
            DisplayPower::Off => {
                // `image::RgbImage::new` already zero-fills every pixel to
                // black, so leaving the loop above unrun is the all-black
                // image.
            }
        }

        let mut buffer = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut buffer), image::ImageFormat::Png)
            .expect("in-memory PNG encode should never fail");
        Some(buffer)
    }

    /// Convenience wrapper: encode and write straight to a file, for the
    /// verification example and for manual inspection.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the file couldn't be written. Returns
    /// `Ok(())` as a no-op if `flush` has never been called (nothing to
    /// save yet) — callers that need to distinguish "no frame" from
    /// "saved" should use [`HeadlessSurface::encode_png`] directly.
    pub fn save_png(&self, path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
        match self.encode_png() {
            Some(bytes) => std::fs::write(path, bytes),
            None => Ok(()),
        }
    }
}

impl DisplaySurface for HeadlessSurface {
    type Error = Infallible;

    fn flush(&mut self, framebuffer: &FrameBuffer565) -> Result<(), Self::Error> {
        let width = framebuffer.width();
        let height = framebuffer.height();
        let pixels: Vec<Rgb565> = framebuffer.pixels().map(|Pixel(_, color)| color).collect();
        self.last_frame = Some(CapturedFrame { width, height, pixels });
        Ok(())
    }

    fn set_power(&mut self, power: DisplayPower) -> Result<(), Self::Error> {
        self.power = power;
        Ok(())
    }
}

/// A [`HeadlessSurface`] shared between the render-loop thread and the HTTP
/// server thread: the render loop's `DisplaySurface::flush` and
/// `GET /api/screenshot` (served from a different thread — see
/// `emulator::desktop::http_server::HttpServer`) both need to see the
/// *same* captured frame, so this wraps the surface in an `Arc<Mutex<_>>`
/// rather than handing the loop an owned `HeadlessSurface` the HTTP server
/// has no way to read.
#[derive(Clone, Default)]
pub struct SharedHeadlessSurface(Arc<Mutex<HeadlessSurface>>);

impl SharedHeadlessSurface {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Hands out a second owner of the same underlying `HeadlessSurface` —
    /// e.g. for `SyncServer::set_screenshot_surface` to read whatever
    /// frame this surface's `flush` most recently wrote, from a different
    /// thread.
    #[must_use]
    pub fn handle(&self) -> Arc<Mutex<HeadlessSurface>> {
        Arc::clone(&self.0)
    }

    /// The most recently requested display power level.
    #[must_use]
    pub fn power(&self) -> DisplayPower {
        self.0.lock().unwrap().power()
    }
}

impl DisplaySurface for SharedHeadlessSurface {
    type Error = Infallible;

    fn flush(&mut self, framebuffer: &FrameBuffer565) -> Result<(), Self::Error> {
        self.0.lock().unwrap().flush(framebuffer)
    }

    fn set_power(&mut self, power: DisplayPower) -> Result<(), Self::Error> {
        self.0.lock().unwrap().set_power(power)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_graphics::pixelcolor::WebColors;
    use embedded_graphics::prelude::{Point, RgbColor, Size};
    use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
    use embedded_graphics::{draw_target::DrawTarget, prelude::Primitive, Drawable};

    #[test]
    fn encode_png_returns_none_before_the_first_flush() {
        let surface = HeadlessSurface::new();
        assert!(surface.encode_png().is_none());
    }

    #[test]
    fn flush_then_encode_round_trips_a_solid_color_frame() {
        let mut framebuffer = FrameBuffer565::new(4, 4);
        framebuffer.clear(Rgb565::RED).unwrap();

        let mut surface = HeadlessSurface::new();
        surface.flush(&framebuffer).unwrap();

        let png_bytes = surface.encode_png().expect("frame was flushed");
        let decoded = image::load_from_memory(&png_bytes).unwrap().to_rgb8();
        assert_eq!(decoded.width(), 4);
        assert_eq!(decoded.height(), 4);
        let expected = image::Rgb([Rgb565::RED.r() << 3, Rgb565::RED.g() << 2, Rgb565::RED.b() << 3]);
        for pixel in decoded.pixels() {
            assert_eq!(*pixel, expected);
        }
    }

    #[test]
    fn flush_replaces_the_previously_captured_frame() {
        let mut red_framebuffer = FrameBuffer565::new(2, 2);
        red_framebuffer.clear(Rgb565::RED).unwrap();
        let mut blue_framebuffer = FrameBuffer565::new(2, 2);
        blue_framebuffer.clear(Rgb565::BLUE).unwrap();

        let mut surface = HeadlessSurface::new();
        surface.flush(&red_framebuffer).unwrap();
        surface.flush(&blue_framebuffer).unwrap();

        let png_bytes = surface.encode_png().unwrap();
        let decoded = image::load_from_memory(&png_bytes).unwrap().to_rgb8();
        let expected = image::Rgb([Rgb565::BLUE.r() << 3, Rgb565::BLUE.g() << 2, Rgb565::BLUE.b() << 3]);
        assert_eq!(*decoded.get_pixel(0, 0), expected);
    }

    #[test]
    fn preserves_pixel_positions_not_just_colors_present() {
        // Regression guard for the row-major reconstruction math in
        // `encode_png`: draw a single 1x1 rectangle away from the origin
        // and confirm it lands at the same coordinates in the decoded PNG.
        let mut framebuffer = FrameBuffer565::new(5, 5);
        Rectangle::new(Point::new(3, 1), Size::new(1, 1))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::GREEN))
            .draw(&mut framebuffer)
            .unwrap();

        let mut surface = HeadlessSurface::new();
        surface.flush(&framebuffer).unwrap();
        let decoded = image::load_from_memory(&surface.encode_png().unwrap()).unwrap().to_rgb8();

        let green = image::Rgb([Rgb565::GREEN.r() << 3, Rgb565::GREEN.g() << 2, Rgb565::GREEN.b() << 3]);
        assert_eq!(*decoded.get_pixel(3, 1), green);
        assert_eq!(*decoded.get_pixel(0, 0), image::Rgb([0, 0, 0]));
    }

    #[test]
    fn a_handle_observes_frames_flushed_through_the_shared_surface() {
        // This is the sharing guarantee W5 depends on: the render loop
        // owns a `SharedHeadlessSurface` and calls `flush` on it, while
        // the HTTP server thread only ever holds a `handle()` — it must
        // see exactly what the loop wrote, with no separate copy to fall
        // out of sync.
        let mut framebuffer = FrameBuffer565::new(2, 2);
        framebuffer.clear(Rgb565::CSS_HOT_PINK).unwrap();

        let mut surface = SharedHeadlessSurface::new();
        let handle = surface.handle();

        assert!(handle.lock().unwrap().encode_png().is_none(), "nothing flushed yet");

        surface.flush(&framebuffer).unwrap();

        let png_bytes = handle.lock().unwrap().encode_png().expect("the handle sees the flush");
        let decoded = image::load_from_memory(&png_bytes).unwrap().to_rgb8();
        let expected = image::Rgb([
            Rgb565::CSS_HOT_PINK.r() << 3,
            Rgb565::CSS_HOT_PINK.g() << 2,
            Rgb565::CSS_HOT_PINK.b() << 3,
        ]);
        assert_eq!(*decoded.get_pixel(0, 0), expected);
    }

    #[test]
    fn cloning_a_shared_surface_shares_the_same_underlying_frame() {
        let mut framebuffer = FrameBuffer565::new(2, 2);
        framebuffer.clear(Rgb565::BLUE).unwrap();

        let mut surface = SharedHeadlessSurface::new();
        let clone = surface.clone();

        surface.flush(&framebuffer).unwrap();

        // `clone` is a second owner of the same `Arc<Mutex<HeadlessSurface>>`,
        // not an independent copy — it must observe the flush `surface`
        // performed after the clone was made.
        assert!(clone.handle().lock().unwrap().encode_png().is_some());
    }
}
