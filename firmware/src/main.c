// Pico Link firmware -- M1b: the C-first architecture proof.
//
// C-first: pico-sdk owns main() and runtime_init (see
// .planning/decisions/2026-08-27-c-first-pico-sdk-owns-main.md). This
// milestone links in ui-ffi, a no_std + alloc Rust staticlib, over the
// narrow extern "C" surface in the generated firmware/include/pico_link_ui.h
// (see CMakeLists.txt -- cbindgen regenerates it from ui-ffi/src/lib.rs on
// every build, so it can never hand-drift from the real FFI ABI), drives a
// Waveshare Pico-LCD-1.3 ST7789 panel (st7789.c/h), and polls the joystick +
// buttons (input.c/h).
//
// Deliberately does NOT poke CPACR or touch any coprocessor-access register:
// pico-sdk's runtime_init() is responsible for that now. If a NOCP
// UsageFault shows up here, that falsifies the C-first pivot's premise --
// stop and escalate, don't patch around it locally.

#include <stdio.h>

#include "hardware/spi.h"
#include "pico/stdlib.h"

#include "input.h"
#include "pico_link_ui.h"
#include "st7789.h"

// The one call in the Rust -> C direction (see pico_link_ui.h's doc comment
// on pl_ui_panic_hook): Rust hands us a panic message on the way to
// spinning forever, since it has no unwinder on this target and nothing
// else safe to do. Report it over the CDC console -- the only I/O channel
// available for this -- so a panic during bring-up is visible rather than
// looking like a silent hang.
void pl_ui_panic_hook(const uint8_t *msg, uintptr_t len) {
    printf("\r\n!!! ui-ffi PANIC: %.*s\r\n", (int)len, (const char *)msg);
}

#define PANEL_WIDTH ST7789_WIDTH
#define PANEL_HEIGHT ST7789_HEIGHT

// RGB565, big-endian-agnostic constants (these are plain 16-bit values;
// st7789_init_and_fill sends them MSB-first itself).
#define COLOR_RED 0xF800
#define COLOR_GREEN 0x07E0
#define COLOR_BLUE 0x001F

