// Pico Link firmware -- M1a bring-up.
//
// C-first: pico-sdk owns main() and runtime_init (see
// .planning/decisions/2026-08-27-c-first-pico-sdk-owns-main.md). This
// milestone has no Rust in it at all -- it exists to de-risk the CMake +
// pico-sdk build/flash/console topology before any FFI is introduced.
//
// Deliberately does NOT poke CPACR or touch any coprocessor-access register:
// pico-sdk's runtime_init() is responsible for that now. If a NOCP
// UsageFault shows up here, that falsifies the C-first pivot's premise --
// stop and escalate, don't patch around it locally.

#include <stdio.h>

#include "pico/stdlib.h"

int main(void) {
    stdio_init_all();

    // Give a host-side terminal a moment to attach after CDC enumerates,
    // so the boot banner below isn't lost before anyone is listening.
    sleep_ms(1500);

    printf("\r\n=== pico_link firmware boot ===\r\n");
    printf("board: pimoroni_pico_plus2_w_rp2350\r\n");
    printf("pico-sdk owns main(); no Rust in this build.\r\n");

    uint32_t heartbeat = 0;
    while (true) {
        printf("heartbeat %lu\r\n", (unsigned long)heartbeat++);
        sleep_ms(1000);
    }

    return 0;
}
