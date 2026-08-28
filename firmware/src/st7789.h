// ST7789 panel driver for the Waveshare Pico-LCD-1.3 (240x240, 4-wire SPI).
//
// Ported from firmware-spike/src/main.rs:285-410's embassy-rp version,
// verbatim IN BEHAVIOUR -- same command sequence, same hard-won fixes (CS
// held low across command AND parameters, INVON mandatory on this panel,
// CASET/RASET ends inclusive). See st7789.c for the fix-by-fix commentary
// carried over from that spike.
//
// Pins per CLAUDE.md (Waveshare Pico-LCD-1.3):
//   GP8 DC, GP9 CS, GP10 SCK, GP11 MOSI, GP12 RST, GP13 BL.
#ifndef PICO_LINK_ST7789_H
#define PICO_LINK_ST7789_H

#include <stdint.h>

#include "hardware/spi.h"

#define ST7789_WIDTH 240
#define ST7789_HEIGHT 240

#define ST7789_PIN_DC 8
#define ST7789_PIN_CS 9
#define ST7789_PIN_SCK 10
#define ST7789_PIN_MOSI 11
#define ST7789_PIN_RST 12
#define ST7789_PIN_BL 13

// CS framing is confirmed on this board (pico-link-cz0.2/pico-link-14l) --
// the earlier white screen at 20MHz predated the CS-held-low-across-
// parameters fix and is not evidence against a fast clock. Raised from the
// deliberately-conservative 1MHz bring-up value one step at a time,
// verifying a clean panel + CDC frame-timing line at each rate
// (pico-link-14l); step back down and note the highest clean rate if a
// given value misbehaves.
#define ST7789_INIT_BAUDRATE_HZ (40 * 1000 * 1000)

void st7789_init(spi_inst_t *spi);

// Hardware reset, register init, and a full-screen solid-colour fill
// (blocking, one SPI byte-write at a time -- st7789_blit_framebuffer below
// is the DMA fast path once the framebuffer stage is reached). `color` is a
// native RGB565 value; the byte order sent over the wire is big-endian
// (MSB first) regardless of host endianness, matching the panel's expected
// wire format.
void st7789_init_and_fill(spi_inst_t *spi, uint16_t color);

// Blits `pixel_count` native-endian (little-endian on this target) RGB565
// pixels from `px` to the panel via DMA, byte-swapping to the panel's
// big-endian wire format in hardware on the way out (channel_config_set_
// bswap) -- this is what removes the need for a 115KB byte-swapped staging
// buffer; see ui-ffi's FrameBuffer565::as_raw_u16 doc comment. Assumes
// st7789_init_and_fill already ran (window set once at init, per the M1b
// design) and blocks until the DMA transfer completes -- callers must not
// call into the Rust side again (pl_ui_render etc, which would overwrite
// the framebuffer memory this DMA is reading from) until this returns.
void st7789_blit_framebuffer(spi_inst_t *spi, const uint16_t *px, uint32_t pixel_count);

#endif // PICO_LINK_ST7789_H
