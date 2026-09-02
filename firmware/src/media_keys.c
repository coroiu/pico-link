#include "media_keys.h"

#include <stdbool.h>
#include <stdint.h>

#include "hardware/sync.h"

#include "tusb.h"

#include "usb_pump.h"

// T3 (pico-link-47z.3): MPSC, not SPSC -- same fix as bt.c's ring
// (firmware/src/bt.c:50-127) and for the identical reason. This ring now
// has TWO producer contexts: debug_remote.c's console commands (thread
// context, via pl_media_keys_push_tap) and the AVRCP target passthrough
// handler in a2dp.c (BTstack/cyw43 IRQ context, priority 0xFF, via
// pl_media_keys_push_press/release). The console producer is kept
// deliberately -- pico-link-47z.5 wants a debug command that pushes a bare
// press without its release, which push_press() alone still supports.
//
// Plain volatile head/tail (T2's original scheme) is a read-modify-write
// race the moment a second producer context exists: push()'s
// read-check-write-write sequence on s_ring_head can be preempted by the
// IRQ producer between the read and the final write, and both producers
// then compute the same `head`, both write the same slot, and one event
// is silently lost without even incrementing the drop counter. So push()
// runs its entire body inside save_and_disable_interrupts()/
// restore_interrupts(), exactly like pl_bt_ring_push: this also correctly
// serializes the two THREAD-context producers (console command handler
// and, hypothetically, any future one) since disabling interrupts on this
// single core excludes both the IRQ producer and any other thread-context
// caller for the duration. The critical section is tiny (a few field
// writes, no memcpy unlike bt.c's ring), so the interrupt-disable window
// is negligible. The consumer (pl_media_keys_drain) still only touches
// s_ring_tail from the superloop and needs no lock of its own.
//
// Capacity: 8 is generous for what a human can generate via the debug
// console (one command line drives exactly one press+release pair) and
// for what AVRCP passthrough can generate (one operation_id at a time --
// see design sec 3.3, "one-key-at-a-time is sufficient").
#define PL_MEDIA_KEYS_RING_CAPACITY 8

typedef struct {
    uint16_t usage; // consumer usage for a press; ignored (sent as 0) for a release
    bool pressed;
} pl_media_key_event_t;

static pl_media_key_event_t s_ring[PL_MEDIA_KEYS_RING_CAPACITY];
static volatile uint8_t s_ring_head; // producer-owned
static volatile uint8_t s_ring_tail; // consumer-owned (pl_media_keys_drain, thread context)
static volatile uint32_t s_ring_drop_count;

// Consumer-side-only state (touched exclusively from pl_media_keys_drain,
// thread context, never from a producer) for the 600ms safety release --
// see design sec 3.3. s_key_active is true from the moment a press report
// is CONFIRMED SENT until either a release report is confirmed sent or the
// safety timeout forces one.
static bool s_key_active;
static uint16_t s_key_active_usage;
static uint64_t s_key_active_since_us;

// Not optional -- see design sec 3.3: a lost release, a link drop
// mid-press, or a sink that only ever sends press leaves the host holding
// a media key down. 600ms is short enough that a human never notices a
// forced release fire on an intentionally-held key (nothing in this
// firmware currently holds one that long on purpose) and long enough to
// never race a normal press/release pair arriving close together.
#define PL_MEDIA_KEYS_SAFETY_TIMEOUT_US (600ull * 1000ull)

void pl_media_keys_init(void) {
    s_ring_head = 0;
    s_ring_tail = 0;
    s_ring_drop_count = 0;
    s_key_active = false;
    s_key_active_usage = 0;
    s_key_active_since_us = 0;
}

