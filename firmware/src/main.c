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

#include <stdint.h>
#include <stdio.h>

#include "btstack.h"
#include "hardware/gpio.h"
#include "hardware/spi.h"
#include "pico/bootrom.h"
#include "pico/cyw43_arch.h"
#include "pico/stdlib.h"

// btstack.h (above, via btstack_hid.h) and TinyUSB's class/hid/hid_device.h
// both declare a global `hid_report_type_t` enum with identical member
// names -- BTstack's is its own unrelated Classic-HID-host module, pulled
// in unconditionally by the btstack.h umbrella header. This collides only
// in a translation unit that includes BOTH headers, which as of enabling
// CFG_TUD_HID (bead pico-link-47z.1, USB HID consumer-control interface)
// is just this file. main.c calls no tud_hid_*() function -- that's
// usb_descriptors.c's job -- so locally force CFG_TUD_HID off before
// pulling in tusb.h here; tusb_config.h's own include guard
// (PICO_LINK_TUSB_CONFIG_H) means this override sticks for this TU only.
// Every other class knob (CDC, AUDIO) is untouched and still enabled here.
#include "tusb_config.h"
#undef CFG_TUD_HID
#define CFG_TUD_HID 0
#include "tusb.h"

#include "a2dp.h"
#include "codec_ldac.h"
#include "bt.h"
#include "dsp.h"
#ifdef PL_DEBUG_REMOTE
#include "debug_remote.h"
#endif
// Bead pico-link-9eq2.3.2: fault.h is included ONLY here (design sec 7.2)
// -- see that header's module doc for why the include graph stays
// one-directional (main.c -> fault.c -> {a2dp,usb_audio,pcm_ring}).
#include "fault.h"
#include "input.h"
#include "media_keys.h"
#include "ldac_bench.h"
#include "panic_recorder.h"
#include "persist.h"
#include "pico_link_ui.h"
#include "pl_log_ring.h"
#include "pl_loop_prof.h"
#include "st7789.h"
#include "usb_audio.h"
#include "usb_config_itf.h"
#include "usb_pump.h"
#include "volume.h"
#include "watchdog_sup.h"

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
    // Bead pico-link-okx (F4): THE literal first statement of main() --
    // snapshots watchdog_hw->reason/scratch[] before anything else in this
    // firmware (including pl_log_ring_init() right below) can run and
    // possibly disturb them. See watchdog_sup.h's doc comment on this call.
    pl_wdt_capture_boot_reason();

    // Bead pico-link-okx (F1): zero the console byte ring before ANYTHING
    // else -- pl_log()/pl_log_locked() push into it unconditionally, and
    // nothing downstream of this line may call either before it has run.
    // Bead pico-link-okx (F3b): "zero" is no longer quite right -- see this
    // function's own doc comment in pl_log_ring.h. A warm reset now KEEPS
    // whatever backlog didn't get drained before the reset happened.
    pl_log_ring_init();

    // Bead pico-link-okx (F3b): note the recovery explicitly, as the VERY
    // FIRST thing pushed into the ring this boot -- pl_log() is safe to call
    // this early (it only pushes bytes into the ring, see its own doc
    // comment; no USB/stdio init required). Because the ring is strictly
    // FIFO, this line (and therefore every byte still queued from the
    // previous boot) is guaranteed to drain BEFORE the "=== pico_link
    // firmware boot ===" banner a few lines down, giving a reader a clear
    // marker for where the surviving backlog ends and this boot begins.
    if (pl_log_ring_recovered_backlog()) {
        pl_log("log-ring: recovered %lu unread byte(s) from the previous session (warm reset)\r\n",
               (unsigned long)pl_log_ring_recovered_backlog_bytes());
    }

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

    // Bead pico-link-okx (D12): turns on the RP2350's SOF hardware
    // interrupt via TinyUSB's public SOF_CONSUMER_USER path (this build's
    // AUDIO_FEEDBACK_METHOD_DISABLED means nothing else requests it -- see
    // usb_pump.c's tud_sof_cb doc comment for the full chain, verified by
    // reading pico-sdk 2.1.1's usbd.c/audio_device.c/dcd_rp2040.c). Needed
    // for the s_sof_isr_count instrument; not needed by anything else in
    // this firmware. (This used to say sof_phase_hist -- bead pico-link-wbq
    // deleted that histogram as a structurally dead instrument, but the SOF
    // callback is still needed, now for pico-link-2ap's ISO-OUT
    // discriminator. Corrected under pico-link-8er.) Must run after tusb_init() (usbd_sof_enable asserts
    // the stack is initialized).
    //
    // Bead pico-link-1av: this call ALONE does not survive enumeration --
    // usbd.c's configuration_reset() (called on the first real
    // SET_CONFIGURATION, and on every subsequent bus reset) tu_varclr()s
    // the whole _usbd_dev struct, wiping the sof_consumer bit this sets.
    // Kept here anyway so s_sof_isr_count has data even before the
    // first enumeration; usb_pump.c's tud_mount_cb() override is the call
    // that actually keeps it armed post-enumeration -- see that function's
    // doc comment for the full chain.
    tud_sof_cb_enable(true);

    // Bead pico-link-tfj: brings up pl_usb_mutex and the 1ms-timer/0xC0-IRQ
    // worker that services tud_task() from here on. This firmware no
    // longer calls tud_task() from the superloop at all (see below).
    pl_usb_pump_init();

    stdio_init_all();

    // Give a host-side terminal a moment to attach after CDC enumerates,
    // so the boot banner below isn't lost before anyone is listening.
    sleep_ms(1500);

    pl_log("\r\n=== pico_link firmware boot ===\r\n");
    pl_log("board: pimoroni_pico_plus2_w_rp2350\r\n");
    pl_log("pico-sdk owns main(); ui-ffi (Rust core) linked in over FFI.\r\n");

    // Bead pico-link-ufh: classify *why* the board booted (power-on vs. an
    // unattributed hardware watchdog expiry vs. a deliberate
    // watchdog_reboot). Bead pico-link-okx (F4): the register read this
    // used to do live now happened at the top of this function via
    // pl_wdt_capture_boot_reason() -- this call only formats that snapshot,
    // so it is no longer racing pl_wdt_arm()'s scratch[4] stamp. Kept here
    // (before pl_panic_report_and_clear(), below) purely for boot-log
    // ordering: it peeks the ALREADY-CAPTURED scratch[0] value to avoid
    // double-reporting a supervised WDT trip that pl_panic_report_and_clear()
    // is about to print in full.
    pl_wdt_report_boot_reason();

    // bd pico-link-gap: report (and clear) a panic record left by the
    // reboot that just happened, if there is one. Placed here -- after the
    // CDC attach-grace sleep above, before anything else that could itself
    // panic -- so the report has the best chance of a listener actually
    // being attached, and so it isn't lost underneath later boot output.
    // Also handles the pico-link-ufh watchdog-trip breadcrumb (PL_PANIC_MAGIC_WDT).
    pl_panic_report_and_clear();

    // Panel bring-up itself is proven on real hardware as of M1b (bd
    // pico-link-cz0.2). An earlier version of this M2 session's main.c
    // dropped the RGB staging fills entirely, on the mistaken assumption
    // they were purely a bring-up diagnostic -- they are NOT. st7789_init()
    // only brings up the GPIO/SPI peripheral; the actual panel bring-up
    // (hardware reset, SLPOUT, COLMOD, MADCTL, INVON, the initial CASET/
    // RASET address window (now set via st7789_set_window on every blit,
    // not once -- see st7789_blit_rect, pico-link-7h5.7), DISPON, and
    // turning the backlight on) all lives inside
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
    // Reusable diagnostic (off by default -- enable with a real CMake
    // option, `cmake -B build -DPL_DIAG_COLOR_TEST=ON`, see
    // firmware/CMakeLists.txt; NOT via CMAKE_C_FLAGS, which replaces
    // rather than appends to the pico-sdk toolchain file's seeded
    // -mcpu/-march flags -- pico-link-ukk): an exposure-immune
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
    // Reusable diagnostic (off by default -- enable with a real CMake
    // option, `cmake -B build -DPL_DIAG_MADCTL_TEST=ON`, see
    // firmware/CMakeLists.txt; NOT via CMAKE_C_FLAGS -- pico-link-ukk).
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
    // Bead pico-link-okx (F2b): converted from printf to pl_log for
    // consistency with the rest of this firmware's console output --
    // low-priority (this is a diagnostic-only build path), done to keep
    // this the last raw printf() call site.
    pl_log("PL_DIAG_MADCTL_TEST: cycling MADCTL 0x00/0x60/0xA0/0xC0, ~5s each, corner pattern\r\n");
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
            pl_log("PL_DIAG_MADCTL_TEST: candidate %d/4 -- MADCTL=0x%02X, %d dot(s) in RED corner\r\n",
                   idx + 1, madctl, idx + 1);

            sleep_ms(5000);
        }
    }
