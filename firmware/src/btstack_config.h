// Pico Link btstack_config.h -- M2 scope: bring the radio up through
// pico-sdk's own HCI transport (pico_btstack_hci_transport_cyw43, the
// ready-made piece the C-first pivot exists to use -- see
// .planning/decisions/2026-08-27-c-first-pico-sdk-owns-main.md), read an
// HCI Read Local Version response, run a GAP Classic inquiry, and log a
// user-selected Connect intent. Classic-only, buffer sizes sized for a
// scan plus a single ACL connection attempt -- NOT tuned for A2DP/LDAC
// streaming; that is M4's job and these numbers will need to grow with it.
//
// Retyped from reading (never copying) BTstack's own
// example/embedded/btstack_config.h shape and, per the bead's explicit
// instruction, USBPods' btstack_config.h (read from a scratch clone
// outside this repo tree -- copying it would inherit GPL-3 across the link
// boundary). The values below are our own choice for our own scope, not a
// transcription.
#ifndef BTSTACK_CONFIG_H
#define BTSTACK_CONFIG_H

#define ENABLE_CLASSIC
#define ENABLE_LOG_ERROR
#define ENABLE_LOG_INFO
// hci_dump_embedded_stdout.c (linked unconditionally by pico_btstack_base)
// hard-errors at compile time without this.
#define ENABLE_PRINTF_HEXDUMP

// pico-sdk's cyw43 HCI transport + async_context run loop combination
// needs these two -- millisecond timers (no embedded tick source here) and
// btstack_assert mapped onto pico-sdk's own assert().
#define HAVE_EMBEDDED_TIME_MS
#define HAVE_ASSERT

#define HCI_ACL_PAYLOAD_SIZE (1691 + 4)
#define HCI_OUTGOING_PRE_BUFFER_SIZE 4
// Required by pico_btstack_hci_transport_cyw43 (btstack_hci_transport_cyw43.c).
#define HCI_ACL_CHUNK_SIZE_ALIGNMENT 4

// One inquiry scan, at most one outgoing connection attempt at a time --
// this milestone never opens an L2CAP channel or SDP query, so those stay
// at the floor of 1 rather than 0 (some BTstack internals assume at least
// one service/channel slot exists).
#define MAX_NR_HCI_CONNECTIONS 1
#define MAX_NR_L2CAP_CHANNELS 1
#define MAX_NR_L2CAP_SERVICES 1
#define MAX_NR_BTSTACK_LINK_KEY_DB_MEMORY_ENTRIES 1
#define MAX_NR_WHITELIST_ENTRIES 1
#define MAX_NR_SM_LOOKUP_ENTRIES 1

// pico_btstack_classic links btstack_link_key_db_tlv.c unconditionally
// (flash-backed link key storage), which hard-errors at compile time
// without this -- one link key is all M2 needs (no pairing/bonding flow
// yet, just inquiry + a logged connect intent).
#define NVM_NUM_LINK_KEYS 1

// Flow control + buffer limits to avoid overrunning the cyw43 shared SPI
// bus -- the bead's banked hardware evidence proved the bus itself works
// (read32_swapped round-trip), but that was register-level traffic, not
// sustained HCI packet flow, so keep these conservative rather than
// assuming headroom that hasn't been measured yet.
#define MAX_NR_CONTROLLER_ACL_BUFFERS 3
#define MAX_NR_CONTROLLER_SCO_PACKETS 3
#define ENABLE_HCI_CONTROLLER_TO_HOST_FLOW_CONTROL
#define HCI_HOST_ACL_PACKET_LEN 1024
#define HCI_HOST_ACL_PACKET_NUM 3
#define HCI_HOST_SCO_PACKET_LEN 120
#define HCI_HOST_SCO_PACKET_NUM 3

#endif // BTSTACK_CONFIG_H
