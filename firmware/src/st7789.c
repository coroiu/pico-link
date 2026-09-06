// ST7789 panel driver -- see st7789.h. Ported from
// firmware-spike/src/main.rs:285-410 (embassy-rp) verbatim IN BEHAVIOUR;
// the fix-by-fix comments below are carried over from that spike because
// they were hard-won there and the mistakes they guard against are just as
// easy to reintroduce in a from-scratch C port.

#include "st7789.h"
#include "watchdog_sup.h"

#include <assert.h>
#include <stdio.h>

#include "hardware/dma.h"
#include "hardware/gpio.h"
#include "pico/stdlib.h"

#include "usb_pump.h"

#define ST7789_CMD_CASET 0x2A
#define ST7789_CMD_RASET 0x2B
#define ST7789_CMD_RAMWR 0x2C
#define ST7789_CMD_MADCTL 0x36
#define ST7789_CMD_COLMOD 0x3A
#define ST7789_CMD_SLPOUT 0x11
#define ST7789_CMD_DISPON 0x29
#define ST7789_CMD_SWRESET 0x01
#define ST7789_CMD_INVON 0x21

static spi_inst_t *s_spi;
static int s_dma_chan = -1;

static inline void cs_low(void) { gpio_put(ST7789_PIN_CS, 0); }
static inline void cs_high(void) { gpio_put(ST7789_PIN_CS, 1); }
static inline void dc_low(void) { gpio_put(ST7789_PIN_DC, 0); }
static inline void dc_high(void) { gpio_put(ST7789_PIN_DC, 1); }

// A single CS-low window covers the command byte AND its parameter bytes
// (DC toggled low-then-high in between, CS held throughout). The ST7789
// datasheet's write sequences show CSX staying asserted for the whole
// command+parameter transaction; raising CS between the command and its
// data (the spike's original bug) makes many panels discard the command or
// ignore the parameters that follow.
static void st7789_command(uint8_t cmd, const uint8_t *params, size_t len) {
    cs_low();
    dc_low();
    spi_write_blocking(s_spi, &cmd, 1);
    if (len > 0) {
        dc_high();
        spi_write_blocking(s_spi, params, len);
    }
    cs_high();
}

// Sets the panel's CASET/RASET address window, inclusive on both ends
// (x1/y1 are the LAST column/row painted, not one-past-the-end -- see the
// comment this superseded in st7789_init_and_fill for why that off-by-one
// matters on a 240px axis). This is the ONE path to the panel's address
// window; every blit (st7789_blit_rect, and the init solid fill) goes
// through it, replacing the retired M1b "set the window once at init"
// contract. Per pico-link-7h5.1's measured transform under MADCTL 0x60,
// the framebuffer rect -> panel window mapping is IDENTITY -- no axis
// swap, no mirror, no offset, despite MV being set -- so (x0,y0,x1,y1) are
// passed straight through to CASET/RASET with no correction term.
static void st7789_set_window(uint16_t x0, uint16_t y0, uint16_t x1, uint16_t y1) {
    uint8_t caset_params[4] = {(uint8_t)(x0 >> 8), (uint8_t)(x0 & 0xff), (uint8_t)(x1 >> 8), (uint8_t)(x1 & 0xff)};
    st7789_command(ST7789_CMD_CASET, caset_params, sizeof(caset_params));
    uint8_t raset_params[4] = {(uint8_t)(y0 >> 8), (uint8_t)(y0 & 0xff), (uint8_t)(y1 >> 8), (uint8_t)(y1 & 0xff)};
    st7789_command(ST7789_CMD_RASET, raset_params, sizeof(raset_params));
}

