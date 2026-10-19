// Pico Link firmware -- independent host-clock-offset reference. See
// host_clock_ref.h's module doc for the why, the edge-timestamp design,
// and the sign convention.
#include "host_clock_ref.h"

#include <stdbool.h>

// 10 seconds: long enough that ISR-latency jitter (low microseconds) is a
// tiny fraction of the span (~0.1ppm resolution per host_clock_ref.h's
// doc), short enough to get a fresh reading inside a single short
// hardware-capture session rather than needing a 10-minute soak.
#define PL_HOST_CLOCK_WINDOW_US (10u * 1000u * 1000u)

// This USB controller (RP2350's dcd, like RP2040's) is full-speed only --
// one SOF per 1ms, never the 125us high-speed microframe cadence -- see
// usb_pump.c's module doc. If this firmware ever grows a high-speed PHY,
// this constant (and the comment above it) must change together.
#define PL_HOST_SOF_HZ 1000u

static bool s_have_window_start;
static uint64_t s_window_start_time_us;
static uint32_t s_window_start_sof;
static int32_t s_cached_ppm = PL_HOST_CLOCK_PPM_NOT_READY;

static void start_window(uint64_t sof_time_us, uint32_t sof_count) {
    s_window_start_time_us = sof_time_us;
    s_window_start_sof = sof_count;
    s_have_window_start = true;
}

void pl_host_clock_ref_poll(uint64_t sof_time_us, uint32_t sof_count) {
    if (!s_have_window_start) {
        start_window(sof_time_us, sof_count);
        return;
    }

    // sof_count is a free-running uint32_t incremented only by the SOF
    // ISR (usb_pump.c); plain modular subtraction is correct across at
    // most one wraparound, which a 10s window at 1000 SOF/s (10,000
    // counts) is nowhere near forcing.
    uint32_t sof_delta = sof_count - s_window_start_sof;
    if (sof_delta == 0) {
        // No new edge since the window started (or since the last call
        // -- see this function's doc comment): nothing to measure yet,
        // and nothing to corrupt by waiting. Do NOT restart the window
        // here -- that would mean a quiet bus (SOF briefly not serviced)
        // keeps resetting the start point and the window never completes.
        return;
    }

    // us_delta is the EXACT elapsed time between two real SOF edges (the
    // window-start edge and this call's edge) -- not a time window chosen
    // by anything outside the SOF stream itself, which is what removes
    // the +/-1-SOF phase ambiguity the first cut of this bead had (see
    // host_clock_ref.h's module doc). sof_time_us is itself monotonic
    // (time_us_64(), read inside usb_pump.c's SOF ISR), so this
    // subtraction cannot underflow.
    uint64_t us_delta = sof_time_us - s_window_start_time_us;
    if (us_delta < PL_HOST_CLOCK_WINDOW_US) {
        return;
    }

    // host_ppm = ((sof_delta / (us_delta / 1,000,000)) / PL_HOST_SOF_HZ -
    // 1) * 1e6, i.e. "measured SOF rate vs nominal, in ppm" -- rearranged
    // to the form below to divide once. Double, not fixed-point: this is
    // a ~once-per-10s console diagnostic, not a real-time control path
    // (unlike drain_servo.c's accrual, which must stay fixed-point for
    // the reasons documented there), and the codebase already uses float/
    // double elsewhere for non-real-time computation (a2dp.c, dsp.c).
    double sof_delta_d = (double)sof_delta;
    double us_delta_d = (double)us_delta;
    double ppm = ((sof_delta_d * 1000.0) / us_delta_d - 1.0) * 1000000.0;
    s_cached_ppm = (int32_t)ppm;

    start_window(sof_time_us, sof_count);
}

int32_t pl_host_clock_ref_ppm(void) {
    return s_cached_ppm;
}

void pl_host_clock_ref_bus_reset(void) {
    s_have_window_start = false;
    s_cached_ppm = PL_HOST_CLOCK_PPM_NOT_READY;
}
