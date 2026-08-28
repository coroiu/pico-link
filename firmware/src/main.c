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

#include "btstack.h"
#include "hardware/spi.h"
#include "pico/cyw43_arch.h"
#include "pico/stdlib.h"
#include "tusb.h"

#include "bt.h"
#include "input.h"
#include "pico_link_ui.h"
#include "st7789.h"
#include "usb_audio.h"

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
    // M3: this project now owns TinyUSB's init/task loop (see
    // CMakeLists.txt linking `tinyusb_device`, and tusb_config.h/
    // usb_descriptors.c/.h's module docs). stdio_init_all() below still
    // wires up the CDC console through pico_stdio_usb's stdio_usb.c, but
    // that code now ASSERTS tud_inited() rather than calling tusb_init()
    // itself -- so this call has to come first, and tud_task() has to be
    // pumped by this file's superloop from here on (pico_stdio_usb's own
    // background low-priority-IRQ tud_task() driver is compiled out once
    // LIB_TINYUSB_DEVICE is set).
    tusb_rhport_init_t dev_init = {
        .role = TUSB_ROLE_DEVICE,
        .speed = TUSB_SPEED_AUTO,
    };
    tusb_init(BOARD_TUD_RHPORT, &dev_init);

    stdio_init_all();

    // Give a host-side terminal a moment to attach after CDC enumerates,
    // so the boot banner below isn't lost before anyone is listening.
    sleep_ms(1500);

    printf("\r\n=== pico_link firmware boot ===\r\n");
    printf("board: pimoroni_pico_plus2_w_rp2350\r\n");
    printf("pico-sdk owns main(); ui-ffi (Rust core) linked in over FFI.\r\n");

    // Panel bring-up itself is proven on real hardware as of M1b (bd
    // pico-link-cz0.2). An earlier version of this M2 session's main.c
    // dropped the RGB staging fills entirely, on the mistaken assumption
    // they were purely a bring-up diagnostic -- they are NOT. st7789_init()
    // only brings up the GPIO/SPI peripheral; the actual panel bring-up
    // (hardware reset, SLPOUT, COLMOD, MADCTL, INVON, the one-time CASET/
    // RASET address window st7789_blit_framebuffer's own doc comment
    // depends on, DISPON, and turning the backlight on) all lives inside
    // st7789_init_and_fill -- see st7789.c. Dropping that call left the
    // panel held in reset with the backlight off: st7789_blit_framebuffer
    // was still dutifully DMA'ing pixels, just into a panel that was never
    // taken out of reset. This cost a real debugging detour during M2's
    // Bluetooth bring-up (bd pico-link-cz0.3) before the camera+CDC
    // evidence pointed back here, not at BT at all. One fill call, no hold
    // sleep -- the multi-second colour-hold loop is still gone (a real
    // simplification, not the bug), just not the fill itself.
    st7789_init(spi1);
    printf("st7789_init OK (SPI1, DC=%d CS=%d SCK=%d MOSI=%d RST=%d BL=%d, %d Hz)\r\n",
           ST7789_PIN_DC, ST7789_PIN_CS, ST7789_PIN_SCK, ST7789_PIN_MOSI, ST7789_PIN_RST, ST7789_PIN_BL,
           ST7789_INIT_BAUDRATE_HZ);
    st7789_init_and_fill(spi1, 0x0000); // black -- the Rust UI's first render replaces this immediately
    printf("st7789_init_and_fill OK -- panel out of reset, backlight on\r\n");

#ifdef PL_DIAG_COLOR_TEST
    // Reusable diagnostic (undefined by default -- pass -DPL_DIAG_COLOR_TEST
    // to CMAKE_C_FLAGS/CMAKE_CXX_FLAGS to enable): an exposure-immune
    // relative colour test, first run for pico-link-14l. Top half of the
    // panel in the theme BACKGROUND constant (core/src/render/theme.rs:47,
    // Rgb565::new(1,4,4) -> raw 0x0884), bottom half in pure white 0xFFFF,
    // one photograph, compare the two halves to each other WITHIN that
    // frame -- absolute webcam RGB swings with auto white balance between
    // captures, but two patches in one frame are directly comparable. Halts
    // here (never reaches BT/the Rust UI) so the pattern stays on screen for
    // the camera. Result banked on pico-link-14l: BACKGROUND photographed as
    // a clearly saturated blue (sampled ~RGB(128,183,243), channel spread
    // ~115) against the white half's near-neutral ~RGB(207,214,221),
    // channel spread ~14 -- BACKGROUND is nowhere near white, so colour is
    // not washed out/inverted to white on this panel.
    static uint16_t s_color_test_buf[PANEL_WIDTH * PANEL_HEIGHT];
    for (int y = 0; y < PANEL_HEIGHT; y++) {
        uint16_t color = (y < PANEL_HEIGHT / 2) ? 0x0884 : 0xFFFF;
        for (int x = 0; x < PANEL_WIDTH; x++) {
            s_color_test_buf[y * PANEL_WIDTH + x] = color;
        }
    }
    st7789_blit_framebuffer(spi1, s_color_test_buf, PANEL_WIDTH * PANEL_HEIGHT);
    printf("PL_DIAG_COLOR_TEST: blitted half-BACKGROUND(0x0884)/half-WHITE(0xFFFF) -- halting\r\n");
    while (true) {
        tight_loop_contents();
    }