void st7789_init(spi_inst_t *spi) {
    s_spi = spi;

    gpio_init(ST7789_PIN_DC);
    gpio_set_dir(ST7789_PIN_DC, GPIO_OUT);
    gpio_init(ST7789_PIN_CS);
    gpio_set_dir(ST7789_PIN_CS, GPIO_OUT);
    gpio_put(ST7789_PIN_CS, 1);
    gpio_init(ST7789_PIN_RST);
    gpio_set_dir(ST7789_PIN_RST, GPIO_OUT);
    gpio_init(ST7789_PIN_BL);
    gpio_set_dir(ST7789_PIN_BL, GPIO_OUT);
    gpio_put(ST7789_PIN_BL, 0);

    gpio_set_function(ST7789_PIN_SCK, GPIO_FUNC_SPI);
    gpio_set_function(ST7789_PIN_MOSI, GPIO_FUNC_SPI);

    uint actual_baud = spi_init(s_spi, ST7789_INIT_BAUDRATE_HZ);
    pl_log("st7789: requested %d Hz, actual %u Hz\r\n", ST7789_INIT_BAUDRATE_HZ, actual_baud);
    spi_set_format(s_spi, 8, SPI_CPOL_0, SPI_CPHA_0, SPI_MSB_FIRST);

    s_dma_chan = dma_claim_unused_channel(true);
}

