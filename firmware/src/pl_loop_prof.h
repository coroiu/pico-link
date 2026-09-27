// Pico Link firmware -- superloop per-phase timing histograms (bead
// pico-link-p1r).
//
// WHY THIS EXISTS: pico-link-p1r measured drain_calls/s = 6.14 on hardware
// during LDAC streaming (673s capture, drn priority slot), against an idle
// figure of ~28fps (pico-link-br2). That is ~164ms/iteration under load vs
// ~35ms idle -- roughly 130ms/iteration is unaccounted for. The leading
// hypothesis is st7789_blit_framebuffer (12-38ms blocking, pico-link-3uq),
// but 38ms does not explain 164ms, so the gap must be ATTRIBUTED BY
// MEASUREMENT before anything is changed -- see this bead's description.
//
// This module instruments every phase of the main.c superloop body with a
// small fixed-bucket histogram (cumulative since boot, same "newest
// snapshot" philosophy as pl_prio.h) plus an exact running max, then
// reports p50/p95/p99/max/count for one phase per second (round-robin) via
// the pl_prio.h non-starvable priority counter channel, slot 3. A mean
// would hide the tail that actually threatens a dwell budget -- this
// project already learned that lesson the hard way (pico-link-cz0.5.8) --
// so this module never reports a mean, only percentiles and an exact max.
//
// INSTRUMENTATION ONLY. This module records and reports; it changes no
// firmware behaviour. No allocation, no floating point in the hot path:
// pl_loop_prof_record() increments one bucket counter and compares a
// uint64_t max. Percentile computation (integer-only) happens only in
// pl_loop_prof_publish_next(), which runs at most once a second.
//
// THREAD CONTEXT ONLY for every function here, same contract as pl_prio.h:
// pl_loop_prof_record() is called only from the main.c superloop body, and
// pl_loop_prof_publish_next() only from that same loop's existing 1Hz
// shared-report block (see main.c, pico-link-okx D11). No lock is taken --
// single-threaded caller on core 0, same reasoning as pl_prio.h's module
// doc; if a future caller ever needs to record from an IRQ, that is the
// moment to add one, not before.
#ifndef PICO_LINK_PL_LOOP_PROF_H
#define PICO_LINK_PL_LOOP_PROF_H

#include <stdint.h>

// One entry per phase of the main.c superloop body. Order matches the
// order phases actually run in the loop, purely for readability -- the
// round-robin publish cursor in pl_loop_prof.c walks this enum in order
// but that is an implementation detail, not a contract.
typedef enum {
    // GPIO X+Y BOOTSEL-hold check + pl_link_input_poll + pl_ui_input.
    PL_LOOP_PHASE_INPUT = 0,
    // pl_debug_remote_poll + pl_ui_input (PL_DEBUG_REMOTE builds only; the
    // histogram simply never gets samples in a release build).
    PL_LOOP_PHASE_DEBUG_REMOTE,
    // pl_bt_drain_events (PL_DIAG_SKIP_BT off only).
    PL_LOOP_PHASE_BT_DRAIN,
    // pl_ui_tick.
    PL_LOOP_PHASE_UI_TICK,
    // pl_ui_render.
    PL_LOOP_PHASE_UI_RENDER,
    // st7789_blit_framebuffer -- the pico-link-3uq blit-split candidate.
    PL_LOOP_PHASE_BLIT,
    // pl_bt_poll_commands (PL_DIAG_SKIP_BT off only).
    PL_LOOP_PHASE_BT_POLL_CMDS,
    // The ~1Hz usb-audio pl_log() report block.
    PL_LOOP_PHASE_AUDIO_REPORT,
    // The every-60-frames per-frame render/blit pl_log() report block.
    PL_LOOP_PHASE_FRAME_REPORT,
    // The 1Hz shared-report block: pl_usb_pump_report + pl_a2dp_report +
    // pl_a2dp_publish_counters (this module's own publish call is
    // deliberately excluded from its own measurement -- see main.c).
    PL_LOOP_PHASE_SHARED_REPORT,
    // pl_log_ring_drain (every iteration, unconditional).
    PL_LOOP_PHASE_LOG_DRAIN,
    // pl_wdt_service + pl_wdt_report.
    PL_LOOP_PHASE_WDT_SERVICE,
    // pl_config_itf_poll_telemetry (bead pico-link-jyhk.4). Expected ~0
    // whenever no web page is attached (the whole call is one poll-
    // recency check), and one pl_ui_telemetry encode's cost at most once
    // every 20ms while a page is polling.
    PL_LOOP_PHASE_TELEMETRY,
    // pl_config_itf_poll_library + pl_config_itf_poll_host_op +
    // pl_config_itf_poll_preview_lease (bead pico-link-jyhk.21, GET_LIBRARY/
    // HOST_OP/GET_OP_STATUS). Same "~0 when unattended" shape as
    // PL_LOOP_PHASE_TELEMETRY: library generation is poll-recency gated,
    // HOST_OP only costs anything the iteration a request actually landed,
    // and the preview lease check is a timestamp compare.
    PL_LOOP_PHASE_EQ_MGMT,
    // The pacing sleep_us() at the bottom of the loop (expected to be ~0
    // whenever the body already exceeds frame_budget_us).
    PL_LOOP_PHASE_SLEEP,
    // The whole iteration, frame_start_us to the next iteration's
    // frame_start_us. Should be >= the sum of every other phase; the
    // residual (total minus the sum of the rest) is whatever this
    // instrumentation itself still doesn't cover.
    PL_LOOP_PHASE_TOTAL,
    PL_LOOP_PHASE_COUNT
} pl_loop_phase_t;

// Records one observed duration (microseconds) for `phase`. O(1): one
// bucket increment, one max comparison. THREAD CONTEXT ONLY.
void pl_loop_prof_record(pl_loop_phase_t phase, uint64_t duration_us);

// Formats ONE phase's p50/p95/p99/max/count into pl_prio slot 3 (see
// pl_prio.h) and advances the round-robin cursor to the next phase. Call
// at most once a second -- main.c calls this from the existing 1Hz
// shared-report block, so a full cycle over all PL_LOOP_PHASE_COUNT phases
// takes PL_LOOP_PHASE_COUNT seconds. THREAD CONTEXT ONLY, same contract as
// pl_prio_publish().
void pl_loop_prof_publish_next(void);

#endif // PICO_LINK_PL_LOOP_PROF_H
