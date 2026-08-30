// Pico Link btstack_config.h -- grown for M4 S1 (A2DP source, SBC) from its
// M2 scope of bringing the radio up through pico-sdk's own HCI transport
// (pico_btstack_hci_transport_cyw43, the ready-made piece the C-first pivot
// exists to use -- see
// .planning/decisions/2026-08-27-c-first-pico-sdk-owns-main.md). M2 sized
// this file for inquiry plus a single logged connect intent; M4 needs a real
// signalling (AVDTP) channel, a media (AVDTP) channel, AVRCP, and SDP --
// see .planning/design/2026-08-29-a2dp-source-pipeline.md sec 9, which
// names MAX_NR_L2CAP_CHANNELS/MAX_NR_L2CAP_SERVICES as "definitely too
// small" at their old value of 1.
//
// Retyped from reading (never copying) BTstack's own
// example/embedded/btstack_config.h shape and a2dp_source_demo.c, and per
// the bead's explicit instruction, USBPods' btstack_config.h (read from a
// scratch clone outside this repo tree -- copying it would inherit GPL-3
// across the link boundary). The values below are our own choice for our
// own scope, not a transcription. Per design sec 9: BTstack #errors loudly
// by name for anything missing, so most of these were found by letting the
// build fail and reading the error -- EXCEPT the ACL buffer counts
// (MAX_NR_CONTROLLER_ACL_BUFFERS / HCI_HOST_ACL_PACKET_NUM), which throttle
// SILENTLY (media-send backlog / pkt_fail, not a compile error) and were
// raised from M2's inquiry-only floor of 3 on that basis, not measurement
// yet -- sec 12.5 leaves the exact right number as an open hardware
// question for a future tuning pass.
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

// One ACL connection at a time (still true for M4: one paired sink). L2CAP
// channels now need to cover AVDTP signalling + AVDTP media + AVRCP (target
// and controller) concurrently on that one connection, plus SDP queries
// made against the sink while pairing -- design sec 9. L2CAP services
// (registered PSMs) cover AVDTP, AVRCP, and SDP itself.
#define MAX_NR_HCI_CONNECTIONS 1
#define MAX_NR_L2CAP_CHANNELS 6
#define MAX_NR_L2CAP_SERVICES 4
#define MAX_NR_BTSTACK_LINK_KEY_DB_MEMORY_ENTRIES 1
#define MAX_NR_WHITELIST_ENTRIES 1
#define MAX_NR_SM_LOOKUP_ENTRIES 1

// pico_btstack_classic links btstack_link_key_db_tlv.c unconditionally
// (flash-backed link key storage), which hard-errors at compile time
// without this -- one link key is all this project needs (one paired sink
// at a time, no multi-device bonding yet).
#define NVM_NUM_LINK_KEYS 1

// --- M4 additions: AVDTP/A2DP/AVRCP/SDP (design sec 9) ---
//
// One stream endpoint per codec_table.c row (S1: SBC only, so 1 -- table
// grows, this must grow with it per design sec 4.1), one AVDTP connection
// (the single paired sink), one A2DP-source-side connection tracked
// implicitly through it, and AVRCP target + controller connections for the
// same sink (registered per a2dp_source_demo.c's SDP/service shape -- see
// a2dp.c's module doc for why AVRCP is plumbed even though S1 doesn't act
// on transport controls yet).
// pico-link-cz0.5.5 (LDAC L2): raised from 1 -- codec_table.c still has
// only the SBC row, but MAX_NR_AVDTP_STREAM_ENDPOINTS bounds how many rows
// a2dp.c's endpoint-registration loop (pl_a2dp_init) can ever create, and
// this bead's whole point is switching that loop over to explicit
// negotiation ahead of L3 adding the LDAC row. Bump it now, not when L3
// lands, so this bead can be built and reasoned about against its real
// target shape.
#define MAX_NR_AVDTP_STREAM_ENDPOINTS 2
#define MAX_NR_AVDTP_CONNECTIONS 1
#define MAX_NR_AVRCP_CONNECTIONS 1

// SDP: four service records at boot (A2DP Source, AVRCP Target, AVRCP
// Controller, Device ID -- matching a2dp_source_demo.c's
// a2dp_source_and_avrcp_services_init()), plus enough query buffer space
// for one on-demand SDP client query issued against the sink we're
// connecting to (a2dp_source_establish_stream drives this internally to
// discover the sink's AVDTP PSM).
#define ENABLE_SDP
#define SDP_CLIENT_MAX_ATTRIBUTE_VALUE_SIZE 512

// Flow control + buffer limits to avoid overrunning the cyw43 shared SPI
// bus. M2's floor of 3 was sized for inquiry-only traffic; sustained A2DP
// media (one SBC media packet roughly every ~13ms, design sec 3.3) wants
// more outstanding buffers so the link doesn't stall waiting on ACL credit
// -- design sec 9 says start at 8 and tune against pkt_fail, since this is
// the one class of limit that throttles SILENTLY rather than #error-ing at
// compile time.
#define MAX_NR_CONTROLLER_ACL_BUFFERS 8
#define MAX_NR_CONTROLLER_SCO_PACKETS 3
#define ENABLE_HCI_CONTROLLER_TO_HOST_FLOW_CONTROL
#define HCI_HOST_ACL_PACKET_LEN 1024
#define HCI_HOST_ACL_PACKET_NUM 8
#define HCI_HOST_SCO_PACKET_LEN 120
#define HCI_HOST_SCO_PACKET_NUM 3

// pico-link-cz0.5.5 (LDAC L2): defined as of this bead -- switches off
// BTstack's implicit SBC-hardcoded auto-selection (a2dp.c:591-626 in the
// vendored BTstack source) and parks its state machine at
// A2DP_DISCOVERY_DONE after A2DP_SUBEVENT_SIGNALING_CAPABILITIES_COMPLETE,
// waiting for us to call a2dp_source_set_config_*. Our a2dp.c now handles
// that event and runs the preference-ordered codec_table.c walk -- design
// sec 4.1, .planning/design/2026-08-30-ldac.md Q2/Q5.
#define ENABLE_A2DP_EXPLICIT_CONFIG

#endif // BTSTACK_CONFIG_H