void st7789_init_and_fill(spi_inst_t *spi, uint16_t color) {
    (void)spi; // already stashed by st7789_init; kept as a parameter so the
               // caller's intent (which SPI instance) is visible at the
               // call site without a second global to keep in sync.

    // Hardware reset: active low.
    gpio_put(ST7789_PIN_RST, 1);
    sleep_ms(10);
    gpio_put(ST7789_PIN_RST, 0);
    sleep_ms(10);
    gpio_put(ST7789_PIN_RST, 1);
    sleep_ms(120);

    st7789_command(ST7789_CMD_SWRESET, NULL, 0);
    sleep_ms(150);

    st7789_command(ST7789_CMD_SLPOUT, NULL, 0);
    sleep_ms(120);

    uint8_t colmod_param = 0x55; // 16 bits/pixel, RGB565
    st7789_command(ST7789_CMD_COLMOD, &colmod_param, 1);
    // MADCTL = 0x60 (MX | MV, no MY) -- the only value of the four pure-
    // rotation candidates {0x00, 0x60, 0xA0, 0xC0} that this specific panel
    // renders correctly. Corrected 2026-08-28 (bead pico-link-zzq), a
    // follow-up to pico-link-g7o: that bead's 0xA0 was chosen from a
    // low-resolution webcam photo of TEXT, which cannot distinguish a
    // 180-degree rotation from a horizontal mirror -- both read "reversed".
    // This time was measured with an ASYMMETRIC CORNER TEST PATTERN (solid
    // RED/GREEN/BLUE/WHITE quadrants, RED carrying N dots to identify the
    // candidate) cycled through all four MADCTL values with the SAME
    // physical setup, so the four results are directly comparable:
    //   0x00 -- clean full-frame image, but a 90-degree-CCW rotation of
    //           0x60's result (see below), not upright in this mounting.
    //   0x60 -- clean full-frame image, IDENTITY mapping: the software's
    //           top-left/top-right/bottom-left/bottom-right quadrants land
    //           on the matching physical corners with no rotation and no
    //           mirroring. Confirmed upright, cable exiting LEFT, matching
    //           Andreas's independent by-hand inspection (the bead's "known
    //           good" baseline, established BEFORE any camera was involved).
    //   0xA0, 0xC0 -- NOT clean rotations on this panel: both render as a
    //           corrupted three-band image (~80px / ~120px / ~40px column
    //           bands instead of a clean 120/120 split), reproduced
    //           identically across two full test cycles and unaffected by
    //           reissuing CASET/RASET after the MADCTL write (ruled out a
    //           stale address-counter as the cause) and by sweeping a CASET
    //           column offset 0/20/40/60/80 (ruled out a simple GRAM-offset
    //           fix at those values). Both corrupted candidates are exactly
    //           the two with MY set, so the defect tracks the MY (row scan
    //           direction) bit specifically, not MX or MV.
    //
    // NET RESULT: no MADCTL value among the four pure rotations gives
    // "upright, cable exiting right" on this hardware -- that would need a
    // clean 180-degree rotation from 0x60, and both candidates that should
    // supply it are the corrupted ones. Reverting to 0x60 (upright, cable
    // LEFT) rather than shipping 0xA0's mirror or a known-corrupted 0xC0/
    // 0xA0 image. The cable-exits-right requirement is tracked as follow-up
    // work (needs the MY-addressing defect fixed first, a different bug
    // than "pick the right MADCTL byte") -- see bead comments for the full
    // four-candidate photo evidence.
    //
    // This panel is square (240x240), so the CASET/RASET window below is
    // unaffected by MV's row/column exchange for the WORKING candidates
    // (0x00, 0x60) -- no per-rotation offset is needed for those. If a
    // future fix for the MY defect needs one, it is added to
    // caset_params/raset_params below, NOT here.
    uint8_t madctl_param = 0x60;
    st7789_command(ST7789_CMD_MADCTL, &madctl_param, 1);
    // The Waveshare Pico-LCD-1.3 panel needs display inversion on, or
    // colours render photo-negative. Wrong colour alone wouldn't explain an
    // all-white screen, but it will bite the moment the fill itself is
    // fixed, so it goes in now.
    st7789_command(ST7789_CMD_INVON, NULL, 0);

    // RETIRED M1b CONTRACT: this used to set CASET/RASET ONCE HERE, full
    // 0..239 on both axes, and every subsequent blit (st7789_blit_
    // framebuffer) just reissued a bare RAMWR and relied on the panel's own
    // write-pointer wraparound. Superseded by st7789_blit_rect
    // (pico-link-7h5.7): every blit now sets its own window via
    // st7789_set_window, including this init fill, so a window bug can't
    // hide behind a full-frame path that never re-addresses the panel.
    st7789_set_window(0, 0, ST7789_WIDTH - 1, ST7789_HEIGHT - 1);

    // RAMWR's pixel stream is itself parameter data to the command, so it
    // must stay inside the same CS-low window as the command byte too --
    // same fix as above, just with a big params stream instead of a few
    // register bytes. Written directly rather than through st7789_command
    // to avoid building a 115200-byte buffer just to prove a solid fill.
    cs_low();
    dc_low();
    uint8_t ramwr = ST7789_CMD_RAMWR;
    spi_write_blocking(s_spi, &ramwr, 1);
    dc_high();
    uint8_t px[2] = {(uint8_t)(color >> 8), (uint8_t)(color & 0xff)};
    for (uint32_t i = 0; i < (uint32_t)ST7789_WIDTH * (uint32_t)ST7789_HEIGHT; i++) {
        spi_write_blocking(s_spi, px, 2);
    }
    cs_high();

    st7789_command(ST7789_CMD_DISPON, NULL, 0);
    sleep_ms(20);

    // Backlight on. The panel is otherwise driven correctly with the
    // backlight off, but nothing would be visible.
    gpio_put(ST7789_PIN_BL, 1);
}

