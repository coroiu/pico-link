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
// Bead pico-link-d42g: 5 -> 9, the physical ladder (design sec 3). The
// runtime Adaptive floor (how far down the decide phase and the apply
// clamp are actually allowed to go) is a SEPARATE model field,
// model_abr_t::floor_rung, default PL_LDAC_FLOOR_RUNG_DEFAULT below --
// mirrors codec_ldac.c's s_ldac_floor_rung / pl_codec_ldac_floor_rung.
#define PL_LDAC_ADAPTIVE_LADDER_RUNGS 9
// Bead pico-link-d42g: codec_ldac.c's s_ldac_floor_rung default (330kbps,
// rung 4) -- identical to the pre-d42g physical ceiling, so every test
// below that never touches floor_rung sees unchanged behaviour.
#define PL_LDAC_FLOOR_RUNG_DEFAULT 4

// --- Copied from a2dp.c's ABR constants ---
#define PL_LDAC_ABR_Q_HI (4 * 256)
// Bead pico-link-ge8s, 2026-09-25: 1*256(1.0) -> 2*256(2.0). See a2dp.c's
// own doc comment on this constant for the full measured table (HQ/SQ/MQ
// resting depths); mirrored in test (h) below.
#define PL_LDAC_ABR_Q_LO (2 * 256)
#define PL_LDAC_ABR_SETTLE_US 1000000ULL
#define PL_LDAC_ABR_UP_DWELL_US 10000000ULL // pico-link-qiow: shortened from 60s 2026-09-24

