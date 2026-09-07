// Pico Link firmware -- host-buildable test for bead pico-link-7jol.3: the
// LDAC ABR controller's decide/apply logic
// (.planning/design/2026-09-07-ldac-abr-control-loop.md sec 2-3 and 6.2).
//
// MODEL test, not a link test -- same convention as
// test_a2dp_tx_ring_count.c and test_a2dp_priming_cushion_frames_per_packet.c.
// firmware/src/a2dp.c and codec_ldac.c cannot be linked on host (BTstack,
// pico-sdk, ldacBT.h). The two functions below are copied verbatim from:
//   - a2dp.c's pl_a2dp_media_timer_handler DECIDE-phase block (the q_ema
//     update and the step-down/step-up trigger logic, design sec 2-3)
//   - codec_ldac.c's pl_codec_ldac_apply_pending_tuning APPLY-phase
//     function (the one-rung-per-call walk and rail handling, design sec
//     6.2), with ldacBT_alter_eqmid_priority() replaced by a test-
//     injectable stub (model_ldac_alter) so this file needs no libldac.
// If either real function changes, update both here and there.
//
// What this proves: the pure step/hysteresis/dwell state machine (a) never
// steps inside the dead band, (b) steps down fast under sustained
// congestion, (c) refuses to step up until a full, PROVABLY CLEAN dwell
// window has elapsed, (d) cannot oscillate faster than the asymmetric
// dwell bounds (design sec 3.4's central claim), and (e) never advances
// the rung counter past a rail.
//
// Build + run (no CMake target -- same convention as the other firmware/
// tests/test_*.c files):
//   cc -std=c11 -Wall -Wextra firmware/tests/test_ldac_abr_controller.c \
//      -o /tmp/test_ldac_abr_controller && /tmp/test_ldac_abr_controller
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>

// --- Copied from codec_ldac.h / codec_ldac.c ---
#define PL_LDAC_ADAPTIVE_LADDER_RUNGS 5

// --- Copied from a2dp.c's ABR constants ---
#define PL_LDAC_ABR_Q_HI (4 * 256)
#define PL_LDAC_ABR_Q_LO (1 * 256)
#define PL_LDAC_ABR_SETTLE_US 1000000ULL
#define PL_LDAC_ABR_UP_DWELL_US 60000000ULL

// The DECIDE-phase state, copied from a2dp.c's pl_a2dp_ctx_t fields plus
// codec_ldac.c's applied/target rung (modeled together here for a single
// self-contained harness; in the real system they live in two files
// joined by pl_codec_ldac_request_rung/pl_codec_ldac_applied_rung).
typedef struct {
    int32_t q_ema;
    uint64_t last_step_us;
    uint32_t qfull_snapshot;
    int32_t applied_rung;
    int32_t target_rung;
    // Test-only failure injection for the APPLY phase. 0 == success,
    // nonzero == ldacBT_alter_eqmid_priority failed. Code review,
    // 2026-09-07: the real function's every reachable failure path
    // returns LDACBT_ERR_ALTER_EQMID_LIMITED (ldacBT_api.c:321-341,
    // including the pkt_type != _2_DH5 case design sec 0.2 names as the
    // one persistent-failure mode) -- so this model does not re-classify
    // by value at all, matching the fixed real function: our own rung
    // counter already ruled the rail case out before this is even
    // called, so ANY nonzero return here is a fault, unconditionally.
    // Defaults to 0 (always succeeds).
    int inject_apply_status;
    uint32_t steps_down;
    uint32_t steps_up;
    uint32_t rail_hits;
    uint32_t apply_fail;
} model_abr_t;

static void model_abr_reset(model_abr_t *c, uint32_t stop_queue_full_now) {
    c->q_ema = 0;
    c->last_step_us = 0;
    c->qfull_snapshot = stop_queue_full_now;
    c->applied_rung = 0;
    c->target_rung = 0;
    c->inject_apply_status = 0;
    c->steps_down = 0;
    c->steps_up = 0;
    c->rail_hits = 0;
    c->apply_fail = 0;
}

