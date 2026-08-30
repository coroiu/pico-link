// Pico Link firmware -- software watchdog supervisor. See watchdog_sup.h's
// module doc for the shape; this file is the implementation.
//
// Design of record: .planning/design/2026-08-30-watchdog.md (bead
// pico-link-ufh). Everything below implements that design's Step 1-5 (the
// module, producers wired elsewhere, the CMake option). Steps 0, 6, 7, 8
// (boot-time cyw43_arch_init measurement, the first observe-only hardware
// run, enabling tripping, and handing blackout_begin/end to cz0.6.1) are
// deliberately NOT this bead's scope -- they need hardware, which another
// agent holds at the time of writing.
//
// Gated on PL_WATCHDOG (CMakeLists.txt option, default ON): every function
// below compiles to an empty body when PL_WATCHDOG is not defined, so an
// -DPL_WATCHDOG=OFF build still links (every call site in usb_pump.c,
// bt.c, a2dp.c and main.c is unconditional) but the kicks/service/arm/report
// calls compile away to nothing.
//
// --- Hardware watchdog timeout arithmetic (from the design doc; copied
// here so PL_WDT_TIMEOUT_MS is never unexplained folklore) ---
//
//   Term                                              ms
//   Flash sector erase, IRQs off (cz0.6.1)            400  (worst case)
//   16 x 256B page programs @ 3ms worst                48
//   pl_ui_render + st7789_blit_framebuffer             60  (blit measured 38.6ms, pico-link-14l, rounded up)
//   Console output: ~6 pl_log lines x 2ms timeout      12  (CMakeLists.txt PICO_STDIO_USB_STDOUT_TIMEOUT_US)
//   Frame pacing sleep                                 16  (main.c)
//   Worst legitimate iteration                        536
//   Round up                                           600
//   x3 safety factor (jitter, 0xFF IRQ preemption,    1800
//     future codecs)
//   Chosen                                            2000
//
// 2000ms is 12% of the 16.777s hardware ceiling (WATCHDOG_LOAD_BITS on
// RP2350) -- room to grow if a future codec's encode cost lands heavier
// than SBC's.
//
// --- Per-subsystem deadlines (design doc's subsystem table) ---
//
//   PL_WDT_USB_TIMER  100ms  (expected 1000Hz -- the 1ms timer/0xC0 IRQ path)
//   PL_WDT_USB_TASK   250ms  (expected ~1000Hz minus contention -- tud_task() reached)
//   PL_WDT_BTSTACK   1000ms  (expected 10Hz -- a dedicated 100ms run-loop heartbeat timer)
//   PL_WDT_MEDIA      500ms  (expected 100Hz WHILE STREAMING -- the A2DP media timer;
//                             disabled outside PL_A2DP_MEDIA_STREAMING)
//
// USB-audio packet flow itself is deliberately NOT a subsystem here -- it
// is report-only (usb_pump.c's existing counters), never a reset trigger.
// A host legitimately idling in alt-setting 1 is indistinguishable, from
// here, from a dead endpoint; a false reboot mid-listening is worse than a
// missed detection.
#include "watchdog_sup.h"

#include "hardware/structs/watchdog.h"
#include "hardware/watchdog.h"
#include "pico/time.h"

#include "panic_recorder.h"
#include "usb_pump.h"

#define PL_WDT_TIMEOUT_MS 2000u

static const uint32_t PL_WDT_DEADLINE_US[PL_WDT_COUNT] = {
    [PL_WDT_USB_TIMER] = 100000u,
    [PL_WDT_USB_TASK] = 250000u,
    [PL_WDT_BTSTACK] = 1000000u,
    [PL_WDT_MEDIA] = 500000u,
};

typedef struct {
    volatile uint32_t counter; // written by pl_wdt_kick(); one writer per slot
    uint32_t last_seen;        // last counter value observed by pl_wdt_service()
    uint64_t last_change_us;   // when last_seen last changed
    uint32_t max_staleness_us; // worst staleness since the last pl_wdt_report()
    bool enabled;
} pl_wdt_state_t;

