// Pico Link firmware -- M2: Bluetooth Classic bring-up through pico-sdk's
// own HCI transport.
//
// This is the whole point of the C-first pivot
// (.planning/decisions/2026-08-27-c-first-pico-sdk-owns-main.md): under
// the old Rust-first architecture, three sessions never got the radio up
// at all (a GPIO-coprocessor HardFault, then a hang in cyw43_spi_init's
// PIO/DMA claim sequence). Here the HCI transport
// (pico_btstack_hci_transport_cyw43, linked via the pico_btstack_cyw43
// CMake target) is ready-made -- this file never touches an HCI transport
// implementation, only BTstack's public HCI/GAP API.
//
// Packet-handler state machine shape (INIT -> start inquiry on
// BTSTACK_EVENT_STATE/HCI_STATE_WORKING, then GAP_EVENT_INQUIRY_RESULT/
// GAP_EVENT_INQUIRY_COMPLETE in the active state) is the same shape as
// BTstack's own upstream example/gap_inquiry.c (read directly from the
// pico-sdk's vendored btstack submodule -- BTstack's own example code,
// not USBPods, so no GPL-3 concern), reworked to push into the ui-ffi
// devices screen instead of a local device table.
#include <stdio.h>
#include <string.h>

#include "hardware/sync.h"
#include "pico/time.h" // time_us_64 -- pl_a2dp_avrcp_volume_service's deadline arg (T3, pico-link-4v2.3)

#include "btstack.h"

#include "a2dp.h"
#include "codec_ldac.h"
#include "bt.h"
#include "persist.h"
#include "pico_link_ui.h"
#include "usb_pump.h"
#include "watchdog_sup.h"

// One inquiry scan runs for INQUIRY_DURATION_UNITS * 1.28s -- 8 units is
// BTstack's own gap_inquiry.c example's INQUIRY_INTERVAL, long enough for
// nearby headphones in pairing mode to be found in one pass.
#define PL_INQUIRY_DURATION_UNITS 8

// Bead pico-link-ufh: the BTstack heartbeat's period. A dedicated timer,
// not a reuse of a2dp.c's media timer -- that one only exists while a
// stream is established, so without this there would be no evidence of the
// run loop at all in the idle/scanning state, which is most of the
// device's life. See watchdog_sup.h's module doc.
#define PL_WDT_BTSTACK_HEARTBEAT_MS 100

static struct PlUi *g_ui;
static btstack_packet_callback_registration_t hci_event_callback_registration;
static btstack_timer_source_t s_wdt_heartbeat_timer;

// --- pico-link-6o2: a C-side ring decouples the event producers -- the
// BTstack packet handler in IRQ context AND the command handler in thread
// context -- from the pl_ui_push_event call (single consumer, superloop) ---
//
// pl_bt_packet_handler runs in low_priority_irq_handler under
// pico_cyw43_arch_threadsafe_background (firmware/CMakeLists.txt) -- i.e.
// INTERRUPT CONTEXT -- and it used to call pl_ui_push_event (Rust) directly
// from there. That could preempt the superloop mid-pl_ui_render and mutate
// the model underneath it, and race the superloop's own Rust-heap
// allocations. Same class of bug pico-link-5am already fixed for input
// (see input.c): the three pl_bt_push_* helpers below no longer call into
// Rust themselves -- they build a PlEvent and push it into this ring;
// pl_bt_drain_events (called from the superloop, thread context, in
// main.c) is the only thing that ever calls pl_ui_push_event for
// Bluetooth-domain events.
//
// MPSC, not SPSC (review correction, pico-link-6o2): unlike input.c's ring,
// which has exactly one push call site (the IRQ sampler), this ring has
// TWO producer contexts -- pl_bt_start_scan and pl_bt_push_link_state are
// called both from pl_bt_packet_handler (IRQ context: BTSTACK_EVENT_STATE/
// HCI_STATE_WORKING and GAP_EVENT_INQUIRY_COMPLETE) and from
// pl_bt_poll_commands (thread context, superloop: PL_COMMAND_TAG_START_SCAN
// and PL_COMMAND_TAG_CONNECT). Plain volatile head/tail is NOT sufficient
// here: pl_bt_ring_push's read-check-write-write sequence is a
// read-modify-write on s_bt_ring_head, and if the IRQ producer preempts the
// thread-context producer between its read and its final write, both
// compute the same `head`, both write the same slot, and the loser's event
// is silently lost WITHOUT incrementing s_bt_ring_drop_count -- a corrupted,
// unreported drop, exactly the class of bug this bead exists to remove.
//
// Fix: pl_bt_ring_push runs its entire body -- including the name-buffer
// memcpy -- inside one save_and_disable_interrupts/restore_interrupts
// critical section, rather than a lock-free two-phase reserve/publish
// split. The data copied per push is small and bounded (at most
// sizeof(struct PlEvent) + PL_BT_RING_NAME_CAP bytes, ~272 bytes -- a few
// microseconds at 150MHz), so the interrupt-disable window is short and
// bounded; a reserve-then-commit scheme would avoid that window but adds
// exactly the kind of subtle ordering surface this bug came from, for no
// real latency win at this size. The consumer (pl_bt_drain_events) still
// runs single-threaded from the superloop and only ever touches
// s_bt_ring_tail, so it needs no lock of its own -- only the two producers
// contend, and the critical section serializes them.
//
// PlEvent's DeviceDiscovered payload carries a borrowed `name` pointer
// (valid, per pico_link_ui.h's doc comment, only for the duration of the
// pl_ui_push_event call) -- across a deferred ring, the pointer's original
// backing (pl_bt_handle_inquiry_result's stack-local name_buf) is long gone
// by drain time. So each ring entry embeds its own name buffer; the
// producer memcpy's the name bytes in, and the consumer re-points
// event.payload.device_discovered.name at the ring entry's own buffer
// immediately before calling pl_ui_push_event -- still satisfying "borrowed
// only for the duration of the call" because the ring entry's storage is
// static and outlives the call by construction.
//
// Capacity: 32 slots, 31 usable (one slot is always kept empty so a full
// ring's next_head != tail is distinguishable from an empty ring's
// head == tail -- the same head/tail discipline as input.c). Sized for a
// full inquiry burst, not just a single result: GAP_EVENT_INQUIRY_RESULT
// can report the same or additional devices repeatedly over the whole
// ~10s inquiry window (PL_INQUIRY_DURATION_UNITS above), and the superloop
// only drains once per frame -- if a render+blit stalls the loop (SPI blit
// time, see pico-link-14l), several results can pile up before the next
// drain. 31 usable slots covers a burst well beyond the handful of nearby
// BR/EDR devices realistically seen in one inquiry window, with
// LinkStateChanged/DevicesCleared events (one each per scan start/stop)
// costing negligible extra slots against that budget. A ring of 4 (the
// bead's called-out failure case) would not survive even a single dense
// office's inquiry result burst.
#define PL_BT_RING_CAPACITY 32
// Matches pl_bt_handle_inquiry_result's own name_buf cap below (and
// BTstack's gap_inquiry.c example) -- the EIR name field cannot exceed this.
#define PL_BT_RING_NAME_CAP 240

// --- Bead pico-link-sfw6: break-before-make device switching ---
//
// Design `.planning/design/2026-09-25-device-switch-break-before-make.md`.
// No dual connection (MAX_NR_HCI_CONNECTIONS stays 1) -- switching devices
// means disconnecting A's ACL, waiting for BTstack to actually free the
// HCI slot (hci.c frees it only AFTER DISCONNECTION_COMPLETE is emitted to
// every handler, well after core sees LinkStateChanged(Idle)), then paging
// B. Run-loop/IRQ context only, same as everything else pico-link-ouw
// already gates: pl_bt_connect_or_switch runs from pl_bt_pending_service,
// pl_bt_switch_service runs from the heartbeat right after it.
typedef enum {
    PL_BT_SWITCH_NONE,
    PL_BT_SWITCH_WAIT_ACL_DOWN,
    PL_BT_SWITCH_PAGING,
} pl_bt_switch_state_t;

// Andreas's ruling (bead comments): B failing must leave the device fully
// disconnected, never reconnect A. 5s covers a real page timeout
// (~5.1s, a2dp.c:3287's SIGNALING_CONNECTION_ESTABLISHED status path) with
// margin against a slot that somehow never frees.
#define PL_BT_SWITCH_DEADLINE_US 5000000ull

static pl_bt_switch_state_t s_switch_state;
static bd_addr_t s_switch_target;
static uint64_t s_switch_deadline_us;

// Forward declarations: pl_bt_push_connect_succeeded/_failed (below) hook
// the switch state machine's terminal-outcome edge, but pl_bt_any_acl_up/
// pl_bt_update_scan_mode aren't defined until later in this file.
static bool pl_bt_any_acl_up(void);
static void pl_bt_update_scan_mode(void);

typedef struct {
    struct PlEvent event;
    // Backing storage for event.payload.device_discovered.name once
    // event.tag == PL_EVENT_TAG_DEVICE_DISCOVERED; unused (and its contents
    // meaningless) for every other tag -- see the ring's doc comment above.
    uint8_t name_buf[PL_BT_RING_NAME_CAP];
} pl_bt_ring_entry_t;

static pl_bt_ring_entry_t s_bt_ring[PL_BT_RING_CAPACITY];
static volatile uint8_t s_bt_ring_head; // producer-owned (IRQ or thread context, see above -- lock-protected)
static volatile uint8_t s_bt_ring_tail; // consumer-owned (superloop drain only, no lock needed)
// Diagnostics only: counts events dropped because the ring was full when a
// producer tried to push. Bumped inside pl_bt_ring_push's critical section
// (so it can never race with itself even with two producer contexts),
// read/reported from pl_bt_drain_events in thread context. Never touch this
// from the packet handler with printf -- see the drop-reporting note on
// pl_bt_drain_events below.
static volatile uint32_t s_bt_ring_drop_count;

// Enqueues one event. Called from both IRQ context (the packet handler) and
// thread context (pl_bt_poll_commands) -- see the MPSC note above -- so the
// whole check-and-write sequence runs inside one interrupt-disabled critical
// section. `name`/`name_len` are optional (NULL/0 for every tag but
// DeviceDiscovered) and are memcpy'd into the ring entry's own buffer --
// never stored as a raw pointer -- for the borrow-lifetime reason in the
// ring's doc comment above. `name_len` is clamped to PL_BT_RING_NAME_CAP
// defensively (today's only caller already caps it, but a future caller
// that doesn't must not overrun name_buf), and the clamped value is what's
// written back into the event's own name_len field so a drained event is
// self-consistent with what was actually copied.
//
// Overflow policy: drop the newest (this event), keep everything already
// queued. Matches input.c's choice and for the same reason -- overwriting
// an undrained slot would corrupt the consumer's in-progress read of it,
// while dropping the newest loses exactly one event (a missed/duplicate
// inquiry result is survivable; BTstack will report a still-present device
// again on its next report within the same scan).
static void pl_bt_ring_push(struct PlEvent event, const uint8_t *name, uint16_t name_len) {
    if (name_len > PL_BT_RING_NAME_CAP) {
        name_len = PL_BT_RING_NAME_CAP;
    }
    if (event.tag == PL_EVENT_TAG_DEVICE_DISCOVERED) {
        event.payload.device_discovered.name_len = name_len;
    }

    uint32_t irq_state = save_and_disable_interrupts();
    uint8_t head = s_bt_ring_head;
    uint8_t next_head = (uint8_t)((head + 1) % PL_BT_RING_CAPACITY);
    if (next_head == s_bt_ring_tail) {
        s_bt_ring_drop_count++;
        restore_interrupts(irq_state);
        return;
    }
    pl_bt_ring_entry_t *entry = &s_bt_ring[head];
    entry->event = event;
    if (name != NULL && name_len > 0) {
        memcpy(entry->name_buf, name, name_len);
    }
    s_bt_ring_head = next_head;
    restore_interrupts(irq_state);
}