// Copied verbatim (in spirit) from a2dp.c's pl_a2dp_media_timer_handler
// DECIDE-phase block. `stop_queue_full_now` stands in for
// s_ctx.stop_queue_full; `request_rung` stands in for
// pl_codec_ldac_request_rung (here, a direct field write, since this model
// combines both files' state into one struct).
static void model_decide(model_abr_t *c, uint32_t tx_count_now, uint64_t now_us, uint32_t stop_queue_full_now) {
    c->q_ema += (((int32_t)tx_count_now * 256) - c->q_ema) >> 4;

    bool past_settle = (now_us - c->last_step_us) > PL_LDAC_ABR_SETTLE_US;

    if (c->q_ema >= PL_LDAC_ABR_Q_HI && past_settle && c->applied_rung < PL_LDAC_ADAPTIVE_LADDER_RUNGS - 1) {
        c->target_rung = c->applied_rung + 1;
        c->last_step_us = now_us;
        c->q_ema = (int32_t)tx_count_now * 256;
        c->qfull_snapshot = stop_queue_full_now;
    } else if (c->q_ema <= PL_LDAC_ABR_Q_LO && c->applied_rung > 0 &&
               (now_us - c->last_step_us) > PL_LDAC_ABR_UP_DWELL_US && stop_queue_full_now == c->qfull_snapshot) {
        c->target_rung = c->applied_rung - 1;
        c->last_step_us = now_us;
        c->q_ema = (int32_t)tx_count_now * 256;
        c->qfull_snapshot = stop_queue_full_now;
    }
}

// Copied verbatim (in spirit) from codec_ldac.c's
// pl_codec_ldac_apply_pending_tuning, POST code-review-fix (2026-09-07):
// our own rung counter's at_rail check runs FIRST and returns before the
// (stubbed) library is ever consulted, so by construction any nonzero
// return from it here is a genuine fault -- there is no second
// classification via an error code (deleted: every reachable real
// failure path returns the same LIMITED code, so re-checking it could
// only ever re-derive "rail", silently burying a real fault). c-
// >inject_apply_status stands in for ldacBT_alter_eqmid_priority's
// return (0 == success).
static void model_apply(model_abr_t *c) {
    int32_t target = c->target_rung;
    if (target == c->applied_rung) {
        return;
    }
    bool stepping_down = target > c->applied_rung;
    bool at_rail = stepping_down ? (c->applied_rung >= PL_LDAC_ADAPTIVE_LADDER_RUNGS - 1) : (c->applied_rung <= 0);
    if (at_rail) {
        c->rail_hits++;
        return;
    }
    int status = c->inject_apply_status;
    if (status != 0) {
        c->apply_fail++;
        return;
    }
    if (stepping_down) {
        c->applied_rung++;
        c->steps_down++;
    } else {
        c->applied_rung--;
        c->steps_up++;
    }
}

