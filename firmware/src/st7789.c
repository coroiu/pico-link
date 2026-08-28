// ST7789 panel driver -- see st7789.h. Ported from
// firmware-spike/src/main.rs:285-410 (embassy-rp) verbatim IN BEHAVIOUR;
// the fix-by-fix comments below are carried over from that spike because
// they were hard-won there and the mistakes they guard against are just as
// easy to reintroduce in a from-scratch C port.

#include "st7789.h"

#include <stdio.h>

#include "hardware/dma.h"
#include "hardware/gpio.h"
#include "pico/stdlib.h"

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
    printf("st7789: requested %d Hz, actual %u Hz\r\n", ST7789_INIT_BAUDRATE_HZ, actual_baud);
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
    uint8_t madctl_param = 0x00;
    st7789_command(ST7789_CMD_MADCTL, &madctl_param, 1);
    // The Waveshare Pico-LCD-1.3 panel needs display inversion on, or
    // colours render photo-negative. Wrong colour alone wouldn't explain an
    // all-white screen, but it will bite the moment the fill itself is
    // fixed, so it goes in now.
    st7789_command(ST7789_CMD_INVON, NULL, 0);

    // CASET/RASET addresses are INCLUSIVE start/end column and row, so the
    // end value for a 240px axis is 239 (0x00EF), not 240 -- 240 (0x00F0)
    // programs a 241px window on a 240px panel. Set once here; every
    // subsequent blit (st7789_blit_framebuffer) just reissues RAMWR and
    // relies on the panel's own write-pointer wraparound, per the M1b
    // design.
    const uint16_t x_end = ST7789_WIDTH - 1;
    const uint16_t y_end = ST7789_HEIGHT - 1;
    uint8_t caset_params[4] = {0x00, 0x00, (uint8_t)(x_end >> 8), (uint8_t)(x_end & 0xff)};
    st7789_command(ST7789_CMD_CASET, caset_params, sizeof(caset_params));
    uint8_t raset_params[4] = {0x00, 0x00, (uint8_t)(y_end >> 8), (uint8_t)(y_end & 0xff)};
    st7789_command(ST7789_CMD_RASET, raset_params, sizeof(raset_params));

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

void st7789_blit_framebuffer(spi_inst_t *spi, const uint16_t *px, uint32_t pixel_count) {
    (void)spi;

    // RAMWR in 8-bit mode (a single command byte), matching every other
    // command write -- the panel's internal write pointer was already
    // wrapped back to the window's start by the previous full-window fill,
    // per the M1b design's "set the window once at init" contract, so no
    // CASET/RASET here, just RAMWR to (re)arm the write.
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

    dma_channel_configure(s_dma_chan, &c, &spi_get_hw(s_spi)->dr, px, pixel_count, true);
    dma_channel_wait_for_finish_blocking(s_dma_chan);

    // Wait for the SPI peripheral itself to finish shifting out the last
    // frame (the DMA engine only guarantees the FIFO was fully written,
    // not that the wire transfer completed) before dropping CS -- matches
    // the datasheet's requirement that CSX stay asserted for the whole
    // transaction.
    while (spi_is_busy(s_spi)) {
        tight_loop_contents();
    }

    cs_high();

    // Switch back to 8-bit framing so the next command write (this
    // function's own next RAMWR, or any other register command) doesn't
    // silently get sent as a 16-bit frame.
    spi_set_format(s_spi, 8, SPI_CPOL_0, SPI_CPHA_0, SPI_MSB_FIRST);
}
