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
#include "hardware/gpio.h"
#include "hardware/spi.h"
#include "pico/bootrom.h"
#include "pico/cyw43_arch.h"
#include "pico/stdlib.h"
#include "tusb.h"

#include "bt.h"
#include "input.h"
#include "panic_recorder.h"
#include "pico_link_ui.h"
#include "st7789.h"
#include "usb_audio.h"
#include "usb_pump.h"

// The one call in the Rust -> C direction (see pico_link_ui.h's doc comment
// on pl_ui_panic_hook): Rust hands us a panic message on the way to
// spinning forever, since it has no unwinder on this target and nothing
// else safe to do. Deliberately does NOT printf/pl_log here (bd
// pico-link-gap): the recorder's whole point is to survive a panic WITHOUT
// depending on the USB console, since USB is exactly what was observed dead
// alongside a panic on 2026-08-28 -- logging first would risk the
// documented stdio-mutex-deadlock-after-a-fault failure mode (worsened by
// bead pico-link-tfj's 0xC0-worker/pl_usb_mutex machinery, which a panic
// could leave held) before the record is even safely written.
// pl_panic_record_rust arms the watchdog, records to watchdog scratch + an
// uninitialized-RAM buffer, and reboots; the report is printed by
// pl_panic_report_and_clear() on the *next* boot instead.
void pl_ui_panic_hook(const uint8_t *msg, uintptr_t len) {
    pl_panic_record_rust(msg, len);
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

    // Bead pico-link-tfj: brings up pl_usb_mutex and the 1ms-timer/0xC0-IRQ
    // worker that services tud_task() from here on. Must run before
    // stdio_init_all()/any pl_log() call -- pl_log()'s mutex has to exist
    // first, and this firmware no longer calls tud_task() from the
    // superloop at all (see below).
    pl_usb_pump_init();

    stdio_init_all();

    // Give a host-side terminal a moment to attach after CDC enumerates,
    // so the boot banner below isn't lost before anyone is listening.
    sleep_ms(1500);

    pl_log("\r\n=== pico_link firmware boot ===\r\n");
    pl_log("board: pimoroni_pico_plus2_w_rp2350\r\n");
    pl_log("pico-sdk owns main(); ui-ffi (Rust core) linked in over FFI.\r\n");

    // bd pico-link-gap: report (and clear) a panic record left by the
    // reboot that just happened, if there is one. Placed here -- after the
    // CDC attach-grace sleep above, before anything else that could itself
    // panic -- so the report has the best chance of a listener actually
    // being attached, and so it isn't lost underneath later boot output.
    pl_panic_report_and_clear();

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
    pl_log("st7789_init OK (SPI1, DC=%d CS=%d SCK=%d MOSI=%d RST=%d BL=%d, %d Hz)\r\n",
           ST7789_PIN_DC, ST7789_PIN_CS, ST7789_PIN_SCK, ST7789_PIN_MOSI, ST7789_PIN_RST, ST7789_PIN_BL,
           ST7789_INIT_BAUDRATE_HZ);
    st7789_init_and_fill(spi1, 0x0000); // black -- the Rust UI's first render replaces this immediately
    pl_log("st7789_init_and_fill OK -- panel out of reset, backlight on\r\n");

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
    pl_log("PL_DIAG_COLOR_TEST: blitted half-BACKGROUND(0x0884)/half-WHITE(0xFFFF) -- halting\r\n");
    while (true) {
        tight_loop_contents();
    }
#endif

#ifdef PL_DIAG_MADCTL_TEST
    // Diagnostic for pico-link-zzq: the merged 0xA0 "rotation" fix (bd
    // pico-link-g7o) actually MIRRORS the panel, and both the earlier 0x60
    // and 0xA0 attempts were judged by rotating a blurry photo until text
    // read upright -- a broken measurement, since mirrored and 180-rotated
    // text look identical in a low-res frame. This cycles all four pure-
    // rotation MADCTL candidates and renders an ASYMMETRIC CORNER PATTERN
    // for each, so a single photo per candidate settles rotation vs.
    // mirroring unambiguously by which corner holds which colour -- no
    // text, no photo-rotating, no correlating against a timestamp.
    //
    // Software framebuffer layout (fixed, independent of MADCTL -- MADCTL
    // only changes how the panel maps this raster onto physical pixels):
    //   top-left    (x<120,y<120): RED,   with N small black dots encoding
    //                              the candidate index (N = 1..4) so the
    //                              candidate is readable from the RED
    //                              corner alone, wherever that corner ends
    //                              up physically.
    //   top-right   (x>=120,y<120): GREEN
    //   bottom-left (x<120,y>=120): BLUE
    //   bottom-right(x>=120,y>=120): WHITE
    // A true rotation permutes the four corners cyclically (or by 180) and
    // preserves each corner's own content orientation-invariant identity
    // (still a solid colour block, dots still legible in the RED one). A
    // mirror instead swaps two ADJACENT corners while leaving the other
    // pair fixed on one axis -- e.g. RED and GREEN trade places but BLUE
    // and WHITE do not (or vice versa) -- which is the tell.
    static const uint8_t s_madctl_candidates[4] = {0x00, 0x60, 0xA0, 0xC0};
    static uint16_t s_madctl_test_buf[PANEL_WIDTH * PANEL_HEIGHT];
    printf("PL_DIAG_MADCTL_TEST: cycling MADCTL 0x00/0x60/0xA0/0xC0, ~5s each, corner pattern\r\n");
    while (true) {
        for (int idx = 0; idx < 4; idx++) {
            uint8_t madctl = s_madctl_candidates[idx];
            st7789_set_madctl(madctl);
            // Reissue CASET/RASET after every MADCTL change -- see
            // st7789_reset_window's doc comment. Testing the hypothesis that
            // the sliver artifact seen on the MY-set candidates (0xA0, 0xC0)
            // is a stale address-counter state from changing scan direction
            // without reissuing the window, not a GRAM offset that needs
            // compensating (an x-offset sweep at 0/20/40/60/80 did not clean
            // it up -- see bead comments).
            st7789_reset_window();

            for (int y = 0; y < PANEL_HEIGHT; y++) {
                for (int x = 0; x < PANEL_WIDTH; x++) {
                    uint16_t color;
                    if (x < PANEL_WIDTH / 2 && y < PANEL_HEIGHT / 2) {
                        color = COLOR_RED;
                    } else if (x >= PANEL_WIDTH / 2 && y < PANEL_HEIGHT / 2) {
                        color = COLOR_GREEN;
                    } else if (x < PANEL_WIDTH / 2 && y >= PANEL_HEIGHT / 2) {
                        color = COLOR_BLUE;
                    } else {
                        color = 0xFFFF; // white
                    }
                    s_madctl_test_buf[y * PANEL_WIDTH + x] = color;
                }
            }
            // Stamp (idx+1) black dots inside the RED quadrant, well clear
            // of its edges, 12px squares on a 20px pitch starting at (20,20).
            for (int dot = 0; dot <= idx; dot++) {
                int ox = 20 + dot * 20;
                int oy = 20;
                for (int dy = 0; dy < 12; dy++) {
                    for (int dx = 0; dx < 12; dx++) {
                        s_madctl_test_buf[(oy + dy) * PANEL_WIDTH + (ox + dx)] = 0x0000;
                    }
                }
            }

            st7789_blit_framebuffer(spi1, s_madctl_test_buf, PANEL_WIDTH * PANEL_HEIGHT);
            printf("PL_DIAG_MADCTL_TEST: candidate %d/4 -- MADCTL=0x%02X, %d dot(s) in RED corner\r\n",
                   idx + 1, madctl, idx + 1);

            sleep_ms(5000);
        }
    }
#endif

    // --- The Rust UI ---
    struct PlUi *ui = pl_ui_create(PANEL_WIDTH, PANEL_HEIGHT);
    if (ui == NULL) {
        pl_log("pl_ui_create FAILED -- halting\r\n");
        while (true) {
            tight_loop_contents();
        }
    }
    pl_log("pl_ui_create OK\r\n");

    pl_link_input_init();
    pl_log("pl_link_input_init OK\r\n");

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
        pl_log("cyw43_arch_init FAILED -- halting\r\n");
        while (true) {
            tight_loop_contents();
        }
    }
    pl_log("cyw43_arch_init OK\r\n");

    pl_bt_init(ui);