#endif

#ifdef PL_DIAG_WINDOW_TEST
    // Reusable diagnostic (off by default -- enable with a real CMake
    // option, `cmake -B build -DPL_DIAG_WINDOW_TEST=ON`, see
    // firmware/CMakeLists.txt; NOT via CMAKE_C_FLAGS -- pico-link-ukk).
    // pico-link-7h5.1 (M-1, blocking hardware measurement for the
    // damage-rect design, .planning/design/2026-09-06-damage-rect-render-
    // and-partial-blit.md section 7.3): MADCTL is 0x60 (MV set), so
    // CASET/RASET may address the PANEL's axes, not the framebuffer's --
    // a framebuffer row band is not necessarily a RASET range. This paints
    // ONE off-centre, NON-SQUARE band via an EXPLICIT raw CASET/RASET pair
    // (st7789_diag_fill_window, bypassing the once-at-init window entirely)
    // and halts so it can be photographed. A centred square would prove
    // almost nothing; this rect is deliberately off-centre on both axes
    // and non-square so an axis swap, a mirror, or an offset are all
    // visually distinguishable in one photo.
    //
    // Intended framebuffer rect (raw values issued to CASET/RASET, NO
    // MADCTL/axis correction applied -- that correction is the unknown
    // this diagnostic measures): x in [160,209] (50px wide), y in [15,154]
    // (140px tall) -- right-of-centre, upper-biased, tall-narrow. Rest of
    // the panel is a contrasting dark navy so the band's edges are sharp.
    const uint16_t band_x0 = 160, band_x1 = 209; // 50px wide
    const uint16_t band_y0 = 15, band_y1 = 154;  // 140px tall
    pl_log("PL_DIAG_WINDOW_TEST: full-panel fill 0x0010 (dark navy) via raw window [0,239]x[0,239]\r\n");
    st7789_diag_fill_window(0, 0, PANEL_WIDTH - 1, PANEL_HEIGHT - 1, 0x0010);
    pl_log("PL_DIAG_WINDOW_TEST: band fill 0xFFFF (white) via raw window "
           "CASET=[%u,%u] RASET=[%u,%u] (framebuffer-rect intent: x0=%u w=%u y0=%u h=%u)\r\n",
           band_x0, band_x1, band_y0, band_y1, band_x0, (unsigned)(band_x1 - band_x0 + 1), band_y0,
           (unsigned)(band_y1 - band_y0 + 1));
    st7789_diag_fill_window(band_x0, band_y0, band_x1, band_y1, 0xFFFF);
    pl_log("PL_DIAG_WINDOW_TEST: halted for photograph\r\n");
    while (true) {
        tight_loop_contents();
    }
