// Pico Link firmware -- implementation of the superloop per-phase timing
// histograms. See pl_loop_prof.h's module doc for the why; this file is
// the how.
#include "pl_loop_prof.h"

#include <string.h>

#include "pl_prio.h"

// Bucket upper bounds in microseconds. Chosen to span from sub-frame
// (50us) to well past the ~164ms/iteration this bead measured, with an
// overflow bucket for anything at or above the last bound. 13 finite
// bounds + 1 overflow bucket = 14 buckets/phase.
static const uint32_t kBucketBoundsUs[] = {
    50,     100,    200,    500,     1000,    2000,   5000,
    10000,  20000,  50000,  100000,  200000,  500000,
};
#define PL_LOOP_PROF_NUM_BOUNDS (sizeof(kBucketBoundsUs) / sizeof(kBucketBoundsUs[0]))
#define PL_LOOP_PROF_NUM_BUCKETS (PL_LOOP_PROF_NUM_BOUNDS + 1u) // + overflow

// Sentinel reported for a percentile that falls in the overflow bucket --
// "at least 500ms", not a fabricated finite value.
#define PL_LOOP_PROF_OVERFLOW_US 999999u

typedef struct {
    uint32_t buckets[PL_LOOP_PROF_NUM_BUCKETS];
    uint64_t total_count;
    uint64_t max_us;
} pl_loop_phase_hist_t;

static pl_loop_phase_hist_t s_hist[PL_LOOP_PHASE_COUNT];
static uint32_t s_publish_cursor = 0;

// Short fixed codes for the "ph=" field, one per pl_loop_phase_t, in enum
// order. Kept short (<=4 chars) so the formatted line stays well inside
// PL_PRIO_SLOT_LEN regardless of which phase is selected.
static const char *const kPhaseCodes[PL_LOOP_PHASE_COUNT] = {
    "in",   // PL_LOOP_PHASE_INPUT
    "dbg",  // PL_LOOP_PHASE_DEBUG_REMOTE
    "btd",  // PL_LOOP_PHASE_BT_DRAIN
    "tick", // PL_LOOP_PHASE_UI_TICK
    "rend", // PL_LOOP_PHASE_UI_RENDER
    "blit", // PL_LOOP_PHASE_BLIT
    "btp",  // PL_LOOP_PHASE_BT_POLL_CMDS
    "aud",  // PL_LOOP_PHASE_AUDIO_REPORT
    "frm",  // PL_LOOP_PHASE_FRAME_REPORT
    "shr",  // PL_LOOP_PHASE_SHARED_REPORT
    "log",  // PL_LOOP_PHASE_LOG_DRAIN
    "wdt",  // PL_LOOP_PHASE_WDT_SERVICE
    "tel",  // PL_LOOP_PHASE_TELEMETRY
    "slp",  // PL_LOOP_PHASE_SLEEP
    "tot",  // PL_LOOP_PHASE_TOTAL
};

void pl_loop_prof_record(pl_loop_phase_t phase, uint64_t duration_us) {
    if ((uint32_t)phase >= PL_LOOP_PHASE_COUNT) {
        return;
    }
    pl_loop_phase_hist_t *h = &s_hist[phase];

    uint32_t bucket = (uint32_t)PL_LOOP_PROF_NUM_BOUNDS; // default: overflow
    for (uint32_t i = 0; i < PL_LOOP_PROF_NUM_BOUNDS; i++) {
        if (duration_us < kBucketBoundsUs[i]) {
            bucket = i;
            break;
        }
    }
    h->buckets[bucket]++;
    h->total_count++;
    if (duration_us > h->max_us) {
        h->max_us = duration_us;
    }
}

// Integer-only percentile lookup: walks buckets low to high, returns the
// upper bound of the first bucket whose cumulative count reaches
// `percent`% of total_count. This is a conservative (upper-bound)
// approximation -- exact only if the target rank lands exactly on a
// bucket boundary -- which is the right direction to err for a value that
// feeds a dwell-budget-style threshold (pico-link-cz0.5.8's lesson: don't
// let a summary statistic hide a tail).
static uint32_t percentile_us(const pl_loop_phase_hist_t *h, uint32_t percent) {
    if (h->total_count == 0) {
        return 0;
    }
    uint64_t target = (h->total_count * percent + 99) / 100; // ceil
    if (target == 0) {
        target = 1;
    }
    uint64_t cumulative = 0;
    for (uint32_t i = 0; i < PL_LOOP_PROF_NUM_BUCKETS; i++) {
        cumulative += h->buckets[i];
        if (cumulative >= target) {
            return (i < PL_LOOP_PROF_NUM_BOUNDS) ? kBucketBoundsUs[i] : PL_LOOP_PROF_OVERFLOW_US;
        }
    }
    // Unreachable if total_count matches the sum of buckets, but fall back
    // to the overflow sentinel rather than an uninitialized read.
    return PL_LOOP_PROF_OVERFLOW_US;
}

void pl_loop_prof_publish_next(void) {
    uint32_t phase = s_publish_cursor;
    s_publish_cursor = (s_publish_cursor + 1u) % (uint32_t)PL_LOOP_PHASE_COUNT;

    const pl_loop_phase_hist_t *h = &s_hist[phase];
    uint32_t p50 = percentile_us(h, 50);
    uint32_t p95 = percentile_us(h, 95);
    uint32_t p99 = percentile_us(h, 99);

    // Wire format (parse by whitespace-split key=value tokens after "lpf"):
    //   lpf ph=<code> p50=<us> p95=<us> p99=<us> mx=<us> n=<count>
    // p50/p95/p99 are bucket-upper-bound approximations in microseconds
    // (999999 means "at or above 500000us", the overflow bucket); mx is
    // the EXACT running max in microseconds; n is the cumulative sample
    // count for this phase since boot. One phase is published per call,
    // round-robin over pl_loop_phase_t in enum order, so a full cycle over
    // all phases takes PL_LOOP_PHASE_COUNT calls (main.c calls this once a
    // second, from its existing 1Hz shared-report block).
    pl_prio_publish(
        3,
        "lpf ph=%s p50=%07lu p95=%07lu p99=%07lu mx=%08lu n=%010lu",
        kPhaseCodes[phase],
        (unsigned long)p50,
        (unsigned long)p95,
        (unsigned long)p99,
        (unsigned long)h->max_us,
        (unsigned long)h->total_count
    );
}