#else
    pl_log("PL_DIAG_SKIP_BT set -- skipping cyw43_arch_init/pl_bt_init\r\n");
#endif

    // Superloop on core0 only (M1b design, unchanged by M2). Every
    // iteration: poll debounced input edges, forward to Rust, tick,
    // render, blit, print per-frame timing, then poll one queued
    // Bluetooth command -- all still plain C driving the same FFI surface,
    // no new threading model. BTstack's own work happens in the cyw43
    // background IRQ (pico_cyw43_arch_threadsafe_background, linked in
    // CMakeLists.txt), not in this loop.
    //
    // Bead pico-link-tfj: tud_task()/pl_usb_audio_task() are NO LONGER
    // called here. The isochronous OUT endpoint is only re-armed from
    // inside tud_task() (audio_device.c's audiod_xfer_cb), on a ~1ms
    // deadline the render+blit+BT-poll loop below cannot meet (it was
    // ~55ms at the time this was diagnosed) -- see usb_pump.h's module
    // doc. pl_usb_pump_init() (called above, before stdio_init_all())
    // already installed a 1ms-timer-driven, 0xC0-priority IRQ worker that
    // services both.
    PlIntent intents[8];
    uint64_t audio_report_start_us = time_us_64();
    uint32_t audio_report_start_bytes = pl_usb_audio_pcm_bytes_total();
    // ~60Hz budget, matching the emulator's frame budget and the loop's
    // former sleep_ms(16) pace -- but now a DEADLINE measured from
    // frame_start_us rather than an unconditional sleep, since the loop
    // body itself (blit alone: 38.6ms as of pico-link-14l) can already
    // exceed it. Bead pico-link-tfj: the old unconditional sleep_ms(16)
    // added a flat 16ms on top of a body that was already the dominant
    // cost, for no reason -- it never actually paced anything to 60Hz.
    const uint64_t frame_budget_us = 16000;
    // Bead pico-link-l60 (C): hardware-independent BOOTSEL escape hatch --
    // hold X+Y to force BOOTSEL regardless of whether picotool's software
    // reset path (usb_reset.c) works, so a bad flash never costs more than
    // one physical replug ever again. Read raw GPIOs directly rather than
    // going through pl_link_input_poll: that path is deliberately
    // press-EDGE-only (see input.h's module doc) and cannot express a
    // hold. Pull-ups are configured by pl_link_input_init() (called
    // above), so 0 == pressed. Two simultaneous buttons is not a UI
    // gesture, so this cannot collide with navigation, and it runs in
    // thread context, not the 0xC0 IRQ worker.
    //
    // 60 loop iterations is NOT ~1s of wall-clock hold time here, despite
    // the frame_budget_us above being 16000 -- that budget is a target,
    // not a measured pace, and this file's own pico-link-tfj comments note
    // the loop body (blit alone) measures ~38.6ms, well over budget most
    // iterations. At that real pace 60 iterations is closer to 2-3s. The
    // counter is left at 60 anyway (a slightly-longer-than-a-second hold
    // is exactly what you want from an escape hatch -- it must not fire on
    // an accidental two-button bump), but a hold that "hasn't triggered
    // yet" at the 1s mark is expected, not evidence the hatch is broken.
    uint32_t xy_held_frames = 0;
    const uint32_t xy_held_frames_for_reset = 60; // ~2-3s at this loop's real (not budgeted) pace -- see comment above

    while (true) {
        uint64_t frame_start_us = time_us_64();

        if (gpio_get(PL_INPUT_PIN_X) == 0 && gpio_get(PL_INPUT_PIN_Y) == 0) {
            xy_held_frames++;
            if (xy_held_frames >= xy_held_frames_for_reset) {
                reset_usb_boot(0, 0);
                // does not return
            }
        } else {
            xy_held_frames = 0;
        }

        size_t n = pl_link_input_poll(intents, 8);
        if (n > 0) {
            pl_ui_input(ui, intents, n);
        }
#ifndef PL_DIAG_SKIP_BT
        // Drains events the BTstack packet handler queued from IRQ context
        // (pico-link-6o2) and makes the real pl_ui_push_event calls here, in
        // thread context, before this frame ticks/renders -- so a device
        // discovered or a link-state change is visible in the same frame
        // it arrived, not one frame late.
        pl_bt_drain_events(ui);
#endif
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
                pl_log(
                    "usb-audio: streaming rate=%luHz ch=2 bits=16 measured=%lu B/s (%lu bytes / %llums)\r\n",
                    (unsigned long)pl_usb_audio_sample_rate(),
                    (unsigned long)((uint64_t)delta_bytes * 1000000ULL / delta_us),
                    (unsigned long)delta_bytes,
                    (unsigned long long)(delta_us / 1000)
                );
            } else {
                pl_log("usb-audio: idle (alt 0 / not streaming), total=%lu bytes\r\n", (unsigned long)bytes_now);
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
            pl_log(
                "frame %lu: render=%lluus blit=%lluus total=%lluus\r\n",
                (unsigned long)frame_count,
                (unsigned long long)(render_end_us - render_start_us),
                (unsigned long long)(blit_end_us - render_end_us),
                (unsigned long long)(blit_end_us - frame_start_us)
            );
        }

        // Bead pico-link-tfj instrumentation -- see usb_pump.h's doc
        // comment on pl_usb_pump_report for what the three counters mean.
        pl_usb_pump_report();

        // No dirty-gate here: pl_ui_render (unlike core's own Runner::step)
        // re-renders unconditionally every call -- see its doc comment in
        // pico_link_ui.h. Blitting every iteration regardless is simple and
        // correct, if not maximally efficient -- a later milestone can add
        // a "was this frame actually new" signal to the FFI surface if the
        // redundant-blit cost turns out to matter.
        //
        // Bead pico-link-tfj: replaced the old unconditional sleep_ms(16)
        // with a deadline measured from frame_start_us -- the body above
        // (blit alone: 38.6ms as of pico-link-14l) already exceeds the
        // budget most of the time, so this sleeps only the remainder (zero,
        // in practice) rather than always adding a further flat 16ms.
        uint64_t frame_elapsed_us = time_us_64() - frame_start_us;
        if (frame_elapsed_us < frame_budget_us) {
            sleep_us((uint32_t)(frame_budget_us - frame_elapsed_us));
        }
    }

    return 0;
}