// Drains every event currently queued and makes the real pl_ui_push_event
// (Rust) call for each, in thread-context order. Intended to be called once
// per superloop iteration (see main.c), same convention as
// pl_link_input_poll.
//
// Drop reporting: no printf inside the IRQ path (pico-link-icb probe 1
// already showed that hangs the board), so the IRQ side only counts;
// this function -- thread context -- prints, and only when the count has
// actually grown since the last report, so a healthy run never prints
// anything here.
void pl_bt_drain_events(struct PlUi *ui) {
    static uint32_t s_last_reported_drops = 0;

    while (s_bt_ring_tail != s_bt_ring_head) {
        uint8_t tail = s_bt_ring_tail;
        pl_bt_ring_entry_t *entry = &s_bt_ring[tail];
        struct PlEvent event = entry->event;
        if (event.tag == PL_EVENT_TAG_DEVICE_DISCOVERED) {
            event.payload.device_discovered.name = entry->name_buf;
        }
        pl_ui_push_event(ui, event);
        s_bt_ring_tail = (uint8_t)((tail + 1) % PL_BT_RING_CAPACITY);
    }

    uint32_t drops = s_bt_ring_drop_count;
    if (drops != s_last_reported_drops) {
        pl_log("BT: event ring overflow, dropped %lu event(s) total\r\n", (unsigned long)drops);
        s_last_reported_drops = drops;
    }
}

// --- pico-link-a67: one PlEvent union in, replacing the old
// pl_ui_set_link_state/pl_ui_add_device/pl_ui_clear_devices setter trio ---
//
// Small helpers so each call site below builds one PlEvent value and pushes
// it (now via the ring above -- pico-link-6o2), rather than repeating the
// version/tag/payload boilerplate. Every PlEvent must carry
// PL_EVENT_ABI_VERSION -- pl_ui_push_event silently no-ops on a mismatch
// (see pico_link_ui.h's doc comment on pl_ui_push_event), so a helper that
// always sets it is cheap insurance against a call site accidentally
// leaving it zero-initialized. These helpers run in IRQ context (the
// packet handler) AND thread context (pl_bt_poll_commands, via
// pl_bt_push_link_state at the CONNECT case, and pl_bt_start_scan at
// START_SCAN) -- both are safe because pl_bt_ring_push serializes the two
// producer contexts under a critical section (see its doc comment above),
// not because neither calls into Rust any more.

static void pl_bt_push_link_state(enum PlLinkState state) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_LINK_STATE_CHANGED,
        .payload = {.link_state_changed = {.state = state}},
    };
    pl_bt_ring_push(event, NULL, 0);
}

// Bead pico-link-88xs, design .planning/design/2026-09-08-link-state-vs-
// discovery-axis.md section 7: the SECOND, independent axis. A GAP inquiry
// does not disconnect A2DP, so scanning must never travel on
// PL_EVENT_TAG_LINK_STATE_CHANGED (PL_LINK_STATE_SCANNING no longer even
// exists) -- it has its own tag and never touches the link axis. Same
// shape as pl_bt_push_link_state above, same IRQ-context/thread-context
// safety note.
static void pl_bt_push_discovery_state(enum PlDiscoveryState state) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_DISCOVERY_STATE_CHANGED,
        .payload = {.discovery_state_changed = {.state = state}},
    };
    pl_bt_ring_push(event, NULL, 0);
}

static void pl_bt_push_devices_cleared(void) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_DEVICES_CLEARED,
        .payload = {0},
    };
    pl_bt_ring_push(event, NULL, 0);
}

// `class_of_device` added by bead pico-link-znb.11 (E9) -- BTstack's raw
// 24-bit Class-of-Device from the inquiry result, passed through
// uninterpreted (core decides is-audio-sink, see PlDeviceDiscoveredPayload's
// doc comment in pico_link_ui.h). PL_EVENT_ABI_VERSION bumped 3 -> 4 for the
// payload shape change.
static void pl_bt_push_device_discovered(
    const uint8_t *addr, const uint8_t *name, uint16_t name_len, int8_t rssi, uint32_t class_of_device
) {
    struct PlEvent event = {.version = PL_EVENT_ABI_VERSION, .tag = PL_EVENT_TAG_DEVICE_DISCOVERED};
    memcpy(event.payload.device_discovered.addr, addr, 6);
    event.payload.device_discovered.name = NULL; // patched at drain time -- see pl_bt_drain_events
    event.payload.device_discovered.name_len = name_len;
    event.payload.device_discovered.rssi = rssi;
    event.payload.device_discovered.class_of_device = class_of_device;
    pl_bt_ring_push(event, name, name_len);
}

// --- pico-link-4vb.7 (T3): in-flight connect-target name cache ---
//
// design section 5.3's "Why the name rides on Connect": PlConnectPayload now
// carries name/name_len, but the record is only actually written later, at
// a2dp.c's A2DP_SUBEVENT_STREAM_ESTABLISHED (pl_persist_save_device_now) --
// by then all C has is the addr. So PL_COMMAND_TAG_CONNECT's handler below
// (and pl_bt_debug_connect, the PL_DEBUG_REMOTE bypass, which has no name
// and caches name_len = 0) stash the name here; persist.c reads it back via
// pl_bt_get_connect_target_name.
//
// Contexts: written from thread context only (both callers -- the CONNECT
// command handler and pl_bt_debug_connect -- run from the main.c superloop).
// Read from the cyw43/BTstack background async_context (persist.c's
// pl_persist_save_device_now, which a2dp.c calls from its own IRQ-context
// packet handler). In practice these never overlap in time (the write
// happens when the user picks a device; the read happens only after BTstack
// has actually established a stream for that same connect attempt, well
// after), but this struct is >32 bytes and not atomically readable/writable
// as a unit, so both sides still take the same short critical-section
// approach as bt.c's other cross-context statics (pl_bt_pending_push,
// pl_persist_request_save_device) rather than relying on that timing.
static uint8_t s_connect_target_addr[6];
static uint8_t s_connect_target_name[32];
static uint8_t s_connect_target_name_len;
static bool s_connect_target_valid;

static void pl_bt_set_connect_target(const uint8_t addr[6], const uint8_t *name, uint8_t name_len) {
    uint32_t irq_state = save_and_disable_interrupts();
    memcpy(s_connect_target_addr, addr, 6);
    if (name != NULL && name_len > 0) {
        uint8_t copy_len = name_len > (uint8_t)sizeof(s_connect_target_name) ? (uint8_t)sizeof(s_connect_target_name) : name_len;
        memcpy(s_connect_target_name, name, copy_len);
        s_connect_target_name_len = copy_len;
    } else {
        s_connect_target_name_len = 0;
    }
    s_connect_target_valid = true;
    restore_interrupts(irq_state);
}

void pl_bt_get_connect_target_name(const uint8_t addr[6], uint8_t out_name[32], uint8_t *out_name_len) {
    uint32_t irq_state = save_and_disable_interrupts();
    if (s_connect_target_valid && memcmp(s_connect_target_addr, addr, 6) == 0) {
        memcpy(out_name, s_connect_target_name, sizeof(s_connect_target_name));
        *out_name_len = s_connect_target_name_len;
    } else {
        *out_name_len = 0;
    }
    restore_interrupts(irq_state);
}

// --- M4 S1 additions (bead pico-link-cz0.5.2): exported so a2dp.c can push
// through this same ring -- see bt.h's doc comment on why that's the right
// seam (a2dp.c's A2DP/AVRCP packet handler is another IRQ-context producer,
// same MPSC shape pico-link-6o2 already built this ring to handle).

void pl_bt_push_link_state_connected(void) {
    pl_bt_push_link_state(PL_LINK_STATE_CONNECTED);
}

// Bead pico-link-4vb.5: the disconnected counterpart above -- see bt.h's
// doc comment on this declaration for why SIGNALING_CONNECTION_RELEASED
// is the right call site.
void pl_bt_push_link_state_disconnected(void) {
    pl_bt_push_link_state(PL_LINK_STATE_IDLE);
}

void pl_bt_push_connect_step(uint32_t step) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_CONNECT_STEP_CHANGED,
        .payload = {.connect_step_changed = {.step = step}},
    };
    pl_bt_ring_push(event, NULL, 0);
}

void pl_bt_push_connect_succeeded(const uint8_t *addr, bool degraded) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_CONNECT_SUCCEEDED,
        .payload = {.connect_succeeded = {.degraded = degraded ? 1 : 0}},
    };
    memcpy(event.payload.connect_succeeded.addr, addr, 6);
    pl_bt_ring_push(event, NULL, 0);
    // Bead pico-link-sfw6, design sec 2: this and pl_bt_push_connect_failed
    // below are the only two terminal-outcome call sites a2dp.c has for a
    // connect attempt (rejected/timeout/refused/no-sink/success) -- hooking
    // both here covers every switch-target outcome with no a2dp.c edits.
    // Only PAGING matters: a plain connect-from-idle (state NONE) has
    // nothing to end here.
    if (s_switch_state == PL_BT_SWITCH_PAGING) {
        s_switch_state = PL_BT_SWITCH_NONE;
        pl_bt_update_scan_mode();
    }
}

void pl_bt_push_connect_failed(const uint8_t *addr, uint32_t reason) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_CONNECT_FAILED,
        .payload = {.connect_failed = {.reason = reason}},
    };
    memcpy(event.payload.connect_failed.addr, addr, 6);
    pl_bt_ring_push(event, NULL, 0);
    // Bead pico-link-sfw6: see pl_bt_push_connect_succeeded's doc comment
    // above -- same hook, same reasoning. Covers every B-failure path
    // (a2dp.c:3168/3287/3503/3579/3644) with no a2dp.c edits. The
    // WAIT_ACL_DOWN deadline-timeout path (pl_bt_switch_service) clears
    // state and updates scan mode itself instead, since it's not PAGING
    // when it fires.
    if (s_switch_state == PL_BT_SWITCH_PAGING) {
        s_switch_state = PL_BT_SWITCH_NONE;
        pl_bt_update_scan_mode();
    }
}

// Bead pico-link-1v5: pushes Event::CodecChanged. `name`'s bytes are
// copied by value into the PlEvent's own fixed-size buffer right here
// (never stored as a pointer), so -- unlike
// pl_bt_push_device_discovered -- pl_bt_ring_push's separate deferred
// name-buffer path is not needed; NULL/0 is passed for that parameter.
void pl_bt_push_codec_changed(const uint8_t *addr, const char *name, uint8_t name_len, uint32_t nominal_bitrate_bps) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_CODEC_CHANGED,
        .payload = {.codec_changed = {.name_len = name_len, .nominal_bitrate_bps = nominal_bitrate_bps}},
    };
    memcpy(event.payload.codec_changed.addr, addr, 6);
    if (name_len > sizeof(event.payload.codec_changed.name)) {
        name_len = (uint8_t)sizeof(event.payload.codec_changed.name);
        event.payload.codec_changed.name_len = name_len;
    }
    memcpy(event.payload.codec_changed.name, name, name_len);
    pl_bt_ring_push(event, NULL, 0);
}

// Bead pico-link-4v2.5 (VT5), design section 7: pushes Event::VolumeChanged.
// Called from volume.c's apply_and_propagate, thread context, only when
// `emit` is true (the two real host/sink edges -- never T2's debug-console
// "VOL SET" path, whose PL_VOLUME_SOURCE_CONSOLE has no representable
// value in this event, design section 7). Unlike Event::LevelsChanged
// (deleted from this ring by pico-link-nli.5, G4, because a continuous
// ~4Hz sample is not "an event"), a volume change really is discrete and
// rare -- the ring's drop-newest-on-full policy is the right one here, so
// this stays on the ring rather than growing a second seqlock. `source` is
// volume.h's PlVolumeSource raw value (0=host/1=sink/2=device), matching
// this crate's PlVolumeSource discriminants exactly.
void pl_bt_push_volume_changed(uint8_t level, bool muted, uint8_t source) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_VOLUME_CHANGED,
        .payload = {.volume_changed = {.level = level, .muted = (uint8_t)(muted ? 1 : 0), .source = source}},
    };
    pl_bt_ring_push(event, NULL, 0);
}

// Bead pico-link-du0 (design section 21 E17/C8) originally put
// Event::LevelsChanged through this ring. Deleted by pico-link-nli.5 (G4):
// a level is not an event, and this ring's drop-newest-on-full policy is
// exactly wrong for a value where only the freshest reading matters. See
// a2dp.c's s_level_snapshot doc comment for the seqlock that replaced it
// and pl_a2dp_poll_levels for the thread-context pl_ui_push_event call
// that now does what this function used to.