int main(void) {
    // --- (a) dead band: a steady, moderate queue never steps. ---
    {
        model_abr_t c;
        model_abr_reset(&c, 0);
        uint64_t now = 0;
        for (int i = 0; i < 2000; i++) {
            now += 10000; // 10ms ticks
            model_decide(&c, 2, now, 0); // 2 slots, inside the 1.0..4.0 dead band
            model_apply(&c);
        }
        assert(c.applied_rung == 0);
        assert(c.steps_down == 0 && c.steps_up == 0);
        printf("ok:   dead band -- 20s of steady moderate occupancy triggers no step\n");
    }

    // --- (b) congestion at the current rung steps down within ~1s; once
    // the plant model relieves at the new rung (as a real link would once
    // fewer/larger packets ease the tx queue), the settle lockout holds
    // and no second step fires even though the queue briefly stayed above
    // the dead band during the settle window itself. `now` starts well
    // past PL_LDAC_ABR_SETTLE_US -- like the real system's time_us_64()
    // (an absolute uptime clock, never 0 at STREAM_STARTED), so the very
    // first evaluation's `past_settle` gate (checked against
    // last_step_us == 0, the fresh-controller sentinel) is already true
    // and does not itself delay the first step. Starting `now` at 0 would
    // conflate that one-time sentinel gate with the settle lockout this
    // test means to exercise. ---
    {
        model_abr_t c;
        model_abr_reset(&c, 0);
        uint64_t now = 10ULL * PL_LDAC_ABR_SETTLE_US;
        int step_at_tick = -1;
        for (int i = 0; i < 300; i++) { // 3s of ticks
            now += 10000;
            // Plant: congested (7 slots, pinned at the rail) only while
            // still at rung 0; relieved (2 slots, inside the dead band)
            // once the controller has stepped down even once.
            uint32_t tx_count = (c.applied_rung == 0) ? 7u : 2u;
            model_decide(&c, tx_count, now, 0);
            model_apply(&c);
            if (c.steps_down == 1 && step_at_tick < 0) {
                step_at_tick = i;
            }
        }
        assert(step_at_tick >= 0);
        // EMA tau ~160ms (shift 4); q_ema crosses 4.0 slots well inside 1s.
        assert(step_at_tick < 100); // < 1s
        assert(c.applied_rung == 1);
        assert(c.steps_down == 1); // exactly one -- the settle lockout held
        printf("ok:   congestion steps down exactly once inside 1s once relieved, settle lockout holds\n");
    }

    // --- (c) a single stop_queue_full delta during the up-dwell window
    // vetoes the step up, even after 60s+ has elapsed and q_ema is low. ---
    {
        model_abr_t c;
        model_abr_reset(&c, 0);
        c.applied_rung = 2; // start mid-ladder, as if already stepped down
        c.target_rung = 2;  // keep target in sync -- applied/target only diverge via a real decide() step
        uint64_t now = 0;
        uint32_t stop_queue_full = 0;
        for (int i = 0; i < 6001; i++) { // 60.01s
            now += 10000;
            if (i == 3000) {
                stop_queue_full++; // one rail hit mid-window
            }
            model_decide(&c, 0, now, stop_queue_full); // empty queue -- well under Q_LO
            model_apply(&c);
        }
        assert(c.steps_up == 0);
        assert(c.applied_rung == 2);
        printf("ok:   a single stop_queue_full delta during the up-dwell vetoes the step up\n");
    }

    // --- (c-2) the same window with NO stop_queue_full delta steps up
    // exactly once after UP_DWELL_US. ---
    {
        model_abr_t c;
        model_abr_reset(&c, 5); // nonzero baseline -- delta, not absolute value, is what matters
        c.applied_rung = 2;
        c.target_rung = 2; // keep target in sync -- see (c)'s comment
        c.qfull_snapshot = 5;
        uint64_t now = 0;
        for (int i = 0; i < 6001; i++) {
            now += 10000;
            model_decide(&c, 0, now, 5); // stable, no delta
            model_apply(&c);
        }
        assert(c.steps_up == 1);
        assert(c.applied_rung == 1);
        printf("ok:   a clean 60s dwell with zero stop_queue_full delta steps up exactly once\n");
    }

    // --- (d) cannot oscillate faster than the asymmetric dwell bounds.
    // Model a plant that is genuinely marginal: at rung 0 it is
    // over capacity (q_ema pinned high); the moment it steps to rung 1,
    // model the queue as draining (q_ema pinned low). Design sec 3.4
    // predicts a limit cycle whose PERIOD is bounded below by
    // UP_DWELL_US -- i.e. at most one down+up round trip per up-dwell
    // window, never faster. ---
    {
        model_abr_t c;
        model_abr_reset(&c, 0);
        uint64_t now = 0;
        uint64_t last_down_step_us[8];
        int down_steps_seen = 0;
        uint32_t stop_queue_full = 0;
        // Run for 5 up-dwell windows' worth of time.
        uint64_t total_us = 5ULL * PL_LDAC_ABR_UP_DWELL_US;
        uint32_t prev_applied = c.applied_rung;
        for (uint64_t t = 0; t < total_us; t += 10000) {
            now = t;
            // Plant model: over capacity at rung 0, drains at rung >= 1.
            uint32_t tx_count = (c.applied_rung == 0) ? 7 : 0;
            model_decide(&c, tx_count, now, stop_queue_full);
            model_apply(&c);
            if (c.applied_rung > (int32_t)prev_applied) {
                if (down_steps_seen < 8) {
                    last_down_step_us[down_steps_seen] = now;
                }
                down_steps_seen++;
            }
            prev_applied = c.applied_rung;
        }
        assert(down_steps_seen >= 2); // the cycle must actually repeat in this window
        for (int i = 1; i < down_steps_seen && i < 8; i++) {
            uint64_t period = last_down_step_us[i] - last_down_step_us[i - 1];
            // The period between consecutive DOWN steps must be at least
            // the up-dwell (the time spent waiting to earn a step back up)
            // -- this is the falsifiable form of "cannot oscillate faster
            // than once per minute" (design sec 3.4).
            assert(period >= PL_LDAC_ABR_UP_DWELL_US);
        }
        printf(
            "ok:   a genuinely marginal plant limit-cycles no faster than one round trip per "
            "up-dwell window (%d down-steps observed over 5 windows)\n",
            down_steps_seen
        );
    }

    // --- (e) never advances the rung counter past a rail, and a rail hit
    // while the walk is at the boundary is counted, not silently ignored
    // or mistaken for a fault. ---
    {
        model_abr_t c;
        model_abr_reset(&c, 0);
        c.applied_rung = PL_LDAC_ADAPTIVE_LADDER_RUNGS - 1; // already at the floor (MQ)
        c.target_rung = PL_LDAC_ADAPTIVE_LADDER_RUNGS;      // would-be request past the ceiling
        // model_apply's own at_rail check catches this before ever calling
        // the (stubbed) library, regardless of target -- request_rung's
        // real-world clamp (codec_ldac.c) would never produce a target
        // this far out of range, but the apply phase must be safe even if
        // it somehow did.
        model_apply(&c);
        assert(c.applied_rung == PL_LDAC_ADAPTIVE_LADDER_RUNGS - 1);
        assert(c.rail_hits == 1);
        assert(c.steps_down == 0);
        printf("ok:   apply phase refuses to advance past the floor rail, counts it instead\n");
    }

    // --- (f) requested vs applied stay distinct until the walk catches
    // up, and repeated apply() calls converge one rung at a time (bead
    // trap #4: a stuck walk must be observable via this divergence). ---
    {
        model_abr_t c;
        model_abr_reset(&c, 0);
        c.target_rung = 4; // a 4-rung jump, as if a pinned->Adaptive switch requested MQ
        for (int i = 0; i < 4; i++) {
            assert(c.target_rung != c.applied_rung); // still diverged
            model_apply(&c);
        }
        assert(c.applied_rung == 4);
        assert(c.target_rung == c.applied_rung); // converged
        assert(c.steps_down == 4);
        printf("ok:   a multi-rung request converges one rung per apply() call, requested/applied "
               "distinct until it does\n");
    }

    // --- (g) a genuine apply fault (not a rail) is counted separately and
    // does not silently advance the rung -- the walk retries every call,
    // matching design sec 11.1(c)'s "cannot wedge" claim. Injects
    // LDACBT_ERR_ALTER_EQMID_LIMITED itself -- the ONLY failure code the
    // real vendored library ever actually returns from this function
    // (ldacBT_api.c:321-341) -- to prove the model classifies it as a
    // fault here, not a rail: our own rung counter already ruled the
    // rail case out (applied_rung is 0, strictly inside the ladder, and
    // we are stepping DOWN from it) before this call is even reached,
    // so per design sec 11.1(b) that is what makes it a fault regardless
    // of which value the library hands back. ---
    {
        model_abr_t c;
        model_abr_reset(&c, 0);
        c.target_rung = 1;
        c.inject_apply_status = 21; // LDACBT_ERR_ALTER_EQMID_LIMITED
        model_apply(&c);
        assert(c.applied_rung == 0);
        assert(c.apply_fail == 1);
        assert(c.rail_hits == 0);
        // Now the fault clears (as libldac's own retry-every-call behavior
        // would eventually see, e.g. after whatever caused it resolves) --
        // the very next call succeeds with no special handling needed.
        c.inject_apply_status = 0;
        model_apply(&c);
        assert(c.applied_rung == 1);
        assert(c.steps_down == 1);
        printf("ok:   a genuine apply fault is counted distinctly from a rail hit and retries cleanly, "
               "even when it carries libldac's own LIMITED error code\n");
    }

    printf("ALL LDAC ABR CONTROLLER MODEL TESTS PASSED\n");
    return 0;
}
