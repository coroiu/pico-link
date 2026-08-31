// Pico Link firmware -- see pl_log_ring.h's module doc for the why.
#include "pl_log_ring.h"

#include <stdbool.h>
#include <string.h>

#include "hardware/sync.h"
#include "pico/time.h"
#include "tusb.h"

#include "pl_prio.h"
#include "usb_pump.h"

// Bead pico-link-auh: 4096 -> 8192. This is a BAND-AID, recorded as one --
// see Ada's design comment (2026-08-31), section 3. The measured boot
// burst is ~10.4KB (4073 held + 6309 dropped at the old 4096 size), so
// 8192 still drops part of it; the actual fix for steady-state saturation
// is the priority channel below (pl_prio.h) plus, out of scope here,
// splitting st7789_blit_framebuffer so the drain ceiling isn't coupled to
// frame rate (filed as a follow-up to pico-link-3uq). Do NOT read "grew
// the ring" as "fixed auh" -- it isn't. Do NOT go to 16KB.
#define PL_LOG_RING_SIZE 8192u // power of two -- see the mask use below

// Bead pico-link-auh: a priority slot must always fit an otherwise-empty
// CDC TX FIFO in one shot -- see pl_prio.h's module doc and the
// reservation rule in pl_log_ring_drain() below.
_Static_assert(PL_PRIO_SLOT_LEN < CFG_TUD_CDC_TX_BUFSIZE, "priority slot must fit an empty CDC TX FIFO");

// Bead pico-link-okx (F3b): s_buf/s_write/s_read live in .uninitialized_data
// (NOLOAD -- a watchdog reset does not clear SRAM, same mechanism
// watchdog_sup.c's s_loop_trace/s_loop_ring/s_boot_seq already use and that
// is proven to survive by that file's own reset path) so a reset that cut a
// drain off mid-backlog does not destroy the very evidence of what the
// board was saying as it died -- "the debug instrument is implicated in the
// bug it was installed to find" (this bead's design comment, Ada,
// 2026-08-30) no longer means the evidence is lost too. s_ring_magic
// distinguishes a real prior session (keep s_write/s_read, let the next
// drain emit the old backlog followed by this boot's own logging, in
// strict FIFO order -- no separate "read the old tail" path is needed) from
// a cold boot / a power cycle that did not preserve SRAM (start both at 0;
// s_buf's actual garbage content is never read in that case, since
// available = write - read = 0 until this session's own pushes advance
// write past it).
#define PL_LOG_RING_MAGIC 0x504c4c52u // "PLLR"
static volatile uint32_t s_ring_magic __attribute__((section(".uninitialized_data.pl_log_ring_magic")));
static char s_buf[PL_LOG_RING_SIZE] __attribute__((section(".uninitialized_data.pl_log_ring_buf")));
// Byte offsets, monotonically increasing (never wrapped themselves -- only
// the index into s_buf, via the mask, wraps). write is touched only inside
// the push critical section; read is touched only by the single drainer.
// Both are plain volatile, same "benign race, single writer per field"
// convention as every other counter in this firmware (see e.g.
// usb_audio.c's module doc).
static volatile uint32_t s_write __attribute__((section(".uninitialized_data.pl_log_ring_write")));
static volatile uint32_t s_read __attribute__((section(".uninitialized_data.pl_log_ring_read")));

static volatile uint32_t s_bytes_dropped;
static volatile uint32_t s_push_hold_us_total;
static volatile uint32_t s_push_hold_us_max;

// Bead pico-link-okx (F2). s_drain_skips: pl_log_ring_drain() found queued
// bytes but pl_usb_lock_try() failed (the 0xC0 worker held pl_usb_mutex) --
// the drain skipped this tick entirely rather than blocking. s_backlog_hwm:
// lifetime high-water mark of (s_write - s_read), i.e. how large the
// backlog ever got -- never reset, see this bead's design comment for why
// a lifetime max is more useful here than a windowed one. Both surfaced in
// usb_pump.c's "usb-pump-logring" report line.
static volatile uint32_t s_drain_skips;
static volatile uint32_t s_backlog_hwm;

// Set by pl_log_ring_init() -- true iff a valid previous session's
// backlog was found and preserved (not persistence-defeating-reset).
// pl_log_ring_recovered_backlog_bytes() lets main.c log that fact once,
// before pushing its own boot banner into the same ring.
static bool s_recovered;
static uint32_t s_recovered_bytes;