// Bead pico-link-4vb.2 (bug 3): pushes Event::WizardAutoDismiss -- no
// payload, same shape as pl_bt_push_devices_cleared above. Called from
// a2dp.c's wizard-dismiss timer handler (IRQ-context producer, same ring).
void pl_bt_push_wizard_auto_dismiss(void) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_WIZARD_AUTO_DISMISS,
        .payload = {0},
    };
    pl_bt_ring_push(event, NULL, 0);
}

// Bead pico-link-4vb.7 (T3), reshaping bead pico-link-cz0.6's original:
// pushes Event::StoreLoaded{status, count}. Called exactly once, from
// pl_bt_init below (thread context -- main() calls it directly, before the
// superloop even starts), right after pl_persist_init() has read the flash
// store AND after `count` x PairedDeviceUpserted have already been pushed
// for every surviving record (this call is the TERMINATOR of that
// sequence, design section 5.2) -- see pl_bt_init's BTSTACK_EVENT_STATE
// case below. `status` is the raw wire value of ui-ffi's PlStoreStatus.
// No longer carries a device address: the auto-reconnect POLICY (which
// device, if any, to reconnect to) now lives entirely in `core`, computed
// from the `paired` list this event's preceding PairedDeviceUpserted
// pushes just built (design point 7 / section 5.2) -- C only loads, stages
// and (later) flushes, never decides.
static void pl_bt_push_store_loaded(uint32_t status, uint8_t count) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_STORE_LOADED,
        .payload = {.store_loaded = {.status = (uint8_t)status, .count = count}},
    };
    pl_bt_ring_push(event, NULL, 0);
}

// Bead pico-link-4vb.7 (T3), design section 5.1: pushes
// Event::PairedDeviceUpserted{addr, name, name_len, mru_seq}. See bt.h's
// doc comment for the two call sites (persist.c's write path, and this
// file's own boot sequence).
void pl_bt_push_paired_device_upserted(const uint8_t addr[6], const uint8_t name[32], uint8_t name_len, uint32_t mru_seq, uint8_t ldac_quality, uint16_t preset_id) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_PAIRED_DEVICE_UPSERTED,
        .payload = {.paired_device_upserted = {.name_len = name_len, .mru_seq = mru_seq, .ldac_quality = ldac_quality, .preset_id = preset_id}},
    };
    memcpy(event.payload.paired_device_upserted.addr, addr, 6);
    memcpy(event.payload.paired_device_upserted.name, name, sizeof(event.payload.paired_device_upserted.name));
    pl_bt_ring_push(event, NULL, 0);
}

// Bead pico-link-4vb.7 (T3), design section 5.1: pushes
// Event::PairedDeviceForgotten{addr}.
void pl_bt_push_paired_device_forgotten(const uint8_t addr[6]) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_PAIRED_DEVICE_FORGOTTEN,
        .payload = {.paired_device_forgotten = {0}},
    };
    memcpy(event.payload.paired_device_forgotten.addr, addr, 6);
    pl_bt_ring_push(event, NULL, 0);
}

// Bead pico-link-4vb.7 (T3), design section 5.1: pushes
// Event::PairedStoreFull -- no payload, like DevicesCleared/WizardAutoDismiss.
void pl_bt_push_paired_store_full(void) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_PAIRED_STORE_FULL,
        .payload = {0},
    };
    pl_bt_ring_push(event, NULL, 0);
}

// Bead pico-link-ryw.6, design sec 2.2/3.2: pushes Event::PresetLoaded{id,
// blob}. `blob`'s bytes are copied by value into the PlEvent's own fixed
// buffer right here, same convention as pl_bt_push_codec_changed's `name`
// -- `blob_len` is clamped to the destination buffer's size before the
// copy, defensively (persist.c's own PL_PERSIST_PRESET_BLOB_LEN already
// matches PL_DSP_PRESET_BLOB_LEN, so this should never actually clamp).
void pl_bt_push_preset_loaded(uint16_t id, uint8_t blob_len, const uint8_t *blob) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_PRESET_LOADED,
        .payload = {.preset_loaded = {.id = id, .blob_len = blob_len}},
    };
    if (blob_len > sizeof(event.payload.preset_loaded.blob)) {
        blob_len = (uint8_t)sizeof(event.payload.preset_loaded.blob);
        event.payload.preset_loaded.blob_len = blob_len;
    }
    memcpy(event.payload.preset_loaded.blob, blob, blob_len);
    pl_bt_ring_push(event, NULL, 0);
}

// Bead pico-link-ryw.6, design sec 2.4/3.2: pushes Event::PresetDeleted{id}.
void pl_bt_push_preset_deleted(uint16_t id) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_PRESET_DELETED,
        .payload = {.preset_deleted = {.id = id}},
    };
    pl_bt_ring_push(event, NULL, 0);
}

// Bead pico-link-ryw.6, design sec 2.2: pushes
// Event::PresetStoreLoaded{count, status, next_id} -- the terminator of
// bt.c's boot-time PresetLoaded push sequence, same shape
// pl_bt_push_store_loaded is for PairedDeviceUpserted's boot sequence.
//
// `next_id` added by bead pico-link-ryw.14, Ada's preset-id-allocation
// contract ([`PL_EVENT_ABI_VERSION`] 6 -> 7): C's own preset-id
// high-water mark -- `core` now allocates every id itself and must be
// seeded with this before it can safely allocate anything, or a fresh id
// could alias one C already holds for a deleted-then-reused slot.
void pl_bt_push_preset_store_loaded(uint32_t status, uint16_t count, uint16_t next_id) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_PRESET_STORE_LOADED,
        .payload = {.preset_store_loaded = {.count = count, .status = (uint8_t)status, .next_id = next_id}},
    };
    pl_bt_ring_push(event, NULL, 0);
}

// --- HCI Read Local Version Information: the acceptance-criterion probe ---
//
// Fires once, the first time BTSTACK_EVENT_STATE reports HCI_STATE_WORKING
// -- i.e. once BTstack itself considers the radio fully brought up. Prints
// both the raw return-parameter bytes and the decoded fields, per the
// bead's "raw bytes AND decode so the evidence is auditable" acceptance
// criterion.
static void pl_bt_handle_read_local_version_complete(const uint8_t *params, uint16_t params_len) {
    pl_log("BT: HCI Read Local Version Information, raw bytes:");
    for (uint16_t i = 0; i < params_len; i++) {
        pl_log(" %02x", params[i]);
    }
    pl_log("\r\n");

    // Standard HCI Command Complete return parameters for this command:
    // Status(1) HCI_Version(1) HCI_Revision(2,LE) LMP_Version(1)
    // Manufacturer_Name(2,LE) LMP_Subversion(2,LE) -- 9 bytes total.
    if (params_len < 9) {
        pl_log("BT: Read Local Version response too short to decode (%u bytes)\r\n", params_len);
        return;
    }
    uint8_t status = params[0];
    uint8_t hci_version = params[1];
    uint16_t hci_revision = (uint16_t)(params[2] | (params[3] << 8));
    uint8_t lmp_version = params[4];
    uint16_t manufacturer = (uint16_t)(params[5] | (params[6] << 8));
    uint16_t lmp_subversion = (uint16_t)(params[7] | (params[8] << 8));
    pl_log(
        "BT: decoded -- status=0x%02x hci_version=0x%02x hci_revision=0x%04x "
        "lmp_version=0x%02x manufacturer=0x%04x lmp_subversion=0x%04x\r\n",
        status, hci_version, hci_revision, lmp_version, manufacturer, lmp_subversion
    );
}

// Does the actual radio work for a scan start -- BTstack API calls only, no
// UI push. Safe to call from any context BTstack itself considers "the run
// loop context" (see the pending-queue doc comment below): today that's
// pl_bt_packet_handler's BTSTACK_EVENT_STATE case (IRQ context, unchanged)
// and, as of pico-link-ouw, pl_bt_pending_service (also IRQ context, via the
// heartbeat timer).
static void pl_bt_start_scan_radio(void) {
    pl_log("BT: starting GAP inquiry (%d.%ds)\r\n", (PL_INQUIRY_DURATION_UNITS * 128) / 100, (PL_INQUIRY_DURATION_UNITS * 128) % 100);
    gap_inquiry_start(PL_INQUIRY_DURATION_UNITS);
}

// Used only from pl_bt_packet_handler (IRQ context) where pushing the UI
// state and starting the radio in one atomic-looking call was always safe --
// unlike the thread-context caller in pl_bt_poll_commands, which as of
// pico-link-ouw pushes the UI state itself and defers only the radio half
// (see the pending-queue section below).
static void pl_bt_start_scan(void) {
    pl_bt_push_devices_cleared();
    pl_bt_push_discovery_state(PL_DISCOVERY_STATE_SCANNING);
    pl_bt_start_scan_radio();
}

// Does the actual radio work for a scan cancel -- see pl_bt_start_scan_radio's
// doc comment; same split for the same reason (pico-link-ouw).
//
// pico-link-znb.2 (E1, MVP-blocking): stops an in-flight GAP inquiry.
// gap_inquiry_stop() itself triggers GAP_EVENT_INQUIRY_COMPLETE (same as a
// natural timeout), so pl_bt_packet_handler's existing
// GAP_EVENT_INQUIRY_COMPLETE case pushes PL_DISCOVERY_STATE_IDLE (bead
// pico-link-88xs -- was PL_LINK_STATE_IDLE; inquiry-complete no longer
// touches the link axis at all) -- no separate push needed here. Compiled
// but its runtime effect is UNVERIFIED (board is wedged, see pico-link-icb;
// this bead may not block on hardware).
static void pl_bt_cancel_scan_radio(void) {
    pl_log("BT: cancelling GAP inquiry\r\n");
    gap_inquiry_stop();
}

static void pl_bt_handle_inquiry_result(const uint8_t *packet) {
    bd_addr_t addr;
    gap_event_inquiry_result_get_bd_addr(packet, addr);

    int8_t rssi = 0;
    if (gap_event_inquiry_result_get_rssi_available(packet)) {
        rssi = (int8_t)gap_event_inquiry_result_get_rssi(packet);
    }

    // 240 bytes is BTstack's own example's buffer size for this (see
    // gap_inquiry.c) -- the EIR name field cannot exceed that.
    char name_buf[240];
    uint16_t name_len = 0;
    if (gap_event_inquiry_result_get_name_available(packet)) {
        name_len = gap_event_inquiry_result_get_name_len(packet);
        if (name_len > sizeof(name_buf)) {
            name_len = sizeof(name_buf);
        }
        memcpy(name_buf, gap_event_inquiry_result_get_name(packet), name_len);
    }

    uint32_t class_of_device = gap_event_inquiry_result_get_class_of_device(packet);

    pl_log(
        "BT: inquiry result %02x:%02x:%02x:%02x:%02x:%02x rssi=%d cod=0x%06lx name=\"%.*s\"\r\n",
        addr[0], addr[1], addr[2], addr[3], addr[4], addr[5], rssi, (unsigned long)class_of_device, (int)name_len, name_buf
    );
    pl_bt_push_device_discovered(addr, (const uint8_t *)name_buf, name_len, rssi, class_of_device);
}