static pl_wdt_state_t s_state[PL_WDT_COUNT];
static uint64_t s_last_report_us;

const char *pl_wdt_subsys_name(pl_wdt_subsys_t s) {
    switch (s) {
        case PL_WDT_USB_TIMER:
            return "USB_TIMER";
        case PL_WDT_USB_TASK:
            return "USB_TASK";
        case PL_WDT_BTSTACK:
            return "BTSTACK";
        case PL_WDT_MEDIA:
            return "MEDIA";
        default:
            return "UNKNOWN";
    }
}

// Survives a watchdog reset: .uninitialized_data is NOLOAD, and a watchdog
// reset does not clear SRAM. The magic distinguishes a real record from
// cold-boot garbage.
#define PL_LOOP_TRACE_MAGIC 0x4C4F4F50u  // "LOOP"

typedef struct {
    uint32_t magic;
    uint32_t checkpoint;      // last checkpoint reached
    uint32_t prev_checkpoint; // the one before it -- shows direction of travel
    uint32_t seq;             // superloop iteration count
    uint32_t last_us;         // time_us_32() when the last mark was stamped
    uint32_t loop_top_us;     // time_us_32() at the top of the current iteration
} pl_loop_trace_t;

// volatile: the fields are written by one function and read by another in
// the same TU after a reset, with nothing in between the compiler can see.
// Round 3's soak2 record (LAST=BT_POLL_CMDS PREV=BLIT_SPI_DRAIN) is
// internally inconsistent -- BLIT_EXIT sits unconditionally between those
// two marks and must have been stamped -- so the pair is not trustworthy on
// its own. volatile removes reordering/elision as an explanation, and the
// ring below removes the need to reason from two values at all.
static volatile pl_loop_trace_t __attribute__((section(".uninitialized_data.pl_loop_trace")))
    s_loop_trace;

// Last PL_WDT_RING_LEN marks with timestamps. At ~20 marks per frame and
// ~30fps this covers roughly the last 25ms of superloop history, which is
// enough to show the mark sequence LEADING INTO a stall plus the exact
// microsecond gap -- strictly more information than LAST/PREV, and it makes
// an inconsistent pair diagnosable instead of merely puzzling.
#define PL_WDT_RING_LEN 16u
typedef struct {
    uint32_t magic;
    uint32_t head;                        // next slot to write
    uint8_t cp[PL_WDT_RING_LEN];
    uint32_t us[PL_WDT_RING_LEN];
} pl_loop_ring_t;

static volatile pl_loop_ring_t __attribute__((section(".uninitialized_data.pl_loop_ring")))
    s_loop_ring;

static const char *const PL_WDT_CP_NAMES[PL_WDT_CP_COUNT] = {
    "NONE", "LOOP_TOP", "INPUT_POLL", "UI_INPUT", "DEBUG_REMOTE", "BT_DRAIN",
    "UI_TICK", "UI_RENDER", "BLIT_ENTER", "BLIT_DMA_WAIT", "BLIT_SPI_DRAIN",
    "BLIT_EXIT", "BT_POLL_CMDS", "BT_POLL_FFI", "BT_POLL_DISPATCH",
    "CMD_NONE", "CMD_SCAN_CALL", "CMD_SCAN_RET", "CMD_CONNECT_ENTER",
    "CMD_CONNECT_A2DP", "CMD_CONNECT_RET", "CMD_CANCEL_SCAN_CALL",
    "CMD_CANCEL_SCAN_RET", "CMD_OTHER",
    "REPORT", "REPORT_FRAME", "REPORT_SHARED", "LOG_DRAIN", "WDT_SERVICE",
};

