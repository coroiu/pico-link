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
//   - T2: firmware/src/debug_remote.c's "MEDIA PLAYPAUSE" / "MEDIA NEXT" /
//     "MEDIA PREV" console commands, thread context, calling
//     pl_media_keys_push_tap() below -- proves the USB half end to end
//     (console command pauses/skips on the host) with NO Bluetooth
//     involved, so a later AVRCP failure is unambiguous. Kept live
//     alongside T3, not retired -- pico-link-47z.5 wants a debug command
//     that pushes a bare press without its release.
//   - T3 (pico-link-47z.3, this bead): the AVRCP target passthrough
//     handler in a2dp.c, running on the cyw43/BTstack background IRQ
//     (priority 0xFF), calling pl_media_keys_push_press/release() as real
//     press/release AVRCP_SUBEVENT_OPERATION events arrive.
//   - The consumer, pl_media_keys_drain(), runs once per superloop
//     iteration (thread context) and is the only caller of tud_hid_report().
//
// MPSC, not SPSC: with both producer contexts live, push() in
// media_keys.c runs its whole body under save_and_disable_interrupts()/
// restore_interrupts() -- the same fix bt.c's ring applies for the
// identical reason (see bt.c:50-127's doc comment). This was chosen over
// retiring the console producer specifically so pico-link-47z.5 stays
// possible.
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
// Bead pico-link-4v2.1 (VT1, volume-sync risk gate): HID Consumer Volume
// Increment/Decrement. Pushed via pl_media_keys_push_tap() from
// debug_remote.c's "VOL HOSTUP"/"VOL HOSTDOWN" console commands to measure
// whether a HID tap moves macOS's own output-volume slider -- see
// .planning/design/2026-09-02-volume-sync.md sec 6, mechanism M2.
#define PL_MEDIA_KEY_USAGE_VOLUME_INCREMENT 0x00E9u
#define PL_MEDIA_KEY_USAGE_VOLUME_DECREMENT 0x00EAu

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