// --- pico-link-oevr: page scan / inquiry scan ownership ---
//
// Design (Ada, bead pico-link-oevr DESIGN comment, Q1/Q3/Q4/Q5): the sole
// owner of gap_connectable_control/gap_discoverable_control in this tree.
// connectable = "no ACL up" -- with MAX_NR_HCI_CONNECTIONS 1, any page
// received while an ACL already exists is declined anyway (hci.
// c:3787-3820, 0x0d limited resources), so page scan then is pure air-time
// cost with zero function; ACL gating strictly dominates gating on the
// A2DP stream (fewer transitions, no race against AVDTP signaling, covers
// the 0x0b stale-ACL retry path). discoverable is always off -- a source
// has no reason to be found by inquiry, and the pairing wizard is
// outbound (gap_inquiry_start / paging), needing neither scan.
//
// Recomputes "any ACL up" from BTstack's own connection list on every
// call rather than tracking a local bool, so this can never drift from
// ground truth (Q4) -- cheap, since MAX_NR_HCI_CONNECTIONS is 1.
//
// Requires ENABLE_EXPLICIT_CONNECTABLE_MODE_CONTROL (btstack_config.h):
// without it, l2cap_register_service silently re-enables page scan behind
// this function's back the first time anything registers a service at
// runtime (l2cap.c:5023-5026) -- this is the load-bearing #define that
// makes this function's ownership real rather than accidental.
//
// Measured on hardware (bead pico-link-oevr verification round): BTstack
// allocates an hci_connection_t -- and gap_get_connection_type on its
// handle already reports GAP_CONNECTION_ACL -- the moment an OUTBOUND
// connection attempt starts (hci.c's SEND_CREATE_CONNECTION/
// SENT_CREATE_CONNECTION states), long before HCI_EVENT_CONNECTION_COMPLETE
// fires, and it stays in the list through teardown
// (SEND_DISCONNECT/SENT_DISCONNECT/RECEIVED_DISCONNECTION_COMPLETE) --
// hci.c:4707-4718 emits HCI_EVENT_DISCONNECTION_COMPLETE to this file's
// packet handler BEFORE hci_shutdown_connection() frees it. A plain
// "does an ACL-type hci_connection_t exist" check therefore reports ACL-up
// during an in-flight outbound connect (this file never calls
// pl_bt_update_scan_mode during that window, so it doesn't affect the
// actual gating decision -- but it WOULD lie to the pl_bt_scan_connectable
// report below) and, more seriously, would still read true at the exact
// moment the DISCONNECTION_COMPLETE handler calls
// pl_bt_update_scan_mode() to turn scan back on. Only CONNECTION_STATE
// OPEN means an ACL that is actually established and would draw the
// 0x0d page decline this whole design leans on -- check state, not just
// presence.
static uint32_t s_scan_mode_changes;

// Bead pico-link-pigd: count of inbound classic connection requests refused
// by pl_bt_connection_filter (defined below, after pl_bt_scan_mode_changes).
static uint32_t s_rejected_inbound_count;

static bool pl_bt_any_acl_up(void) {
    btstack_linked_list_iterator_t it;
    hci_connections_get_iterator(&it);
    while (btstack_linked_list_iterator_has_next(&it)) {
        hci_connection_t *connection = (hci_connection_t *)btstack_linked_list_iterator_next(&it);
        if (connection->state == OPEN && gap_get_connection_type(connection->con_handle) == GAP_CONNECTION_ACL) {
            return true;
        }
    }
    return false;
}

// Bead pico-link-sfw6, design sec 2 + sec 6: the full-ACL teardown the
// switch needs. pl_a2dp_disconnect (a2dp.c) is AVDTP-only -- it closes the
// AVDTP signaling channel but leaves AVRCP's channel up, which keeps the
// ACL (and the HCI slot) alive. gap_disconnect on an OPEN ACL instead tears
// down the whole ACL in one call; L2CAP then closes every channel riding on
// it (AVDTP media + signaling, AVRCP), so the existing STREAM_RELEASED/
// SIGNALING_CONNECTION_RELEASED handlers in a2dp.c already do the rest --
// core1 quiesce, tx flush, pl_pcm_reset, a2dp_cid cleared, Idle pushed --
// with no new code there. Same iterator as pl_bt_any_acl_up above; with
// MAX_NR_HCI_CONNECTIONS == 1 this loop only ever finds at most one ACL,
// but stays correct if that pool size ever changes. Shared by the switch
// (pl_bt_connect_or_switch below) and by PL_BT_PENDING_DISCONNECT's
// servicing (pl_bt_pending_service) -- see that case's comment for why
// user Disconnect needed this fold-in too.
static void pl_bt_disconnect_all_open_acl(void) {
    btstack_linked_list_iterator_t it;
    hci_connections_get_iterator(&it);
    while (btstack_linked_list_iterator_has_next(&it)) {
        hci_connection_t *connection = (hci_connection_t *)btstack_linked_list_iterator_next(&it);
        if (connection->state == OPEN && gap_get_connection_type(connection->con_handle) == GAP_CONNECTION_ACL) {
            uint8_t status = gap_disconnect(connection->con_handle);
            pl_log("BT: gap_disconnect handle=0x%04x status=0x%02x\r\n", connection->con_handle, status);
        }
    }
}

// Sets connectable = !any_acl_up() && switch.state == NONE, discoverable =
// always off. Idempotent (gap_connectable_control/gap_discoverable_control
// no-op on an unchanged value, hci.c:5824) -- safe to call redundantly from
// every call site below. Logs and counts only on an edge, not every call,
// so the periodic debug report's scan_mode_changes counter reflects real
// transitions.
//
// Bead pico-link-sfw6, design sec 4 "Scan-mode owner": the switch.state
// term closes the one window any_acl_up() alone leaves open -- A's ACL
// drops (any_acl_up() -> false) while B is still being paged, and without
// this term scan would flip connectable=1 for the ~100ms-5s gap in
// between, letting A re-page us and steal the pool-of-one slot out from
// under B's own page. This function stays the ONLY caller of
// gap_connectable_control (ENABLE_EXPLICIT_CONNECTABLE_MODE_CONTROL,
// btstack_config.h, is untouched -- no new call site).
static void pl_bt_update_scan_mode(void) {
    static bool s_last_connectable = true;
    static bool s_have_last = false;

    bool connectable = !pl_bt_any_acl_up() && s_switch_state == PL_BT_SWITCH_NONE;
    if (!s_have_last || connectable != s_last_connectable) {
        pl_log("BT: scan mode connectable=%d discoverable=0\r\n", (int)connectable);
        s_scan_mode_changes++;
        s_last_connectable = connectable;
        s_have_last = true;
    }
    gap_connectable_control(connectable ? 1 : 0);
    gap_discoverable_control(0);
}

// Bead pico-link-oevr, hardware verification (Tess): exposes current scan
// state and transition count for the periodic debug report (a2dp.c's
// pl_a2dp_report), so a hardware round can read scan state instead of
// inferring it. Bead pico-link-sfw6: mirrors pl_bt_update_scan_mode's
// switch.state gate exactly, so this report can never claim connectable
// during a switch's disconnect/paging window.
bool pl_bt_scan_connectable(void) {
    return !pl_bt_any_acl_up() && s_switch_state == PL_BT_SWITCH_NONE;
}

uint32_t pl_bt_scan_mode_changes(void) {
    return s_scan_mode_changes;
}

// --- pico-link-pigd: refuse inbound classic connections from unpaired devices ---
//
// Follow-up to pico-link-oevr's Q2 (Ada's DESIGN comment there). We are an
// A2DP SOURCE: the only legitimate inbound classic connection is
// already-paired headphones re-paging us after a link loss (a2dp.c's
// 0x0b-retry path documents this). Everything else that pages us -- a
// stranger, or the Mac that paired with us on 2026-09-24 while we were
// briefly discoverable (pico-link-oevr's Q3 already turns discoverable off
// permanently, but an address that already knows us doesn't need inquiry to
// find us again) -- should never get past the connection request.
//
// gap_register_classic_connection_filter's callback (hci.c:3798-3803) fires
// synchronously from HCI_EVENT_CONNECTION_REQUEST handling, BEFORE
// create_connection_for_bd_addr_and_type allocates an hci_connection_t and
// before any SSP/pairing exchange starts. Returning 0 makes hci.c decline
// with ERROR_CODE_CONNECTION_REJECTED_DUE_TO_SECURITY_REASONS and `return`
// out of the case (packet[11] link_type covers both HCI_LINK_TYPE_ACL and
// SCO/eSCO -- an SCO request only ever follows an ACL from the same address,
// which this same filter already vetted, so applying the same known-device
// check to every link_type is correct, not just harmless). This is THE ONLY
// call site gap_classic_accept_callback has in vendored BTstack (grepped
// hci.c) -- it cannot fire for anything else:
//   - outbound connects (pl_bt_connect, the pairing wizard's paging): this
//     device is the one calling hci_send_cmd(&hci_create_connection), never
//     the one receiving CONNECTION_REQUEST.
//   - GAP_EVENT_INQUIRY_RESULT / inquiry: a completely different HCI event,
//     handled by pl_bt_handle_inquiry_result above; this filter never runs
//     for it.
//   - the pairing wizard in general: pairing with a FRESH device is always
//     us paging out (see pico-link-oevr's Q2 note "Pairing wizard:
//     unaffected. Pairing is outbound").
//
// "Known" is decided against BOTH our persisted device store (persist.c,
// the source of truth for what pico-link's UI calls "paired" -- Settings'
// Forget action removes a device from here) AND BTstack's own link key DB.
// Requiring both, not either, closes the one disagreement that matters:
// pl_persist_forget_device already deletes the BTstack link key in the same
// call (persist.c:976-982, "forgetting removes the link key too"), so in
// steady state the two never disagree -- but if they ever did (a forgotten
// device whose link key somehow survived in the TLV, or a link key that was
// never actually completed for a record we still hold), accepting the
// re-page would mean re-pairing (or resuming a session) with a device our
// own UI says we don't trust, which is exactly the hole this bead exists to
// close. So either side saying "not known" rejects -- there is no
// legitimate case where a device should connect that our persisted store
// doesn't also recognise.
static int pl_bt_connection_filter(bd_addr_t addr, hci_link_type_t link_type) {
    uint8_t codec_id, ldac_quality;
    bool known_to_persist = pl_persist_get_device_settings(addr, &codec_id, &ldac_quality);

    link_key_t link_key;
    link_key_type_t link_key_type;
    bool has_link_key = gap_get_link_key_for_bd_addr(addr, link_key, &link_key_type);

    if (known_to_persist && has_link_key) {
        return 1;
    }

    s_rejected_inbound_count++;
    pl_log(
        "BT: refused inbound connection from %02x:%02x:%02x:%02x:%02x:%02x link_type=%u -- "
        "known_to_persist=%d has_link_key=%d (not a paired device)\r\n",
        addr[0], addr[1], addr[2], addr[3], addr[4], addr[5], (unsigned)link_type, (int)known_to_persist,
        (int)has_link_key
    );
    return 0;
}

uint32_t pl_bt_rejected_inbound_count(void) {
    return s_rejected_inbound_count;
}

