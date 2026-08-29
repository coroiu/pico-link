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

#include "btstack.h"

#include "bt.h"
#include "pico_link_ui.h"

// One inquiry scan runs for INQUIRY_DURATION_UNITS * 1.28s -- 8 units is
// BTstack's own gap_inquiry.c example's INQUIRY_INTERVAL, long enough for
// nearby headphones in pairing mode to be found in one pass.
#define PL_INQUIRY_DURATION_UNITS 8

static struct PlUi *g_ui;
static btstack_packet_callback_registration_t hci_event_callback_registration;

// --- pico-link-6o2: a C-side SPSC ring decouples the BTstack packet handler
// (producer) from the pl_ui_push_event call (consumer) ---
//
// pl_bt_packet_handler runs in low_priority_irq_handler under
// pico_cyw43_arch_threadsafe_background (firmware/CMakeLists.txt) -- i.e.
// INTERRUPT CONTEXT -- and it used to call pl_ui_push_event (Rust) directly
// from there. That could preempt the superloop mid-pl_ui_render and mutate
// the model underneath it, and race the superloop's own Rust-heap
// allocations. Same class of bug pico-link-5am already fixed for input
// (see input.c), and the same fix shape: the three pl_bt_push_* helpers
// below no longer call into Rust themselves -- they build a PlEvent and
// memcpy it into this ring; pl_bt_drain_events (called from the superloop,
// thread context, in main.c) is the only thing that ever calls
// pl_ui_push_event for Bluetooth-domain events.
//
// Single-producer (the packet handler, IRQ)/single-consumer (the superloop,
// thread context) discipline, same as input.c's ring: each side owns
// exactly one index (s_bt_ring_head/s_bt_ring_tail), so plain volatile
// uint8_t reads/writes are sufficient -- there is no read-modify-write race
// on either index, and a single-byte load/store is inherently atomic on
// Cortex-M33. No additional DMB/barrier is needed beyond `volatile` because
// this is a same-core producer/consumer pair (the IRQ preempts the
// superloop on core 0; there is no second core touching this ring, and
// core0-only is still true as of pico-link-6o2 -- the core1 UI split is
// pico-link-yz6, not yet done): Cortex-M33 does not reorder the *program
// order* of a single core's memory accesses relative to itself the way a
// multi-core system would need a DMB to fix, so the only ordering hazard is
// the *compiler* reordering across the IRQ boundary, which `volatile`
// already prevents. This is the identical justification input.c already
// uses and had reviewed; it is not repeated invention.
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
// Capacity: 32, sized for a full inquiry burst, not just a single result.
// GAP_EVENT_INQUIRY_RESULT can report the same or additional devices
// repeatedly over the whole ~10s inquiry window (PL_INQUIRY_DURATION_UNITS
// above), and the superloop only drains once per frame -- if a render+blit
// stalls the loop (SPI blit time, see pico-link-14l), several results can
// pile up before the next drain. 32 covers a burst well beyond the handful
// of nearby BR/EDR devices realistically seen in one inquiry window, with
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
static volatile uint8_t s_bt_ring_head; // producer-owned (IRQ packet handler)
static volatile uint8_t s_bt_ring_tail; // consumer-owned (superloop drain)
// Diagnostics only: counts events dropped because the ring was full when the
// packet handler tried to push. Bumped from IRQ context (hence volatile,
// plain increment -- single producer, no read-modify-write race with the
// consumer which never writes this), read/reported from pl_bt_drain_events
// in thread context. Never touch this from the packet handler with printf
// -- see the drop-reporting note on pl_bt_drain_events below.
static volatile uint32_t s_bt_ring_drop_count;

