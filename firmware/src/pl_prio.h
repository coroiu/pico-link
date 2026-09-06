// Pico Link firmware -- non-starvable priority counter channel (bead
// pico-link-auh).
//
// WHY THIS EXISTS: pl_log_ring.c (bead pico-link-okx) made the console
// non-blocking by shedding data under load instead of stalling the
// superloop -- correct behaviour, but it means ANY line pushed through
// pl_log()/pl_log_ring_push() can be silently dropped if the ring is full.
// pico-link-auh's soak measured backlog_hwm at ~4075/4096 and log_drops
// jumping by 6309 in the boot burst alone, and log_drops=16283395 in a
// later streaming capture -- the verbose console cannot be trusted to
// deliver any particular line, including the very counters a diagnosis
// (the LDAC PL_A2DP_MAX_ENCODE_DWELL_US falsifier, a2dp.c:149) depends on.
//
// THE FIX IS NOT A BIGGER OR SECOND RING. The payload here is a snapshot
// of CUMULATIVE counters, not a message stream: if a snapshot cannot be
// sent right now, the correct behaviour is to send the NEWEST snapshot
// later, never a stale queued one and never nothing. A fixed-count array
// of fixed-width, overwrite-in-place slots gives exactly that: a slot
// cannot overflow (there is nothing to overflow into), cannot drop (the
// old value is simply superseded), and cannot go stale past one publish
// cycle. That property -- not clever scheduling -- is what "non-starvable"
// means here.
//
// THREAD CONTEXT ONLY. pl_prio_publish() must be called only from thread
// context (the superloop, e.g. the 1Hz shared-report block in main.c).
// NEVER from an IRQ, NEVER from the 0xC0 USB worker. This module takes NO
// lock, deliberately: its only two actors today, the publisher (thread
// context, main.c's shared-report block) and the emitter
// (pl_log_ring_drain(), also thread context, same superloop), run
// serially on core 0 and can never interleave with each other, so a
// seqlock or critical section would protect against a race that cannot
// happen. If a future caller ever needs to publish from an IRQ, THAT is
// the moment to add a seqlock -- do not add one speculatively now.
//
// INTEGRATION SEAM: pl_prio_emit_one() is called from exactly one place,
// pl_log_ring_drain() (pl_log_ring.c), under that function's existing
// single pl_usb_lock_try() acquisition, before the log ring's own bytes
// are drained. pl_log_ring_drain() remains the ONLY function in this
// firmware that calls tud_cdc_write() -- see pl_log_ring.h's module doc.
// Do not call pl_prio_emit_one() from anywhere else.
#ifndef PICO_LINK_PL_PRIO_H
#define PICO_LINK_PL_PRIO_H

#include <stdint.h>

// Exact width of every slot, in bytes, padded with spaces and terminated
// CRLF. Chosen to be strictly less than CFG_TUD_CDC_TX_BUFSIZE (256,
// tusb_config.h) so a slot always fits an otherwise-empty CDC TX FIFO --
// see the _Static_assert next to this constant's use in pl_log_ring.c.
#define PL_PRIO_SLOT_LEN 128u

// Fixed slot assignment (see this bead's design comment for the full
// rationale of each):
//   0 = "ctr" -- a2dp LDAC-falsifier counters (a2dp.c, pl_a2dp_publish_counters)
//   1 = "atr" -- pl_log producer attribution, top-3 by bytes (pl_log_ring.c)
//   2 = "drn" -- drain-side saturation ledger (pl_log_ring.c)
//   3 = "lpf" -- pl_loop_prof.c's per-phase latency histogram (round-robins
//       one pl_loop_phase_t per publish)
//   4 = "vol" -- bead pico-link-4v2.1 (VT1, volume-sync risk gate):
//       debug_remote.c's "VOL GET"/"VOL WATCH" snapshot. Added because
//       logging the feature unit's FU SET events via plain pl_log() was
//       measured unreliable under this firmware's background log-ring
//       congestion (log_drops in the tens of thousands within seconds of
//       boot -- the same problem this module exists to solve for the
//       other four slots). Only ever published to under PL_DEBUG_REMOTE
//       (debug_remote.c is only compiled under that flag -- see
//       CMakeLists.txt), so slot 4 is gated on it below: a release build
//       does not statically allocate a slot it will never publish to.
#ifdef PL_DEBUG_REMOTE
#define PL_PRIO_SLOT_COUNT 5u
#else
#define PL_PRIO_SLOT_COUNT 4u
#endif

// Formats `fmt`/`...` into slot `slot_id`, right-padded with spaces to
// PL_PRIO_SLOT_LEN - 2 and terminated with CRLF, then marks it fresh.
// Overwrites the slot unconditionally -- a slot that had not yet been
// emitted is silently superseded by the newer snapshot, which is correct:
// see this file's module doc for why "newest value, sent later" is the
// only sane behaviour for a cumulative-counter snapshot.
//
// THREAD CONTEXT ONLY -- see this file's module doc. slot_id must be <
// PL_PRIO_SLOT_COUNT; out-of-range calls are ignored.
//
// The formatted text (before padding) must fit within PL_PRIO_SLOT_LEN - 2
// bytes; vsnprintf truncates safely if it doesn't, it never overflows the
// slot.
void pl_prio_publish(uint8_t slot_id, const char *fmt, ...) __attribute__((format(printf, 2, 3)));

// Returns true iff at least one slot is currently marked fresh (i.e. has a
// snapshot pending emission). Called by pl_log_ring_drain() to decide
// whether to reserve room for a priority slot before draining the log
// ring -- see pl_log_ring.c's reservation rule.
int pl_prio_any_fresh(void);

// Selects exactly one fresh slot (round-robin over slots that are
// currently fresh), clears its fresh flag, points *out_data at that
// slot's PL_PRIO_SLOT_LEN-byte buffer (owned by this module, valid until
// the next pl_prio_publish()/pl_prio_emit_one() call -- the caller must
// write it out before either can run again, which pl_log_ring_drain()'s
// single-threaded, non-reentrant call shape guarantees), and returns
// PL_PRIO_SLOT_LEN. Returns 0 and leaves *out_data untouched if no slot
// is fresh.
//
// Deliberately does NOT call tud_cdc_write() itself and does not take a
// destination buffer to copy into -- pl_log_ring_drain() remains the ONLY
// function in this firmware that touches tud_cdc_write() (see
// pl_log_ring.h's module doc); this function only hands back a pointer
// for that single call site to write.
//
// INTERNAL to the pl_log_ring.c integration seam -- see this file's module
// doc. Call ONLY from pl_log_ring_drain().
uint32_t pl_prio_emit_one(const char **out_data);

#endif // PICO_LINK_PL_PRIO_H