void st7789_blit_rect(const uint16_t *fb, uint16_t stride, uint16_t x, uint16_t y, uint16_t w, uint16_t h) {
    pl_wdt_mark(PL_WDT_CP_BLIT_ENTER);

    // Phase 1 (pico-link-7h5.7): full-width row bands only. Restricting to
    // x==0, w==stride keeps the source pixels contiguous in the
    // framebuffer (fb + y*stride, count h*stride) so this stays ONE DMA
    // transfer with CASET fixed and only RASET varying -- no per-row DMA,
    // no chaining. Column-clipped blit is pico-link-7h5.11, explicitly out
    // of scope here; the assert below is the boundary of what this
    // function is allowed to do until that bead lands.
    assert(x == 0 && w == stride);

    const uint16_t x1 = (uint16_t)(x + w - 1);
    const uint16_t y1 = (uint16_t)(y + h - 1);
    st7789_set_window(x, y, x1, y1);

    const uint16_t *px = fb + (uint32_t)y * stride;
    uint32_t pixel_count = (uint32_t)h * stride;

    // RAMWR in 8-bit mode (a single command byte), matching every other
    // command write. st7789_set_window above just reprogrammed CASET/RASET
    // and left the panel's internal write pointer at the window's start,
    // so a bare RAMWR (re)arms the write -- no separate address-counter
    // reset needed.
    cs_low();
    dc_low();
    uint8_t ramwr = ST7789_CMD_RAMWR;
    spi_write_blocking(s_spi, &ramwr, 1);
    dc_high();

    // Switch to 16-bit SPI frames for the pixel burst: each 16-bit-wide
    // DMA write to spi_get_hw(spi)->dr below pushes one RGB565 pixel VALUE
    // into the PL022 TX FIFO in a single frame; the PL022 then shifts that
    // 16-bit value out MSB-first per its DSS=16 configuration, which is
    // exactly the same byte order st7789_init_and_fill's manual two-byte
    // write above (color >> 8, then color & 0xff) already sends.
    //
    // NO byte-swap here -- deliberately corrected from the original M1b
    // design, which called for channel_config_set_bswap(true). A same-width
    // (16-bit-to-16-bit) DMA transfer on this bus is endianness-transparent:
    // it copies the bit pattern of the source halfword into the destination
    // halfword without reinterpreting it as a byte stream, so it already
    // reconstructs the correct native RGB565 VALUE with no swap needed --
    // identical in effect to the blocking path above. Evidence (bd
    // pico-link-cz0.2, coordinator's webcam review): a solid RED/GREEN/BLUE
    // fill via the blocking path (this function's sibling, no DMA, no
    // bswap) photographed correctly on the real panel, proving INVON/MADCTL/
    // CS-framing are all fine; the REAL UI content, rendered via THIS DMA
    // path with bswap enabled, photographed as almost exactly the bitwise
    // inversion (~value) of the emulator's reference colours -- not the
    // scrambled-but-not-inverted result plain byte-reordering of a 16-bit
    // value would produce (checked numerically against the actual
    // background/header RGB565 constants in core/src/render/theme.rs).
    // Removing the unneeded swap converges this path back to the
    // already-proven-correct blocking-path behaviour.
    spi_set_format(s_spi, 16, SPI_CPOL_0, SPI_CPHA_0, SPI_MSB_FIRST);

    dma_channel_config c = dma_channel_get_default_config(s_dma_chan);
    channel_config_set_transfer_data_size(&c, DMA_SIZE_16);
    channel_config_set_dreq(&c, spi_get_dreq(s_spi, true));
    channel_config_set_read_increment(&c, true);
    channel_config_set_write_increment(&c, false);
    channel_config_set_bswap(&c, false);

    pl_wdt_mark(PL_WDT_CP_BLIT_DMA_WAIT);
    dma_channel_configure(s_dma_chan, &c, &spi_get_hw(s_spi)->dr, px, pixel_count, true);
    dma_channel_wait_for_finish_blocking(s_dma_chan);

    // Wait for the SPI peripheral itself to finish shifting out the last
    // frame (the DMA engine only guarantees the FIFO was fully written,
    // not that the wire transfer completed) before dropping CS -- matches
    // the datasheet's requirement that CSX stay asserted for the whole
    // transaction.
    pl_wdt_mark(PL_WDT_CP_BLIT_SPI_DRAIN);
    while (spi_is_busy(s_spi)) {
        tight_loop_contents();
    }

    cs_high();

    // Switch back to 8-bit framing so the next command write (this
    // function's own next RAMWR, or any other register command) doesn't
    // silently get sent as a 16-bit frame.
    spi_set_format(s_spi, 8, SPI_CPOL_0, SPI_CPHA_0, SPI_MSB_FIRST);
    pl_wdt_mark(PL_WDT_CP_BLIT_EXIT);
}

