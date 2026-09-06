// Pico Link firmware -- canonical volume state, loop-breaking rule and
// circuit breaker (design .planning/design/2026-09-02-volume-sync.md
// sec 3/4; bead pico-link-4v2.2, VT2 of the volume-sync epic pico-link-4v2).
//
// SCOPE OF THIS BEAD (T2): console-only. This module owns the canonical
// 0..127 (AVRCP absolute-volume domain) volume value in THREAD CONTEXT --
// the superloop is the only context permitted to eventually reach both
// BTstack (via bt.c's existing pending-action queue/heartbeat, per
// pico-link-ouw) and the USB feature unit's outbound state (design sec 2's
// context table). T3 (host->headphones) and T4 (headphones->host) wire the
// two inbound latches below to real producers -- usb_audio.c's FU SET on
// the 0xC0 worker IRQ, bt.c's AVRCP VOLUME_CHANGED notification on the
// cyw43 background IRQ 0xFF -- and the two outbound latches to real
// consumers. Until then the only producer is debug_remote.c's "VOL SET n"
// console command, and the outbound latches are written but read by
// nobody: T2 deliberately emits nothing, it only logs what it would have
// emitted (see pl_volume_debug_set's doc comment).
//
// LOOP RULE (design sec 4): a value is applied to canonical and
// propagated to both peers iff, after quantisation into the canonical
// 0..127 domain, it differs from the current canonical level. Equal is
// absorbed silently. We NEVER echo a peer's raw value back at them -- the
// emission is always derived from canonical, never from whatever the
// latch happened to hold. Origin tagging and suppression windows are
// deliberately NOT used (design sec 4c) -- do not reintroduce them.
//
// DOMAIN MAPPING (design sec 5, T1-verified on pico-link-4v2.1): the
// AVRCP absolute-volume domain (0..127) IS the canonical domain -- no
// mapping needed there. The UAC2 feature unit's dB domain is mapped via a
// bijective RANGE bMin=-12700 bMax=0 bRes=100 (127 steps of 0.39dB):
//     cur   = level * 100 - 12700
//     level = round((cur + 12700) / 100), clamped 0..127
// T1 measured macOS sending exact bRes multiples against the OLD
// (bMin=-12800, bRes=256) range with no off-grid values ever seen, which
// is the load-bearing assumption behind changing the descriptor RANGE
// response (feature_unit_get_request, usb_audio.c) to this one -- see
// that function's doc comment for the verified verdict.
//
// MUTE (design sec 5): AVRCP has no mute concept. Canonical tracks a
// separate `muted`/`pre_mute` pair for a future device-side mute source
// (out of scope here, design sec 8) -- an inbound sink level of 0 must
// NOT set `muted`. Not exercised by T2's console surface (no "VOL MUTE"
// command exists yet); kept as state so T3/T4 have somewhere to put it
// without a second reshape of this module.
//
// CIRCUIT BREAKER (design sec 4.3): insurance against a sink whose
// snap-to-grid turns out not to be idempotent, which would otherwise
// oscillate forever (the one real hazard section 4.1's termination proof
// depends on idempotence to rule out). More than 8 accepted propagations
// within a 1s rolling window trips a 2s cooldown during which incoming
// changes are logged and dropped -- neither applied nor propagated.
// Origin tagging is deliberately not used anywhere in this module (design
// sec 4c), so the breaker necessarily counts ALL accepted propagations
// regardless of source; T1's own measured sweep (11 distinct SETs across
// an 11-point 0..100% AppleScript sweep) is nowhere near this rate for
// ordinary use.
#ifndef PICO_LINK_VOLUME_H
#define PICO_LINK_VOLUME_H

#include <stdbool.h>
#include <stdint.h>

// Carried for display/diagnostics only -- NOT used by the loop-breaking
// rule (design sec 4c/7). Matches the PlEvent `source` field T5 will add
// (0=host, 1=sink, 2=device), plus a debug-only value for T2's console.
typedef enum {
    PL_VOLUME_SOURCE_HOST = 0,
    PL_VOLUME_SOURCE_SINK = 1,
    PL_VOLUME_SOURCE_DEVICE = 2,
    PL_VOLUME_SOURCE_CONSOLE = 3, // T2 debug-only origin; not part of design sec 7's PlEvent set
} PlVolumeSource;

// Call once at startup, before any notify/service/debug call below.
void pl_volume_init(void);

// --- Inbound edges -- IRQ-safe, callable from ANY context (design sec 2) ---

// T3 (not yet wired to a real caller): usb_audio.c's
// feature_unit_set_request, 0xC0 worker IRQ. `cur` is the raw
// FU_CTRL_VOLUME bCur the host sent, in the RANGE descriptor's dB*100
// domain (see this header's domain-mapping doc comment above). Quantises
// into the canonical domain and latches it -- coalescing, like every
// other latch in this firmware: a burst before the next
// pl_volume_service() call collapses to the latest value.
void pl_volume_notify_host_raw(int16_t cur);

// T4 (not yet wired to a real caller): bt.c's
// AVRCP_SUBEVENT_NOTIFICATION_VOLUME_CHANGED handler, cyw43 background
// IRQ 0xFF. `level` is already in the canonical 0..127 domain (AVRCP's
// own domain) -- clamped defensively on ingest.
void pl_volume_notify_sink(uint8_t level);

// --- Thread-context service, called once per superloop frame ---

// Drains host_pending then sink_pending, in that fixed order (design sec
// 4.2 -- simultaneous changes are totally ordered by the drain, last one
// wins; no arbitration is built), and applies the loop rule to each in
// turn. Also owns the circuit breaker's rolling time window. `now_us` is
// the caller's time_us_64(), same convention as pl_media_keys_drain's
// `now_us` parameter.
void pl_volume_service(uint64_t now_us);

// --- Debug-only entry point (T2's console surface) ---

// debug_remote.c's "VOL SET n" command: applies the SAME loop rule as the
// two real edges above (n is already in the canonical domain -- no
// quantisation needed) and LOGS what it would emit to each peer via
// pl_prio.h's slot 4, rather than actually emitting anything -- T3/T4
// have not wired a real consumer to the outbound latches yet, so writing
// them today would be inert regardless; the log line is what makes T2
// independently verifiable. `n` is clamped to 0..127. Subject to the same
// circuit breaker as the real edges (source is PL_VOLUME_SOURCE_CONSOLE).
void pl_volume_debug_set(uint8_t n, uint64_t now_us);

// --- Accessors (thread context; T5's Rust seam will read these) ---
uint8_t pl_volume_level(void);
bool pl_volume_muted(void);

// T3 (not yet wired to a real caller besides bt.c's heartbeat handler):
// reads and clears the outbound AVRCP latch, IRQ-safe -- callable from
// ANY context (design sec 2), specifically the cyw43 background IRQ 0xFF.
// Returns true and fills `out_level` iff a new value is pending; false
// (out_level untouched) if nothing changed since the last call.
bool pl_volume_take_avrcp_desired(uint8_t *out_level);

#endif // PICO_LINK_VOLUME_H