// Enqueues one event (IRQ context only). `name`/`name_len` are optional
// (NULL/0 for every tag but DeviceDiscovered) and are memcpy'd into the
// ring entry's own buffer -- never stored as a raw pointer -- for the
// borrow-lifetime reason in the ring's doc comment above.
//
// Overflow policy: drop the newest (this event), keep everything already
// queued. Matches input.c's choice and for the same reason -- overwriting
// an undrained slot would corrupt the consumer's in-progress read of it,
// while dropping the newest loses exactly one event (a missed/duplicate
// inquiry result is survivable; BTstack will report a still-present device
// again on its next report within the same scan).
static void pl_bt_ring_push(struct PlEvent event, const uint8_t *name, uint16_t name_len) {
    uint8_t head = s_bt_ring_head;
    uint8_t next_head = (uint8_t)((head + 1) % PL_BT_RING_CAPACITY);
    if (next_head == s_bt_ring_tail) {
        s_bt_ring_drop_count++;
        return;
    }
    pl_bt_ring_entry_t *entry = &s_bt_ring[head];
    entry->event = event;
    if (name != NULL && name_len > 0) {
        uint16_t copy_len = name_len > PL_BT_RING_NAME_CAP ? PL_BT_RING_NAME_CAP : name_len;
        memcpy(entry->name_buf, name, copy_len);
    }
    s_bt_ring_head = next_head;
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
        printf("BT: event ring overflow, dropped %lu event(s) total\r\n", (unsigned long)drops);
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
// packet handler) or thread context (pl_bt_poll_commands, via
// pl_bt_push_link_state at the CONNECT case) -- either is fine now, since
// neither calls into Rust directly any more.

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

// --- HCI Read Local Version Information: the acceptance-criterion probe ---
//
// Fires once, the first time BTSTACK_EVENT_STATE reports HCI_STATE_WORKING
// -- i.e. once BTstack itself considers the radio fully brought up. Prints
// both the raw return-parameter bytes and the decoded fields, per the
// bead's "raw bytes AND decode so the evidence is auditable" acceptance
// criterion.
static void pl_bt_handle_read_local_version_complete(const uint8_t *params, uint16_t params_len) {
    printf("BT: HCI Read Local Version Information, raw bytes:");
    for (uint16_t i = 0; i < params_len; i++) {
        printf(" %02x", params[i]);
    }
    printf("\r\n");

    // Standard HCI Command Complete return parameters for this command:
    // Status(1) HCI_Version(1) HCI_Revision(2,LE) LMP_Version(1)
    // Manufacturer_Name(2,LE) LMP_Subversion(2,LE) -- 9 bytes total.
    if (params_len < 9) {
        printf("BT: Read Local Version response too short to decode (%u bytes)\r\n", params_len);
        return;
    }
    uint8_t status = params[0];
    uint8_t hci_version = params[1];
    uint16_t hci_revision = (uint16_t)(params[2] | (params[3] << 8));
    uint8_t lmp_version = params[4];
    uint16_t manufacturer = (uint16_t)(params[5] | (params[6] << 8));
    uint16_t lmp_subversion = (uint16_t)(params[7] | (params[8] << 8));
    printf(
        "BT: decoded -- status=0x%02x hci_version=0x%02x hci_revision=0x%04x "
        "lmp_version=0x%02x manufacturer=0x%04x lmp_subversion=0x%04x\r\n",
        status, hci_version, hci_revision, lmp_version, manufacturer, lmp_subversion
    );
}

static void pl_bt_start_scan(void) {
    printf("BT: starting GAP inquiry (%d.%ds)\r\n", (PL_INQUIRY_DURATION_UNITS * 128) / 100, (PL_INQUIRY_DURATION_UNITS * 128) % 100);
    pl_bt_push_devices_cleared();
    pl_bt_push_link_state(PL_LINK_STATE_SCANNING);
    gap_inquiry_start(PL_INQUIRY_DURATION_UNITS);
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

    printf(
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
                printf("BT: HCI_STATE_WORKING -- radio up\r\n");
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
            printf("BT: inquiry complete\r\n");
            pl_bt_push_link_state(PL_LINK_STATE_IDLE);
            break;

        default:
            break;
    }
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

    printf("BT: powering on HCI (async -- BTSTACK_EVENT_STATE/HCI_STATE_WORKING follows)\r\n");
    hci_power_control(HCI_POWER_ON);
}

void pl_bt_poll_commands(struct PlUi *ui) {
    PlCommand command = pl_ui_poll_command(ui);

    // Defensive ABI version check (pico-link-a67) -- Rust is the sole
    // producer of PlCommand and always sets this correctly today, but a
    // mismatch here means the payload union must not be trusted under this
    // build's variant shapes, so bail rather than switch on `tag` at all.
    if (command.version != PL_COMMAND_ABI_VERSION) {
        printf("BT: pl_ui_poll_command version mismatch (got %u, expected %u) -- ignoring\r\n", command.version, PL_COMMAND_ABI_VERSION);
        return;
    }

    switch (command.tag) {
        case PL_COMMAND_TAG_START_SCAN:
            pl_bt_start_scan();
            break;

        case PL_COMMAND_TAG_CONNECT: {
            // M2's acceptance criterion is that this is observable over
            // CDC, not that a connection actually opens -- see bt.h's doc
            // comment on this function.
            const uint8_t *addr = command.payload.connect.addr;
            printf("BT: PL_CMD_CONNECT %02x:%02x:%02x:%02x:%02x:%02x\r\n", addr[0], addr[1], addr[2], addr[3], addr[4], addr[5]);
            pl_bt_push_link_state(PL_LINK_STATE_CONNECTING);
            break;
        }

        case PL_COMMAND_TAG_NONE:
        default:
            break;
    }
}