// Bead pico-link-auh, section 2a: producer attribution. Open-addressed,
// 32 entries, updated ONLY inside pl_log_ring_push_attr()'s existing
// save_and_disable_interrupts() critical section below -- no new critical
// section, no lock elsewhere. `used` distinguishes an empty slot from a
// genuine pc==0 attribution (pl_log_ring_push()'s "no attribution"
// wrapper passes pc=0, which is itself a valid, trackable bucket).
#define PL_LOG_ATTR_TABLE_SIZE 32u
typedef struct {
    bool used;
    uint32_t pc;
    uint32_t bytes;
    uint32_t drops;
} pl_log_attr_entry_t;
static pl_log_attr_entry_t s_attr_table[PL_LOG_ATTR_TABLE_SIZE];

// Bead pico-link-auh, section 2b: the drain-side saturation ledger --
// deliberately separate from s_bytes_dropped/s_drain_skips/s_backlog_hwm
// above (those predate this bead and are surfaced elsewhere); these three
// are new and exist solely to answer "is the bottleneck production or
// drain": room_zero counts pl_log_ring_drain() calls that got zero room to
// give the log ring (either tud_cdc_write_available() returned 0, or the
// priority reservation rule above consumed all of it -- both are "the log
// ring got nothing this call"); bytes_drained_total is the cumulative sum
// of w (bytes tud_cdc_write() actually accepted for the LOG ring, not
// counting priority-slot bytes); bytes_pushed_total is the cumulative sum
// of len for every successful (non-dropped) pl_log_ring_push_attr() call.
// bytes_pushed_total + s_bytes_dropped vs bytes_drained_total is the
// ledger Ada's design comment (section 2b) calls for.
static volatile uint32_t s_room_zero;
static volatile uint32_t s_bytes_drained_total;
static volatile uint32_t s_bytes_pushed_total;
// Cumulative count of pl_log_ring_drain() calls, incremented unconditionally
// at entry (unlike s_drain_skips, which counts only the lock-contended
// subset). This is drn's `c=` field -- the denominator for interpreting
// every other drn counter as a rate.
static volatile uint32_t s_drain_call_count;

// Bead pico-link-auh: this file's own 1Hz clock for publishing slots 1
// (atr) and 2 (drn) -- both are internal to this file, unlike slot 0
// (ctr), which a2dp.c publishes at main.c's existing shared-report point.
// Thread context only, same as everything else in pl_log_ring_drain().
static uint64_t s_last_prio_report_us;

void pl_log_ring_init(void) {
    // Bead pico-link-okx (F3b): THE persistence check -- this must NOT
    // unconditionally zero s_write/s_read (the pre-F3b body did exactly
    // that, which would have silently defeated persistence the moment
    // s_buf/s_write/s_read moved to NOLOAD: the bytes would still be
    // sitting in SRAM but nothing would ever know to read them).
    if (s_ring_magic == PL_LOG_RING_MAGIC) {
        s_recovered = true;
        s_recovered_bytes = s_write - s_read; // unsigned modular arithmetic, same convention as everywhere else in this file
        // s_write/s_read intentionally NOT reset here -- see the module
        // doc above.
    } else {
        // Cold boot, or a true power cycle that did not preserve SRAM.
        s_write = 0;
        s_read = 0;
        s_recovered = false;
        s_recovered_bytes = 0;
    }
    s_ring_magic = PL_LOG_RING_MAGIC;

    s_bytes_dropped = 0;
    s_push_hold_us_total = 0;
    s_push_hold_us_max = 0;
    s_drain_skips = 0;
    s_backlog_hwm = 0;

    // Bead pico-link-auh.
    memset(s_attr_table, 0, sizeof(s_attr_table));
    s_room_zero = 0;
    s_bytes_drained_total = 0;
    s_bytes_pushed_total = 0;
    s_drain_call_count = 0;
    s_last_prio_report_us = 0;
}

// Forward declaration -- defined below pl_log_ring_drain(), which calls it
// unconditionally at entry. See its definition for what it does.
static void pl_log_ring_publish_diag_slots(void);

