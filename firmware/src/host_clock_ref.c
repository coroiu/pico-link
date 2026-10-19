// Pico Link firmware -- independent host-clock-offset reference. See
// host_clock_ref.h's module doc for the why and the sign convention.
#include "host_clock_ref.h"

#include <stdbool.h>

// 10 seconds: long enough that a +/-20ppm agreement bar (design sec 6)
// resolves to a handful of SOF counts of slack (10s * 1000 SOF/s *
// 20e-6 = 0.2 SOFs), short enough to get a fresh reading inside a single
// short hardware-capture session rather than needing a 10-minute soak.
#define PL_HOST_CLOCK_WINDOW_US (10u * 1000u * 1000u)

// This USB controller (RP2350's dcd, like RP2040's) is full-speed only --
// one SOF per 1ms, never the 125us high-speed microframe cadence -- see
// usb_pump.c's module doc. If this firmware ever grows a high-speed PHY,
// this constant (and the comment above it) must change together.
#define PL_HOST_SOF_HZ 1000u

static bool s_have_window_start;
static uint64_t s_window_start_us;
static uint32_t s_window_start_sof;
static int32_t s_cached_ppm = PL_HOST_CLOCK_PPM_NOT_READY;

static void start_window(uint64_t now_us, uint32_t sof_count) {
    s_window_start_us = now_us;
    s_window_start_sof = sof_count;
    s_have_window_start = true;
}

void pl_host_clock_ref_poll(uint64_t now_us, uint32_t sof_count) {
    if (!s_have_window_start) {
        start_window(now_us, sof_count);
        return;
    }

    // now_us is time_us_64() -- monotonic, never wraps in any firmware
    // uptime that matters here -- so this subtraction cannot underflow as
    // long as the caller passes a now_us at or after the start sample,
    // which main.c's single shared report-clock call site guarantees.
    uint64_t elapsed_us = now_us - s_window_start_us;
    if (elapsed_us < PL_HOST_CLOCK_WINDOW_US) {
        return;
    }

    // sof_count is a free-running uint32_t incremented only by the SOF
    // ISR (usb_pump.c); plain modular subtraction is correct across at
    // most one wraparound, which a 10s window at 1000 SOF/s (10,000
    // counts) is nowhere near forcing.
    uint32_t sof_delta = sof_count - s_window_start_sof;

    // Expected SOF count if the host's crystal matched ours exactly:
    // elapsed_us / (1,000,000 / PL_HOST_SOF_HZ) = elapsed_us *
    // PL_HOST_SOF_HZ / 1,000,000. ppm = (actual - expected) / expected *
    // 1e6, rearranged to avoid an intermediate division by a
    // near-but-not-exactly-elapsed_us/1000 expected value:
    //   ppm = (sof_delta * 1,000,000/PL_HOST_SOF_HZ - elapsed_us)
    //         * 1e6 / elapsed_us
    // All-integer (project convention, same reasoning as
    // drain_servo.c's fixed-point accrual): int64_t throughout, elapsed_us
    // is >=10,000,000 here so the final division denominator is never
    // near zero, and the numerator stays well inside int64_t range even
    // at the +/-1000ppm-equivalent extremes this measurement could show.
    int64_t us_per_sof = 1000000 / (int64_t)PL_HOST_SOF_HZ;
    int64_t numerator = ((int64_t)sof_delta * us_per_sof - (int64_t)elapsed_us) * 1000000;
    s_cached_ppm = (int32_t)(numerator / (int64_t)elapsed_us);

    start_window(now_us, sof_count);
}

int32_t pl_host_clock_ref_ppm(void) {
    return s_cached_ppm;
}

void pl_host_clock_ref_bus_reset(void) {
    s_have_window_start = false;
    s_cached_ppm = PL_HOST_CLOCK_PPM_NOT_READY;
}
