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
#include "usb_pump.h"

// One inquiry scan runs for INQUIRY_DURATION_UNITS * 1.28s -- 8 units is
// BTstack's own gap_inquiry.c example's INQUIRY_INTERVAL, long enough for
// nearby headphones in pairing mode to be found in one pass.
#define PL_INQUIRY_DURATION_UNITS 8

static struct PlUi *g_ui;
static btstack_packet_callback_registration_t hci_event_callback_registration;

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

static void pl_bt_start_scan(void) {
    pl_log("BT: starting GAP inquiry (%d.%ds)\r\n", (PL_INQUIRY_DURATION_UNITS * 128) / 100, (PL_INQUIRY_DURATION_UNITS * 128) % 100);
    pl_ui_clear_devices(g_ui);
    pl_ui_set_link_state(g_ui, PL_LINK_STATE_SCANNING);
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

    pl_log(
        "BT: inquiry result %02x:%02x:%02x:%02x:%02x:%02x rssi=%d name=\"%.*s\"\r\n",
        addr[0], addr[1], addr[2], addr[3], addr[4], addr[5], rssi, (int)name_len, name_buf
    );
    pl_ui_add_device(g_ui, addr, (const uint8_t *)name_buf, name_len, rssi);
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
            pl_ui_set_link_state(g_ui, PL_LINK_STATE_IDLE);
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

    pl_log("BT: powering on HCI (async -- BTSTACK_EVENT_STATE/HCI_STATE_WORKING follows)\r\n");
    hci_power_control(HCI_POWER_ON);
}

void pl_bt_poll_commands(struct PlUi *ui) {
    PlCommand command = pl_ui_poll_command(ui);
    switch (command.tag) {
        case PL_COMMAND_TAG_START_SCAN:
            pl_bt_start_scan();
            break;

        case PL_COMMAND_TAG_CONNECT:
            // M2's acceptance criterion is that this is observable over
            // CDC, not that a connection actually opens -- see bt.h's doc
            // comment on this function.
            pl_log(
                "BT: PL_CMD_CONNECT %02x:%02x:%02x:%02x:%02x:%02x\r\n",
                command.addr[0], command.addr[1], command.addr[2], command.addr[3], command.addr[4], command.addr[5]
            );
            pl_ui_set_link_state(ui, PL_LINK_STATE_CONNECTING);
            break;

        case PL_COMMAND_TAG_NONE:
        default:
            break;
    }
}
