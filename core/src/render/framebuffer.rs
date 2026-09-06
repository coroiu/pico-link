//! The canonical Rgb565 in-RAM framebuffer: the app core's single render
//! output, per the presentation-surface ADR. Every run mode (headless,
//! windowed, real-target) differs only in how it *flushes* this buffer to a
//! physical or virtual display (`DisplaySurface::flush`, implemented in
//! later beads); the buffer itself, and everything that draws into it, is
//! platform-free and lives here in `pico-link-core`.
//!
//! Backed by `embedded-graphics-framebuf`, whose `FrameBuf<C, B>` is generic
//! over a storage backend `B`. The upstream crate only ships backends for
//! fixed-size arrays (`[C; N]` / `&mut [C; N]`), which would force a
//! compile-time resolution into this crate — exactly what the render core
//! must avoid (no `128`/`320`/`170` literals baked into widgets or the
//! buffer type). [`HeapBuffer`] is a small local newtype implementing
//! `FrameBufferBackend` over a `Vec<Rgb565>` instead, so [`FrameBuffer565`]
//! can be sized at runtime from whatever `DisplaySurface` reports.
//!
//! See: .planning/decisions/2026-08-11-presentation-surface-run-mode-seam.md

use alloc::vec;
use alloc::vec::Vec;
use core::convert::Infallible;

use embedded_graphics::{
    draw_target::DrawTarget,
    geometry::{Dimensions, OriginDimensions},
    pixelcolor::{raw::RawU16, IntoStorage, Rgb565},
    prelude::{Point, Size},
    primitives::Rectangle,
    Pixel,
};
use embedded_graphics_framebuf::{backends::FrameBufferBackend, FrameBuf};

/// Heap-backed storage for [`FrameBuf`], backed by raw `u16` storage rather
/// than `Rgb565` values directly.
///
/// This is a **correctness** fix, not a style preference: `Rgb565` is a
/// nested `repr(Rust)` newtype (`Rgb565(RawU16(u16))`), and Rust's `repr(Rust)`
/// makes no layout guarantee that such a type is bit-identical to a bare
/// `u16` -- there is no `repr(transparent)` anywhere in that chain. Storing
/// `Vec<Rgb565>` and then having C DMA that memory directly (as
/// [`FrameBuffer565::as_raw_u16`] exists to support -- see M1b) would
/// reinterpret memory the compiler never promised has that shape. Storing
/// `Vec<u16>` instead and converting at the `FrameBufferBackend` boundary
/// (`RawU16::new` / `Rgb565::from`, both cheap newtype wraps of a value
/// already in a register) sidesteps the question entirely: the backing
/// storage this type hands out a `&[u16]` view of really is `u16`s, by
/// construction, not by assumption.
///
/// Local newtype required by Rust's orphan rules: neither
/// `FrameBufferBackend` (foreign trait, from `embedded-graphics-framebuf`)
/// nor `Vec<u16>` (foreign type, from `alloc`) is defined in this crate, so
/// a direct `impl` is not allowed.
struct HeapBuffer(Vec<u16>);

impl FrameBufferBackend for HeapBuffer {
    type Color = Rgb565;

    fn set(&mut self, index: usize, color: Self::Color) {
        self.0[index] = color.into_storage();
    }

    fn get(&self, index: usize) -> Self::Color {
        Rgb565::from(RawU16::new(self.0[index]))
    }

    fn nr_elements(&self) -> usize {
        self.0.len()
    }
}

/// The shared Rgb565 framebuffer the render core draws into and a
/// `DisplaySurface` implementation flushes out. Resolution is a runtime
/// parameter (see [`FrameBuffer565::new`]) — nothing in this type, or in
/// anything that draws into it, hardcodes a specific panel's dimensions.
pub struct FrameBuffer565 {
    inner: FrameBuf<Rgb565, HeapBuffer>,
}