static void pl_bt_packet_handler(uint8_t packet_type, uint16_t channel, uint8_t *packet, uint16_t size) {
    (void)channel;
    (void)size;

    if (packet_type != HCI_EVENT_PACKET) {
        return;
    }

    uint8_t event = hci_event_packet_get_type(packet);
    switch (event) {
        case BTSTACK_EVENT_STATE:
            if (btstack_event_state_get_state(packet) == HCI_STATE_WORKING) {
                pl_log("BT: HCI_STATE_WORKING -- radio up\r\n");
                hci_send_cmd(&hci_read_local_version_information);

                // Bead pico-link-4vb.7 (T3), design section 5.2: NOW it's
                // safe to push the boot-loaded device records (radio is
                // fully up) -- pushed before pl_bt_start_scan() below so
                // core has folded every record into its `paired` list, and
                // computed its own MRU-max auto-reconnect target, before
                // any scan-driven UI change lands. One PairedDeviceUpserted
                // per surviving record, THEN StoreLoaded{status, count} as
                // the terminator -- core no longer needs an address on
                // StoreLoaded itself (that was C deciding the reconnect
                // target; now core does, from the `paired` list this
                // sequence just built). Reads straight from persist.c's
                // boot-time snapshot -- no re-read of flash,
                // pl_persist_init() already did that in pl_bt_init above.
                uint8_t boot_device_count = pl_persist_boot_device_count();
                for (uint8_t i = 0; i < boot_device_count; i++) {
                    uint8_t boot_addr[6];
                    uint8_t boot_name[32];
                    uint8_t boot_name_len;
                    uint32_t boot_mru_seq;
                    uint8_t boot_ldac_quality;
                    uint16_t boot_preset_id;
                    pl_persist_boot_device_at(i, boot_addr, boot_name, &boot_name_len, &boot_mru_seq, &boot_ldac_quality, &boot_preset_id);
                    pl_bt_push_paired_device_upserted(boot_addr, boot_name, boot_name_len, boot_mru_seq, boot_ldac_quality, boot_preset_id);
                }
                pl_bt_push_store_loaded((uint32_t)pl_persist_boot_status(), boot_device_count);

                // Bead pico-link-oevr: no ACL up yet at this point (the
                // radio has only just come up), so this reasserts
                // connectable=1/discoverable=0 -- same state pl_bt_init
                // already set below, before hci_power_control, but
                // restated here per the design's explicit call-site list
                // (Q5 step 3) in case anything reset it between init and
                // HCI_STATE_WORKING.
                pl_bt_update_scan_mode();

                pl_bt_start_scan();
            }
            break;

        case HCI_EVENT_COMMAND_COMPLETE:
            if (hci_event_command_complete_get_command_opcode(packet) == hci_read_local_version_information.opcode) {
                const uint8_t *params = hci_event_command_complete_get_return_parameters(packet);
                // Command Complete's return-parameters length is the whole
                // event minus the 3-byte HCI event header (event
                // code, param total length, num_hci_command_packets) and
                // the 2-byte opcode this return-parameters block sits
                // after -- i.e. size - 5, matching
                // hci_event_command_complete_get_return_parameters'\''s own
                // pointer arithmetic (event + 5).
                pl_bt_handle_read_local_version_complete(params, (uint16_t)(size - 5));
            }
            break;

        case GAP_EVENT_INQUIRY_RESULT:
            pl_bt_handle_inquiry_result(packet);
            break;

        case GAP_EVENT_INQUIRY_COMPLETE:
            pl_log("BT: inquiry complete\r\n");
            // Bead pico-link-88xs: was pl_bt_push_link_state(PL_LINK_STATE_IDLE),
            // which wiped a live, connected model's codec/addr/level/bitrate
            // whenever an inquiry ended while the A2DP link was still up
            // (design section 1). Inquiry-complete has no opinion about the
            // A2DP link and now has no way to express one.
            pl_bt_push_discovery_state(PL_DISCOVERY_STATE_IDLE);
            break;

        // Bead pico-link-648 diagnostic: raw HCI_EVENT_CONNECTION_COMPLETE
        // bytes, to answer -- with a measurement, not an assumption --
        // whether the CYW43439 controller populates a usable connection
        // handle in the Connection_Complete event when status is
        // ERROR_CODE_ACL_CONNECTION_ALREADY_EXISTS (0x0b). hci.c's own
        // internal handler (hci.c:3837-3881) only reads the handle field
        // inside the `status == 0` branch and calls
        // hci_handle_connection_failed() (which frees the local
        // hci_connection_t with no further use of any handle) on any
        // other status -- so this is the only way to see what the
        // controller actually sent on the failure path. Wire format:
        // Status(1) Handle(2,LE) BD_ADDR(6) Link_Type(1) Encryption(1),
        // starting at packet[2] -- Handle before BD_ADDR (see BTstack's own
        // hci_event_connection_complete_get_connection_handle/_get_bd_addr
        // accessors in btstack_event.h, offsets 3 and 5 respectively). The
        // code below already reads it in this order; this comment previously
        // stated BD_ADDR before Handle.
        case HCI_EVENT_CONNECTION_COMPLETE: {
            uint8_t status = packet[2];
            bd_addr_t addr;
            reverse_bd_addr(&packet[5], addr);
            uint16_t handle = little_endian_read_16(packet, 3);
            pl_log(
                "BT: HCI_EVENT_CONNECTION_COMPLETE status=0x%02x addr=%02x:%02x:%02x:%02x:%02x:%02x handle=0x%04x "
                "link_type=%u encryption=%u\r\n",
                status, addr[0], addr[1], addr[2], addr[3], addr[4], addr[5], handle, packet[11], packet[12]
            );
            // Bead pico-link-oevr, Q4: a failed CONNECTION_COMPLETE
            // (status != 0, e.g. 0x0b ACL_CONNECTION_ALREADY_EXISTS) must
            // not turn scan off -- no ACL actually came up.
            if (status == 0) {
                pl_bt_update_scan_mode();
            }
            break;
        }

        // Bead pico-link-oevr, Q1/Q5: the other half of the ACL-up edge --
        // an ACL going away (headset power-off, out of range, link loss)
        // must turn page scan back on so the headset (or a fresh pairing)
        // can page us again. pl_bt_any_acl_up() recomputes from BTstack's
        // own connection list rather than trusting this event's handle, so
        // this is correct even for a handle this file never logged a
        // CONNECTION_COMPLETE for.
        case HCI_EVENT_DISCONNECTION_COMPLETE: {
            uint8_t status = hci_event_disconnection_complete_get_status(packet);
            uint16_t handle = hci_event_disconnection_complete_get_connection_handle(packet);
            uint8_t reason = hci_event_disconnection_complete_get_reason(packet);
            pl_log(
                "BT: HCI_EVENT_DISCONNECTION_COMPLETE status=0x%02x handle=0x%04x reason=0x%02x\r\n", status, handle,
                reason
            );
            pl_bt_update_scan_mode();
            break;
        }

        default:
            break;
    }
}

// --- pico-link-ouw: defer thread-context BTstack calls onto the run loop ---
//
// pl_bt_start_scan_radio (gap_inquiry_start), pl_bt_cancel_scan_radio
// (gap_inquiry_stop) and pl_a2dp_connect (a2dp_source_establish_stream) are
// BTstack API calls. BTstack's own contract is that its API is called only
// from "the run loop" -- in this build that is the cyw43/BTstack background
// IRQ under pico_cyw43_arch_threadsafe_background (firmware/CMakeLists.txt),
// which is exactly where pl_bt_packet_handler and pl_bt_wdt_heartbeat_handler
// already run. Before this bead, pl_bt_poll_commands (thread context, the
// main.c superloop) called these directly with no synchronization at all --
// not even the async_context lock -- while the background IRQ could run
// concurrently and touch the same BTstack run-loop state. Ada found this
// while designing pico-link-2pq (see
// .planning/design/2026-08-30-cancel-connect.md's "Why the teardown is
// deferred to the heartbeat" section) and filed it separately as this bead.
// It was investigated as a candidate cause of the pico-link-okx stall and
// REFUTED for that role (okx's stall was LAST=LOG_DRAIN, unrelated) -- this
// fix stands on its own as a correctness fix, not a stall fix.
//
// Fix shape, matching pico-link-2pq's D4: never call these from thread
// context. pl_bt_poll_commands (and pl_bt_debug_connect, the PL_DEBUG_REMOTE
// bypass path -- also thread context, see debug_remote.c) do only UI-event
// pushes (already safe -- pl_bt_ring_push serializes both producer contexts)
// and enqueue a small request here; pl_bt_wdt_heartbeat_handler, running in
// the correct IRQ context on its existing 100ms period, drains the queue and
// makes the real BTstack calls. Same MPSC-with-critical-section idiom as
// pl_bt_ring_push above (pico-link-6o2), just carrying commands in the
// opposite direction (thread -> run loop instead of run loop -> thread).
// Capacity 8 is generous against the realistic case (at most one scan/cancel
// and one connect in flight at a time; the UI can't issue more before the
// prior one lands) while still being cheap.
#define PL_BT_PENDING_CAPACITY 8

typedef enum {
    PL_BT_PENDING_START_SCAN,
    PL_BT_PENDING_CANCEL_SCAN,
    PL_BT_PENDING_CONNECT,
    PL_BT_PENDING_DISCONNECT,
    // Bead pico-link-cz0.6 (M5 persistence), code-review finding 1: reuses
    // this exact queue/heartbeat idiom (pico-link-ouw) for persist.c's
    // deferred flash write, rather than inventing a second mechanism --
    // see persist.h's module doc "Reentrancy" section for why the write
    // MUST run from this queue's IRQ-context consumer, never thread
    // context. Carries no addr (persist.c already has the pending record
    // staged in its own s_pending_addr).
    PL_BT_PENDING_PERSIST_WRITE,
    // Bead pico-link-4vb.7 (T3), design section 5.1/6: reuses this exact
    // queue/heartbeat idiom for persist.c's forget-device flash write, for
    // the same reason PL_BT_PENDING_PERSIST_WRITE does --
    // pl_persist_forget_device touches the shared btstack_tlv_flash_bank
    // instance directly and MUST run on the cyw43/BTstack background
    // async_context (persist.h's Reentrancy doc). Carries the target addr,
    // same as PL_BT_PENDING_CONNECT.
    PL_BT_PENDING_FORGET_DEVICE,
    // Bead pico-link-7jol.5, generalized by pico-link-ryw.6: reuses this
    // exact queue/heartbeat idiom for persist.c's field-masked per-device
    // settings write (codec_id/ldac_quality/preset_id) -- same reentrancy
    // reason as PL_BT_PENDING_PERSIST_WRITE/PL_BT_PENDING_FORGET_DEVICE.
    // Carries no addr -- persist.c already has the pending settings staged
    // in its own s_device_settings_pending_addr, same convention as
    // PL_BT_PENDING_PERSIST_WRITE.
    PL_BT_PENDING_SET_DEVICE_SETTINGS,
    // Bead pico-link-ryw.6: reuses this exact queue/heartbeat idiom for
    // persist.c's PL:P preset save/delete drain -- same reentrancy reason
    // as the other PL_BT_PENDING_* persist entries. Carries no payload --
    // persist.c already has the pending operation staged in its own
    // ordered per-id table (bead pico-link-ryw.14 replaced the old
    // single-slot staging vars with that table; both this tag and
    // PL_BT_PENDING_DELETE_PRESET below now drain the SAME table via the
    // same pl_persist_execute_pending_save_preset_write/
    // pl_persist_execute_pending_delete_preset_write functions -- see
    // persist.h's doc comments).
    PL_BT_PENDING_SAVE_PRESET,
    // See PL_BT_PENDING_SAVE_PRESET above -- kept as its own tag for
    // logging clarity, but persist.c only ever enqueues the SAVE_PRESET
    // tag now (pl_persist_service checks one shared "table non-empty"
    // condition); this tag's switch-case handler still correctly drains
    // the table if it's ever reached.
    PL_BT_PENDING_DELETE_PRESET,
    // Bead pico-link-qivj.5 (S11): reuses this exact queue/heartbeat idiom
    // for persist.c's PL:S:0 display-settings write -- same reentrancy
    // reason as the other PL_BT_PENDING_* persist entries. Carries no
    // addr -- persist.c already has the pending record staged in its own
    // s_display_pending_mode/s_display_pending_timeout_s.
    PL_BT_PENDING_SET_DISPLAY_SETTINGS,
    // Bead pico-link-8pp1.4 (S3): reuses this exact queue/heartbeat idiom
    // for persist.c's PL:S:1 cushion-policy write -- same reentrancy
    // reason as the other PL_BT_PENDING_* persist entries. Carries no
    // addr -- persist.c already has the pending record staged in its own
    // s_cushion_pending_policy.
    PL_BT_PENDING_SET_CUSHION_POLICY,
    // Bead pico-link-d42g.3 (F3): reuses this exact queue/heartbeat idiom
    // for persist.c's PL:S:2 Adaptive-floor write -- same reentrancy
    // reason as the other PL_BT_PENDING_* persist entries. Carries no
    // addr -- persist.c already has the pending record staged in its own
    // s_abr_floor_pending_floor.
    PL_BT_PENDING_SET_ABR_FLOOR,
} pl_bt_pending_tag_t;

typedef struct {
    pl_bt_pending_tag_t tag;
    bd_addr_t addr; // meaningful for PL_BT_PENDING_CONNECT and PL_BT_PENDING_FORGET_DEVICE
} pl_bt_pending_entry_t;

static pl_bt_pending_entry_t s_bt_pending[PL_BT_PENDING_CAPACITY];
static volatile uint8_t s_bt_pending_head; // producer-owned (thread context, lock-protected)
static volatile uint8_t s_bt_pending_tail; // consumer-owned (heartbeat/IRQ context only, no lock needed)
// Diagnostics (pico-link-ouw verification): bumped so a hardware log proves
// the deferral actually happens -- an enqueue log line from thread context
// followed by a service log line from IRQ context, with these counters
// distinguishing "queued but not yet serviced" from "dropped, queue full".
static volatile uint32_t s_bt_pending_enqueued_count;
static volatile uint32_t s_bt_pending_serviced_count;
static volatile uint32_t s_bt_pending_drop_count;

