// Pico Link firmware -- USB HID consumer-control media keys (bead
// pico-link-47z.2, T2 of .planning/design/2026-09-02-media-keys.md).
//
// Owns EVERY tud_hid_* call in this firmware. main.c:31-39 locally
// #undef/#defines CFG_TUD_HID to 0 to dodge an enum collision between
// BTstack's btstack_hid.h and TinyUSB's hid_device.h (both are included
// there) -- that override is safe ONLY because main.c itself never calls a
// tud_hid_* function. This module is the one place that may. Do not add a
// tud_hid_* call anywhere else; route through the functions below instead.
//
// Two producer contexts, one consumer:
//   - T2 (this bead): firmware/src/debug_remote.c's "MEDIA PLAYPAUSE" /
//     "MEDIA NEXT" / "MEDIA PREV" console commands, thread context, calling
//     pl_media_keys_push_tap() below -- proves the USB half end to end
//     (console command pauses/skips on the host) with NO Bluetooth
//     involved, so a later AVRCP failure is unambiguous.
//   - T3 (pico-link-47z.3, not yet wired): the AVRCP target passthrough
//     handler, running on the cyw43/BTstack background IRQ (priority
//     0xFF), calling pl_media_keys_push_press/release() as real
//     press/release AVRCP_SUBEVENT_OPERATION events arrive.
//   - The consumer, pl_media_keys_drain(), runs once per superloop
//     iteration (thread context) and is the only caller of tud_hid_report().
//
// NOTE for T3: this ring's SPSC contract (see media_keys.c) assumes a
// single producer context. During T2 that is true (only the console
// pushes). Once T3 adds the AVRCP IRQ producer, if the console commands
// remain reachable in the same build, there would be two producer
// contexts and the plain volatile head/tail scheme below is not safe
// against a console push racing an AVRCP-handler push (both write
// s_ring_head). T3 must either retire the console commands or add proper
// synchronization -- flagging here rather than solving it speculatively.
#ifndef PICO_LINK_MEDIA_KEYS_H
#define PICO_LINK_MEDIA_KEYS_H

#include <stdint.h>

// HID Consumer usages this firmware ever sends -- see design doc sec 3.4.
// T3 will map AVRCP_OPERATION_ID_PLAY/PAUSE to PLAY_PAUSE (a toggle, not
// discrete play vs pause -- see design sec 3.4 for why), FORWARD to
// SCAN_NEXT, BACKWARD to SCAN_PREV. STOP is defined for completeness but
// nothing pushes it yet.
#define PL_MEDIA_KEY_USAGE_PLAY_PAUSE 0x00CDu
#define PL_MEDIA_KEY_USAGE_SCAN_NEXT  0x00B5u
#define PL_MEDIA_KEY_USAGE_SCAN_PREV  0x00B6u
#define PL_MEDIA_KEY_USAGE_STOP       0x00B7u

// Call once at startup, before the first pl_media_keys_push_*/drain call.
// Resets the ring and safety-timeout state. No hardware side effects (the
// HID interface itself is initialized by TinyUSB via usb_descriptors.c).
void pl_media_keys_init(void);

// Producer side: pushes a key-down for `usage` into this module's own
// small ring. Callable from ANY context (including BTstack's IRQ, for
// T3) -- touches only the ring's head index and one slot, never TinyUSB.
// On ring-full, drops the newest entry and counts it (see
// pl_media_keys_dropped()) rather than blocking or overwriting an
// undrained entry.
void pl_media_keys_push_press(uint16_t usage);

// Producer side: pushes a key-up (empty report) into the ring. Same
// context/overflow rules as pl_media_keys_push_press().
void pl_media_keys_push_release(void);

// Convenience for T2's console-driven testing ONLY: pushes a press
// immediately followed by a release, modeling a single "tap" with no
// separate physical press/release timing (the console has no notion of a
// held key). T3's AVRCP handler must NOT use this -- it has real,
// independently-timed press and release events and must call
// pl_media_keys_push_press/release() separately.
void pl_media_keys_push_tap(uint16_t usage);

// Consumer side: call exactly once per superloop iteration, thread context
// only. `now_us` is the caller's current timestamp (time_us_64()), passed
// in rather than re-read here so every call site in one frame agrees on
// "now", same convention as pl_usb_pump_report's report_dt_us.
//
// Peeks (does not pop) the oldest queued ring entry, attempts to send it
// as a HID report under pl_usb_lock_try(), and pops it ONLY if the send
// succeeded -- tud_hid_report() returns false when the endpoint is still
// busy, and popping before a confirmed send is the stuck-key bug (a
// dropped release wedges the host's media stack). A failed send or an
// unavailable lock leaves the entry queued for the next call.
//
// Also runs the 600ms safety force-release: if a press was successfully
// sent with no corresponding release sent since, and now_us shows 600ms
// have elapsed, forces an empty report through the same
// attempt-and-only-clear-on-success path (bypassing the ring, since this
// is a safety net, not a queued event).
void pl_media_keys_drain(uint64_t now_us);

// Diagnostics only: cumulative ring entries dropped for a full ring since
// boot. Never drained/reset -- see input.c's identical convention.
uint32_t pl_media_keys_dropped(void);

#endif // PICO_LINK_MEDIA_KEYS_H
