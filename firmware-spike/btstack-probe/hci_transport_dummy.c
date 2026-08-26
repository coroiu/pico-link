// Dummy hci_transport_t for the link-only spike (gate 3/4): proves BTstack's
// vtable-style transport port compiles/links against a Rust-owned binary.
// The REAL transport (gate 5) bridges this to cyw43::BtDriver in Rust.
#include <stddef.h>
#include "hci_transport.h"

static void dummy_init(const void *transport_config) { (void)transport_config; }
static int dummy_open(void) { return 0; }
static int dummy_close(void) { return 0; }
static void dummy_register_packet_handler(void (*handler)(uint8_t packet_type, uint8_t *packet, uint16_t size)) { (void)handler; }
static int dummy_can_send_packet_now(uint8_t packet_type) { (void)packet_type; return 1; }
static int dummy_send_packet(uint8_t packet_type, uint8_t *packet, int size) {
    (void)packet_type; (void)packet; (void)size;
    return 0;
}

static const hci_transport_t dummy_transport = {
    "dummy",
    &dummy_init,
    &dummy_open,
    &dummy_close,
    &dummy_register_packet_handler,
    &dummy_can_send_packet_now,
    &dummy_send_packet,
    NULL, // set_baudrate
    NULL, // reset_link
    NULL, // set_sco_config
};

const hci_transport_t *hci_transport_dummy_instance(void) {
    return &dummy_transport;
}