static const char *pl_bt_pending_tag_name(pl_bt_pending_tag_t tag) {
    switch (tag) {
        case PL_BT_PENDING_START_SCAN:
            return "START_SCAN";
        case PL_BT_PENDING_CANCEL_SCAN:
            return "CANCEL_SCAN";
        case PL_BT_PENDING_CONNECT:
            return "CONNECT";
        case PL_BT_PENDING_DISCONNECT:
            return "DISCONNECT";
        case PL_BT_PENDING_PERSIST_WRITE:
            return "PERSIST_WRITE";
        case PL_BT_PENDING_FORGET_DEVICE:
            return "FORGET_DEVICE";
        case PL_BT_PENDING_SET_DEVICE_SETTINGS:
            return "SET_DEVICE_SETTINGS";
        case PL_BT_PENDING_SAVE_PRESET:
            return "SAVE_PRESET";
        case PL_BT_PENDING_DELETE_PRESET:
            return "DELETE_PRESET";
        case PL_BT_PENDING_SET_DISPLAY_SETTINGS:
            return "SET_DISPLAY_SETTINGS";
        case PL_BT_PENDING_SET_CUSHION_POLICY:
            return "SET_CUSHION_POLICY";
        case PL_BT_PENDING_SET_ABR_FLOOR:
            return "SET_ABR_FLOOR";
        default:
            return "?";
    }
}

// Enqueues one deferred BTstack call. Called only from thread context
// (pl_bt_poll_commands, pl_bt_debug_connect) -- unlike pl_bt_ring_push this
// queue has exactly one producer context, but it still needs the critical
// section because the consumer (pl_bt_pending_service, IRQ context) can
// preempt the producer mid read-modify-write of s_bt_pending_head.
//
// Bead pico-link-j5su: returns true if the entry was actually queued, false
// if the queue was full and it was dropped. Every pl_bt_enqueue_*_write
// wrapper below propagates this so persist.c can decide whether it's safe
// to latch its own *_write_enqueued flag -- latching on a drop was the bug
// (Ada, pico-link-ryw.14 review): the flag would stay true forever with no
// queued entry left to ever clear it, wedging that write kind until reboot.
static bool pl_bt_pending_push(pl_bt_pending_tag_t tag, const uint8_t *addr) {
    uint32_t irq_state = save_and_disable_interrupts();
    uint8_t head = s_bt_pending_head;
    uint8_t next_head = (uint8_t)((head + 1) % PL_BT_PENDING_CAPACITY);
    if (next_head == s_bt_pending_tail) {
        s_bt_pending_drop_count++;
        restore_interrupts(irq_state);
        pl_log("BT: pending-action queue full, dropped deferred %s\r\n", pl_bt_pending_tag_name(tag));
        return false;
    }
    s_bt_pending[head].tag = tag;
    if (tag == PL_BT_PENDING_CONNECT || tag == PL_BT_PENDING_FORGET_DEVICE) {
        memcpy(s_bt_pending[head].addr, addr, sizeof(bd_addr_t));
    }
    s_bt_pending_head = next_head;
    s_bt_pending_enqueued_count++;
    restore_interrupts(irq_state);
    pl_log(
        "BT: queued deferred %s from thread context (enqueued=%lu)\r\n", pl_bt_pending_tag_name(tag),
        (unsigned long)s_bt_pending_enqueued_count
    );
    return true;
}

// Bead pico-link-sfw6, design sec 2: turns a Connect that arrives while an
// ACL is up into a break-before-make switch. Called from
// pl_bt_pending_service's PL_BT_PENDING_CONNECT case below (run-loop/IRQ
// context) instead of calling pl_a2dp_connect directly -- state, the
// prepare-switch cancels, pl_bt_update_scan_mode and gap_disconnect all
// require that context (pico-link-ouw).
static void pl_bt_connect_or_switch(const uint8_t *addr) {
    if (s_switch_state == PL_BT_SWITCH_WAIT_ACL_DOWN) {
        // A second Connect while still waiting for A's slot to free:
        // last press wins, same attempt otherwise unchanged.
        memcpy(s_switch_target, addr, sizeof(bd_addr_t));
        pl_log(
            "BT: switch target overwritten target=%02x:%02x:%02x:%02x:%02x:%02x\r\n", addr[0], addr[1], addr[2],
            addr[3], addr[4], addr[5]
        );
        return;
    }
    if (pl_bt_any_acl_up()) {
        // No ACL up but state == PAGING (an attempt already in flight, no
        // slot occupied yet) falls through to the plain pl_a2dp_connect
        // below, same as state == NONE -- both keep today's behaviour
        // (today's instant 0x56 failure for the in-flight case).
        memcpy(s_switch_target, addr, sizeof(bd_addr_t));
        s_switch_state = PL_BT_SWITCH_WAIT_ACL_DOWN;
        s_switch_deadline_us = time_us_64() + PL_BT_SWITCH_DEADLINE_US;
        pl_log(
            "BT: switch start target=%02x:%02x:%02x:%02x:%02x:%02x\r\n", addr[0], addr[1], addr[2], addr[3], addr[4],
            addr[5]
        );
        pl_bt_push_connect_step(PL_CONNECT_STEP_DISCONNECTING);
        pl_a2dp_prepare_switch();
        pl_bt_update_scan_mode();
        pl_bt_disconnect_all_open_acl();
        return;
    }
    pl_a2dp_connect(addr);
}

// Bead pico-link-sfw6, design sec 2: the switch state machine's other
// half. Called from pl_bt_wdt_heartbeat_handler right after
// pl_bt_pending_service, same 100ms period and IRQ/run-loop context -- a
// switch that started this same tick is only serviced starting next tick,
// which is harmless (gap_disconnect just fired; the ACL cannot be down
// yet regardless).
static void pl_bt_switch_service(void) {
    if (s_switch_state != PL_BT_SWITCH_WAIT_ACL_DOWN) {
        return;
    }
    if (!pl_bt_any_acl_up() && pl_a2dp_session_idle()) {
        s_switch_state = PL_BT_SWITCH_PAGING;
        pl_log(
            "BT: switch ACL down, paging %02x:%02x:%02x:%02x:%02x:%02x\r\n", s_switch_target[0], s_switch_target[1],
            s_switch_target[2], s_switch_target[3], s_switch_target[4], s_switch_target[5]
        );
        pl_bt_push_connect_step(PL_CONNECT_STEP_CONNECTING);
        pl_a2dp_connect(s_switch_target);
        return;
    }
    if (time_us_64() >= s_switch_deadline_us) {
        pl_log("BT: switch timeout waiting for ACL down\r\n");
        pl_bt_push_connect_failed(s_switch_target, PL_FAILURE_REASON_RADIO_ERROR);
        // Andreas's ruling: B failing leaves the device disconnected, no
        // reconnect-A fallback. state = NONE here (not PAGING) is exactly
        // why pl_bt_push_connect_failed's own hook doesn't already do this
        // -- a late DISCONNECTION_COMPLETE for A must only ever produce
        // Idle from here on, never page B.
        s_switch_state = PL_BT_SWITCH_NONE;
        pl_bt_update_scan_mode();
    }
}

// Drains every request currently queued and makes the real BTstack call for
// each. Called only from pl_bt_wdt_heartbeat_handler (IRQ context) -- the
// single consumer, so it only ever touches s_bt_pending_tail and needs no
// lock of its own, same discipline as pl_bt_drain_events above.
static void pl_bt_pending_service(void) {
    while (s_bt_pending_tail != s_bt_pending_head) {
        uint8_t tail = s_bt_pending_tail;
        pl_bt_pending_entry_t entry = s_bt_pending[tail];
        s_bt_pending_tail = (uint8_t)((tail + 1) % PL_BT_PENDING_CAPACITY);
        s_bt_pending_serviced_count++;
        pl_log(
            "BT: servicing deferred %s on the run loop (serviced=%lu)\r\n", pl_bt_pending_tag_name(entry.tag),
            (unsigned long)s_bt_pending_serviced_count
        );
        switch (entry.tag) {
            case PL_BT_PENDING_START_SCAN:
                pl_bt_start_scan_radio();
                break;
            case PL_BT_PENDING_CANCEL_SCAN:
                pl_bt_cancel_scan_radio();
                break;
            case PL_BT_PENDING_CONNECT:
                // Bead pico-link-sfw6: was a direct pl_a2dp_connect call --
                // now routed through the switch decision (a plain connect
                // from idle still ends up calling pl_a2dp_connect exactly
                // as before).
                pl_bt_connect_or_switch(entry.addr);
                break;
            case PL_BT_PENDING_DISCONNECT:
                // Bead pico-link-sfw6, design sec 6: pl_a2dp_disconnect
                // alone is AVDTP-only and can leave AVRCP's channel (and
                // therefore the ACL) up -- fold in the same full-ACL
                // teardown the switch uses, so a user-initiated Disconnect
                // (bt.c's PL_COMMAND_TAG_DISCONNECT) actually frees the
                // slot too.
                pl_a2dp_disconnect();
                pl_bt_disconnect_all_open_acl();
                break;
            case PL_BT_PENDING_PERSIST_WRITE:
                // Bead pico-link-cz0.6, code-review finding 1: the ONLY
                // call site for this function -- IRQ/async_context, same
                // serialized execution stream BTstack's own put_link_key
                // call runs on. See persist.h's "Reentrancy" doc.
                pl_persist_execute_pending_write();
                break;
            case PL_BT_PENDING_FORGET_DEVICE:
                // Bead pico-link-4vb.7 (T3): same reentrancy contract as
                // PL_BT_PENDING_PERSIST_WRITE above -- pl_persist_forget_device
                // touches the shared flash TLV instance directly and must
                // run only from here. Pushes PairedDeviceForgotten itself
                // on success (persist.c).
                pl_persist_forget_device(entry.addr);
                break;
            case PL_BT_PENDING_SET_DEVICE_SETTINGS:
                // Bead pico-link-7jol.5, generalized by pico-link-ryw.6:
                // same reentrancy contract as PL_BT_PENDING_PERSIST_WRITE
                // above. Pushes PairedDeviceUpserted itself on an actual
                // write (persist.c's pl_persist_rmw, the shared RMW core).
                pl_persist_execute_pending_device_settings_write();
                break;
            case PL_BT_PENDING_SAVE_PRESET:
                // Bead pico-link-ryw.6: same reentrancy contract as
                // PL_BT_PENDING_PERSIST_WRITE above. Pushes PresetLoaded
                // itself on an actual write (persist.c).
                pl_persist_execute_pending_save_preset_write();
                break;
            case PL_BT_PENDING_DELETE_PRESET:
                // Bead pico-link-ryw.6: same reentrancy contract as
                // PL_BT_PENDING_PERSIST_WRITE above. Pushes PresetDeleted
                // itself on an actual deletion (persist.c).
                pl_persist_execute_pending_delete_preset_write();
                break;
            case PL_BT_PENDING_SET_DISPLAY_SETTINGS:
                // Bead pico-link-qivj.5 (S11): same reentrancy contract as
                // PL_BT_PENDING_PERSIST_WRITE above.
                pl_persist_execute_pending_display_settings_write();
                break;
            case PL_BT_PENDING_SET_CUSHION_POLICY:
                // Bead pico-link-8pp1.4 (S3): same reentrancy contract as
                // PL_BT_PENDING_PERSIST_WRITE above.
                pl_persist_execute_pending_cushion_policy_write();
                break;
            case PL_BT_PENDING_SET_ABR_FLOOR:
                // Bead pico-link-d42g.3 (F3): same reentrancy contract as
                // PL_BT_PENDING_PERSIST_WRITE above.
                pl_persist_execute_pending_abr_floor_write();
                break;
        }
    }
}

// Bead pico-link-cz0.6 (M5 persistence), code-review finding 1: the only
// way persist.c ever gets its staged write actually performed -- enqueues
// onto the pending-action queue above (thread-context-safe, same as every
// other pl_bt_pending_push call site) so pl_persist_execute_pending_write()
// runs from pl_bt_pending_service's IRQ/async_context, never from
// persist.c's own thread-context caller (pl_persist_service, the
// superloop). See persist.h's module doc for the full rationale.
bool pl_bt_enqueue_persist_write(void) {
    return pl_bt_pending_push(PL_BT_PENDING_PERSIST_WRITE, NULL);
}

// Bead pico-link-7jol.5. See bt.h's doc comment.
bool pl_bt_enqueue_device_settings_write(void) {
    return pl_bt_pending_push(PL_BT_PENDING_SET_DEVICE_SETTINGS, NULL);
}