impl FrameBuffer565 {
    /// Allocates a new framebuffer of the given size, cleared to black.
    ///
    /// # Panics
    ///
    /// Panics if `width * height` overflows `usize` (not a realistic
    /// concern for any display this project targets).
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        let pixel_count = width as usize * height as usize;
        let data = vec![Rgb565::default().into_storage(); pixel_count];
        Self {
            inner: FrameBuf::new(HeapBuffer(data), width as usize, height as usize),
        }
    }

    #[must_use]
    pub fn width(&self) -> u32 {
        self.inner.width() as u32
    }

    #[must_use]
    pub fn height(&self) -> u32 {
        self.inner.height() as u32
    }

    /// Reads back a single pixel. Primarily for tests and the PNG-dump
    /// verification path (see `examples/render_scene.rs`); the render path
    /// itself only ever writes via `DrawTarget`.
    #[must_use]
    pub fn pixel(&self, p: Point) -> Rgb565 {
        self.inner.get_color_at(p)
    }

    /// Iterates every pixel in the buffer in row-major order, for surfaces
    /// (or tests) that need to walk the whole framebuffer, e.g. to encode a
    /// PNG or hand pixels to `minifb`.
    pub fn pixels(&self) -> impl Iterator<Item = Pixel<Rgb565>> + '_ {
        self.inner.into_iter()
    }

    /// Writes every pixel's RGB565 value into `out`, two bytes per pixel,
    /// in row-major order, in **panel byte order** (big-endian / MSB
    /// first) — the wire format the MIPI DCS `RAMWR` pixel stream expects
    /// (ST7789 and most other RGB565 panels), regardless of the host
    /// CPU's own endianness (the RP2350's Cortex-M33 is little-endian; the panel doesn't
    /// care what the host is).
    ///
    /// This exists for a `DisplaySurface` that wants to bypass a
    /// per-pixel draw-target/iterator abstraction for its hot blit path
    /// (the Pico Plus 2 W's `St7789Surface` blits this buffer's raw bytes
    /// directly over SPI in one large transfer instead of going through
    /// `mipidsi`'s per-pixel
    /// `set_pixels` iterator) — it deliberately reads the backing
    /// `Vec<Rgb565>` directly (via [`FrameBuf`]'s public `data` field)
    /// rather than through [`FrameBuffer565::pixels`]'s `FrameBuf`
    /// iterator, which computes a `Point` per pixel that this bulk path
    /// has no use for.
    ///
    /// Platform-neutral: no `unsafe`, no reinterpret-casting the backing
    /// `Vec<Rgb565>`'s memory — just a straightforward per-pixel
    /// `into_storage().to_be_bytes()` loop, so this stays valid for any
    /// `DisplaySurface` on any host endianness, not just this project's
    /// current little-endian RP2350 Cortex-M33 target.
    ///
    /// # Panics
    ///
    /// Panics if `out.len() != width() * height() * 2`.
    pub fn write_be_bytes(&self, out: &mut [u8]) {
        let raw: &[u16] = &self.inner.data.0;
        assert_eq!(out.len(), raw.len() * 2, "write_be_bytes: `out` must be exactly width*height*2 bytes");

        for (chunk, &value) in out.chunks_exact_mut(2).zip(raw) {
            chunk.copy_from_slice(&value.to_be_bytes());
        }
    }

    /// Borrows the backing storage as raw little-endian `u16` RGB565 values,
    /// one per pixel in row-major order -- the CPU-native form a `no_std`
    /// `DisplaySurface` (the RP2350 firmware's ST7789 driver, M1b) DMAs
    /// straight out of Rust memory. Deliberately the CPU's native `u16`
    /// endianness (little-endian on this project's Cortex-M33 target), NOT
    /// the panel's big-endian wire format -- unlike [`Self::write_be_bytes`],
    /// which exists specifically to produce that wire format. The DMA
    /// consumer is expected to byte-swap in hardware (e.g. the RP2350 DMA
    /// engine's `bswap` option) on the way out, which is exactly why this
    /// method hands back native values rather than pre-swapping them here.
    #[must_use]
    pub fn as_raw_u16(&self) -> &[u16] {
        &self.inner.data.0
    }
}

