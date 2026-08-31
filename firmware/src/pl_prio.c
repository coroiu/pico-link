// Pico Link firmware -- see pl_prio.h's module doc for the why.
#include "pl_prio.h"

#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>

static char s_slot[PL_PRIO_SLOT_COUNT][PL_PRIO_SLOT_LEN];
static bool s_slot_fresh[PL_PRIO_SLOT_COUNT];
static uint8_t s_slot_rr;

void pl_prio_publish(uint8_t slot_id, const char *fmt, ...) {
    if (slot_id >= PL_PRIO_SLOT_COUNT) {
        return;
    }

    char *slot = s_slot[slot_id];

    va_list args;
    va_start(args, fmt);
    int n = vsnprintf(slot, PL_PRIO_SLOT_LEN - 2, fmt, args);
    va_end(args);

    uint32_t len = (n > 0) ? (uint32_t)n : 0;
    if (len > PL_PRIO_SLOT_LEN - 2) {
        len = PL_PRIO_SLOT_LEN - 2; // vsnprintf's return value can exceed what it actually wrote
    }

    // Right-pad with spaces to PL_PRIO_SLOT_LEN - 2, then terminate CRLF --
    // fixed width end to end, exactly PL_PRIO_SLOT_LEN bytes, so the
    // reservation math in pl_log_ring.c is exact and the host-side parse
    // is trivial.
    for (uint32_t i = len; i < PL_PRIO_SLOT_LEN - 2; i++) {
        slot[i] = ' ';
    }
    slot[PL_PRIO_SLOT_LEN - 2] = '\r';
    slot[PL_PRIO_SLOT_LEN - 1] = '\n';

    // Overwrite unconditionally -- a not-yet-emitted slot is silently
    // superseded, which is correct: see this module's header doc.
    s_slot_fresh[slot_id] = true;
}

int pl_prio_any_fresh(void) {
    for (uint32_t i = 0; i < PL_PRIO_SLOT_COUNT; i++) {
        if (s_slot_fresh[i]) {
            return 1;
        }
    }
    return 0;
}

uint32_t pl_prio_emit_one(const char **out_data) {
    // Round-robin the starting point so no single slot can be starved by
    // another slot being published every cycle -- with only
    // PL_PRIO_SLOT_COUNT=4 slots and pl_log_ring_drain() called every
    // superloop iteration (~25-30Hz), any fresh slot is picked up within a
    // handful of calls regardless.
    for (uint32_t i = 0; i < PL_PRIO_SLOT_COUNT; i++) {
        uint8_t idx = (uint8_t)((s_slot_rr + i) % PL_PRIO_SLOT_COUNT);
        if (s_slot_fresh[idx]) {
            s_slot_fresh[idx] = false;
            s_slot_rr = (uint8_t)((idx + 1) % PL_PRIO_SLOT_COUNT);
            *out_data = s_slot[idx];
            return PL_PRIO_SLOT_LEN;
        }
    }
    return 0;
}
