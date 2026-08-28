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

// --- pico-link-a67: one PlEvent union in, replacing the old
// pl_ui_set_link_state/pl_ui_add_device/pl_ui_clear_devices setter trio ---
//
// Small helpers so each call site below builds one PlEvent value and pushes
// it, rather than repeating the version/tag/payload boilerplate. Every
// PlEvent must carry PL_EVENT_ABI_VERSION -- pl_ui_push_event silently
// no-ops on a mismatch (see pico_link_ui.h's doc comment on
// pl_ui_push_event), so a helper that always sets it is cheap insurance
// against a call site accidentally leaving it zero-initialized.

static void pl_bt_push_link_state(enum PlLinkState state) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_LINK_STATE_CHANGED,
        .payload = {.link_state_changed = {.state = state}},
    };
    pl_ui_push_event(g_ui, event);
}

static void pl_bt_push_devices_cleared(void) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_DEVICES_CLEARED,
        .payload = {0},
    };
    pl_ui_push_event(g_ui, event);
}

static void pl_bt_push_device_discovered(const uint8_t *addr, const uint8_t *name, uint16_t name_len, int8_t rssi) {
    struct PlEvent event = {.version = PL_EVENT_ABI_VERSION, .tag = PL_EVENT_TAG_DEVICE_DISCOVERED};
    memcpy(event.payload.device_discovered.addr, addr, 6);
    event.payload.device_discovered.name = name;
    event.payload.device_discovered.name_len = name_len;
    event.payload.device_discovered.rssi = rssi;
    pl_ui_push_event(g_ui, event);
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