impl OriginDimensions for FrameBuffer565 {
    fn size(&self) -> Size {
        self.inner.size()
    }
}

/// Bound to `Error = Infallible`: only the surface adapters (later beads)
/// talk to fallible hardware; the core's draw path itself can never fail.
/// See: .planning/decisions/2026-08-11-presentation-surface-run-mode-seam.md
impl DrawTarget for FrameBuffer565 {
    type Color = Rgb565;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        self.inner.draw_iter(pixels)
    }

    fn clear(&mut self, color: Self::Color) -> Result<(), Self::Error> {
        // Delegates to `fill_solid` (matching the trait's own default
        // `clear` -> `fill_solid` chain) instead of `self.inner.clear`
        // (embedded-graphics-framebuf's own `clear`, per-pixel via
        // `set_color_at` -- see this module's doc-adjacent bead
        // `pico-link-t26`): a full-screen clear is the single largest
        // fill any real repaint does, so it must go through the fast
        // row-wise path below, not stay on the one this bead exists to
        // route around.
        self.fill_solid(&self.bounding_box(), color)
    }

    /// Fills a rectangular area with a single solid color as row-wise
    /// slice writes (`chunks`/`fill`), rather than the trait's default
    /// chain (`fill_solid` -> `fill_contiguous` -> `draw_iter`), which
    /// costs a `Point` construction, an iterator hop, a bounds check and
    /// a `y*width+x` index computation *per pixel* for something that is,
    /// underneath, one repeated `u16` value written across a contiguous
    /// run. Bead `pico-link-t26`.
    ///
    /// `area` is clipped to the framebuffer's bounds via
    /// [`Rectangle::intersection`] first -- after that clip, `top_left` is
    /// guaranteed `>= (0, 0)` and `bottom_right() < (width, height)` (the
    /// intersection of any rectangle with a `(0, 0)`-anchored bounding box
    /// can't produce a negative `top_left`, since `component_max` floors
    /// it at the box's own `(0, 0)`), so every row's start/end index below
    /// is in-bounds without a further per-row check. A zero-sized
    /// intersection (fully off-canvas `area`) draws nothing, matching the
    /// trait's own contract ("no intersection" -> no pixels), and no
    /// current call site benefits from the "self-only-not-other" cases in
    /// [`Rectangle::intersection`], since one side of this intersection is
    /// always `self.bounding_box()`, an origin-anchored rectangle with a
    /// nonzero size (the framebuffer's dimensions are never zero -- see
    /// [`FrameBuffer565::new`]'s doc comment).
    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        let area = area.intersection(&self.bounding_box());
        if area.size.width == 0 || area.size.height == 0 {
            return Ok(());
        }

        let stride = self.width() as usize;
        let raw = color.into_storage();
        #[allow(clippy::cast_sign_loss)] // clipped to `bounding_box()` above: `top_left` is `>= (0, 0)` by construction (see doc comment).
        let (left, top) = (area.top_left.x as usize, area.top_left.y as usize);
        let (w, h) = (area.size.width as usize, area.size.height as usize);

        let data = &mut self.inner.data.0;
        for y in top..top + h {
            let row_start = y * stride + left;
            data[row_start..row_start + w].fill(raw);
        }
        Ok(())
    }

    /// Fills a rectangular area with a stream of possibly-different
    /// colors, row-wise: one contiguous slice write per row instead of a
    /// `Point`/bounds-check/index-compute per pixel. Bead `pico-link-t26`.
    ///
    /// Matches the trait's documented contract exactly: `colors` is
    /// consumed in row-major order over the *unclipped* `area` (so an
    /// `area` that runs off-canvas still consumes the right number of
    /// items from `colors` to stay aligned with whatever the caller pairs
    /// this with -- `u8g2-fonts`' glyph background fill is exactly such a
    /// caller, see `render_as_box_fill` in the vendored crate), and stops
    /// early without erroring if `colors` yields fewer than
    /// `area.size.width * area.size.height` items (`Iterator::by_ref` +
    /// `take` below leaves the loop the moment a row's iterator would
    /// otherwise run dry mid-row, exactly like the trait's own default
    /// `area.points().zip(colors)` implementation, which likewise stops
    /// as soon as either side of the `zip` is exhausted).
    ///
    /// Off-canvas rows/columns are skipped without writing, but *only
    /// after* consuming their share of `colors` -- otherwise a
    /// partially-clipped area (some rows on-canvas, some not) would
    /// desync `colors` from the rows it's still being zipped against.
    fn fill_contiguous<I>(&mut self, area: &Rectangle, colors: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Self::Color>,
    {
        let stride = self.width() as usize;
        let canvas_width = self.width() as i32;
        let canvas_height = self.height() as i32;
        let area_width = area.size.width as usize;

        let mut colors = colors.into_iter();
        let data = &mut self.inner.data.0;

        for row in 0..area.size.height as i32 {
            let y = area.top_left.y + row;
            let x0 = area.top_left.x;
            #[allow(clippy::cast_possible_wrap)] // `area_width` is a `u32`-derived `usize` far below `i32::MAX`.
            let x1 = x0 + area_width as i32;

            // Row entirely on-canvas: the common case (glyph/rect fills
            // are laid out within the panel), and the one this bead's fix
            // is for -- a single contiguous slice, indexed once per row
            // rather than bounds-checked per pixel.
            if y >= 0 && y < canvas_height && x0 >= 0 && x1 <= canvas_width {
                #[allow(clippy::cast_sign_loss)] // just checked `y >= 0` and `x0 >= 0` above.
                let row_start = y as usize * stride + x0 as usize;
                let slice = &mut data[row_start..row_start + area_width];
                for dst in slice.iter_mut() {
                    let Some(color) = colors.next() else {
                        // `colors` ran dry mid-row -- matches the trait
                        // contract ("not required to provide width*height
                        // pixels... should return without error"). Nothing
                        // left for any later row either, so stop entirely
                        // rather than desync further rows against `colors`.
                        return Ok(());
                    };
                    *dst = color.into_storage();
                }
                continue;
            }

            // Row partially or fully off-canvas: still must consume
            // exactly `area_width` items from `colors` to stay
            // row-major-aligned with whatever rows follow (a caller like
            // `u8g2-fonts`' glyph box-fill relies on that ordering), but
            // only writes the columns that land on-canvas.
            for col in 0..area_width {
                let Some(color) = colors.next() else {
                    return Ok(());
                };
                #[allow(clippy::cast_possible_wrap)] // `col` is bounded by `area_width`.
                let x = x0 + col as i32;
                if y < 0 || y >= canvas_height || x < 0 || x >= canvas_width {
                    continue;
                }
                #[allow(clippy::cast_sign_loss)] // just bounds-checked `x >= 0` and `y >= 0` above.
                let index = y as usize * stride + x as usize;
                data[index] = color.into_storage();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_graphics::{
        prelude::{Primitive, RgbColor, WebColors},
        primitives::{PrimitiveStyle, Rectangle},
        Drawable,
    };

    #[test]
    fn new_is_sized_correctly_and_cleared_to_black() {
        let fb = FrameBuffer565::new(37, 11);
        assert_eq!(fb.width(), 37);
        assert_eq!(fb.height(), 11);
        assert_eq!(fb.pixel(Point::new(0, 0)), Rgb565::BLACK);
        assert_eq!(fb.pixel(Point::new(36, 10)), Rgb565::BLACK);
    }

    #[test]
    fn drawing_a_rect_sets_pixels_inside_and_leaves_outside_untouched() {
        let mut fb = FrameBuffer565::new(10, 10);
        Rectangle::new(Point::new(2, 2), Size::new(3, 3))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::RED))
            .draw(&mut fb)
            .unwrap();

        assert_eq!(fb.pixel(Point::new(2, 2)), Rgb565::RED);
        assert_eq!(fb.pixel(Point::new(4, 4)), Rgb565::RED);
        assert_eq!(fb.pixel(Point::new(5, 5)), Rgb565::BLACK);
        assert_eq!(fb.pixel(Point::new(0, 0)), Rgb565::BLACK);
    }

    #[test]
    fn resolution_is_a_runtime_parameter_not_a_compile_time_constant() {
        // Two arbitrary, non-standard sizes prove the type isn't secretly
        // bound to any particular panel's dimensions.
        let a = FrameBuffer565::new(13, 5);
        let b = FrameBuffer565::new(401, 233);
        assert_eq!(a.width() * a.height(), 65);
        assert_eq!(b.width() * b.height(), 401 * 233);
    }

    #[test]
    fn pixels_iterates_in_row_major_order() {
        let mut fb = FrameBuffer565::new(2, 2);
        fb.inner.set_color_at(Point::new(0, 0), Rgb565::RED);
        fb.inner.set_color_at(Point::new(1, 0), Rgb565::GREEN);
        fb.inner.set_color_at(Point::new(0, 1), Rgb565::BLUE);
        fb.inner.set_color_at(Point::new(1, 1), Rgb565::WHITE);

        let colors: Vec<Rgb565> = fb.pixels().map(|Pixel(_, c)| c).collect();
        assert_eq!(colors, vec![Rgb565::RED, Rgb565::GREEN, Rgb565::BLUE, Rgb565::WHITE]);
    }

    #[test]
    fn write_be_bytes_matches_row_major_order_and_big_endian_storage() {
        use embedded_graphics::pixelcolor::IntoStorage;
        use embedded_graphics::prelude::RgbColor;

        let mut fb = FrameBuffer565::new(2, 2);
        fb.inner.set_color_at(Point::new(0, 0), Rgb565::RED);
        fb.inner.set_color_at(Point::new(1, 0), Rgb565::GREEN);
        fb.inner.set_color_at(Point::new(0, 1), Rgb565::BLUE);
        fb.inner.set_color_at(Point::new(1, 1), Rgb565::WHITE);

        let mut bytes = [0u8; 8];
        fb.write_be_bytes(&mut bytes);

        let expected: Vec<u8> = [Rgb565::RED, Rgb565::GREEN, Rgb565::BLUE, Rgb565::WHITE]
            .into_iter()
            .flat_map(|c| c.into_storage().to_be_bytes())
            .collect();
        assert_eq!(bytes.to_vec(), expected, "must match pixels()'s row-major order, MSB first per pixel");

        // Sanity-check big-endian specifically: RED in Rgb565 is a nonzero
        // high byte (0xF800 >> ... the top 5 bits are the red channel), so
        // if this were accidentally little-endian, byte 0 would be 0x00,
        // not the true high byte.
        assert_ne!(bytes[0], 0, "first byte of a RED pixel must be the high (MSB) byte, not 0 (which little-endian would produce)");
    }

    #[test]
    #[should_panic(expected = "write_be_bytes: `out` must be exactly width*height*2 bytes")]
    fn write_be_bytes_panics_on_wrong_buffer_size() {
        let fb = FrameBuffer565::new(2, 2);
        let mut too_small = [0u8; 4];
        fb.write_be_bytes(&mut too_small);
    }

    // --- bead pico-link-t26: fill_solid/fill_contiguous row-wise fast
    // path, tested directly for the clipping edge cases the A4 property
    // test (in `app.rs`, over real screens) wouldn't necessarily exercise
    // on its own -- fully off-canvas, partially off-canvas on every edge,
    // and a `colors` iterator shorter than the area.

    #[test]
    fn fill_solid_entirely_on_canvas_sets_exactly_the_rect_and_nothing_else() {
        let mut fb = FrameBuffer565::new(10, 10);
        fb.fill_solid(&Rectangle::new(Point::new(2, 3), Size::new(4, 2)), Rgb565::RED).unwrap();

        for y in 0..10 {
            for x in 0..10 {
                let inside = (2..6).contains(&x) && (3..5).contains(&y);
                let expected = if inside { Rgb565::RED } else { Rgb565::BLACK };
                assert_eq!(fb.pixel(Point::new(x, y)), expected, "pixel ({x}, {y})");
            }
        }
    }

    #[test]
    fn fill_solid_clips_a_rect_hanging_off_every_edge() {
        let mut fb = FrameBuffer565::new(5, 5);
        // Straddles all four edges: top-left at (-2, -2), 9x9 on a 5x5 canvas.
        fb.fill_solid(&Rectangle::new(Point::new(-2, -2), Size::new(9, 9)), Rgb565::BLUE).unwrap();

        for y in 0..5 {
            for x in 0..5 {
                assert_eq!(fb.pixel(Point::new(x, y)), Rgb565::BLUE, "pixel ({x}, {y}) should be inside the clipped fill");
            }
        }
    }

    #[test]
    fn fill_solid_fully_off_canvas_draws_nothing() {
        let mut fb = FrameBuffer565::new(5, 5);
        fb.fill_solid(&Rectangle::new(Point::new(100, 100), Size::new(3, 3)), Rgb565::RED).unwrap();

        for y in 0..5 {
            for x in 0..5 {
                assert_eq!(fb.pixel(Point::new(x, y)), Rgb565::BLACK);
            }
        }
    }

    #[test]
    fn fill_solid_zero_sized_area_draws_nothing_and_does_not_panic() {
        let mut fb = FrameBuffer565::new(5, 5);
        fb.fill_solid(&Rectangle::new(Point::new(1, 1), Size::new(0, 0)), Rgb565::RED).unwrap();
        for y in 0..5 {
            for x in 0..5 {
                assert_eq!(fb.pixel(Point::new(x, y)), Rgb565::BLACK);
            }
        }
    }

    #[test]
    fn fill_solid_matches_styled_rectangle_draw_byte_for_byte() {
        // Same fill, one via the direct `fill_solid` call (this bead's
        // fast path), one via the ordinary `Rectangle::into_styled(...).draw()`
        // entry point every real widget actually uses -- proves the fast
        // path is reachable through, and agrees with, the normal
        // embedded-graphics call chain, not just when called directly.
        let mut direct = FrameBuffer565::new(20, 20);
        direct.fill_solid(&Rectangle::new(Point::new(3, 4), Size::new(6, 5)), Rgb565::GREEN).unwrap();

        let mut via_styled = FrameBuffer565::new(20, 20);
        Rectangle::new(Point::new(3, 4), Size::new(6, 5))
            .into_styled(PrimitiveStyle::with_fill(Rgb565::GREEN))
            .draw(&mut via_styled)
            .unwrap();

        for y in 0..20 {
            for x in 0..20 {
                assert_eq!(direct.pixel(Point::new(x, y)), via_styled.pixel(Point::new(x, y)), "pixel ({x}, {y})");
            }
        }
    }

    #[test]
    fn clear_matches_a_full_bounding_box_fill_solid() {
        let mut cleared = FrameBuffer565::new(7, 4);
        cleared.clear(Rgb565::CSS_ORANGE).unwrap();

        let mut filled = FrameBuffer565::new(7, 4);
        filled.fill_solid(&Rectangle::new(Point::zero(), Size::new(7, 4)), Rgb565::CSS_ORANGE).unwrap();

        for y in 0..4 {
            for x in 0..7 {
                assert_eq!(cleared.pixel(Point::new(x, y)), filled.pixel(Point::new(x, y)));
            }
        }
    }

    #[test]
    fn fill_contiguous_writes_row_major_order_matching_the_trait_contract() {
        let mut fb = FrameBuffer565::new(4, 3);
        let colors = [
            Rgb565::RED,
            Rgb565::GREEN,
            Rgb565::BLUE,
            Rgb565::WHITE,
            Rgb565::CSS_ORANGE,
            Rgb565::CSS_PURPLE,
        ];
        fb.fill_contiguous(&Rectangle::new(Point::new(1, 1), Size::new(3, 2)), colors).unwrap();

        // Row 0 and column 0 untouched.
        for x in 0..4 {
            assert_eq!(fb.pixel(Point::new(x, 0)), Rgb565::BLACK);
        }
        assert_eq!(fb.pixel(Point::new(0, 1)), Rgb565::BLACK);
        assert_eq!(fb.pixel(Point::new(0, 2)), Rgb565::BLACK);

        // Row-major within the area.
        assert_eq!(fb.pixel(Point::new(1, 1)), Rgb565::RED);
        assert_eq!(fb.pixel(Point::new(2, 1)), Rgb565::GREEN);
        assert_eq!(fb.pixel(Point::new(3, 1)), Rgb565::BLUE);
        assert_eq!(fb.pixel(Point::new(1, 2)), Rgb565::WHITE);
        assert_eq!(fb.pixel(Point::new(2, 2)), Rgb565::CSS_ORANGE);
        assert_eq!(fb.pixel(Point::new(3, 2)), Rgb565::CSS_PURPLE);
    }

    #[test]
    fn fill_contiguous_clips_horizontally_but_stays_aligned_with_the_next_row() {
        // Area's left edge is off-canvas by one column; the *first* color
        // of each row must still be consumed (and discarded) to keep the
        // second row's colors aligned with the second row's on-canvas
        // columns, exactly like `area.points().zip(colors)` would.
        let mut fb = FrameBuffer565::new(3, 2);
        let colors = [Rgb565::RED, Rgb565::GREEN, Rgb565::BLUE, Rgb565::WHITE, Rgb565::CSS_ORANGE, Rgb565::CSS_PURPLE];
        fb.fill_contiguous(&Rectangle::new(Point::new(-1, 0), Size::new(3, 2)), colors).unwrap();

        // Row 0: columns -1, 0, 1 map to RED (dropped), GREEN, BLUE.
        assert_eq!(fb.pixel(Point::new(0, 0)), Rgb565::GREEN);
        assert_eq!(fb.pixel(Point::new(1, 0)), Rgb565::BLUE);
        // Row 1: columns -1, 0, 1 map to WHITE (dropped), CSS_ORANGE, CSS_PURPLE.
        assert_eq!(fb.pixel(Point::new(0, 1)), Rgb565::CSS_ORANGE);
        assert_eq!(fb.pixel(Point::new(1, 1)), Rgb565::CSS_PURPLE);
    }

    #[test]
    fn fill_contiguous_stops_cleanly_when_colors_runs_out_mid_row() {
        let mut fb = FrameBuffer565::new(4, 2);
        // Only 3 colors for a 4x2 = 8-pixel area: should draw the first
        // 3 pixels of row 0 and stop, without panicking or drawing
        // garbage into the rest.
        let colors = [Rgb565::RED, Rgb565::GREEN, Rgb565::BLUE];
        fb.fill_contiguous(&Rectangle::new(Point::zero(), Size::new(4, 2)), colors).unwrap();

        assert_eq!(fb.pixel(Point::new(0, 0)), Rgb565::RED);
        assert_eq!(fb.pixel(Point::new(1, 0)), Rgb565::GREEN);
        assert_eq!(fb.pixel(Point::new(2, 0)), Rgb565::BLUE);
        assert_eq!(fb.pixel(Point::new(3, 0)), Rgb565::BLACK);
        for x in 0..4 {
            assert_eq!(fb.pixel(Point::new(x, 1)), Rgb565::BLACK);
        }
    }

    #[test]
    fn fill_contiguous_fully_off_canvas_consumes_colors_but_draws_nothing() {
        let mut fb = FrameBuffer565::new(3, 3);
        let colors = [Rgb565::RED; 4];
        fb.fill_contiguous(&Rectangle::new(Point::new(100, 100), Size::new(2, 2)), colors).unwrap();

        for y in 0..3 {
            for x in 0..3 {
                assert_eq!(fb.pixel(Point::new(x, y)), Rgb565::BLACK);
            }
        }
    }
}
