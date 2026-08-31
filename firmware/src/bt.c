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

#include "btstack.h"

#include "a2dp.h"
#include "bt.h"
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

static void pl_bt_push_devices_cleared(void) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_DEVICES_CLEARED,
        .payload = {0},
    };
    pl_bt_ring_push(event, NULL, 0);
}

static void pl_bt_push_device_discovered(const uint8_t *addr, const uint8_t *name, uint16_t name_len, int8_t rssi) {
    struct PlEvent event = {.version = PL_EVENT_ABI_VERSION, .tag = PL_EVENT_TAG_DEVICE_DISCOVERED};
    memcpy(event.payload.device_discovered.addr, addr, 6);
    event.payload.device_discovered.name = NULL; // patched at drain time -- see pl_bt_drain_events
    event.payload.device_discovered.name_len = name_len;
    event.payload.device_discovered.rssi = rssi;
    pl_bt_ring_push(event, name, name_len);
}

// --- M4 S1 additions (bead pico-link-cz0.5.2): exported so a2dp.c can push
// through this same ring -- see bt.h's doc comment on why that's the right
// seam (a2dp.c's A2DP/AVRCP packet handler is another IRQ-context producer,
// same MPSC shape pico-link-6o2 already built this ring to handle).

void pl_bt_push_link_state_connected(void) {
    pl_bt_push_link_state(PL_LINK_STATE_CONNECTED);
}

void pl_bt_push_connect_step(uint32_t step) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_CONNECT_STEP_CHANGED,
        .payload = {.connect_step_changed = {.step = step}},
    };
    pl_bt_ring_push(event, NULL, 0);
}

void pl_bt_push_connect_succeeded(bool degraded) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_CONNECT_SUCCEEDED,
        .payload = {.connect_succeeded = {.degraded = degraded ? 1 : 0}},
    };
    pl_bt_ring_push(event, NULL, 0);
}

void pl_bt_push_connect_failed(const uint8_t *addr, uint32_t reason) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_CONNECT_FAILED,
        .payload = {.connect_failed = {.reason = reason}},
    };
    memcpy(event.payload.connect_failed.addr, addr, 6);
    pl_bt_ring_push(event, NULL, 0);
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
    pl_bt_push_link_state(PL_LINK_STATE_SCANNING);
    pl_bt_start_scan_radio();
}

// Does the actual radio work for a scan cancel -- see pl_bt_start_scan_radio's
// doc comment; same split for the same reason (pico-link-ouw).
//
// pico-link-znb.2 (E1, MVP-blocking): stops an in-flight GAP inquiry.
// gap_inquiry_stop() itself triggers GAP_EVENT_INQUIRY_COMPLETE (same as a
// natural timeout), so pl_bt_packet_handler's existing
// GAP_EVENT_INQUIRY_COMPLETE case pushes PL_LINK_STATE_IDLE -- no separate
// push needed here. Compiled but its runtime effect is UNVERIFIED (board is
// wedged, see pico-link-icb; this bead may not block on hardware).
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

    pl_log(
        "BT: inquiry result %02x:%02x:%02x:%02x:%02x:%02x rssi=%d name=\"%.*s\"\r\n",
        addr[0], addr[1], addr[2], addr[3], addr[4], addr[5], rssi, (int)name_len, name_buf
    );
    pl_bt_push_device_discovered(addr, (const uint8_t *)name_buf, name_len, rssi);
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
            pl_bt_push_link_state(PL_LINK_STATE_IDLE);
            break;

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
} pl_bt_pending_tag_t;

typedef struct {
    pl_bt_pending_tag_t tag;
    bd_addr_t addr; // meaningful only for PL_BT_PENDING_CONNECT
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
        default:
            return "?";
    }
}

// Enqueues one deferred BTstack call. Called only from thread context
// (pl_bt_poll_commands, pl_bt_debug_connect) -- unlike pl_bt_ring_push this
// queue has exactly one producer context, but it still needs the critical
// section because the consumer (pl_bt_pending_service, IRQ context) can
// preempt the producer mid read-modify-write of s_bt_pending_head.
static void pl_bt_pending_push(pl_bt_pending_tag_t tag, const uint8_t *addr) {
    uint32_t irq_state = save_and_disable_interrupts();
    uint8_t head = s_bt_pending_head;
    uint8_t next_head = (uint8_t)((head + 1) % PL_BT_PENDING_CAPACITY);
    if (next_head == s_bt_pending_tail) {
        s_bt_pending_drop_count++;
        restore_interrupts(irq_state);
        pl_log("BT: pending-action queue full, dropped deferred %s\r\n", pl_bt_pending_tag_name(tag));
        return;
    }
    s_bt_pending[head].tag = tag;
    if (tag == PL_BT_PENDING_CONNECT) {
        memcpy(s_bt_pending[head].addr, addr, sizeof(bd_addr_t));
    }
    s_bt_pending_head = next_head;
    s_bt_pending_enqueued_count++;
    restore_interrupts(irq_state);
    pl_log(
        "BT: queued deferred %s from thread context (enqueued=%lu)\r\n", pl_bt_pending_tag_name(tag),
        (unsigned long)s_bt_pending_enqueued_count
    );
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
                pl_a2dp_connect(entry.addr);
                break;
            case PL_BT_PENDING_DISCONNECT:
                pl_a2dp_disconnect();
                break;
        }
    }
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
}

void pl_bt_init(struct PlUi *ui) {
    g_ui = ui;

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
            pl_bt_push_link_state(PL_LINK_STATE_SCANNING);
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
            pl_bt_push_link_state(PL_LINK_STATE_CONNECTING);
            pl_wdt_mark(PL_WDT_CP_CMD_CONNECT_A2DP);
            pl_bt_pending_push(PL_BT_PENDING_CONNECT, addr);
            pl_wdt_mark(PL_WDT_CP_CMD_CONNECT_RET);
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