void pl_wdt_mark(pl_wdt_checkpoint_t cp) {
    uint32_t now = time_us_32();
    s_loop_trace.prev_checkpoint = s_loop_trace.checkpoint;
    s_loop_trace.checkpoint = (uint32_t)cp;
    s_loop_trace.last_us = now;
    if (cp == PL_WDT_CP_LOOP_TOP) {
        s_loop_trace.seq++;
        s_loop_trace.loop_top_us = now;
    }
    s_loop_trace.magic = PL_LOOP_TRACE_MAGIC;

    uint32_t h = s_loop_ring.head;
    if (s_loop_ring.magic != PL_LOOP_TRACE_MAGIC || h >= PL_WDT_RING_LEN) {
        h = 0;  // cold boot: garbage index would corrupt memory past the ring
        s_loop_ring.magic = PL_LOOP_TRACE_MAGIC;
    }
    s_loop_ring.cp[h] = (uint8_t)cp;
    s_loop_ring.us[h] = now;
    s_loop_ring.head = (h + 1u) % PL_WDT_RING_LEN;
}

// Logs the surviving trace. Called from pl_wdt_report_boot_reason(). Reports
// the STALL DURATION as (last_us - loop_top_us): how far into the iteration
// the loop got before time stopped.
static void pl_wdt_report_loop_trace(void) {
    if (s_loop_trace.magic != PL_LOOP_TRACE_MAGIC) {
        pl_log("wdt: loop-trace = none (cold boot or SRAM not preserved)\r\n");
        return;
    }
    uint32_t cp = s_loop_trace.checkpoint;
    uint32_t prev = s_loop_trace.prev_checkpoint;
    const char *cpn = (cp < PL_WDT_CP_COUNT) ? PL_WDT_CP_NAMES[cp] : "?";
    const char *prevn = (prev < PL_WDT_CP_COUNT) ? PL_WDT_CP_NAMES[prev] : "?";
    pl_log("wdt: loop-trace LAST=%s PREV=%s seq=%u in_iter_us=%u\r\n",
           cpn, prevn, (unsigned)s_loop_trace.seq,
           (unsigned)(s_loop_trace.last_us - s_loop_trace.loop_top_us));
    // The ring: oldest first, each with the microseconds SINCE THE PREVIOUS
    // mark. The stall is the one huge delta, and the mark printed BEFORE it
    // is the region that hung.
    if (s_loop_ring.magic == PL_LOOP_TRACE_MAGIC) {
        uint32_t h = s_loop_ring.head % PL_WDT_RING_LEN;
        uint32_t prev_us = 0;
        for (uint32_t i = 0; i < PL_WDT_RING_LEN; i++) {
            uint32_t idx = (h + i) % PL_WDT_RING_LEN;
            uint32_t c = s_loop_ring.cp[idx];
            uint32_t t = s_loop_ring.us[idx];
            pl_log("wdt: ring[%u] %s dt_us=%u\r\n", (unsigned)i,
                   (c < PL_WDT_CP_COUNT) ? PL_WDT_CP_NAMES[c] : "?",
                   (unsigned)(i == 0 ? 0u : (t - prev_us)));
            prev_us = t;
        }
        s_loop_ring.magic = 0;
    }
    // Clear so the NEXT boot cannot re-report this one as fresh -- the
    // "reflashing destroys the evidence" trap in reverse.
    s_loop_trace.magic = 0;
}

void pl_wdt_kick(pl_wdt_subsys_t s) {
#ifdef PL_WATCHDOG
    if ((unsigned)s < PL_WDT_COUNT) {
        s_state[s].counter++;
    }
#else
    (void)s;
#endif
}

void pl_wdt_set_enabled(pl_wdt_subsys_t s, bool enabled) {
#ifdef PL_WATCHDOG
    if ((unsigned)s >= PL_WDT_COUNT) {
        return;
    }
    pl_wdt_state_t *st = &s_state[s];
    if (enabled && !st->enabled) {
        // Re-base -- see this function's doc comment in watchdog_sup.h.
        st->last_seen = st->counter;
        st->last_change_us = time_us_64();
        st->max_staleness_us = 0;
    }
    st->enabled = enabled;
#else
    (void)s;
    (void)enabled;
#endif
}