// Bead pico-link-auh, section 2a: find-or-claim `pc`'s slot in the
// attribution table. Called ONLY from inside pl_log_ring_push_attr()'s
// save_and_disable_interrupts() critical section below -- see this file's
// module doc and pl_log_ring.h's doc on pl_log_ring_push_attr() for why no
// separate lock is added here. Linear probe; if the table is full and
// `pc` isn't already in it, the attribution update is silently skipped --
// this only loses ranking precision among the least frequent producers,
// it never affects whether the underlying push itself succeeds or drops.
static pl_log_attr_entry_t *pl_log_attr_find_or_claim(uint32_t pc) {
    int32_t free_idx = -1;
    for (uint32_t i = 0; i < PL_LOG_ATTR_TABLE_SIZE; i++) {
        if (s_attr_table[i].used && s_attr_table[i].pc == pc) {
            return &s_attr_table[i];
        }
        if (!s_attr_table[i].used && free_idx < 0) {
            free_idx = (int32_t)i;
        }
    }
    if (free_idx >= 0) {
        s_attr_table[free_idx].used = true;
        s_attr_table[free_idx].pc = pc;
        s_attr_table[free_idx].bytes = 0;
        s_attr_table[free_idx].drops = 0;
        return &s_attr_table[free_idx];
    }
    return NULL; // table full and pc not already tracked -- see doc above
}

void pl_log_ring_push_attr(const char *data, uint32_t len, uint32_t pc) {
    if (len == 0) {
        return;
    }
    if (len > PL_LOG_RING_SIZE) {
        // Cannot ever fit, no matter how empty the ring is -- drop
        // outright, no critical section needed.
        s_bytes_dropped += len;
        return;
    }

    uint64_t t0 = time_us_64();
    uint32_t save = save_and_disable_interrupts();

    uint32_t used = s_write - s_read; // wraparound-safe: unsigned modular arithmetic
    uint32_t free_space = PL_LOG_RING_SIZE - used;
    if (len > free_space) {
        pl_log_attr_entry_t *e = pl_log_attr_find_or_claim(pc);
        if (e != NULL) {
            e->drops += len;
        }
        restore_interrupts(save);
        s_bytes_dropped += len;
        return;
    }

    uint32_t write_idx = s_write & (PL_LOG_RING_SIZE - 1);
    uint32_t first_chunk = PL_LOG_RING_SIZE - write_idx;
    if (first_chunk > len) {
        first_chunk = len;
    }
    memcpy(&s_buf[write_idx], data, first_chunk);
    if (len > first_chunk) {
        memcpy(&s_buf[0], data + first_chunk, len - first_chunk);
    }
    s_write += len;

    pl_log_attr_entry_t *e = pl_log_attr_find_or_claim(pc);
    if (e != NULL) {
        e->bytes += len;
    }

    restore_interrupts(save);

    s_bytes_pushed_total += len;

    uint32_t hold_us = (uint32_t)(time_us_64() - t0);
    s_push_hold_us_total += hold_us;
    if (hold_us > s_push_hold_us_max) {
        s_push_hold_us_max = hold_us;
    }
}

void pl_log_ring_push(const char *data, uint32_t len) {
    pl_log_ring_push_attr(data, len, 0);
}

