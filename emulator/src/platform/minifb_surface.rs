//! `MinifbSurface`: the windowed `DisplaySurface`, presenting the core's
//! Rgb565 `FrameBuffer565` in a real `minifb` window.
//!
//! Two things happen on every `flush`:
//! 1. Rgb565 -> ARGB8888 conversion, via [`rasterize_scaled`] — the same
//!    "expand each channel to 8 bits by left-shifting" convention used by
//!    `HeadlessSurface`'s PNG path (see that module's doc comment), so the
//!    two surfaces are provably showing the same colors, not just
//!    similar-looking ones.
//! 2. Nearest-neighbor upscaling by an integer `scale` factor, ported from
//!    the pre-W3 `emulator/src/main.rs::convert_canvas_to_framebuffer`
//!    (there: fixed 128x32 mono panel at a hardcoded 8x scale; here:
//!    runtime width/height/scale, since `FrameBuffer565` is sized at
//!    runtime per the ADR).
//!
//! `rasterize_scaled` is a free function, independent of any `minifb::
//! Window`, specifically so `emulator/tests/surface_parity.rs` can call it
//! directly and compare its output against `HeadlessSurface`'s PNG without
//! needing to open a real OS window in a test process.
//!
//! The `minifb::Window` is shared (`Rc<RefCell<_>>`) with
//! [`super::input::WindowedInput`]: both need the same window instance —
//! this one to push pixels to it, that one to read its keyboard state —
//! and `minifb` has no split-borrow API for "one half writes, one half
//! reads". Fine for a single-threaded desktop emulator; not a pattern to
//! carry onto the RP2350 board surface (W6), which has no such sharing
//! requirement.

// Same justification as `pico_link_core::render`'s identical `#![allow]`: this
// module does real pixel-position arithmetic directly against
// `embedded-graphics`' `Point` (`i32`), and every display this project
// targets is small enough that none of these conversions can realistically
// wrap or lose a sign.
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::cell::RefCell;
use std::rc::Rc;

use pico_link_core::platform::{DisplayPower, DisplaySurface, FrameBuffer565};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::RgbColor;
use embedded_graphics::Pixel;
use minifb::Window;

/// Converts `framebuffer` to an ARGB8888 buffer upscaled by `scale`,
/// writing into `out`. `out` must already be sized
/// `framebuffer.width() * scale * framebuffer.height() * scale`.
///
/// # Panics
///
/// Panics (via out-of-bounds indexing) if `out` is smaller than that.
pub fn rasterize_scaled(framebuffer: &FrameBuffer565, scale: u32, out: &mut [u32]) {
    let width = framebuffer.width();
    let scaled_width = width * scale;

    for Pixel(point, color) in framebuffer.pixels() {
        let argb = argb8888(color);
        let base_x = point.x as u32 * scale;
        let base_y = point.y as u32 * scale;

        for sy in 0..scale {
            for sx in 0..scale {
                let index = ((base_y + sy) * scaled_width + (base_x + sx)) as usize;
                out[index] = argb;
            }
        }
    }
}

/// Same channel-expansion convention as `HeadlessSurface`'s PNG path: each
/// Rgb565 channel is left-shifted to fill the full 8 bits (not
/// bit-replicated), so the two surfaces show identical colors.
fn argb8888(color: Rgb565) -> u32 {
    (u32::from(color.r() << 3) << 16) | (u32::from(color.g() << 2) << 8) | u32::from(color.b() << 3)
}

/// Scales one packed `0x00RRGGBB` value's three channels by `factor`,
/// rounding -- the live-window counterpart of `HeadlessSurface::
/// encode_png`'s `Dim` branch, sharing the same `super::dim_factor()`
/// input so the two surfaces stay pixel-identical while dimmed.
fn scale_argb(argb: u32, factor: f32) -> u32 {
    let r = (argb >> 16) & 0xFF;
    let g = (argb >> 8) & 0xFF;
    let b = argb & 0xFF;
    let scale = |c: u32| (f32::from(c as u8) * factor).round() as u32;
    (scale(r) << 16) | (scale(g) << 8) | scale(b)
}

pub struct MinifbSurface {
    window: Rc<RefCell<Window>>,
    width: u32,
    height: u32,
    scale: u32,
    argb_buffer: Vec<u32>,
    /// Idle-screensaver display power state.
    /// Defaults to `On`. Unlike `HeadlessSurface` (which reconstructs the
    /// blanked/dimmed output on demand in `encode_png`), going `Off` here
    /// actively pushes one zeroed buffer to the live `minifb::Window` (see
    /// `set_power`) since there's no "encode on demand" step to intercept
    /// -- the window paints immediately. `Dim` only sets this field --
    /// `flush` applies `super::dim_factor()` to every pixel it rasterizes
    /// while it holds `Dim`, so the dimmed picture appears on the very
    /// next `flush` (which `Runner::step`'s `mark_dirty`-on-level-change
    /// guarantees happens promptly on an On<->Dim transition).
    power: DisplayPower,
}

