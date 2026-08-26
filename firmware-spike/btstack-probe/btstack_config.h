// Minimal btstack_config.h for the pico-link-8v3.1 spike.
// Goal: prove BTstack cross-compiles + links against a Rust-owned binary,
// NOT a production config. Classic-only, inquiry-sized buffers.
#ifndef BTSTACK_CONFIG_H
#define BTSTACK_CONFIG_H

#define HAVE_EMBEDDED_TIME_MS

#define ENABLE_CLASSIC
#define ENABLE_LOG_ERROR
#define ENABLE_LOG_INFO

#define HCI_ACL_PAYLOAD_SIZE (1691 + 4)
#define MAX_NR_HCI_CONNECTIONS 1
#define MAX_NR_L2CAP_CHANNELS 1
#define MAX_NR_L2CAP_SERVICES 1
#define MAX_NR_BTSTACK_LINK_KEY_DB_MEMORY_ENTRIES 1
#define MAX_NR_WHITELIST_ENTRIES 1
#define MAX_NR_SM_LOOKUP_ENTRIES 1

#endif
