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

#include <stdbool.h>
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
// deliberately-conservative 1MHz bring-up value, verified clean on real
// hardware (webcam) at 40MHz and 62MHz -- both landed on the SAME actual
// 37.5MHz (spi_set_baudrate rounds down to the nearest achievable
// clk_peri/(prescale*postdiv); with prescale's 2-254 even-only floor, 75MHz
// (prescale=2, postdiv=1) is the next step up but exceeds a 62MHz request,
// so it wasn't picked). Requesting 75MHz directly hits that divisor exactly
// and is ALSO clean on hardware (blit time roughly halved, 26.9ms -> 13.5ms,
// matching the clock ratio). With prescale's floor of 2 and a 150MHz
// peripheral clock, clk_peri/2 = 75MHz is the SPI hardware ceiling here --
// there is no higher clean divisor to try. st7789_init prints the
// requested-vs-actual Hz achieved (st7789.c) -- re-check that line if this
// peripheral clock configuration ever changes.
#define ST7789_INIT_BAUDRATE_HZ (75 * 1000 * 1000)

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

// Re-issues the MADCTL command with a new parameter byte after init. Used
// by the PL_DIAG_MADCTL_TEST diagnostic (main.c) to cycle candidates
// without a full re-init/reset cycle; not used by normal boot (MADCTL is
// set once inside st7789_init_and_fill). Safe to call any time after
// st7789_init_and_fill has run -- the CASET/RASET window is already the
// full 0..239 square on both axes, which is invariant under MADCTL's
// row/column exchange since the panel is square.
void st7789_set_madctl(uint8_t madctl_param);

// Diagnostic-only (pico-link-zzq): re-issues CASET with an arbitrary
// [x0, x0+239] column window instead of the fixed [0,239] st7789_init_and_fill
// programs. Used to test whether this panel's MY-toggled addressing needs a
// GRAM offset compensation (the classic ST7789-in-a-320-row-GRAM quirk) --
// see the PL_DIAG_MADCTL_TEST block in main.c. Not used by normal boot.
void st7789_set_caset_offset(uint16_t x0);

// Diagnostic-only (pico-link-zzq): re-issues BOTH CASET and RASET to the
// fixed full-frame [0,239]x[0,239] window. Tests the hypothesis that
// changing MADCTL's row/column scan-direction bits at runtime (as the
// PL_DIAG_MADCTL_TEST loop does, unlike normal boot which sets MADCTL once
// before the one-time CASET/RASET in st7789_init_and_fill) leaves the
// panel's internal address counter in a state inconsistent with the new
// scan direction until the window commands are reissued -- st7789_blit_
// framebuffer's RAMWR-only fast path assumes that never needs to happen.
void st7789_reset_window(void);

// Sets the backlight GPIO (GP13) only -- `true` for on, `false` for off.
// Deliberately NOT DISPOFF/SLPIN (the panel-controller sleep commands):
// GP13 is a plain GPIO wired straight to the backlight driver, entirely
// out of band from the SPI1 bus st7789_blit_framebuffer's DMA uses, so
// toggling it can never race an in-flight blit and needs no panel
// re-init on the next wake (see pico-link-i3e /
// .planning/design/2026-09-01-idle-policy-across-the-ffi-seam.md). Safe
// to call every superloop iteration -- idempotent, just a `gpio_put`.
// Requires st7789_init to have run first (it owns GP13's init/direction).
void st7789_set_backlight(bool on);

#endif // PICO_LINK_ST7789_H