// Bead pico-link-okx (F2): rewritten to be bounded and non-blocking -- see
// this bead's design comment (Ada, 2026-08-30) for the full root-cause
// chain this replaces. The old body called fwrite()+fflush() straight to
// stdout, which is pico_stdio_usb's stdio_usb_out_chars() underneath: a
// busy-wait whose escape condition (stdio_usb_connected(), which this
// build makes return tud_ready() -- see firmware/CMakeLists.txt's
// PICO_STDIO_USB_CONNECTION_WITHOUT_DTR comment) is HOST-paced, not
// firmware-bounded -- a host that isn't draining the CDC endpoint kept the
// superloop inside that call indefinitely, well past the 2000ms hardware
// watchdog window.
//
// New shape, no loop, no timeout, no tud_task() call from thread context,
// no dependence on the host whatsoever:
void pl_log_ring_drain(void) {
    s_drain_call_count++; // bead pico-link-auh, section 2b -- drn's `c=` denominator

    // Bead pico-link-auh, section 2a/2b: publish the atr/drn priority
    // slots on this file's own 1Hz clock, independent of whether this
    // particular call finds anything to drain.
    pl_log_ring_publish_diag_slots();

    // Single reader, no lock needed to observe s_write or advance s_read --
    // see this file's header doc.
    uint32_t write_snapshot = s_write;
    uint32_t available = write_snapshot - s_read;
    if (available > s_backlog_hwm) {
        s_backlog_hwm = available;
    }
    // Bead pico-link-zmg, A2: this early return USED to be unconditional,
    // which silently dropped a fresh priority slot whenever the plain log
    // ring happened to be empty -- the priority check further down was
    // never reached. Nothing counted it, so it did not even show up as a
    // drop. It self-masks under sustained logging (the ring is rarely
    // empty then), which is why it cost only the first ~16s of the
    // verification capture: ctr sequences 147-161 vanished, then 633
    // consecutive publishes survived without a single gap.
    //
    // The priority channel's whole promise is that telemetry outlives
    // narration, so it must NOT be gated on narration having something to
    // say. With available == 0 the log-drain tail below computes take == 0
    // and writes nothing, which is correct and harmless.
    if (available == 0 && !pl_prio_any_fresh()) {
        return;
    }

    // Never blocks: false means the 0xC0 worker currently holds
    // pl_usb_mutex (tud_task() mid-call) -- skip this tick, the bytes stay
    // in the ring for the next one. tud_task() is not reentrant, so this
    // gate cannot be removed even though F1 already stopped logging from
    // contending on it.
    if (!pl_usb_lock_try()) {
        s_drain_skips++;
        return;
    }

    if (!tud_ready()) {
        // Not enumerated / suspended -- nothing to write to yet.
        pl_usb_unlock();
        return;
    }

    // cdc_device.c sets the CDC TX FIFO OVERWRITABLE at reset and only
    // clears that on a DTR assert -- which this build's host (macOS,
    // PICO_STDIO_USB_CONNECTION_WITHOUT_DTR=1) never performs (see
    // firmware/CMakeLists.txt and CLAUDE.md's DTR notes). tud_cdc_write()
    // therefore never returns short here; it SILENTLY OVERWRITES unsent
    // bytes instead. Gating on tud_cdc_write_available() before writing is
    // therefore load-bearing for correctness, not an optimisation -- it is
    // the only thing standing between this drain and quietly corrupting
    // its own output.
    uint32_t room = tud_cdc_write_available();
    if (room == 0) {
        s_room_zero++; // bead pico-link-auh, section 2b
        pl_usb_unlock();
        return;
    }

    // Bead pico-link-auh: the priority-slot reservation rule. A fresh
    // priority snapshot (pl_prio.h) gets absolute priority over the
    // verbose log ring, emitted through this same single tud_cdc_write()
    // call site under the lock already held above -- see pl_prio.h's
    // module doc for why this is the ONLY place that ever emits one.
    //
    // If a slot is fresh but there isn't room for a whole one, the log
    // ring gets NOTHING this call (not a partial log write followed by a
    // dropped slot) -- this bounds how long a pending slot can be delayed
    // to at most one drain call: the FIFO drains host-side between calls,
    // so `room` cannot stay below PL_PRIO_SLOT_LEN indefinitely. Combined
    // with PL_PRIO_SLOT_LEN < CFG_TUD_CDC_TX_BUFSIZE (the _Static_assert
    // above), a slot always fits an otherwise-empty FIFO, so this can
    // never deadlock the priority channel against the log ring.
    if (pl_prio_any_fresh()) {
        if (room < PL_PRIO_SLOT_LEN) {
            pl_usb_unlock();
            return;
        }
        const char *slot_data = NULL;
        uint32_t slot_len = pl_prio_emit_one(&slot_data);
        if (slot_len > 0) {
            uint32_t slot_w = tud_cdc_write(slot_data, slot_len);
            tud_cdc_write_flush();
            room -= slot_w;
        }
    }

    if (room == 0) {
        s_room_zero++; // bead pico-link-auh, section 2b -- the priority slot consumed all of it
        pl_usb_unlock();
        return;
    }

    uint32_t read_idx = s_read & (PL_LOG_RING_SIZE - 1);
    uint32_t first_chunk = PL_LOG_RING_SIZE - read_idx;
    if (first_chunk > available) {
        first_chunk = available;
    }
    // Bound to this call's contiguous run of the ring; a wrapped remainder
    // (available > first_chunk) is picked up by the NEXT call, same as any
    // other partial drain -- there is no requirement that one call drains
    // everything queued.
    uint32_t take = available;
    if (take > room) {
        take = room;
    }
    if (take > first_chunk) {
        take = first_chunk;
    }

    // Advance by what tud_cdc_write() ACTUALLY accepted, not by `take` --
    // this is instrument defect #1 from this bead's design comment: the
    // pre-F2 code advanced s_read by `available` unconditionally regardless
    // of what the underlying write accepted, silently discarding whatever
    // stdio_usb's own internal timeout gave up on. tud_cdc_write() itself
    // is documented to accept up to `bufsize` immediately (it is a FIFO
    // copy, not a blocking call), so w == take is the expected case, but
    // trusting the return value rather than the request is what makes this
    // correct even if that ever changes.
    uint32_t w = tud_cdc_write(&s_buf[read_idx], take);
    s_read += w;
    s_bytes_drained_total += w; // bead pico-link-auh, section 2b
    tud_cdc_write_flush();

    pl_usb_unlock();
}