int main(void) {
    stdio_init_all();

    // Give a host-side terminal a moment to attach after CDC enumerates,
    // so the boot banner below isn't lost before anyone is listening.
    sleep_ms(1500);

    printf("\r\n=== pico_link firmware boot ===\r\n");
    printf("board: pimoroni_pico_plus2_w_rp2350\r\n");
    printf("pico-sdk owns main(); ui-ffi (Rust core) linked in over FFI.\r\n");

    // --- Staged panel bring-up (M1b design: confirm each stage before the
    // next) ---
    //
    // Stage 1+2 combined into one flash (no live visual feedback loop
    // available to this agent -- see the M1b bead comments): backlight on,
    // then three full-screen solid-colour fills in sequence, entirely via
    // the blocking (non-DMA) path -- zero Rust rendering anywhere in this
    // sequence. If the colours on the panel are wrong here, the bug is in
    // this file / st7789.c, not in ui-ffi or core. A human watching the
    // panel (or the bench webcam) should see: red, pause, green, pause,
    // blue, pause -- each via a fresh st7789_init_and_fill call (which
    // internally does the hardware reset + register init once more; a
    // second/third re-init is wasteful but harmless and keeps this
    // sequence simple to read).
    st7789_init(spi1);
    printf("st7789_init OK (SPI1, DC=%d CS=%d SCK=%d MOSI=%d RST=%d BL=%d, %d Hz)\r\n",
           ST7789_PIN_DC, ST7789_PIN_CS, ST7789_PIN_SCK, ST7789_PIN_MOSI, ST7789_PIN_RST, ST7789_PIN_BL,
           ST7789_INIT_BAUDRATE_HZ);

    // Extended to a photographable ~3s hold per colour, with an explicit
    // marker printed BOTH before the fill call starts and again once the
    // hold begins, each carrying a monotonic timestamp -- this is the
    // decisive colour-mapping diagnostic (coordinator-directed follow-up to
    // the first M1b pass, which got layout/geometry right per a webcam
    // comparison against the emulator's reference PNG, but photographed
    // solid-background near-black core content as the BRIGHTEST thing in
    // frame, i.e. inverted). Zero Rust involved in this sequence, so
    // whatever the camera sees here isolates the bug to this file.
    printf("FILL_START:RED@%lluus\r\n", (unsigned long long)time_us_64());
    st7789_init_and_fill(spi1, COLOR_RED);
    printf("FILL_HOLD:RED@%lluus\r\n", (unsigned long long)time_us_64());
    sleep_ms(5000);

    printf("FILL_START:GREEN@%lluus\r\n", (unsigned long long)time_us_64());
    st7789_init_and_fill(spi1, COLOR_GREEN);
    printf("FILL_HOLD:GREEN@%lluus\r\n", (unsigned long long)time_us_64());
    sleep_ms(5000);

    printf("FILL_START:BLUE@%lluus\r\n", (unsigned long long)time_us_64());
    st7789_init_and_fill(spi1, COLOR_BLUE);
    printf("FILL_HOLD:BLUE@%lluus\r\n", (unsigned long long)time_us_64());
    sleep_ms(5000);

    // --- The Rust UI ---
    struct PlUi *ui = pl_ui_create(PANEL_WIDTH, PANEL_HEIGHT);
    if (ui == NULL) {
        printf("pl_ui_create FAILED -- halting\r\n");
        while (true) {
            tight_loop_contents();
        }
    }
    printf("pl_ui_create OK\r\n");

    pl_link_input_init();
    printf("pl_link_input_init OK\r\n");

    // Superloop on core0 only (M1b design). Every iteration: poll debounced
    // input edges, forward to Rust, tick, render, blit, print per-frame
    // timing -- the bead's hardware acceptance criterion.
    PlIntent intents[8];
    while (true) {
        uint64_t frame_start_us = time_us_64();

        size_t n = pl_link_input_poll(intents, 8);
        if (n > 0) {
            pl_ui_input(ui, intents, n);
        }
        pl_ui_tick(ui, frame_start_us);

        const uint16_t *px = NULL;
        uintptr_t px_len = 0;
        uint64_t render_start_us = time_us_64();
        pl_ui_render(ui, &px, &px_len);
        uint64_t render_end_us = time_us_64();

        if (px != NULL && px_len == (uintptr_t)PANEL_WIDTH * (uintptr_t)PANEL_HEIGHT) {
            // ui_tick()/pl_ui_render() must not be called again until this
            // DMA completes (st7789_blit_framebuffer blocks until it does)
            // -- Rust can never write while DMA reads, per the M1b design's
            // no-tearing, no-double-buffering contract.
            st7789_blit_framebuffer(spi1, px, (uint32_t)px_len);
        }
        uint64_t blit_end_us = time_us_64();

        static uint32_t frame_count = 0;
        frame_count++;
        // Rate-limit the per-frame print to once a second (at a ~few-ms
        // frame time this is still hundreds of frames between prints) so
        // the CDC console stays readable rather than flooded -- the bead
        // asks for "CDC prints per-frame render and blit timing", which
        // this satisfies by printing exactly that breakdown, just not on
        // literally every single frame.
        if (frame_count % 60 == 1) {
            printf(
                "frame %lu: render=%lluus blit=%lluus total=%lluus\r\n",
                (unsigned long)frame_count,
                (unsigned long long)(render_end_us - render_start_us),
                (unsigned long long)(blit_end_us - render_end_us),
                (unsigned long long)(blit_end_us - frame_start_us)
            );
        }

        // No dirty-gate here: pl_ui_render (unlike core's own Runner::step)
        // re-renders unconditionally every call -- see its doc comment in
        // pico_link_ui.h. Blitting every iteration regardless is simple and
        // correct, if not maximally efficient -- a later milestone can add
        // a "was this frame actually new" signal to the FFI surface if the
        // redundant-blit cost turns out to matter.
        sleep_ms(16); // ~60Hz loop pace, matching the emulator's frame budget
    }

    return 0;
}