void st7789_blit_framebuffer(spi_inst_t *spi, const uint16_t *px, uint32_t pixel_count) {
    (void)spi; // already stashed by st7789_init; see st7789_init_and_fill's
               // matching comment.
    // One-line wrapper over st7789_blit_rect (pico-link-7h5.7), DELIBERATELY,
    // so the full-frame path exercises the exact same window-setting code as
    // every future partial blit, on every single frame -- a window bug
    // cannot hide until the day a partial-rect blit (e.g. the OUT meter)
    // first draws. pixel_count is always ST7789_WIDTH * ST7789_HEIGHT for
    // this call; assert it rather than silently truncating/overrunning if a
    // caller ever passes something else.
    assert(pixel_count == (uint32_t)ST7789_WIDTH * (uint32_t)ST7789_HEIGHT);
    st7789_blit_rect(px, ST7789_WIDTH, 0, 0, ST7789_WIDTH, ST7789_HEIGHT);
}

void st7789_set_madctl(uint8_t madctl_param) {
    st7789_command(ST7789_CMD_MADCTL, &madctl_param, 1);
}

void st7789_set_caset_offset(uint16_t x0) {
    uint16_t x1 = x0 + (ST7789_WIDTH - 1);
    uint8_t caset_params[4] = {(uint8_t)(x0 >> 8), (uint8_t)(x0 & 0xff), (uint8_t)(x1 >> 8), (uint8_t)(x1 & 0xff)};
    st7789_command(ST7789_CMD_CASET, caset_params, sizeof(caset_params));
}

void st7789_reset_window(void) {
    const uint16_t x_end = ST7789_WIDTH - 1;
    const uint16_t y_end = ST7789_HEIGHT - 1;
    uint8_t caset_params[4] = {0x00, 0x00, (uint8_t)(x_end >> 8), (uint8_t)(x_end & 0xff)};
    st7789_command(ST7789_CMD_CASET, caset_params, sizeof(caset_params));
    uint8_t raset_params[4] = {0x00, 0x00, (uint8_t)(y_end >> 8), (uint8_t)(y_end & 0xff)};
    st7789_command(ST7789_CMD_RASET, raset_params, sizeof(raset_params));
}

void st7789_diag_fill_window(uint16_t x0, uint16_t y0, uint16_t x1, uint16_t y1, uint16_t color) {
    uint8_t caset_params[4] = {(uint8_t)(x0 >> 8), (uint8_t)(x0 & 0xff), (uint8_t)(x1 >> 8), (uint8_t)(x1 & 0xff)};
    st7789_command(ST7789_CMD_CASET, caset_params, sizeof(caset_params));
    uint8_t raset_params[4] = {(uint8_t)(y0 >> 8), (uint8_t)(y0 & 0xff), (uint8_t)(y1 >> 8), (uint8_t)(y1 & 0xff)};
    st7789_command(ST7789_CMD_RASET, raset_params, sizeof(raset_params));

    uint32_t count = (uint32_t)(x1 - x0 + 1) * (uint32_t)(y1 - y0 + 1);
    cs_low();
    dc_low();
    uint8_t ramwr = ST7789_CMD_RAMWR;
    spi_write_blocking(s_spi, &ramwr, 1);
    dc_high();
    uint8_t px[2] = {(uint8_t)(color >> 8), (uint8_t)(color & 0xff)};
    for (uint32_t i = 0; i < count; i++) {
        spi_write_blocking(s_spi, px, 2);
    }
    cs_high();
}

void st7789_set_backlight(bool on) {
    // A plain gpio_put -- see st7789.h's doc comment for why this is
    // deliberately not DISPOFF/SLPIN (out of band from SPI1, cannot race
    // st7789_blit_framebuffer's DMA). Idempotent by construction.
    gpio_put(ST7789_PIN_BL, on ? 1 : 0);
}
