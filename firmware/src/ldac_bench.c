// Pico Link firmware -- LDAC L0 on-target offline benchmark implementation.
// See ldac_bench.h's module doc for scope and the design doc it answers to
// (.planning/design/2026-08-30-ldac.md Q1/Q3, bead pico-link-cz0.5.4).
#include "ldac_bench.h"

#include <malloc.h>
#include <math.h>
#include <stdint.h>
#include <string.h>

#include "pico/time.h"

#include "ldacBT.h"
#include "usb_pump.h"

// Standard 2-DH5-target MTU, per ldacBT.h's own doc comment on
// ldac_transport_frame: "The minimum MTU that a L2CAP implementation for
// LDAC shall support is 679 bytes". Not a real AVDTP-negotiated MTU (no
// A2DP wiring in this bead) -- just a representative value to init the
// encoder with, matching what a real negotiation would settle near.
#define PL_LDAC_BENCH_MTU 679

// LDACBT_MAX_NBYTES (ldacBT.h) is the documented max size of one
// ldac_transport_frame sequence -- safe upper bound for the output buffer.
#define PL_LDAC_BENCH_OUT_CAP LDACBT_MAX_NBYTES

#define PL_LDAC_BENCH_SAMPLE_RATE_HZ 48000
#define PL_LDAC_BENCH_CHANNELS 2

// Number of ldacBT_encode() calls to time per EQMID -- enough to get a
// stable mean/max without the benchmark itself taking so long it looks like
// a hang on the console.
#define PL_LDAC_BENCH_ITERATIONS 200

typedef struct {
    int eqmid;
    const char *name;
    uint32_t nominal_bitrate_bps; // per ldacBT.h's own table, @48kHz
} pl_ldac_bench_case_t;

static const pl_ldac_bench_case_t s_cases[] = {
    {LDACBT_EQMID_HQ, "HQ", 990000},
    {LDACBT_EQMID_SQ, "SQ", 660000},
    {LDACBT_EQMID_MQ, "MQ", 330000},
};

// Synthetic PCM: a real (non-silent, non-trivial) two-tone signal, so the
// encoder's bit allocation / quantization loops run their normal path
// rather than any all-zero-block shortcut a real music stream would never
// hit. LDACBT_ENC_LSU samples/channel, interleaved S16, filled once and
// reused every call -- this is a CPU/heap benchmark, not an audio-quality
// one.
static int16_t s_pcm[LDACBT_ENC_LSU * PL_LDAC_BENCH_CHANNELS];

static void pl_ldac_bench_fill_pcm(void) {
    for (int i = 0; i < LDACBT_ENC_LSU; i++) {
        float t = (float)i / (float)PL_LDAC_BENCH_SAMPLE_RATE_HZ;
        float l = 0.5f * sinf(2.0f * (float)M_PI * 440.0f * t);
        float r = 0.5f * sinf(2.0f * (float)M_PI * 660.0f * t);
        s_pcm[i * 2 + 0] = (int16_t)(l * 32000.0f);
        s_pcm[i * 2 + 1] = (int16_t)(r * 32000.0f);
    }
}

static uint32_t pl_ldac_bench_heap_used_bytes(void) {
    // newlib's mallinfo() -- uordblks is total bytes currently allocated
    // from the arena. Adequate for "did this grow" comparisons on a target
    // with no other concurrent allocator activity during the benchmark
    // (BT/UI are skipped -- see main.c's PL_DIAG_LDAC_BENCH gate, which
    // runs before cyw43_arch_init()/pl_ui_create()).
    struct mallinfo info = mallinfo();
    return (uint32_t)info.uordblks;
}

static void pl_ldac_bench_run_case(const pl_ldac_bench_case_t *tc) {
    uint32_t heap_before_handle = pl_ldac_bench_heap_used_bytes();

    HANDLE_LDAC_BT h = ldacBT_get_handle();
    if (h == NULL) {
        pl_log("PL_DIAG_LDAC_BENCH: %s: ldacBT_get_handle() FAILED\r\n", tc->name);
        return;
    }

    int rc = ldacBT_init_handle_encode(h, PL_LDAC_BENCH_MTU, tc->eqmid, LDAC_CCI_STEREO,
                                        LDACBT_SMPL_FMT_S16, PL_LDAC_BENCH_SAMPLE_RATE_HZ);
    if (rc != 0) {
        pl_log("PL_DIAG_LDAC_BENCH: %s: ldacBT_init_handle_encode() FAILED (rc=%d)\r\n", tc->name, rc);
        ldacBT_free_handle(h);
        return;
    }

    uint32_t heap_after_init = pl_ldac_bench_heap_used_bytes();

    static uint8_t out[PL_LDAC_BENCH_OUT_CAP];
    uint64_t total_us = 0;
    uint64_t max_us = 0;
    uint64_t min_us = UINT64_MAX;
    int errors = 0;

    for (int i = 0; i < PL_LDAC_BENCH_ITERATIONS; i++) {
        int pcm_used = 0;
        int stream_sz = 0;
        int frame_num = 0;

        uint64_t start_us = time_us_64();
        int enc_rc = ldacBT_encode(h, s_pcm, &pcm_used, out, &stream_sz, &frame_num);
        uint64_t elapsed_us = time_us_64() - start_us;

        if (enc_rc != 0) {
            errors++;
            continue;
        }

        total_us += elapsed_us;
        if (elapsed_us > max_us) {
            max_us = elapsed_us;
        }
        if (elapsed_us < min_us) {
            min_us = elapsed_us;
        }
    }

    uint32_t heap_after_encode = pl_ldac_bench_heap_used_bytes();

    int ran = PL_LDAC_BENCH_ITERATIONS - errors;
    uint64_t avg_us = ran > 0 ? (total_us / (uint64_t)ran) : 0;

    pl_log("PL_DIAG_LDAC_BENCH: %s (%lu bps nominal): avg=%lu us, min=%lu us, max=%lu us, "
           "errors=%d/%d, heap: handle=%lu B, +init=%lu B, +encode=%lu B (delta after init=%ld B)\r\n",
           tc->name, (unsigned long)tc->nominal_bitrate_bps, (unsigned long)avg_us, (unsigned long)min_us,
           (unsigned long)max_us, errors, PL_LDAC_BENCH_ITERATIONS, (unsigned long)heap_before_handle,
           (unsigned long)heap_after_init, (unsigned long)heap_after_encode,
           (long)(heap_after_encode - heap_after_init));

    ldacBT_close_handle(h);
    ldacBT_free_handle(h);
}

void pl_ldac_bench_run(void) {
    pl_log("PL_DIAG_LDAC_BENCH: starting -- LDACBT_ENC_LSU=%d (design assumed 128), "
           "%d iterations/EQMID, MTU=%d\r\n",
           LDACBT_ENC_LSU, PL_LDAC_BENCH_ITERATIONS, PL_LDAC_BENCH_MTU);
#ifdef _32BIT_FIXED_POINT
    pl_log("PL_DIAG_LDAC_BENCH: build uses _32BIT_FIXED_POINT (INT32 path)\r\n");
#else
    pl_log("PL_DIAG_LDAC_BENCH: build uses default float path (SCALAR=float, "
           "no double-precision arithmetic in the vendored source -- see "
           "firmware/vendor/libldac/PROVENANCE.md)\r\n");
#endif

    pl_ldac_bench_fill_pcm();

    for (size_t i = 0; i < sizeof(s_cases) / sizeof(s_cases[0]); i++) {
        pl_ldac_bench_run_case(&s_cases[i]);
    }

    pl_log("PL_DIAG_LDAC_BENCH: done\r\n");
}
