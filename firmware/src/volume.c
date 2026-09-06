#include "volume.h"

#include "hardware/sync.h"

#ifdef PL_DEBUG_REMOTE
#include "pl_prio.h"
#endif
#include "usb_pump.h" // pl_log() -- only for the circuit-breaker trip line, thread context

// --- Canonical state (thread context only -- design sec 2/3) ---
static uint8_t s_level;    // 0..127
static bool s_muted;
static uint8_t s_pre_mute; // level to restore on unmute

// --- Inbound latches -- written under save_and_disable_interrupts() from
// whichever IRQ owns that edge, cleared by pl_volume_service() under the
// same protection (design sec 3). Already in the canonical domain: the
// host edge quantises at the IRQ boundary (pl_volume_notify_host_raw), the
// sink edge is the canonical domain natively (AVRCP).
static volatile uint8_t s_host_pending;
static volatile bool s_host_dirty;
static volatile uint8_t s_sink_pending;
static volatile bool s_sink_dirty;

// --- Outbound latches -- written by pl_volume_service()/pl_volume_debug_set(),
// read by their eventual T3/T4 consumers. Nobody reads these yet (design
// sec "SCOPE OF THIS BEAD" in volume.h); writing them is inert today but
// keeps the shape T3/T4 will wire into.
static volatile int16_t s_fu_report;
static volatile uint8_t s_avrcp_desired;
static volatile bool s_avrcp_dirty;

// --- Circuit breaker (design sec 4.3): >8 accepted propagations within a
// rolling 1s window trips a 2s cooldown. ~15 lines, no origin tracking
// (design sec 4c forbids it everywhere in this module).
#define PL_VOLUME_BREAKER_MAX_PER_WINDOW 8u
#define PL_VOLUME_BREAKER_WINDOW_US (1000ull * 1000ull)
#define PL_VOLUME_BREAKER_COOLDOWN_US (2000ull * 1000ull)
static uint64_t s_breaker_window_start_us;
static uint32_t s_breaker_count;
static uint64_t s_breaker_cooldown_until_us;

static bool circuit_breaker_allows(uint64_t now_us) {
    if (now_us < s_breaker_cooldown_until_us) {
        return false; // still cooling down from a prior trip
    }
    if (now_us - s_breaker_window_start_us > PL_VOLUME_BREAKER_WINDOW_US) {
        s_breaker_window_start_us = now_us;
        s_breaker_count = 0;
    }
    s_breaker_count++;
    if (s_breaker_count > PL_VOLUME_BREAKER_MAX_PER_WINDOW) {
        s_breaker_cooldown_until_us = now_us + PL_VOLUME_BREAKER_COOLDOWN_US;
        pl_log(
            "volume: circuit breaker TRIPPED (>%u emissions/1s) -- suppressing propagation for 2s\r\n",
            PL_VOLUME_BREAKER_MAX_PER_WINDOW
        );
        return false;
    }
    return true;
}

// --- Domain mapping (design sec 5, T1-verified numbers) ---
// cur = level * 100 - 12700; level = round((cur + 12700) / 100), clamped.
static int16_t level_to_cur(uint8_t level) {
    return (int16_t)((int32_t)level * 100 - 12700);
}

static uint8_t clamp_level(int32_t level) {
    if (level < 0) {
        return 0;
    }
    if (level > 127) {
        return 127;
    }
    return (uint8_t)level;
}

static uint8_t cur_to_level(int16_t cur) {
    // Round-to-nearest, not truncate -- T1 measured macOS sending exact
    // bRes multiples against our declared RANGE, so this is belt-and-
    // braces for a host that doesn't (design sec 5's fallback).
    int32_t numer = (int32_t)cur + 12700;
    int32_t level = (numer >= 0) ? (numer + 50) / 100 : (numer - 50) / 100;
    return clamp_level(level);
}

void pl_volume_init(void) {
    s_level = 0;
    s_muted = false;
    s_pre_mute = 0;
    s_host_dirty = false;
    s_sink_dirty = false;
    s_avrcp_dirty = false;
    s_fu_report = level_to_cur(s_level);
    s_avrcp_desired = s_level;
    s_breaker_window_start_us = 0;
    s_breaker_count = 0;
    s_breaker_cooldown_until_us = 0;
}

void pl_volume_notify_host_raw(int16_t cur) {
    uint8_t level = cur_to_level(cur);
    uint32_t irq_state = save_and_disable_interrupts();
    s_host_pending = level;
    s_host_dirty = true;
    restore_interrupts(irq_state);
}

void pl_volume_notify_sink(uint8_t level) {
    uint32_t irq_state = save_and_disable_interrupts();
    s_sink_pending = clamp_level(level);
    s_sink_dirty = true;
    restore_interrupts(irq_state);
}