bool pl_bt_enqueue_save_preset_write(void) {
    return pl_bt_pending_push(PL_BT_PENDING_SAVE_PRESET, NULL);
}

bool pl_bt_enqueue_delete_preset_write(void) {
    return pl_bt_pending_push(PL_BT_PENDING_DELETE_PRESET, NULL);
}

// Bead pico-link-qivj.5 (S11). See bt.h's doc comment.
bool pl_bt_enqueue_display_settings_write(void) {
    return pl_bt_pending_push(PL_BT_PENDING_SET_DISPLAY_SETTINGS, NULL);
}

// Bead pico-link-8pp1.4 (S3). See bt.h's doc comment.
bool pl_bt_enqueue_cushion_policy_write(void) {
    return pl_bt_pending_push(PL_BT_PENDING_SET_CUSHION_POLICY, NULL);
}

// Bead pico-link-d42g.3 (F3). See bt.h's doc comment.
bool pl_bt_enqueue_abr_floor_write(void) {
    return pl_bt_pending_push(PL_BT_PENDING_SET_ABR_FLOOR, NULL);
}

// Bead pico-link-ufh: permanent 100ms btstack_run_loop timer proving the
// BTstack run loop is still servicing timers at all -- runs whether or not
// a stream is established, unlike a2dp.c's media timer. Same self-rearm
// idiom as pl_a2dp_media_timer_handler (a2dp.c): re-arm first, then do the
// work. Runs in the cyw43/BTstack background IRQ, same context as every
// other BTstack timer callback -- pl_wdt_kick() is a single volatile
// increment, safe from there (see watchdog_sup.h's module doc).
//
// pico-link-ouw: also the sole consumer of the deferred-action queue above --
// this is "the run loop" that pl_bt_poll_commands's BTstack calls are
// deferred onto, at up to PL_WDT_BTSTACK_HEARTBEAT_MS (100ms) latency.
static void pl_bt_wdt_heartbeat_handler(btstack_timer_source_t *ts) {
    btstack_run_loop_set_timer(ts, PL_WDT_BTSTACK_HEARTBEAT_MS);
    btstack_run_loop_add_timer(ts);
    pl_wdt_kick(PL_WDT_BTSTACK);
    pl_bt_pending_service();
    // Bead pico-link-sfw6: the switch state machine's WAIT_ACL_DOWN ->
    // PAGING advance and 5s deadline check -- right after
    // pl_bt_pending_service so a switch that just started this same tick
    // (via PL_BT_PENDING_CONNECT above) is observed starting next tick.
    pl_bt_switch_service();
    // T3 (pico-link-4v2.3), design sec 9: the "-> pl_bt_wdt_heartbeat_handler
    // (0xFF) -> avrcp_controller_set_absolute_volume" hop. Same context as
    // pl_bt_pending_service above, so it's safe to call a2dp.c's AVRCP API
    // from right here.
    pl_a2dp_avrcp_volume_service(time_us_64());
}

void pl_bt_init(struct PlUi *ui) {
    g_ui = ui;

    // Bead pico-link-cz0.6 (M5 persistence): MUST run before
    // hci_power_control(HCI_POWER_ON) below -- hci_set_link_key_db (inside
    // pl_persist_init) is a no-op once the HCI layer is already using a
    // NULL link_key_db (hci.c:567-612), and BTstack itself only reads the
    // link-key DB after power-on. Also before pl_a2dp_init's
    // gap_set_local_name/gap_discoverable_control calls -- no ordering
    // requirement between them today, but keeping storage bring-up first is
    // the more defensive order. Event::StoreLoaded itself is NOT pushed
    // here -- see pl_bt_packet_handler's BTSTACK_EVENT_STATE case below for
    // why (queuing core's auto-reconnect Command::Connect this early would
    // race hci_power_control's own async power-up).
    pl_persist_init();

    // Bead pico-link-ryw.6, design sec 2.2: PUSH the boot-loaded PL:P
    // presets NOW -- unlike the PairedDeviceUpserted/StoreLoaded sequence
    // below (deferred to BTSTACK_EVENT_STATE/HCI_STATE_WORKING because it
    // feeds core's auto-reconnect POLICY, which must not race
    // hci_power_control's async power-up), presets have nothing to do with
    // the radio at all -- pushing them here, in thread context before the
    // superloop even starts, is safe and lets `core` rebuild its preset
    // store as early as possible. One PresetLoaded per surviving record,
    // THEN PresetStoreLoaded{status, count} as the terminator -- same
    // "count x item, then a status terminator" shape as the device-store
    // sequence.
    uint8_t boot_preset_count = pl_persist_boot_preset_count();
    for (uint8_t i = 0; i < boot_preset_count; i++) {
        uint16_t boot_preset_id;
        uint8_t boot_preset_blob_len;
        uint8_t boot_preset_blob[PL_PERSIST_PRESET_BLOB_LEN];
        pl_persist_boot_preset_at(i, &boot_preset_id, &boot_preset_blob_len, boot_preset_blob);
        pl_bt_push_preset_loaded(boot_preset_id, boot_preset_blob_len, boot_preset_blob);
    }
    pl_bt_push_preset_store_loaded((uint32_t)pl_persist_preset_boot_status(), boot_preset_count, pl_persist_preset_next_id());

    // RSSI + EIR (Extended Inquiry Response, which is where a discovered
    // device's name comes from) -- without this, GAP_EVENT_INQUIRY_RESULT
    // never has a name available and every device would show as
    // "(unknown device)" until a separate remote-name request completed.
    hci_set_inquiry_mode(INQUIRY_MODE_RSSI_AND_EIR);

    hci_event_callback_registration.callback = &pl_bt_packet_handler;
    hci_add_event_handler(&hci_event_callback_registration);

    // M4 S1 (bead pico-link-cz0.5.2): A2DP Source + AVRCP + SDP + class of
    // device registration MUST happen before hci_power_control(HCI_POWER_ON)
    // below -- see a2dp.h's doc comment on pl_a2dp_init.
    pl_a2dp_init(ui);

    // Bead pico-link-ufh: start the watchdog heartbeat before HCI powers
    // on, so the run loop is proven alive through the whole power-on
    // sequence, not just once BT is fully up.
    btstack_run_loop_set_timer_handler(&s_wdt_heartbeat_timer, pl_bt_wdt_heartbeat_handler);
    btstack_run_loop_set_timer(&s_wdt_heartbeat_timer, PL_WDT_BTSTACK_HEARTBEAT_MS);
    btstack_run_loop_add_timer(&s_wdt_heartbeat_timer);

    // Bead pico-link-oevr, Q4/Q5 step 3: set the initial scan state
    // explicitly before power-on. Required with
    // ENABLE_EXPLICIT_CONNECTABLE_MODE_CONTROL defined (btstack_config.h)
    // -- l2cap no longer sets connectable=1 on its own, so without this
    // call the radio would come up non-connectable. No ACL exists yet, so
    // this reads connectable=1/discoverable=0. BTstack replays these flags
    // across power-on (hci.c:4875/4897), so setting them here (rather than
    // waiting for HCI_STATE_WORKING) is what actually reaches the
    // controller once the stack comes up.
    pl_bt_update_scan_mode();

    // Bead pico-link-pigd: registered once, at init, same as every other
    // classic GAP callback in this function -- gap_register_classic_
    // connection_filter just stores a function pointer (hci.c:9740), no
    // ordering dependency on hci_power_control like pl_a2dp_init's service
    // registration has, but init is still the natural single place every
    // other one-time HCI callback in this file lives.
    gap_register_classic_connection_filter(&pl_bt_connection_filter);

    pl_log("BT: powering on HCI (async -- BTSTACK_EVENT_STATE/HCI_STATE_WORKING follows)\r\n");
    hci_power_control(HCI_POWER_ON);
}