// Shared push implementation -- both push_press and push_release funnel
// through here so the ring-full policy (drop newest, count it) lives in
// exactly one place. Callable from ANY context (see module doc above) --
// the whole body runs under save_and_disable_interrupts() so the two
// producer contexts (console thread, AVRCP IRQ) can't race s_ring_head.
static void push(uint16_t usage, bool pressed) {
    uint32_t saved_irq = save_and_disable_interrupts();
    uint8_t head = s_ring_head;
    uint8_t next_head = (uint8_t)((head + 1) % PL_MEDIA_KEYS_RING_CAPACITY);
    if (next_head == s_ring_tail) {
        // Ring full -- drop this event rather than overwrite an undrained
        // one (which would corrupt the consumer's view), same policy as
        // input.c's identical ring.
        s_ring_drop_count++;
        restore_interrupts(saved_irq);
        return;
    }
    s_ring[head].usage = usage;
    s_ring[head].pressed = pressed;
    s_ring_head = next_head;
    restore_interrupts(saved_irq);
}

void pl_media_keys_push_press(uint16_t usage) {
    push(usage, true);
}

void pl_media_keys_push_release(void) {
    push(0, false);
}

void pl_media_keys_push_tap(uint16_t usage) {
    // Two separate ring entries, not one -- pl_media_keys_drain() sends
    // and pops them one at a time (peek/send/pop-on-success), so this is
    // exactly "press, then once that's confirmed sent, release", the same
    // shape a real AVRCP press+release pair takes. NOT atomic: if the ring
    // is full for the release push, the press still went out and the
    // 600ms safety timeout is what recovers, same as any other dropped
    // release would need to.
    push(usage, true);
    push(0, false);
}

// Attempts to send one HID report (a press for `usage`, or an all-zero
// release if `usage` is PL_MEDIA_KEY reserved release form -- callers pass
// the exact 16-bit value to put in the report) under pl_usb_lock_try().
// Returns true only if the report was actually accepted by TinyUSB.
static bool try_send_report(uint16_t report_usage) {
    if (!pl_usb_lock_try()) {
        return false; // 0xC0 worker holds the lock this tick -- retry next tick
    }
    // tud_hid_ready() catches "not yet configured" / endpoint stalled
    // without even attempting tud_hid_report(), which would just return
    // false in the same cases -- cheap enough to check either way, and it
    // makes tud_hid_report()'s false return unambiguously mean "endpoint
    // busy" in the pl_log below (T3 will care about that distinction more
    // than T2 does).
    bool sent = tud_hid_ready() && tud_hid_report(0, &report_usage, sizeof(report_usage));
    pl_usb_unlock();
    return sent;
}

void pl_media_keys_drain(uint64_t now_us) {
    // Peek/send/pop-on-success: process at most one ring entry per call.
    // Bounding to one keeps this call's worst-case cost fixed and small
    // (one lock attempt, one HID report) regardless of how many events
    // happen to be queued -- acceptable because entries arrive at human or
    // AVRCP-passthrough speed, both far below the superloop's own rate, so
    // catching up over a few iterations is invisible.
    if (s_ring_tail != s_ring_head) {
        const pl_media_key_event_t *ev = &s_ring[s_ring_tail];
        uint16_t report_usage = ev->pressed ? ev->usage : 0;
        if (try_send_report(report_usage)) {
            s_ring_tail = (uint8_t)((s_ring_tail + 1) % PL_MEDIA_KEYS_RING_CAPACITY);
            if (ev->pressed) {
                s_key_active = true;
                s_key_active_usage = ev->usage;
                s_key_active_since_us = now_us;
            } else {
                s_key_active = false;
            }
        }
        // else: leave the entry queued, try again next call.
    }

    // 600ms safety force-release -- independent of whether a ring entry
    // was processed this call, so a wedged producer (no release ever
    // pushed) is still caught.
    if (s_key_active && (now_us - s_key_active_since_us) >= PL_MEDIA_KEYS_SAFETY_TIMEOUT_US) {
        if (try_send_report(0)) {
            pl_log("media-keys: 600ms safety release forced (usage 0x%04X)\r\n", (unsigned)s_key_active_usage);
            s_key_active = false;
        }
        // else: still stuck (endpoint busy or worker holds the lock) --
        // retried next call, same as any other send failure above.
    }
}

uint32_t pl_media_keys_dropped(void) {
    return s_ring_drop_count;
}