void pl_wdt_arm(void) {
#ifdef PL_WATCHDOG
    uint64_t now = time_us_64();
    for (int i = 0; i < PL_WDT_COUNT; i++) {
        s_state[i].last_seen = s_state[i].counter;
        s_state[i].last_change_us = now;
        s_state[i].max_staleness_us = 0;
    }
    // USB_TIMER/USB_TASK/BTSTACK are live from boot onward. MEDIA starts
    // disabled -- a2dp.c enables it only in A2DP_SUBEVENT_STREAM_STARTED
    // and disables it again on SUSPENDED/RELEASED/SIGNALING_CONNECTION_RELEASED.
    s_state[PL_WDT_USB_TIMER].enabled = true;
    s_state[PL_WDT_USB_TASK].enabled = true;
    s_state[PL_WDT_BTSTACK].enabled = true;
    s_state[PL_WDT_MEDIA].enabled = false;

    pl_log("wdt: arming hardware watchdog timeout_ms=%u pause_on_debug=1 observe_only=%d\r\n",
           (unsigned)PL_WDT_TIMEOUT_MS,
#ifdef PL_WDT_OBSERVE_ONLY
           1
#else
           0
#endif
    );
    // pause_on_debug = true -- deliberately different from panic_recorder's
    // false: a panic must recover even under a debugger, but an SWD halt
    // should not spuriously trip this watchdog. See the design doc's
    // "Debug vs release" section.
    watchdog_enable(PL_WDT_TIMEOUT_MS, /*pause_on_debug=*/true);
#endif
}

void pl_wdt_service(void) {
#ifdef PL_WATCHDOG
    uint64_t now = time_us_64();
    pl_wdt_subsys_t stale_subsys = PL_WDT_COUNT;
    uint32_t stale_ms = 0;

    for (int i = 0; i < PL_WDT_COUNT; i++) {
        pl_wdt_state_t *st = &s_state[i];
        if (!st->enabled) {
            continue;
        }
        uint32_t cur = st->counter;
        if (cur != st->last_seen) {
            st->last_seen = cur;
            st->last_change_us = now;
            continue;
        }
        uint32_t staleness_us = (uint32_t)(now - st->last_change_us);
        if (staleness_us > st->max_staleness_us) {
            st->max_staleness_us = staleness_us;
        }
        if (stale_subsys == PL_WDT_COUNT && staleness_us > PL_WDT_DEADLINE_US[i]) {
            stale_subsys = (pl_wdt_subsys_t)i;
            stale_ms = staleness_us / 1000u;
        }
    }

    if (stale_subsys != PL_WDT_COUNT) {
#ifdef PL_WDT_OBSERVE_ONLY
        // Rollout step 6 (design doc): report-only. Feed unconditionally --
        // staleness is instrumented but never trips a reset yet, so the
        // deadlines above can be turned from judgement into measurement
        // before step 7 enables tripping.
        pl_log("wdt: OBSERVE-ONLY would-trip subsys=%s stale_ms=%u deadline_ms=%u\r\n",
               pl_wdt_subsys_name(stale_subsys), (unsigned)stale_ms,
               (unsigned)(PL_WDT_DEADLINE_US[stale_subsys] / 1000u));
        watchdog_update();
#else
        // Breadcrumb + watchdog_reboot(0, 0, ...) -- a SEPARATE entry point
        // from the panic path (panic_recorder.c), never touches scratch[3],
        // never calls reset_usb_boot. Does not return.
        pl_panic_record_watchdog_stale((uint32_t)stale_subsys, stale_ms);
#endif
        return;
    }

    watchdog_update();
#endif
}

void pl_wdt_blackout_begin(void) {
    // Intentionally empty today -- named seam for cz0.6.1's flash-write
    // path (design doc step 8, out of this bead's scope). Nothing needs to
    // happen here because the caller is about to disable interrupts itself;
    // the real work is in blackout_end()'s re-base.
}

void pl_wdt_blackout_end(void) {
#ifdef PL_WATCHDOG
    // Re-base every deadline to now: during an IRQs-off erase the IRQ-driven
    // subsystems (USB_TIMER/USB_TASK/BTSTACK) genuinely do not progress, and
    // skipping this re-base would be a guaranteed false trip on the first
    // save. Not called from anywhere in this bead -- cz0.6.1 owns wiring it
    // into the flash-write path.
    uint64_t now = time_us_64();
    for (int i = 0; i < PL_WDT_COUNT; i++) {
        s_state[i].last_seen = s_state[i].counter;
        s_state[i].last_change_us = now;
    }
#endif
}