#endif

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

    // --- M2: bring the radio up ---
    //
    // cyw43_arch_init() claims the SPI/PIO/DMA resources the bead's banked
    // hardware evidence already proved work (read32_swapped round-trip),
    // then pl_bt_init() registers BTstack's packet handler and asks for
    // HCI_POWER_ON -- see bt.c's module doc for why this is expected to
    // succeed where three earlier Rust-first sessions could not.
    //
    // Deliberately core0-only, matching USBPods' own working reference
    // (read from a scratch clone outside this repo -- its core1 is
    // launched for nothing BTstack/cyw43-related; core1 never runs there
    // either) rather than inventing a different core split, per this
    // bead's 15-minute precondition.
#ifndef PL_DIAG_SKIP_BT
    if (cyw43_arch_init()) {
        printf("cyw43_arch_init FAILED -- halting\r\n");
        while (true) {
            tight_loop_contents();
        }
    }
    printf("cyw43_arch_init OK\r\n");

    pl_bt_init(ui);
#else
    printf("PL_DIAG_SKIP_BT set -- skipping cyw43_arch_init/pl_bt_init\r\n");
#endif

    // Superloop on core0 only (M1b design, unchanged by M2). Every
    // iteration: poll debounced input edges, forward to Rust, tick,
    // render, blit, print per-frame timing, then poll one queued
    // Bluetooth command -- all still plain C driving the same FFI surface,
    // no new threading model. BTstack's own work happens in the cyw43
    // background IRQ (pico_cyw43_arch_threadsafe_background, linked in
    // CMakeLists.txt), not in this loop.
    PlIntent intents[8];
    uint64_t audio_report_start_us = time_us_64();
    uint32_t audio_report_start_bytes = pl_usb_audio_pcm_bytes_total();
    while (true) {
        // M3: service TinyUSB every iteration -- this is what used to
        // happen for free inside pico_stdio_usb's own background IRQ
        // before this project took over tud_init()/tud_task() (see the
        // comment on tusb_init() above). Draining the audio OUT FIFO right
        // after is this milestone's "consumer": no I2S/LDAC yet (that is
        // M4), just proving PCM arrives correctly and at the right rate.
        tud_task();
        pl_usb_audio_task();

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

#ifndef PL_DIAG_SKIP_BT
        pl_bt_poll_commands(ui);
#endif

        // M3 acceptance evidence: a MEASURED byte rate, not just "it
        // enumerated". Report once a second while actually streaming --
        // sample_rate * channels * bytes/sample is the expected rate;
        // printing the measured one alongside lets a human (or the
        // verification loop) compare them directly.
        uint64_t now_us = time_us_64();
        if (now_us - audio_report_start_us >= 1000000) {
            uint32_t bytes_now = pl_usb_audio_pcm_bytes_total();
            uint32_t delta_bytes = bytes_now - audio_report_start_bytes;
            uint64_t delta_us = now_us - audio_report_start_us;
            if (pl_usb_audio_streaming()) {
                printf(
                    "usb-audio: streaming rate=%luHz ch=2 bits=16 measured=%lu B/s (%lu bytes / %llums)\r\n",
                    (unsigned long)pl_usb_audio_sample_rate(),
                    (unsigned long)((uint64_t)delta_bytes * 1000000ULL / delta_us),
                    (unsigned long)delta_bytes,
                    (unsigned long long)(delta_us / 1000)
                );
            } else {
                printf("usb-audio: idle (alt 0 / not streaming), total=%lu bytes\r\n", (unsigned long)bytes_now);
            }
            audio_report_start_us = now_us;
            audio_report_start_bytes = bytes_now;
        }

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