#endif

#ifdef PL_DIAG_LDAC_BENCH
    // Reusable diagnostic (off by default -- enable with a real CMake
    // option, `cmake -B build -DPL_DIAG_LDAC_BENCH=ON`, see
    // firmware/CMakeLists.txt; NOT via CMAKE_C_FLAGS, which replaces
    // rather than appends to the pico-sdk toolchain file's seeded
    // -mcpu/-march flags -- pico-link-ukk), bead pico-link-cz0.5.4
    // (LDAC L0). Pure CPU/heap measurement: no display, no BT, no A2DP --
    // runs before pl_ui_create()/cyw43_arch_init() below so nothing else is
    // competing for CPU or heap during the measurement. Deliberately does
    // NOT halt afterward (unlike PL_DIAG_COLOR_TEST/PL_DIAG_MADCTL_TEST
    // above) -- falls through into the normal boot flow so
    // pl_debug_remote_poll() keeps servicing the main loop and a `BOOTSEL`
    // CDC command still works for the NEXT reflash. A halted board cannot
    // respond to CDC BOOTSEL (debug_remote only polls from a live main
    // loop -- see debug_remote.h), which cost a physical BOOTSEL press
    // once already during this bead's own measurement.
    pl_ldac_bench_run();
    pl_log("PL_DIAG_LDAC_BENCH: done, continuing normal boot\r\n");
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

    // T2 of the media-keys epic (pico-link-47z.2) -- the USB HID
    // consumer-control ring/drain/safety-timeout. No AVRCP wiring yet
    // (T3, pico-link-47z.3): the only producer today is debug_remote.c's
    // "MEDIA ..." console commands. See media_keys.h's module doc.
    pl_media_keys_init();
    pl_log("pl_media_keys_init OK\r\n");

    // Bead pico-link-4v2.2 (VT2 of the volume-sync epic pico-link-4v2):
    // the canonical volume state/loop-rule/circuit-breaker module. No
    // real producer wired yet (T3/T4) -- only debug_remote.c's "VOL SET"
    // console command drives it today, via pl_volume_debug_set(). See
    // volume.h's module doc.
    pl_volume_init();
    pl_log("pl_volume_init OK\r\n");

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

    // Bead pico-link-qivj.5 (S11): push the PL:S:0 boot snapshot into core,
    // if one was persisted -- thread context, before the superloop, same
    // context a2dp.c's own direct pl_ui_push_event calls use. No-op (core
    // keeps its own default) if pl_persist_boot_display_settings() returns
    // false -- see that function's doc comment for every reason it can.
    {
        uint8_t boot_display_mode;
        uint16_t boot_display_timeout_s;
        if (pl_persist_boot_display_settings(&boot_display_mode, &boot_display_timeout_s)) {
            struct PlEvent event = {
                .version = PL_EVENT_ABI_VERSION,
                .tag = PL_EVENT_TAG_DISPLAY_SETTINGS_LOADED,
                .payload = {.display_settings = {.mode = boot_display_mode, .timeout_s = boot_display_timeout_s}},
            };
            pl_ui_push_event(ui, event);
        }
    }

    // Bead pico-link-8pp1.4 (S3): push the PL:S:1 boot snapshot into core
    // (for a future Settings row/picker, S4) AND apply it live to the
    // resync trim -- UNLIKE the display-settings block above, core has no
    // run loop of its own that touches a2dp.c, so C must apply this one
    // itself (see core/src/audio.rs's module doc). No-op apply (a2dp.c
    // keeps its compiled-in default, Low) if pl_persist_boot_cushion_
    // policy() returns false -- see that function's doc comment for every
    // reason it can.
    {
        uint8_t boot_cushion_policy;
        if (pl_persist_boot_cushion_policy(&boot_cushion_policy)) {
            pl_a2dp_set_cushion_policy(boot_cushion_policy);
            struct PlEvent event = {
                .version = PL_EVENT_ABI_VERSION,
                .tag = PL_EVENT_TAG_CUSHION_POLICY_LOADED,
                .payload = {.cushion_policy = {.policy = boot_cushion_policy}},
            };
            pl_ui_push_event(ui, event);
        }
    }

    // Bead pico-link-d42g.3 (F3), design `.planning/design/2026-09-25-
    // adaptive-floor.md` sec 2/4: push the PL:S:2 boot snapshot into core
    // (for a future Settings row/picker, F4) AND apply it live to the LDAC
    // encoder -- same "core has no run loop of its own" reasoning as the
    // PL:S:1 block above. No-op apply (codec_ldac.c keeps its compiled-in
    // default, rung 4 = 330 kbps) if pl_persist_boot_abr_floor() returns
    // false -- see that function's doc comment for every reason it can.
    {
        uint8_t boot_abr_floor;
        if (pl_persist_boot_abr_floor(&boot_abr_floor)) {
            pl_codec_ldac_set_floor(boot_abr_floor);
            struct PlEvent event = {
                .version = PL_EVENT_ABI_VERSION,
                .tag = PL_EVENT_TAG_ABR_FLOOR_LOADED,
                .payload = {.abr_floor = {.floor = boot_abr_floor}},
            };
            pl_ui_push_event(ui, event);
        }
    }