void pl_wdt_report(void) {
#ifdef PL_WATCHDOG
    uint64_t now = time_us_64();
    if (s_last_report_us != 0 && now - s_last_report_us < 1000000) {
        return;
    }
    s_last_report_us = now;
    for (int i = 0; i < PL_WDT_COUNT; i++) {
        pl_wdt_state_t *st = &s_state[i];
        pl_log("wdt: %s max_stale_ms=%lu deadline_ms=%lu enabled=%d\r\n", pl_wdt_subsys_name((pl_wdt_subsys_t)i),
               (unsigned long)(st->max_staleness_us / 1000u), (unsigned long)(PL_WDT_DEADLINE_US[i] / 1000u),
               st->enabled ? 1 : 0);
        st->max_staleness_us = 0;
    }
#endif
}

// Mirrors hardware_watchdog/watchdog.c's private WATCHDOG_NON_REBOOT_MAGIC
// -- not exposed via any pico-sdk header, so redefined here with a comment
// pointing at the source of truth. watchdog_enable() stamps this into
// scratch[4]; watchdog_reboot() overwrites scratch[4] with 0 (pc == 0,
// "regular flash boot") or 0xb007c0d3 (pc != 0). See
// hardware_watchdog/watchdog.c:77-124.
#define PL_WDT_NON_REBOOT_MAGIC 0x6ab73121u

void pl_wdt_report_boot_reason(void) {
    pl_wdt_report_loop_trace();
#ifdef PL_WATCHDOG
    // MUST run before pl_wdt_arm() -- arming overwrites scratch[4] with
    // PL_WDT_NON_REBOOT_MAGIC via watchdog_enable(), destroying the value
    // this function reads. Also must not run after panic_recorder.c's
    // pl_panic_report_and_clear() has cleared scratch[0..3] for a WDT-magic
    // record, or the check below can't tell a supervised trip apart from
    // "no record at all" -- call this one first (see main.c).
    uint32_t reason = watchdog_hw->reason;
    if (reason == 0) {
        // Scratch is not retained across a true power cycle -- see
        // panic_recorder.c:315-329's banked reasoning, same silicon fact.
        pl_log("wdt: boot reason = power-on / brown-out reset\r\n");
        return;
    }

    uint32_t magic0 = watchdog_hw->scratch[0];
    if (magic0 == PL_PANIC_MAGIC_WDT) {
        // A supervised stale-subsystem trip -- pl_panic_report_and_clear()
        // (called separately, after this function) prints the subsystem
        // name and staleness from scratch[1..2] and clears the record.
        // Nothing to add here.
        return;
    }

    uint32_t scratch4 = watchdog_hw->scratch[4];
    if (scratch4 == PL_WDT_NON_REBOOT_MAGIC) {
        // watchdog_enable() was called (by us, or by panic_recorder.c's
        // arm-first step) but nothing ever reached a deliberate
        // watchdog_reboot() before the timer itself expired -- i.e. the
        // superloop (or whatever armed the dog) died before anything could
        // attribute the failure. A distinct, valuable diagnosis.
        pl_log("wdt: boot reason = UNATTRIBUTED hardware watchdog expiry "
               "(something died before it could be attributed)\r\n");
    } else {
        // scratch4 == 0 (or the reboot-to-address magic 0xb007c0d3, which
        // this firmware never uses) -- a deliberate watchdog_reboot() whose
        // own record, if any, is one of panic_recorder's other magics
        // (PLRP/PLCC/PLHF/PLAS) or usb_reset.c's flash-reset path with no
        // record at all.
        pl_log("wdt: boot reason = deliberate reset (watchdog_reboot -- panic "
               "recorder or a flash/BOOTSEL request)\r\n");
    }
#endif
}