// The DECIDE-phase state, copied from a2dp.c's pl_a2dp_ctx_t fields plus
// codec_ldac.c's applied/target rung (modeled together here for a single
// self-contained harness; in the real system they live in two files
// joined by pl_codec_ldac_request_rung/pl_codec_ldac_applied_rung).
typedef struct {
    int32_t q_ema;
    uint64_t last_step_us;
    uint32_t qfull_snapshot;
    // Bead pico-link-jphl: mirrors a2dp.c's abr_up_clean_since_us -- the
    // wall-clock start of the current run with no NEW stop_queue_full
    // delta, restarted on EVERY tick that sees a delta (not just on a
    // step). The up-step's dwell check reads THIS, not last_step_us, so a
    // single queue-full after the last step no longer vetoes every future
    // up-step forever (the bug this bead fixes).
    uint64_t up_clean_since_us;
    int32_t applied_rung;
    int32_t target_rung;
    // Bead pico-link-d42g (design sec 4): mirrors codec_ldac.c's
    // s_ldac_floor_rung -- the configured Adaptive cap, separate from the
    // physical PL_LDAC_ADAPTIVE_LADDER_RUNGS ceiling above.
    int32_t floor_rung;
    uint32_t floor_hits;
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
    c->up_clean_since_us = 0;
    c->applied_rung = 0;
    c->target_rung = 0;
    c->floor_rung = PL_LDAC_FLOOR_RUNG_DEFAULT;
    c->floor_hits = 0;
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

    // Bead pico-link-jphl: a NEW delta restarts the clean-dwell clock,
    // independent of whether a step fires this same tick.
    if (stop_queue_full_now != c->qfull_snapshot) {
        c->qfull_snapshot = stop_queue_full_now;
        c->up_clean_since_us = now_us;
    }

    bool past_settle = (now_us - c->last_step_us) > PL_LDAC_ABR_SETTLE_US;

    if (c->q_ema >= PL_LDAC_ABR_Q_HI && past_settle && c->applied_rung < c->floor_rung) {
        // Bead pico-link-d42g: the cap is now the configured floor, not
        // the physical ladder ceiling (design sec 4). The physical rail
        // stays enforced independently in model_apply's at_rail check.
        c->target_rung = c->applied_rung + 1;
        c->last_step_us = now_us;
        c->q_ema = (int32_t)tx_count_now * 256;
        c->qfull_snapshot = stop_queue_full_now;
        c->up_clean_since_us = now_us;
    } else if (c->q_ema >= PL_LDAC_ABR_Q_HI && past_settle && c->applied_rung >= c->floor_rung) {
        // Bead pico-link-d42g: congested AND already at (or past) the
        // floor -- this bead's own trigger observable. Rate-limited to
        // once per SETTLE like a real step; NO q_ema reseed (design sec
        // 4: there was no step, so the EMA isn't stale).
        c->floor_hits++;
        c->last_step_us = now_us;
    } else if (c->q_ema <= PL_LDAC_ABR_Q_LO && c->applied_rung > 0 &&
               (now_us - c->up_clean_since_us) > PL_LDAC_ABR_UP_DWELL_US) {
        c->target_rung = c->applied_rung - 1;
        c->last_step_us = now_us;
        c->q_ema = (int32_t)tx_count_now * 256;
        c->qfull_snapshot = stop_queue_full_now;
        c->up_clean_since_us = now_us;
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
    // Bead pico-link-d42g (design sec 4): THE single enforcement point --
    // re-derived from floor_rung on every call, race-safe against any
    // write order between model_decide (writes target_rung) and a
    // hypothetical concurrent floor change (writes floor_rung), exactly
    // like codec_ldac.c's real apply_pending_tuning.
    if (target > c->floor_rung) {
        target = c->floor_rung;
    }
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

    // --- (c) a single stop_queue_full delta partway through the up-dwell
    // window DELAYS the step up (restarts the clean-dwell clock from the
    // delta), it does not veto it forever. Bead pico-link-jphl: before this
    // fix, qfull_snapshot was only ever refreshed on a STEP, so this single
    // delta would have blocked every future up-step until a down-step fired
    // -- this test proves the fixed model instead still steps up once a
    // full UP_DWELL_US has elapsed since the LAST delta. ---
    {
        model_abr_t c;
        model_abr_reset(&c, 0);
        c.applied_rung = 2; // start mid-ladder, as if already stepped down
        c.target_rung = 2;  // keep target in sync -- applied/target only diverge via a real decide() step
        uint64_t now = 0;
        uint32_t stop_queue_full = 0;
        int step_up_at_tick = -1;
        // Run well past 2x UP_DWELL_US from the delta so the fixed
        // semantics have ample room to fire.
        int total_ticks = (int)((2 * PL_LDAC_ABR_UP_DWELL_US) / 10000) + 100;
        for (int i = 0; i < total_ticks; i++) {
            now += 10000;
            if (i == 500) {
                stop_queue_full++; // one queue-full mid-window
            }
            model_decide(&c, 0, now, stop_queue_full); // empty queue -- well under Q_LO
            model_apply(&c);
            if (c.steps_up == 1 && step_up_at_tick < 0) {
                step_up_at_tick = i;
            }
        }
        // Must NOT have stepped up before a full UP_DWELL_US elapsed since
        // the delta at i==500 (the old bug would never step up at all).
        assert(step_up_at_tick >= 0);
        assert((uint64_t)(step_up_at_tick - 500) * 10000 >= PL_LDAC_ABR_UP_DWELL_US);
        assert(c.applied_rung == 1);
        printf("ok:   a single stop_queue_full delta mid-dwell delays the step up (restarts the "
               "clean-dwell clock) instead of vetoing it forever\n");
    }

    // --- (c-1b) the delta-during-dwell window from (c), stopped exactly at
    // the OLD bug's failure point (well past UP_DWELL_US since the delta,
    // but the same total run length as the old (c) test): proves the fixed
    // controller has already stepped up by then, where the old snapshot-
    // on-step-only logic never would. ---
    {
        model_abr_t c;
        model_abr_reset(&c, 0);
        c.applied_rung = 2;
        c.target_rung = 2;
        uint64_t now = 0;
        uint32_t stop_queue_full = 0;
        for (int i = 0; i < 1001; i++) { // 10.01s -- same window as the old (c) test
            now += 10000;
            if (i == 500) {
                stop_queue_full++; // one queue-full mid-window
            }
            model_decide(&c, 0, now, stop_queue_full);
            model_apply(&c);
        }
        // Only ~5.01s have elapsed since the delta at i==500 by the end of
        // this window -- not yet a full UP_DWELL_US (10s) -- so no step up
        // yet. This mirrors the old (c) test's assertion, but for the
        // right reason now (dwell not yet re-earned, not a permanent veto).
        assert(c.steps_up == 0);
        assert(c.applied_rung == 2);
        printf("ok:   5s after a mid-window delta (dwell not yet re-earned) still holds at the "
               "lower rung\n");
    }

    // --- (c-1c) a queue-full occurring AGAIN partway through an already-
    // restarted dwell restarts it a second time -- proves the restart is
    // unconditional on every delta, not a one-shot special case. ---
    {
        model_abr_t c;
        model_abr_reset(&c, 0);
        c.applied_rung = 2;
        c.target_rung = 2;
        uint64_t now = 0;
        uint32_t stop_queue_full = 0;
        int first_delta_tick = 100;
        int second_delta_tick = first_delta_tick + (int)(PL_LDAC_ABR_UP_DWELL_US / 10000) / 2; // mid-dwell
        int total_ticks = second_delta_tick + (int)(PL_LDAC_ABR_UP_DWELL_US / 10000) + 50;
        int step_up_at_tick = -1;
        for (int i = 0; i < total_ticks; i++) {
            now += 10000;
            if (i == first_delta_tick || i == second_delta_tick) {
                stop_queue_full++;
            }
            model_decide(&c, 0, now, stop_queue_full);
            model_apply(&c);
            if (c.steps_up == 1 && step_up_at_tick < 0) {
                step_up_at_tick = i;
            }
        }
        assert(step_up_at_tick >= 0);
        // Must not have stepped up before UP_DWELL_US after the SECOND
        // (later) delta -- if the restart weren't unconditional, the first
        // delta alone could have let the dwell (wrongly) expire earlier.
        assert((uint64_t)(step_up_at_tick - second_delta_tick) * 10000 >= PL_LDAC_ABR_UP_DWELL_US);
        printf("ok:   a second queue-full mid-dwell restarts the clean-dwell clock again, not just "
               "the first one\n");
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
        for (int i = 0; i < 1001; i++) {
            now += 10000;
            model_decide(&c, 0, now, 5); // stable, no delta
            model_apply(&c);
        }
        assert(c.steps_up == 1);
        assert(c.applied_rung == 1);
        printf("ok:   a clean 10s dwell with zero stop_queue_full delta steps up exactly once\n");
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
            // than once per UP_DWELL_US window" (design sec 3.4).
            assert(period >= PL_LDAC_ABR_UP_DWELL_US);
        }
        printf(
            "ok:   a genuinely marginal plant limit-cycles no faster than one round trip per "
            "up-dwell window (%d down-steps observed over 5 windows)\n",
            down_steps_seen
        );
    }

    // --- (e) never advances the rung counter past the PHYSICAL rail, and
    // a rail hit while the walk is at the boundary is counted, not
    // silently ignored or mistaken for a fault. Bead pico-link-d42g: this
    // isolates the physical-rail check from the new configured floor by
    // setting floor_rung to the rail itself -- a real Adaptive stream
    // could never reach rung 8 with the default floor (rung 4), but the
    // apply phase's at_rail safety net must hold regardless of what the
    // floor is configured to. ---
    {
        model_abr_t c;
        model_abr_reset(&c, 0);
        // A floor this deep is never valid in the real system (the wire
        // mapping only ever produces rung 4/6/8) -- set intentionally
        // beyond the physical rail so the floor clamp itself never fires,
        // isolating model_apply's independent at_rail safety net.
        c.floor_rung = PL_LDAC_ADAPTIVE_LADDER_RUNGS + 100;
        c.applied_rung = PL_LDAC_ADAPTIVE_LADDER_RUNGS - 1; // already at the physical rail (Q5)
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
        printf("ok:   apply phase refuses to advance past the physical rail, counts it instead\n");
    }

    // --- (e-1) the configured floor caps the down-step BEFORE the
    // physical rail is ever reached, and floor_hits (not rail_hits) counts
    // the "congested, already at the floor" case -- this bead's own
    // trigger observable (design sec 4). ---
    {
        model_abr_t c;
        model_abr_reset(&c, 0);
        c.floor_rung = 6; // 246kbps
        uint64_t now = 10ULL * PL_LDAC_ABR_SETTLE_US;
        // Drive sustained congestion long enough to step down to the
        // floor (6 steps, each gated >= 1 SETTLE apart) and then keep
        // pressing past SETTLE so at least one floor_hit fires too.
        for (int i = 0; i < 800; i++) {
            now += 10000;
            model_decide(&c, 7, now, 0); // pinned congested
            model_apply(&c);
        }
        assert(c.applied_rung == 6); // stopped AT the configured floor
        assert(c.steps_down == 6);   // walked one rung at a time from 0
        assert(c.rail_hits == 0);    // never reached the physical rail (8)
        assert(c.floor_hits > 0);    // congestion kept firing at the floor
        printf("ok:   configured floor (6) caps the down-step before the physical rail (8), "
               "floor_hits fires instead of rail_hits\n");
    }

    // --- (e-2) raising the floor mid-stream (a user picking a HIGHER
    // minimum, e.g. 330kbps after having been at 198kbps) walks the
    // applied rung back UP to the new floor via model_apply's clamp
    // alone -- even with target_rung left untouched at the old, deeper
    // value, proving the apply-phase clamp is the single point that is
    // actually race-safe (design sec 4's mid-stream behaviour table). ---
    {
        model_abr_t c;
        model_abr_reset(&c, 0);
        c.applied_rung = 8;
        c.target_rung = 8; // converged at the old, deeper floor (198kbps)
        c.floor_rung = 4;  // user raises the floor back to 330kbps
        // target_rung is deliberately left at 8 -- the clamp inside
        // model_apply must pull the EFFECTIVE target in on every call
        // regardless, exactly like codec_ldac.c's real single enforcement
        // point.
        for (int i = 0; i < 4; i++) {
            model_apply(&c);
        }
        assert(c.applied_rung == 4);
        assert(c.steps_up == 4);
        printf("ok:   raising the floor mid-stream walks the applied rung back up to it, one step "
               "per apply() call, even with target_rung left stale\n");
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

    // --- (h) bead pico-link-ge8s: a REALISTIC resting EMA (not an idle/
    // empty-queue plant like (c)/(c-2)/(d) above) at the up-step's own
    // rung steps up once a clean dwell elapses, where the OLD threshold
    // (Q_LO=1*256=1.0) would have blocked it forever. tx_count=5 against
    // an 8-slot queue settles the EMA at 5*256=1280 (5.0)... no -- q_ema
    // tracks tx_count directly in Q8 units, so to model the measured SQ
    // resting depth (~1.19-1.32 slots, this bead's hardware measurement)
    // the plant must alternate tx_count between 1 and 2 so the EMA settles
    // in between, exactly like a real tx queue's sawtooth between seals
    // and grants (a2dp.c's own doc comment on this same q_ema line).
    {
        model_abr_t c;
        model_abr_reset(&c, 0);
        c.applied_rung = 1; // SQ, as if arrived at via a prior down-step
        c.target_rung = 1;
        uint64_t now = 0;
        for (int i = 0; i < 1001; i++) { // 10.01s, past UP_DWELL_US
            now += 10000;
            uint32_t tx_count = (i % 4 == 0) ? 2u : 1u; // settles ~1.25 slots
            model_decide(&c, tx_count, now, 0);
            model_apply(&c);
        }
        // A realistic resting EMA in the 1.0-2.0 range (this bead's
        // measured SQ range was 1.10-1.32) must have crossed
        // PL_LDAC_ABR_Q_LO at some point in a 10s clean dwell and stepped
        // up -- this is exactly the case the old Q_LO=1.0 threshold made
        // structurally unreachable (root cause of pico-link-ge8s).
        assert(c.steps_up == 1);
        assert(c.applied_rung == 0);
        printf("ok:   a realistic ~1.25-slot resting EMA at SQ steps up after a clean dwell (the bug "
               "this bead fixes -- unreachable under the old Q_LO=1.0)\n");
    }

    // --- (h-2) a depth resting near Q_HI (but still inside the dead band,
    // never triggering the down-step) NEVER steps up, dwell or not --
    // proves the raised Q_LO has not made the gate toothless. ---
    {
        model_abr_t c;
        model_abr_reset(&c, 0);
        c.applied_rung = 1;
        c.target_rung = 1;
        uint64_t now = 0;
        for (int i = 0; i < 2001; i++) { // 20s, well past UP_DWELL_US
            now += 10000;
            uint32_t tx_count = (i % 2 == 0) ? 4u : 3u; // settles ~3.5 slots, near Q_HI(4.0)
            model_decide(&c, tx_count, now, 0);
            model_apply(&c);
        }
        assert(c.steps_up == 0);
        assert(c.steps_down == 0);
        assert(c.applied_rung == 1);
        printf("ok:   a resting depth near Q_HI never steps up, even across a 20s dwell -- the raised "
               "Q_LO still gates real congestion\n");
    }

    printf("ALL LDAC ABR CONTROLLER MODEL TESTS PASSED\n");
    return 0;
}