#ifdef PL_ENCODER_ON_CORE1
    // Bead pico-link-nli.4 (G3, epic pico-link-nli): launch core1 into the
    // LDAC encoder loop, after cyw43/BTstack init per design sec 8 -- core1
    // runs forever from here on (design sec 4.2, no corresponding stop
    // call). PL_STACK_SIZE/PICO_CORE1_STACK_SIZE are both already sized for
    // this (CMakeLists.txt, G1's own doc comment there).
    // Bead pico-link-0zr: pl_a2dp_launch_core1() itself asserts (halts,
    // unconditionally, matching persist.c's collision-check precedent) that
    // its .bss-resident core1 stack does not overlap core0's real stack
    // range -- see a2dp.c's doc comment on that stack array for why.
    pl_a2dp_launch_core1();
    pl_log("pl_a2dp_launch_core1 OK -- LDAC encoder running on core1\r\n");
#endif
#else
    pl_log("PL_DIAG_SKIP_BT set -- skipping cyw43_arch_init/pl_bt_init\r\n");
#endif

    // Bead pico-link-ufh: arm the hardware watchdog + software supervisor.
    // Deliberately OUTSIDE the PL_DIAG_SKIP_BT guard above, so diag builds
    // are also armed -- and deliberately AFTER pl_wdt_report_boot_reason()/
    // pl_panic_report_and_clear() above, since arming stamps scratch[4] and
    // would destroy the evidence those two functions read. Boot
    // (tusb_init -> cyw43_arch_init -> pl_bt_init) stays deliberately
    // unprotected -- arming late is what keeps main.c's halt-forever
    // diagnostic paths above this line usable. See the design doc's
    // "Consequences" section.
    pl_wdt_arm();

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

    // Bead pico-link-p1r: previous iteration's frame_start_us, used below
    // to compute PL_LOOP_PHASE_TOTAL as the wall-clock gap between
    // successive top-of-loop timestamps -- the true per-iteration period
    // this bead's 6.14 Hz drain-rate measurement corresponds to. 0 means
    // "no prior iteration yet", so the very first iteration records
    // nothing.
    uint64_t prev_frame_start_us = 0;

    // Bead pico-link-vxc: the last wall-clock time a blit actually
    // happened, for the PL_FORCED_REPAINT_MS backstop below. 0 means "no
    // blit yet" -- with `now - 0` always >= any positive
    // PL_FORCED_REPAINT_MS, this makes the very first iteration force a
    // paint, harmlessly redundant with App::new's dirty:true (core/src/
    // app.rs) but correct even if that ever changed.
    uint64_t last_blit_us = 0;

    while (true) {
        pl_wdt_mark(PL_WDT_CP_LOOP_TOP);
        uint64_t frame_start_us = time_us_64();
        if (prev_frame_start_us != 0) {
            pl_loop_prof_record(PL_LOOP_PHASE_TOTAL, frame_start_us - prev_frame_start_us);
        }
        prev_frame_start_us = frame_start_us;

        if (gpio_get(PL_INPUT_PIN_X) == 0 && gpio_get(PL_INPUT_PIN_Y) == 0) {
            xy_held_frames++;
            if (xy_held_frames >= xy_held_frames_for_reset) {
                reset_usb_boot(0, 0);
                // does not return
            }
        } else {
            xy_held_frames = 0;
        }

        pl_wdt_mark(PL_WDT_CP_INPUT_POLL);
        uint64_t input_start_us = time_us_64();
        size_t n = pl_link_input_poll(intents, 8);
        if (n > 0) {
            pl_ui_input(ui, intents, n);
        }
        // Bead pico-link-p1r: attribute the input-poll phase. Does not
        // include the X+Y BOOTSEL-hold GPIO reads above -- two gpio_get()
        // calls, cheap enough to not need separate attribution.
        pl_loop_prof_record(PL_LOOP_PHASE_INPUT, time_us_64() - input_start_us);
#ifdef PL_DEBUG_REMOTE
        // Bead pico-link-cd3: debug-only NavIntent injection over the CDC
        // console, feeding the SAME pl_ui_input call the GPIO scan above
        // does. Main-loop/thread-context only, same as pl_link_input_poll
        // -- see debug_remote.h's module doc for why that matters.
        pl_wdt_mark(PL_WDT_CP_DEBUG_REMOTE);
        uint64_t debug_remote_start_us = time_us_64();
        PlIntent debug_intents[4];
        size_t debug_n = pl_debug_remote_poll(ui, debug_intents, 4);
        if (debug_n > 0) {
            pl_ui_input(ui, debug_intents, debug_n);
        }
        // Bead pico-link-p1r.
        pl_loop_prof_record(PL_LOOP_PHASE_DEBUG_REMOTE, time_us_64() - debug_remote_start_us);
#endif
        // Bead pico-link-ryw.12.5: drains at most one pending USB preset
        // import per iteration. Thread-context only, unconditional --
        // NOT gated on PL_DEBUG_REMOTE, unlike the block above: this
        // transport exists in release builds. See usb_config_itf.h.
        pl_config_itf_poll(ui);
        // T2 of the media-keys epic (pico-link-47z.2): drains
        // media_keys.c's own ring and runs its 600ms safety-release
        // check. NOT gated behind PL_DEBUG_REMOTE -- T3's AVRCP handler
        // will push into the same ring unconditionally, so the drain must
        // always run regardless of whether the debug console is built in.
        pl_media_keys_drain(frame_start_us);
        // Bead pico-link-4v2.2 (VT2): drains volume.c's host/sink inbound
        // latches and applies the loop rule. NOT gated behind
        // PL_DEBUG_REMOTE for the same reason as pl_media_keys_drain
        // above -- T3/T4's real USB/AVRCP producers will latch into this
        // independent of the debug console; only the "VOL SET" producer
        // is debug-only today.
        pl_volume_service(frame_start_us);
        // T4 (pico-link-4v2.4), design sec 6 mechanism M1: the outbound
        // half of direction B (headphones -> host). volume.c's loop rule
        // (run just above) sets this latch on ANY accepted change --
        // host-origin or sink-origin alike, design sec 3's outbound
        // latches are keyed on "canonical changed", not on which peer
        // caused it. Pushing a host-origin echo back at the host is
        // harmless (it is already on-grid, rule (a) absorbs it on the
        // host's own next SET if any). Both calls MUST happen under
        // pl_usb_mutex -- see pl_usb_audio_send_fu_status_interrupt's own
        // doc comment (usb_audio.c) for why a caller that does not already
        // hold it gets a silent accepted=false, not an error.
        int16_t fu_cur;
        if (pl_volume_take_fu_report(&fu_cur)) {
            if (pl_usb_lock_try()) {
                pl_usb_audio_fu_set_volume(0, fu_cur);
                bool sent = pl_usb_audio_send_fu_status_interrupt();
                pl_usb_unlock();
                if (!sent) {
                    pl_log("volume: FU status interrupt NOT accepted (TinyUSB busy), cur=%d\r\n", (int)fu_cur);
                }
            } else {
                // 0xC0 worker holds pl_usb_mutex this iteration -- the
                // latch was already cleared by pl_volume_take_fu_report,
                // so this specific report is dropped, not retried. Not a
                // correctness problem: the next accepted volume change (of
                // which there WILL be one the moment either peer moves
                // again) re-latches and this consumer runs every frame, so
                // in practice the lock is free well before the next real
                // edge fires.
                pl_log("volume: FU status interrupt SKIPPED (pl_usb_mutex busy), cur=%d\r\n", (int)fu_cur);
            }
        }
#ifndef PL_DIAG_SKIP_BT
        // Drains events the BTstack packet handler queued from IRQ context
        // (pico-link-6o2) and makes the real pl_ui_push_event calls here, in
        // thread context, before this frame ticks/renders -- so a device
        // discovered or a link-state change is visible in the same frame
        // it arrived, not one frame late.
        pl_wdt_mark(PL_WDT_CP_BT_DRAIN);
        uint64_t bt_drain_start_us = time_us_64();
        pl_bt_drain_events(ui);
        // Bead pico-link-p1r.
        pl_loop_prof_record(PL_LOOP_PHASE_BT_DRAIN, time_us_64() - bt_drain_start_us);
#endif
        pl_wdt_mark(PL_WDT_CP_UI_TICK);
        uint64_t ui_tick_start_us = time_us_64();
        pl_ui_tick(ui, frame_start_us);
        // Bead pico-link-p1r.
        pl_loop_prof_record(PL_LOOP_PHASE_UI_TICK, time_us_64() - ui_tick_start_us);

        // Bead pico-link-nli.5 (G4): the OUT-meter's seqlock read. Must
        // stay AFTER pl_ui_tick above, not before -- pl_ui_tick is what
        // advances the clock a fresh LevelsChanged is stamped with
        // (received_at), so reading here means the sample is timestamped
        // in the same iteration that will render it. See a2dp.h's doc
        // comment on pl_a2dp_poll_levels for the staleness bug this
        // ordering closes at the root.
#ifndef PL_DIAG_SKIP_BT
        pl_a2dp_poll_levels(ui);
        // Bead pico-link-7jol.5: same cadence/ordering rationale as
        // pl_a2dp_poll_levels above, but this event carries no clock-
        // dependent field, so placement relative to it doesn't matter --
        // kept adjacent purely because both are "poll a live a2dp.c
        // reading, push if changed" calls.
        pl_a2dp_poll_ldac_bitrate(ui);
        // Bead pico-link-ryw.5, design sec 3.2: pull the active DSP
        // program (core computes it from the connected device's assigned
        // preset) and submit it to core1's engine only when it changed;
        // pl_dsp_service() then runs UNCONDITIONALLY every iteration
        // regardless of this call's result, to retry a still-unacked
        // previous publish (see dsp.h's pl_dsp_service doc comment) --
        // same "poll a live reading, act if changed" shape as
        // pl_a2dp_poll_ldac_bitrate above, kept adjacent for that reason.
        PlDspProgram dsp_program;
        if (pl_ui_take_dsp_program(ui, &dsp_program)) {
            pl_dsp_submit(&dsp_program);
        }
        pl_dsp_service();
#endif

        // Idle-screensaver seam (pico-link-i3e, extended by pico-link-
        // qivj.2 with a Dim level): a LEVEL, read once per iteration right
        // after pl_ui_tick and applied idempotently to the backlight PWM
        // -- see pl_ui_display_power's doc comment and
        // .planning/design/2026-09-01-idle-policy-across-the-ffi-seam.md.
        // No blit_wait to preserve here (pico-link-3uq's split is not
        // merged -- st7789_blit_framebuffer below is still one blocking
        // call, not a start/wait pair), so the gate is simply: skip
        // render+blit while blanked. If 3uq lands first, its unconditional
        // blit_wait for the PREVIOUS frame's DMA must stay ABOVE this
        // gate -- see the design doc's §3.3 ordering rule.
        //
        // display_on means "render+blit allowed", which stays true at Dim
        // (the meter and fault strip must keep drawing, just dimmer) --
        // only Off skips rendering. The backlight itself always tracks
        // core's requested permille exactly, independent of display_on.
        bool display_on = pl_ui_display_power(ui) != PL_DISPLAY_POWER_OFF;
        st7789_set_backlight_permille(pl_ui_backlight_permille(ui));

        // Bead pico-link-vxc: the dirty gate. `pl_ui_dirty` is read AFTER
        // pl_ui_tick (above), since tick is what turns a due
        // Widget::redraw_after into a dirty flag -- reading it before the
        // tick would delay every time-driven repaint by one frame.
        // PL_FORCED_REPAINT_MS is the backstop for panel-side corruption
        // the dirty flag cannot see (see the CMakeLists.txt option's own
        // comment and .planning/design/2026-09-02-dirty-gate-across-the-
        // ffi-seam.md §5); 0 disables it.
        uint64_t dirty_gate_now_us = time_us_64();
        bool forced_repaint_due =
            PL_FORCED_REPAINT_MS != 0 && (dirty_gate_now_us - last_blit_us) >= (uint64_t)PL_FORCED_REPAINT_MS * 1000;
        bool needs_paint = pl_ui_dirty(ui) || forced_repaint_due;

        // Declared at this scope (not inside the `if (display_on &&
        // needs_paint)` block below) because the frame_report_phase print
        // further down references them unconditionally -- defaulted to
        // frame_start_us so a skipped frame's report reads as zero
        // render/blit time rather than undefined.
        uint64_t render_start_us = frame_start_us;
        uint64_t render_end_us = frame_start_us;
        uint64_t blit_end_us = frame_start_us;
        // Whether this iteration actually issued a blit -- used below to
        // gate the PL_LOOP_PHASE_BLIT sample the same way the px != NULL
        // check used to (pico-link-vxc), now that "did we blit" is a
        // quantisation decision rather than just a null-pointer check.
        bool blit_happened = false;
        // The dirty gate sits INSIDE the display-power gate, never beside
        // it (`display_on && needs_paint`, in that order): while blanked
        // we must skip render regardless of dirty, and critically must NOT
        // call pl_ui_render_ex, because that would clear the dirty flag for
        // a frame nobody saw. This is the identical contract core's own
        // Runner::step already encodes (run.rs) -- see the design doc's
        // §3.3 for the full ordering rationale, including why this
        // composes with pico-link-3uq's (unmerged) blit_wait/blit_start
        // split without needing any change there.
        if (display_on && needs_paint) {
            // Bead pico-link-7h5.8: pl_ui_render_ex/PlRenderOut replace
            // pl_ui_render's bare px/px_len pair so the frame's damage rect
            // crosses the FFI seam too -- see design doc
            // .planning/design/2026-09-06-damage-rect-render-and-partial-
            // blit.md §6/§7. pl_ui_render itself is untouched and stays
            // live until pico-link-7h5.10 retires it.
            struct PlRenderOut render_out = {0};
            render_start_us = time_us_64();
            pl_wdt_mark(PL_WDT_CP_UI_RENDER);
            pl_ui_render_ex(ui, &render_out);
            render_end_us = time_us_64();
            // Bead pico-link-p1r.
            pl_loop_prof_record(PL_LOOP_PHASE_UI_RENDER, render_end_us - render_start_us);

            const uint16_t *px = render_out.px;
            bool px_len_ok =
                px != NULL && render_out.px_len == (uintptr_t)PANEL_WIDTH * (uintptr_t)PANEL_HEIGHT;

            if (px_len_ok) {
                if (render_out.version != PL_RENDER_ABI_VERSION) {
                    // ABI mismatch: degrade to slow-and-correct, never
                    // fast-and-wrong (design doc §6) -- blit the whole
                    // frame rather than trust a `rects` shape this build
                    // was not compiled to read.
                    st7789_blit_framebuffer(spi1, px, (uint32_t)render_out.px_len);
                    blit_happened = true;
                } else if (render_out.rect_count == 0) {
                    // Nothing to paint this frame (design doc §6) -- do
                    // not blit at all, not even an empty rect. Reachable
                    // only if pl_ui_dirty was stale; the dirty gate above
                    // is the primary mechanism, this is the belt.
                } else {
                    // Phase 1 (pico-link-7h5.8/.7): quantise core's true
                    // damage rect to a full-width row band -- x=0,
                    // w=PANEL_WIDTH, keeping the reported y/h -- so the
                    // source pixels stay contiguous (fb + y*stride, count
                    // h*stride) and st7789_blit_rect's phase-1 assert
                    // (x==0 && w==stride) holds. Column clipping is
                    // pico-link-7h5.11. Only rects[0] is read: core reports
                    // exactly one bounding rect in phase 1 (design doc
                    // §5); rect_count > 0 here is therefore always 1.
                    const struct PlDamageRect *rect = &render_out.rects[0];
                    // ui_tick()/pl_ui_render_ex() must not be called again
                    // until this DMA completes (st7789_blit_rect blocks
                    // until it does) -- Rust can never write while DMA
                    // reads, per the M1b design's no-tearing, no-double-
                    // buffering contract.
                    st7789_blit_rect(px, (uint16_t)PANEL_WIDTH, 0, rect->y, (uint16_t)PANEL_WIDTH, rect->h);
                    blit_happened = true;
                }
            }
            if (blit_happened) {
                last_blit_us = time_us_64();
            }
            blit_end_us = time_us_64();
            // Bead pico-link-p1r: the pico-link-3uq blit-split candidate --
            // measured here as one blocking call, matching pico-link-14l's
            // 38.6ms figure (full-frame baseline; pico-link-7h5.8 makes
            // this a partial blit on most frames). Only recorded on the
            // frame that actually blits -- a skipped frame did not call
            // st7789_blit_rect/st7789_blit_framebuffer at all, so recording
            // render_end..blit_end unconditionally would falsely attribute
            // ~0us "blit" samples to frames that skipped it.
            if (blit_happened) {
                pl_loop_prof_record(PL_LOOP_PHASE_BLIT, blit_end_us - render_end_us);
            }
        }
        // While blanked (display_on == false) or clean (needs_paint ==
        // false): render+blit are skipped entirely. The blanked case is
        // pico-link-i3e's acceptance criterion -- this is what makes the
        // blank actually take effect on the panel. The clean case is this
        // bead (pico-link-vxc) -- the ~38.6ms blit (pico-link-14l) is paid
        // only when something could actually have changed, per
        // .planning/design/2026-09-02-dirty-gate-across-the-ffi-seam.md.
        // `App::dirty()` is untouched by either half of this gate (same
        // contract as core's own Runner::step), so the next real wake or
        // state change still renders immediately.

#ifndef PL_DIAG_SKIP_BT
        pl_wdt_mark(PL_WDT_CP_BT_POLL_CMDS);
        uint64_t bt_poll_cmds_start_us = time_us_64();
        pl_bt_poll_commands(ui);
        // Bead pico-link-p1r.
        pl_loop_prof_record(PL_LOOP_PHASE_BT_POLL_CMDS, time_us_64() - bt_poll_cmds_start_us);

        // Bead pico-link-cz0.6 (M5 persistence): the ONLY place a real flash
        // write happens -- thread context, every iteration, cheap when
        // nothing is pending (see persist.h's module doc for the full
        // streaming/settle/rate-limit gate).
        pl_persist_service();
#endif
        pl_wdt_mark(PL_WDT_CP_REPORT);

        // M3 acceptance evidence: a MEASURED byte rate, not just "it
        // enumerated". Report once a second while actually streaming --
        // sample_rate * channels * bytes/sample is the expected rate;
        // printing the measured one alongside lets a human (or the
        // verification loop) compare them directly.
        uint64_t now_us = time_us_64();
        // Bead pico-link-p1r: attribute the ~1Hz audio-report block. Timed
        // whether or not it actually fires this iteration -- the common
        // case (no fire) should land in the smallest bucket and cost the
        // histogram nothing but a bucket increment.
        uint64_t audio_report_phase_start_us = now_us;
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
        pl_loop_prof_record(PL_LOOP_PHASE_AUDIO_REPORT, time_us_64() - audio_report_phase_start_us);

        static uint32_t frame_count = 0;
        frame_count++;
        // Rate-limit the per-frame print to once a second (at a ~few-ms
        // frame time this is still hundreds of frames between prints) so
        // the CDC console stays readable rather than flooded -- the bead
        // asks for "CDC prints per-frame render and blit timing", which
        // this satisfies by printing exactly that breakdown, just not on
        // literally every single frame.
        pl_wdt_mark(PL_WDT_CP_REPORT_FRAME);
        uint64_t frame_report_phase_start_us = time_us_64();
        if (frame_count % 60 == 1) {
            pl_log(
                "frame %lu: render=%lluus blit=%lluus total=%lluus\r\n",
                (unsigned long)frame_count,
                (unsigned long long)(render_end_us - render_start_us),
                (unsigned long long)(blit_end_us - render_end_us),
                (unsigned long long)(blit_end_us - frame_start_us)
            );
        }
        // Bead pico-link-p1r.
        pl_loop_prof_record(PL_LOOP_PHASE_FRAME_REPORT, time_us_64() - frame_report_phase_start_us);

        // Bead pico-link-okx (D11): ONE shared ~1s report clock for both
        // pl_usb_pump_report and pl_a2dp_report, replacing their two
        // previously-independent rate-limit clocks -- see
        // pl_usb_pump_report's doc comment (usb_pump.h) for why an
        // unsynchronized pair of ~1.02s windows isn't good enough for this
        // bead's cross-report rate comparisons (sof_isr/s vs packets/s vs
        // enc_frames_total/s).
        static uint64_t s_last_shared_report_us = 0;
        uint64_t shared_now_us = time_us_64();
        // Bead pico-link-p1r: attribute the 1Hz shared-report block
        // (pl_usb_pump_report + pl_a2dp_report + pl_a2dp_publish_counters).
        // Deliberately excludes pl_loop_prof_publish_next() itself (called
        // below, after this block) -- this module's own publish cost is
        // not part of what it is measuring.
        uint64_t shared_report_phase_start_us = shared_now_us;
        if (s_last_shared_report_us == 0 || shared_now_us - s_last_shared_report_us >= 1000000) {
            uint32_t shared_report_dt_us =
                s_last_shared_report_us != 0 ? (uint32_t)(shared_now_us - s_last_shared_report_us) : 0;
            s_last_shared_report_us = shared_now_us;

            // Bead pico-link-tfj instrumentation, extended by pico-link-okx
            // -- see usb_pump.h's doc comment on pl_usb_pump_report.
            pl_wdt_mark(PL_WDT_CP_REPORT_SHARED);
            pl_usb_pump_report(shared_report_dt_us);

            // Bead pico-link-9eq2.3.2, design sec 5.1/7.1: the fault
            // evaluator runs at this SAME 1Hz thread-context point, BEFORE
            // pl_a2dp_report below -- it is usb_audio.c's pl_usb_audio_
            // fill_min()'s sole caller (design sec 5.6.5) and must read it
            // before pl_a2dp_report needs the cached result.
            pl_fault_evaluate(ui, shared_now_us);

            // M4 S1 (bead pico-link-cz0.5.2), design sec 7 -- the a2dp:
            // report line. Thread context only (pl_a2dp_report does no
            // BTstack calls, only pl_log + plain counter reads).
            //
            // Bead pico-link-9eq2.3.2: fill_min is now sourced from
            // pl_fault_last_fill_min() (fault.c's cache of this window's
            // ONE pl_usb_audio_fill_min() read), not a second direct call
            // -- see pl_a2dp_report's own doc comment (a2dp.h) for why.
            pl_a2dp_report(shared_report_dt_us, pl_fault_last_fill_min());

            // Bead pico-link-auh, section 1: the non-starvable slot-0
            // ("ctr") snapshot -- same 1Hz point, does not replace the
            // verbose a2dp: lines above.
            pl_a2dp_publish_counters();

            // Bead pico-link-p1r: one phase's p50/p95/p99/max/count into
            // pl_prio slot 3, round-robin -- same 1Hz cadence as the rest
            // of this block. See pl_loop_prof.h's module doc.
            pl_loop_prof_publish_next();
        }
        pl_loop_prof_record(PL_LOOP_PHASE_SHARED_REPORT, time_us_64() - shared_report_phase_start_us);

        // Bead pico-link-okx (F1): drains whatever pl_log()/pl_log_locked()
        // queued this iteration to the actual console -- THREAD CONTEXT
        // ONLY, never an IRQ or the 0xC0 worker (see pl_log_ring.h's module
        // doc). Called every iteration, not rate-limited, so the ring stays
        // close to empty between report bursts.
        pl_wdt_mark(PL_WDT_CP_LOG_DRAIN);
        uint64_t log_drain_start_us = time_us_64();
        pl_log_ring_drain();
        // Bead pico-link-p1r.
        pl_loop_prof_record(PL_LOOP_PHASE_LOG_DRAIN, time_us_64() - log_drain_start_us);

        // Bead pico-link-ufh: THE feed site -- exactly one call, from
        // thread context, in the superloop. Must never be fed from a timer
        // or IRQ (see watchdog_sup.h's module doc for why). Placed after
        // pl_a2dp_report() and before the pacing sleep below, per the
        // design doc's "Where the feed lives" section.
        pl_wdt_mark(PL_WDT_CP_WDT_SERVICE);
        uint64_t wdt_service_start_us = time_us_64();
        pl_wdt_service();
        pl_wdt_report();
        // Bead pico-link-p1r.
        pl_loop_prof_record(PL_LOOP_PHASE_WDT_SERVICE, time_us_64() - wdt_service_start_us);

        // Bead pico-link-tfj: replaced the old unconditional sleep_ms(16)
        // with a deadline measured from frame_start_us -- the body above
        // (blit alone: 38.6ms as of pico-link-14l) already exceeds the
        // budget most of the time, so this sleeps only the remainder (zero,
        // in practice) rather than always adding a further flat 16ms.
        uint64_t frame_elapsed_us = time_us_64() - frame_start_us;
        // Bead pico-link-p1r: 0 whenever the body already exceeds
        // frame_budget_us (the expected case under load, per the comment
        // above), which is itself a useful data point -- a p50 of 0 here
        // confirms the sleep is not where the missing time goes.
        uint32_t sleep_phase_us = 0;
        if (frame_elapsed_us < frame_budget_us) {
            sleep_phase_us = (uint32_t)(frame_budget_us - frame_elapsed_us);
            sleep_us(sleep_phase_us);
        }
        pl_loop_prof_record(PL_LOOP_PHASE_SLEEP, sleep_phase_us);
    }

    return 0;
}
