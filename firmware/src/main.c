// Pico Link firmware -- M1b: the C-first architecture proof.
//
// C-first: pico-sdk owns main() and runtime_init (see
// .planning/decisions/2026-08-27-c-first-pico-sdk-owns-main.md). This
// milestone links in ui-ffi, a no_std + alloc Rust staticlib, over the
// narrow extern "C" surface in the generated firmware/include/pico_link_ui.h
// (see CMakeLists.txt -- cbindgen regenerates it from ui-ffi/src/lib.rs on
// every build, so it can never hand-drift from the real FFI ABI).
//
// Deliberately does NOT poke CPACR or touch any coprocessor-access register:
// pico-sdk's runtime_init() is responsible for that now. If a NOCP
// UsageFault shows up here, that falsifies the C-first pivot's premise --
// stop and escalate, don't patch around it locally.

#include <stdio.h>

#include "pico/stdlib.h"
#include "pico_link_ui.h"

// The one call in the Rust -> C direction (see pico_link_ui.h's doc comment
// on pl_ui_panic_hook): Rust hands us a panic message on the way to
// spinning forever, since it has no unwinder on this target and nothing
// else safe to do. Report it over the CDC console -- the only I/O channel
// this milestone has -- so a panic during bring-up is visible rather than
// looking like a silent hang.
void pl_ui_panic_hook(const uint8_t *msg, uintptr_t len) {
    printf("\r\n!!! ui-ffi PANIC: %.*s\r\n", (int)len, (const char *)msg);
}

#define PANEL_WIDTH 240
#define PANEL_HEIGHT 240

int main(void) {
    stdio_init_all();

    // Give a host-side terminal a moment to attach after CDC enumerates,
    // so the boot banner below isn't lost before anyone is listening.
    sleep_ms(1500);

    printf("\r\n=== pico_link firmware boot ===\r\n");
    printf("board: pimoroni_pico_plus2_w_rp2350\r\n");
    printf("pico-sdk owns main(); ui-ffi (Rust core) linked in over FFI.\r\n");

    struct PlUi *ui = pl_ui_create(PANEL_WIDTH, PANEL_HEIGHT);
    if (ui == NULL) {
        printf("pl_ui_create FAILED -- halting\r\n");
        while (true) {
            tight_loop_contents();
        }
    }
    printf("pl_ui_create OK\r\n");

    // Sample the same pixel core's own tests use to prove a selection
    // move actually happened (row 0's highlight fill, well past the
    // chip/text -- see core/src/app.rs's
    // handle_input_marks_dirty_and_moving_selection_changes_the_rendered_framebuffer
    // test), NOT pixel 0 -- (0,0) sits in the header bar, which a list
    // selection move never touches, so it would look "unchanged" even with
    // a fully working input path.
    #define SAMPLE_X 200
    #define SAMPLE_Y 18
    #define SAMPLE_INDEX ((SAMPLE_Y) * PANEL_WIDTH + (SAMPLE_X))

    const uint16_t *px = NULL;
    uintptr_t px_len = 0;
    pl_ui_render(ui, &px, &px_len);
    // Snapshot the sample pixel's value NOW, into a local -- `px` itself
    // stays a borrowed pointer into the app's single live framebuffer (see
    // pl_ui_render's doc comment), so re-reading it after the second
    // render below would just show the *new* frame's value, not prove
    // anything changed.
    uint16_t sample_before = (px_len > SAMPLE_INDEX) ? px[SAMPLE_INDEX] : 0;
    printf(
        "pl_ui_render: %lu pixels (expected %d), sample pixel (%d,%d) = 0x%04x\r\n",
        (unsigned long)px_len,
        PANEL_WIDTH * PANEL_HEIGHT,
        SAMPLE_X, SAMPLE_Y,
        sample_before
    );

    // Exercise the input path too: one Down intent should move the
    // selection, which (per pico_link_core::App::handle_input) marks the
    // app dirty -- rendering again should hand back a different value at
    // the sampled pixel than the fresh-app render above if (and only if)
    // the FFI input plumbing is actually wired correctly end to end.
    struct PlIntent down = { .tag = PL_INTENT_TAG_DOWN, .jump_by = 0 };
    pl_ui_input(ui, &down, 1);
    pl_ui_tick(ui, time_us_64());

    const uint16_t *px2 = NULL;
    uintptr_t px2_len = 0;
    pl_ui_render(ui, &px2, &px2_len);
    uint16_t sample_after = (px2_len > SAMPLE_INDEX) ? px2[SAMPLE_INDEX] : 0;
    printf(
        "after Down: %lu pixels, sample pixel (%d,%d) = 0x%04x (%s)\r\n",
        (unsigned long)px2_len,
        SAMPLE_X, SAMPLE_Y,
        sample_after,
        (px_len == px2_len && px2_len > SAMPLE_INDEX && sample_before == sample_after) ? "UNCHANGED, input path may be broken" : "changed as expected"
    );

    uint32_t heartbeat = 0;
    while (true) {
        printf("heartbeat %lu\r\n", (unsigned long)heartbeat++);
        sleep_ms(1000);
    }

    return 0;
}