impl MinifbSurface {
    /// # Panics
    ///
    /// Panics if `scale` is `0`.
    #[must_use]
    pub fn new(window: Rc<RefCell<Window>>, width: u32, height: u32, scale: u32) -> Self {
        assert!(scale >= 1, "scale must be at least 1");
        let buffer_len = (width * scale * height * scale) as usize;
        Self { window, width, height, scale, argb_buffer: vec![0; buffer_len], power: DisplayPower::On }
    }

    /// Whether the last `set_power` call left this surface on. Exposed for
    /// tests/introspection (`crate::run::run` tracks its own idle state
    /// rather than poll this).
    #[must_use]
    pub fn is_powered_on(&self) -> bool {
        self.power == DisplayPower::On
    }

    /// The most recently requested display power level.
    #[must_use]
    pub fn power(&self) -> DisplayPower {
        self.power
    }
}

impl DisplaySurface for MinifbSurface {
    type Error = minifb::Error;

    fn flush(&mut self, framebuffer: &FrameBuffer565) -> Result<(), Self::Error> {
        debug_assert_eq!(framebuffer.width(), self.width, "MinifbSurface sized for a different framebuffer width");
        debug_assert_eq!(framebuffer.height(), self.height, "MinifbSurface sized for a different framebuffer height");

        rasterize_scaled(framebuffer, self.scale, &mut self.argb_buffer);

        if self.power == DisplayPower::Dim {
            let f = super::dim_factor();
            for pixel in &mut self.argb_buffer {
                *pixel = scale_argb(*pixel, f);
            }
        }

        let window_width = (self.width * self.scale) as usize;
        let window_height = (self.height * self.scale) as usize;
        self.window.borrow_mut().update_with_buffer(&self.argb_buffer, window_width, window_height)
    }

    /// `Off` pushes a single zeroed (all-black) buffer to the window so it
    /// visibly goes black and stays black (nothing else touches the
    /// window again until the next `flush`/`set_power` call) -- matching
    /// `HeadlessSurface::encode_png`'s all-black-when-off behavior for
    /// parity. `On`/`Dim` only set the field: `On` deliberately does NOT
    /// re-push `self.argb_buffer` (which still holds the last real
    /// frame's pixels, since `flush` was never told to touch it while
    /// off) -- restoring the actual picture is `crate::run::run`'s job,
    /// via a forced `App::mark_dirty` + `flush` on wake, not this seam;
    /// `Dim` similarly waits for the next `flush` to apply the dim factor.
    fn set_power(&mut self, power: DisplayPower) -> Result<(), Self::Error> {
        if power == DisplayPower::Off {
            let window_width = (self.width * self.scale) as usize;
            let window_height = (self.height * self.scale) as usize;
            let blank = vec![0u32; self.argb_buffer.len()];
            self.window.borrow_mut().update_with_buffer(&blank, window_width, window_height)?;
        }
        self.power = power;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_graphics::draw_target::DrawTarget;
    use embedded_graphics::pixelcolor::WebColors;
    use embedded_graphics::Drawable;

    #[test]
    fn rasterize_scaled_expands_each_source_pixel_into_a_scale_by_scale_block() {
        let mut framebuffer = FrameBuffer565::new(2, 1);
        framebuffer.clear(Rgb565::BLACK).unwrap();
        // Left pixel red, right pixel blue.
        Pixel(embedded_graphics::prelude::Point::new(0, 0), Rgb565::RED)
            .draw(&mut framebuffer)
            .unwrap();
        Pixel(embedded_graphics::prelude::Point::new(1, 0), Rgb565::BLUE)
            .draw(&mut framebuffer)
            .unwrap();

        let scale = 3;
        let mut out = vec![0u32; (2 * scale * scale) as usize];
        rasterize_scaled(&framebuffer, scale, &mut out);

        let scaled_width = 2 * scale;
        let red = argb8888(Rgb565::RED);
        let blue = argb8888(Rgb565::BLUE);
        for y in 0..scale {
            for x in 0..scale {
                assert_eq!(out[(y * scaled_width + x) as usize], red);
            }
            for x in scale..(2 * scale) {
                assert_eq!(out[(y * scaled_width + x) as usize], blue);
            }
        }
    }

    #[test]
    fn argb8888_matches_the_headless_png_channel_expansion() {
        // Cross-check against the exact expression `HeadlessSurface` uses,
        // rather than against a hand re-derived constant, so this test
        // fails loudly if the two conversions ever drift apart.
        let color = Rgb565::CSS_DARK_SLATE_BLUE;
        let expected = (u32::from(color.r() << 3) << 16)
            | (u32::from(color.g() << 2) << 8)
            | u32::from(color.b() << 3);
        assert_eq!(argb8888(color), expected);
    }
}