void pl_bt_poll_commands(struct PlUi *ui) {
    // Bead pico-link-okx round 3: the loop-trace named BT_POLL_CMDS as the
    // last checkpoint before a >2s stall and a hardware watchdog expiry
    // (n=1). These two marks split this function into the Rust FFI call and
    // the C dispatch that follows, so the next expiry says which side.
    pl_wdt_mark(PL_WDT_CP_BT_POLL_FFI);
    PlCommand command = pl_ui_poll_command(ui);
    pl_wdt_mark(PL_WDT_CP_BT_POLL_DISPATCH);

    // Defensive ABI version check (pico-link-a67) -- Rust is the sole
    // producer of PlCommand and always sets this correctly today, but a
    // mismatch here means the payload union must not be trusted under this
    // build's variant shapes, so bail rather than switch on `tag` at all.
    if (command.version != PL_COMMAND_ABI_VERSION) {
        pl_log("BT: pl_ui_poll_command version mismatch (got %u, expected %u) -- ignoring\r\n", command.version, PL_COMMAND_ABI_VERSION);
        return;
    }

    switch (command.tag) {
        case PL_COMMAND_TAG_START_SCAN:
            // pico-link-ouw: this runs in thread context (the main.c
            // superloop). gap_inquiry_start (inside pl_bt_start_scan_radio)
            // is a BTstack API call and must not be made from here -- only
            // the UI-event pushes are done inline (already safe, see
            // pl_bt_ring_push); the radio call is deferred onto the
            // heartbeat handler. See the pending-queue module doc above.
            pl_wdt_mark(PL_WDT_CP_CMD_SCAN_CALL);
            pl_bt_push_devices_cleared();
            pl_bt_push_discovery_state(PL_DISCOVERY_STATE_SCANNING);
            pl_bt_pending_push(PL_BT_PENDING_START_SCAN, NULL);
            pl_wdt_mark(PL_WDT_CP_CMD_SCAN_RET);
            break;

        case PL_COMMAND_TAG_CONNECT: {
            pl_wdt_mark(PL_WDT_CP_CMD_CONNECT_ENTER);
            // M4 S1 (bead pico-link-cz0.5.2): this used to be log-only
            // (M2's acceptance criterion was just that the intent was
            // observable over CDC). Now it actually opens an A2DP source
            // stream -- see a2dp.c's module doc and design sec 4.3's event
            // flow.
            //
            // pico-link-ouw: pl_a2dp_connect (a2dp_source_establish_stream)
            // is a BTstack API call made from thread context here -- deferred
            // onto the heartbeat handler for the same reason as START_SCAN
            // above. The link-state push stays inline; only the radio call
            // is deferred.
            const uint8_t *addr = command.payload.connect.addr;
            pl_log(
                "BT: PL_CMD_CONNECT %02x:%02x:%02x:%02x:%02x:%02x\r\n",
                addr[0], addr[1], addr[2], addr[3], addr[4], addr[5]
            );
            // Bead pico-link-4vb.7 (T3), design section 5.3: cache the
            // name that rode along with this Connect command -- the record
            // isn't actually written until a2dp.c's STREAM_ESTABLISHED,
            // long after this call returns, and that's the only place C
            // ever learns the name at all.
            pl_bt_set_connect_target(addr, command.payload.connect.name, command.payload.connect.name_len);
            pl_bt_push_link_state(PL_LINK_STATE_CONNECTING);
            pl_wdt_mark(PL_WDT_CP_CMD_CONNECT_A2DP);
            pl_bt_pending_push(PL_BT_PENDING_CONNECT, addr);
            pl_wdt_mark(PL_WDT_CP_CMD_CONNECT_RET);
            break;
        }

        case PL_COMMAND_TAG_PERSIST_DEVICE: {
            // Bead pico-link-cz0.6 (M5 persistence), design point 7: core's
            // auto-reconnect/remember-this-device POLICY decided (on
            // Event::ConnectSucceeded) that `addr` is worth remembering;
            // this is C's side of that -- stage it, don't write flash here
            // (thread context, but still subject to the same streaming
            // gate persist.c's own service loop applies -- staging itself
            // is cheap and safe regardless).
            //
            // Bead pico-link-4vb.7 (T3), design section 7 hazard 3: this
            // command's meaning has drifted since Andreas's
            // write-at-pairing-time ruling -- the synchronous
            // pl_persist_save_device_now() path (a2dp.c) already remembers
            // the device (and its name) at STREAM_ESTABLISHED, so by the
            // time core queues this, the record normally already exists.
            // What's left for this handler is the MRU bump (and, later,
            // per-device settings flushes) -- no name to contribute here,
            // hence NULL/0 (persist.c's RMW convention leaves the existing
            // name untouched).
            const uint8_t *addr = command.payload.addr.addr;
            pl_log(
                "BT: PL_CMD_PERSIST_DEVICE %02x:%02x:%02x:%02x:%02x:%02x\r\n", addr[0], addr[1], addr[2], addr[3], addr[4],
                addr[5]
            );
            pl_persist_request_save_device(addr, NULL, 0);
            break;
        }

        case PL_COMMAND_TAG_CANCEL_SCAN:
            // pico-link-ouw: gap_inquiry_stop (inside
            // pl_bt_cancel_scan_radio) is the same class of BTstack call,
            // deferred for the same reason.
            pl_wdt_mark(PL_WDT_CP_CMD_CANCEL_SCAN_CALL);
            pl_bt_pending_push(PL_BT_PENDING_CANCEL_SCAN, NULL);
            pl_wdt_mark(PL_WDT_CP_CMD_CANCEL_SCAN_RET);
            break;

        case PL_COMMAND_TAG_FORGET_DEVICE: {
            // Bead pico-link-4vb.7 (T3), design section 5.1/6: user-initiated
            // "forget this remembered device" (Devices screen's X action, or
            // the pick-one-to-forget flow when the store is full). Deferred
            // onto the pending queue -- pl_persist_forget_device touches the
            // shared flash TLV instance directly and must run on the
            // cyw43/BTstack background async_context, same as every other
            // flash write in this file.
            const uint8_t *addr = command.payload.addr.addr;
            pl_log(
                "BT: PL_CMD_FORGET_DEVICE %02x:%02x:%02x:%02x:%02x:%02x\r\n", addr[0], addr[1], addr[2], addr[3], addr[4],
                addr[5]
            );
            pl_bt_pending_push(PL_BT_PENDING_FORGET_DEVICE, addr);
            break;
        }

        case PL_COMMAND_TAG_DISCONNECT:
            // Bead pico-link-44w: FFI surface only -- no screen queues this
            // yet (design-of-record rule 2 forbids a labelled-but-dead
            // affordance, so the manage-connected-device screen is deferred
            // to a follow-up bead). Body is identical to
            // pl_bt_debug_disconnect's existing debug-only path below: no
            // address, since a2dp.c tracks at most one active connection
            // (s_ctx.a2dp_cid) -- reuse the same PL_BT_PENDING_DISCONNECT
            // pending-queue entry pico-link-nb6 already added.
            pl_bt_pending_push(PL_BT_PENDING_DISCONNECT, NULL);
            break;

        case PL_COMMAND_TAG_SET_DEVICE_LDAC_QUALITY: {
            // Bead pico-link-7jol.5, design `.planning/design/2026-09-07-
            // ldac-quality-selector.md` §5: "A applies the pick
            // immediately... no confirm... saves later". Split in two:
            // (1) the live encoder application below is plain/volatile
            // state only (no flash) and safe to do RIGHT HERE, in thread
            // context; (2) the flash write is staged, not enqueued
            // directly -- persist.c's own streaming gate decides when it
            // is safe, same discipline as every other per-device setting.
            const uint8_t *addr = command.payload.set_device_ldac_quality.addr;
            uint8_t ldac_quality = command.payload.set_device_ldac_quality.ldac_quality;
            pl_log(
                "BT: PL_CMD_SET_DEVICE_LDAC_QUALITY %02x:%02x:%02x:%02x:%02x:%02x quality=%u\r\n", addr[0], addr[1], addr[2],
                addr[3], addr[4], addr[5], ldac_quality
            );
            if (pl_a2dp_is_connected_ldac(addr)) {
                pl_codec_ldac_pin_now(ldac_quality);
            } else {
                // Bead pico-link-xcmx: not silent any more. This is the
                // legitimately-unappliable case (a2dp.h:137-144) -- either
                // this isn't the connected device, or the connected device
                // has fallen back to SBC -- so there's no live encoder to
                // pin. The flash write below still stages normally, so the
                // pick takes effect next time this device connects on LDAC;
                // this line exists so a console capture can tell
                // "unappliable" apart from "broken" without re-deriving it.
                pl_log(
                    "BT: PL_CMD_SET_DEVICE_LDAC_QUALITY %02x:%02x:%02x:%02x:%02x:%02x quality=%u -- not the "
                    "connected LDAC device, skipping live apply (flash write still staged)\r\n",
                    addr[0], addr[1], addr[2], addr[3], addr[4], addr[5], ldac_quality
                );
            }
            pl_persist_request_device_settings(addr, PL_PERSIST_DEVICE_FIELD_LDAC_QUALITY, 0, ldac_quality, 0);
            break;
        }

        case PL_COMMAND_TAG_SET_DISPLAY_SETTINGS: {
            // Bead pico-link-qivj.5 (S11): C only persists -- core owns the
            // live value and applies it itself via pl_ui_tick's own
            // take_display_settings_to_apply() (S3/S4/S8, already landed).
            // This handler's only job is staging the flash write.
            uint8_t mode = command.payload.display_settings.mode;
            uint16_t timeout_s = command.payload.display_settings.timeout_s;
            pl_log("BT: PL_CMD_SET_DISPLAY_SETTINGS mode=%u timeout_s=%u\r\n", (unsigned)mode, (unsigned)timeout_s);
            pl_persist_request_display_settings(mode, timeout_s);
            break;
        }

        case PL_COMMAND_TAG_SET_CUSHION_POLICY: {
            // Bead pico-link-8pp1.4 (S3), design `.planning/design/2026-09-
            // 24-congestion-cushion.md` sec 4: UNLIKE PL_COMMAND_TAG_SET_
            // DISPLAY_SETTINGS above, C both APPLIES this one live (core has
            // no run loop of its own that touches a2dp.c -- see
            // core/src/audio.rs's module doc) AND persists it.
            uint8_t policy = command.payload.cushion_policy.policy;
            pl_log("BT: PL_CMD_SET_CUSHION_POLICY policy=%u\r\n", (unsigned)policy);
            pl_a2dp_set_cushion_policy(policy);
            pl_persist_request_cushion_policy(policy);
            break;
        }

        case PL_COMMAND_TAG_SET_ABR_FLOOR: {
            // Bead pico-link-d42g.3 (F3), design `.planning/design/2026-09-
            // 25-adaptive-floor.md` sec 4: same "apply live AND persist"
            // shape as PL_COMMAND_TAG_SET_CUSHION_POLICY above -- core has
            // no run loop of its own that touches codec_ldac.c.
            uint8_t floor = command.payload.abr_floor.floor;
            pl_log("BT: PL_CMD_SET_ABR_FLOOR floor=%u\r\n", (unsigned)floor);
            pl_codec_ldac_set_floor(floor);
            pl_persist_request_abr_floor(floor);
            break;
        }

        case PL_COMMAND_TAG_SAVE_PRESET: {
            // Bead pico-link-ryw.6, design sec 2.2/3.2: wires the
            // PL_COMMAND_TAG_SAVE_PRESET stub bead pico-link-ryw.5 left
            // log-only to the real PL:P:<slot> flash store. Stages only --
            // C stages, gates and flushes (design point 7's precedent);
            // pl_persist_service() (thread context, the superloop) notices
            // the staged save and enqueues the actual write via bt.c's
            // pending-action queue, same "no direct flash access from
            // thread context" discipline as every other write in this
            // module. `preset_id == 0` means "allocate a fresh id" --
            // persist.c's PresetLoaded echo carries whichever id was
            // actually used.
            uint16_t preset_id = command.payload.save_preset.preset_id;
            uint8_t blob_len = command.payload.save_preset.blob_len;
            pl_log("BT: PL_CMD_SAVE_PRESET preset_id=%u blob_len=%u\r\n", (unsigned)preset_id, (unsigned)blob_len);
            pl_persist_request_save_preset(preset_id, blob_len, command.payload.save_preset.blob);
            break;
        }

        case PL_COMMAND_TAG_DELETE_PRESET: {
            // Bead pico-link-ryw.6, design sec 2.4: same "stage here, real
            // write on the heartbeat" story as PL_COMMAND_TAG_SAVE_PRESET
            // above. Deletes ONLY the PL:P record -- any device still
            // referencing this id keeps a dangling reference, resolved as
            // Off by `core`, never rewritten here.
            uint16_t preset_id = command.payload.delete_preset.preset_id;
            pl_log("BT: PL_CMD_DELETE_PRESET preset_id=%u\r\n", (unsigned)preset_id);
            pl_persist_request_delete_preset(preset_id);
            break;
        }

        case PL_COMMAND_TAG_ASSIGN_PRESET: {
            // Bead pico-link-ryw.6, design sec 2.3/3.2: the LIVE half of
            // this command (the pull API in main.c recomputing the active
            // program from whatever `core` now resolves for the connected
            // device) already works without any C-side handling here --
            // this handler only owns the flash-persist half, routed through
            // the SAME field-masked device-settings staging slot
            // PL_COMMAND_TAG_SET_DEVICE_LDAC_QUALITY uses above (design
            // sec 2.3: "one field-masked pl_persist_request_device_settings
            // ... rather than a fifth bespoke staging slot").
            // `preset_id == PL_PERSIST_PRESET_ID_NONE` (0) clears the
            // assignment (Off).
            const uint8_t *addr = command.payload.assign_preset.addr;
            uint16_t preset_id = command.payload.assign_preset.preset_id;
            pl_log(
                "BT: PL_CMD_ASSIGN_PRESET %02x:%02x:%02x:%02x:%02x:%02x preset_id=%u\r\n", addr[0], addr[1], addr[2],
                addr[3], addr[4], addr[5], (unsigned)preset_id
            );
            pl_persist_request_device_settings(addr, PL_PERSIST_DEVICE_FIELD_PRESET_ID, 0, 0, preset_id);
            break;
        }

        case PL_COMMAND_TAG_NONE:
            pl_wdt_mark(PL_WDT_CP_CMD_NONE);
            break;

        default:
            pl_wdt_mark(PL_WDT_CP_CMD_OTHER);
            break;
    }
}

#ifdef PL_DEBUG_REMOTE
// Bead pico-link-g48 -- see bt.h's doc comment on this declaration. Body
// is deliberately identical to PL_COMMAND_TAG_CONNECT's case above, minus
// the PlCommand/pl_ui_poll_command indirection: there is no discovered
// DeviceEntry to select here (that is the whole point -- this bypasses
// inquiry), so this is called directly from debug_remote.c instead of
// going through the Rust command queue. debug_remote.c's poll function is
// itself called from main.c's superloop (thread context, see
// debug_remote.h's module doc), so this has the exact same pico-link-ouw
// hazard as PL_COMMAND_TAG_CONNECT and gets the same fix: defer the actual
// BTstack call to the heartbeat handler.
void pl_bt_debug_connect(const uint8_t *addr) {
    pl_log(
        "BT: debug-remote CONNECT %02x:%02x:%02x:%02x:%02x:%02x (bypassing inquiry)\r\n",
        addr[0], addr[1], addr[2], addr[3], addr[4], addr[5]
    );
    // Bead pico-link-4vb.7 (T3): no name available here (this bypasses
    // inquiry entirely) -- name_len = 0 caches "no name", which
    // persist.c's RMW convention turns into "keep whatever name is
    // already on record" when the write actually happens.
    pl_bt_set_connect_target(addr, NULL, 0);
    pl_bt_push_link_state(PL_LINK_STATE_CONNECTING);
    pl_bt_pending_push(PL_BT_PENDING_CONNECT, addr);
}

// Bead pico-link-nb6: debug-only disconnect, letting an unattended hardware
// test tear down the current A2DP connection without a human power-cycling
// the headset. No address payload -- there is only ever one active
// connection, tracked entirely inside a2dp.c's s_ctx.a2dp_cid. Same
// pico-link-ouw hazard and same fix as pl_bt_debug_connect above: this runs
// in thread context (via debug_remote.c's superloop poll), so it only
// enqueues; pl_bt_pending_service (IRQ context, the heartbeat handler)
// makes the real a2dp_source_disconnect() call.
void pl_bt_debug_disconnect(void) {
    pl_log("BT: debug-remote DISCONNECT\r\n");
    pl_bt_pending_push(PL_BT_PENDING_DISCONNECT, NULL);
}
#endif