// Bead pico-link-auh, section 2a/2b: once a second, publish the atr
// (producer attribution, top-3 by bytes) and drn (drain-side ledger)
// priority slots. THREAD CONTEXT ONLY, called from pl_log_ring_drain()
// (itself thread-context-only) -- same contract as pl_prio_publish()
// requires, see pl_prio.h's module doc. Runs unconditionally, independent
// of whether this call actually found anything to drain, so the 1Hz
// cadence holds even when the log ring is empty.
static void pl_log_ring_publish_diag_slots(void) {
    uint64_t now_us = time_us_64();
    if (s_last_prio_report_us != 0 && now_us - s_last_prio_report_us < 1000000) {
        return;
    }
    s_last_prio_report_us = now_us;

    // Top-3 attribution entries by bytes. PL_LOG_ATTR_TABLE_SIZE is only
    // 32, so a linear top-3 scan is cheap and simple.
    const pl_log_attr_entry_t *top[3] = {NULL, NULL, NULL};
    for (uint32_t i = 0; i < PL_LOG_ATTR_TABLE_SIZE; i++) {
        if (!s_attr_table[i].used) {
            continue;
        }
        const pl_log_attr_entry_t *cand = &s_attr_table[i];
        for (uint32_t slot = 0; slot < 3; slot++) {
            if (top[slot] == NULL || cand->bytes > top[slot]->bytes) {
                for (uint32_t k = 2; k > slot; k--) {
                    top[k] = top[k - 1];
                }
                top[slot] = cand;
                break;
            }
        }
    }
    pl_prio_publish(
        1,
        "atr pc0=%08lx b0=%08lu pc1=%08lx b1=%08lu pc2=%08lx b2=%08lu",
        (unsigned long)(top[0] != NULL ? top[0]->pc : 0),
        (unsigned long)(top[0] != NULL ? top[0]->bytes : 0),
        (unsigned long)(top[1] != NULL ? top[1]->pc : 0),
        (unsigned long)(top[1] != NULL ? top[1]->bytes : 0),
        (unsigned long)(top[2] != NULL ? top[2]->pc : 0),
        (unsigned long)(top[2] != NULL ? top[2]->bytes : 0)
    );

    pl_prio_publish(
        2,
        "drn c=%08lu s=%08lu z=%08lu bd=%010lu bp=%010lu dr=%010lu",
        (unsigned long)s_drain_call_count,
        (unsigned long)s_drain_skips,
        (unsigned long)s_room_zero,
        (unsigned long)s_bytes_drained_total,
        (unsigned long)s_bytes_pushed_total,
        (unsigned long)s_bytes_dropped
    );
}

uint32_t pl_log_ring_bytes_dropped(void) {
    return s_bytes_dropped;
}

uint32_t pl_log_ring_push_hold_us_total(void) {
    return s_push_hold_us_total;
}

uint32_t pl_log_ring_push_hold_us_max(void) {
    return s_push_hold_us_max;
}

uint32_t pl_log_ring_drain_skips(void) {
    return s_drain_skips;
}

uint32_t pl_log_ring_backlog_hwm(void) {
    return s_backlog_hwm;
}

bool pl_log_ring_recovered_backlog(void) {
    return s_recovered;
}

uint32_t pl_log_ring_recovered_backlog_bytes(void) {
    return s_recovered_bytes;
}
