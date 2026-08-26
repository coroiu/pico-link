// Minimal hal_cpu / hal_tick / hal_time_ms shims for the BTstack embedded
// port. Real implementations will read the RP2350 SysTick/timer directly
// from Rust; these stubs exist only to satisfy the linker for the
// cross-compile/link spike (gate 3/4 of pico-link-8v3.1).
#include <stdint.h>

void hal_cpu_disable_irqs(void) {}
void hal_cpu_enable_irqs(void) {}
void hal_cpu_enable_irqs_and_sleep(void) {}

static void (*tick_handler)(void) = 0;
void hal_tick_init(void) {}
void hal_tick_set_handler(void (*handler)(void)) { tick_handler = handler; }
int hal_tick_get_tick_period_in_ms(void) { return 1; }

uint32_t hal_time_ms(void) { return 0; }