// The loop rule (design sec 4): apply + propagate iff new_level differs
// from canonical; equal is absorbed silently. Never echoes the peer's raw
// value -- the emission is always derived from the new canonical s_level,
// never from `new_level` itself past this point. `emit` selects whether
// this call actually writes the outbound latches (the two real edges,
// once T3/T4 exist) or only logs what it would have written (T2's console
// path) -- see volume.h's "SCOPE OF THIS BEAD" doc comment for why T2
// passes false.
static void apply_and_propagate(uint8_t new_level, PlVolumeSource source, uint64_t now_us, bool emit) {
    if (new_level == s_level) {
        return; // absorbed silently -- design sec 4, rule (a)/(b)
    }
    if (!circuit_breaker_allows(now_us)) {
        pl_log(
            "volume: change from source=%d to level=%u DROPPED (breaker cooling down)\r\n",
            (int)source,
            (unsigned)new_level
        );
        return;
    }

    s_level = new_level;
    int16_t fu_cur = level_to_cur(s_level);
    uint8_t avrcp_level = s_level; // identity -- canonical domain IS the AVRCP domain

    if (emit) {
        s_fu_report = fu_cur;
        s_avrcp_desired = avrcp_level;
        s_avrcp_dirty = true;
    }

    // pico-link-4v2.3 (VT3) code-review fix, inherited from VT2's review:
    // the format string used to hardcode the literal "WOULD-EMIT"
    // regardless of `emit`, while ALSO printing "emit"/"wouldemit" as a
    // separate field -- self-contradictory the moment a real edge sets
    // emit=true (this bead's own T3 wiring does exactly that). The %s
    // already carries the emit/wouldemit distinction; just log it once.
    //
    // Also gated under PL_DEBUG_REMOTE (matches pl_prio.h's own slot-4
    // gating, PL_PRIO_SLOT_COUNT is 4 in release / 5 with debug) -- this
    // call used to run unconditionally even though slot 4 only exists
    // under PL_DEBUG_REMOTE, so a release build silently dropped every
    // real-edge propagation log via pl_prio_publish's bounds check
    // (pl_prio.c) rather than never calling it at all.
#ifdef PL_DEBUG_REMOTE
    pl_prio_publish(
        4,
        "vol %s src=%d lvl=%u host_cur=%d avrcp=%u",
        emit ? "emit" : "wouldemit",
        (int)source,
        (unsigned)s_level,
        (int)fu_cur,
        (unsigned)avrcp_level
    );
#endif
}

void pl_volume_service(uint64_t now_us) {
    // Fixed order -- host then sink (design sec 4.2): simultaneous
    // changes are totally ordered by this drain, last one wins, no
    // arbitration built.
    bool host_dirty;
    uint8_t host_level;
    bool sink_dirty;
    uint8_t sink_level;

    uint32_t irq_state = save_and_disable_interrupts();
    host_dirty = s_host_dirty;
    host_level = s_host_pending;
    s_host_dirty = false;
    sink_dirty = s_sink_dirty;
    sink_level = s_sink_pending;
    s_sink_dirty = false;
    restore_interrupts(irq_state);

    if (host_dirty) {
        apply_and_propagate(host_level, PL_VOLUME_SOURCE_HOST, now_us, true);
    }
    if (sink_dirty) {
        apply_and_propagate(sink_level, PL_VOLUME_SOURCE_SINK, now_us, true);
    }
}

void pl_volume_debug_set(uint8_t n, uint64_t now_us) {
    uint8_t level = clamp_level(n);
    apply_and_propagate(level, PL_VOLUME_SOURCE_CONSOLE, now_us, false);
}

// T3 (pico-link-4v2.3): bt.c's heartbeat handler (cyw43 background IRQ
// 0xFF) reads this once per 100ms tick to decide whether to send a fresh
// AVRCP SET_ABSOLUTE_VOLUME. IRQ-safe both ways -- written by
// apply_and_propagate (thread context, superloop) under
// save_and_disable_interrupts, read-and-cleared here under the same
// protection so a write from the superloop can never interleave with a
// read from 0xFF and hand out a torn value. Coalescing, like every other
// latch in this module: if two propagations land before the heartbeat
// next runs, only the newest survives -- correct for a level.
bool pl_volume_take_avrcp_desired(uint8_t *out_level) {
    uint32_t irq_state = save_and_disable_interrupts();
    bool dirty = s_avrcp_dirty;
    uint8_t level = s_avrcp_desired;
    s_avrcp_dirty = false;
    restore_interrupts(irq_state);
    if (dirty && out_level != NULL) {
        *out_level = level;
    }
    return dirty;
}

uint8_t pl_volume_level(void) {
    return s_level;
}

bool pl_volume_muted(void) {
    return s_muted;
}
