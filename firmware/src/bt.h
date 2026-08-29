// Pico Link firmware -- M2: Bluetooth Classic bring-up through pico-sdk's
// own HCI transport (pico_btstack_hci_transport_cyw43 -- see bt.c's module
// doc). This header is the seam main.c uses; everything BTstack-specific
// stays inside bt.c.
#ifndef PL_BT_H
#define PL_BT_H

#include "pico_link_ui.h"

// Brings up the cyw43 radio's HCI transport and BTstack's Classic GAP
// stack, and registers the packet handler that drives the devices screen
// (pl_ui_set_link_state/pl_ui_add_device/pl_ui_clear_devices). `ui` is
// retained (not copied) for the lifetime of the firmware -- every BTstack
// callback after this call pushes state into it.
//
// Must be called exactly once, after cyw43_arch_init() has already
// succeeded and before btstack_run_loop_execute(). Does not itself power
// the radio on synchronously -- hci_power_control(HCI_POWER_ON) is
// asynchronous, and BTSTACK_EVENT_STATE / HCI_STATE_WORKING (handled
// inside bt.c) is what signals the radio is actually ready.
void pl_bt_init(struct PlUi *ui);

// Polls `ui` for one queued user command (pl_ui_poll_command) and acts on
// it: PL_COMMAND_TAG_START_SCAN clears the device list, sets the link
// state to Scanning, and starts a fresh GAP inquiry; PL_COMMAND_TAG_CONNECT
// logs the requested address and sets the link state to Connecting (M2's
// acceptance criterion is that this is observable over CDC -- actually
// opening an ACL connection is out of scope here, left for the milestone
// that does something with a successful connect); PL_COMMAND_TAG_CANCEL_SCAN
// (pico-link-znb.2) stops an in-flight GAP inquiry via gap_inquiry_stop(),
// which itself raises GAP_EVENT_INQUIRY_COMPLETE and so returns the link to
// Idle through the normal inquiry-complete path. A no-op if no command is
// queued. Intended to be called once per UI frame (see main.c's periodic
// timer) -- drains at most one command per call, so a caller that expects
// several queued commands per frame should call this in a loop instead.
void pl_bt_poll_commands(struct PlUi *ui);

// Drains every Bluetooth-domain event queued by the BTstack packet handler
// (which runs in IRQ context and only ever enqueues -- see bt.c's
// pico-link-6o2 ring doc comment) and makes the corresponding
// pl_ui_push_event calls from here, in thread context. Intended to be
// called once per superloop iteration, same convention as
// pl_link_input_poll/pl_ui_input -- call it before pl_ui_tick/pl_ui_render
// so a frame renders with the Bluetooth events that arrived before it, not
// one frame late.
void pl_bt_drain_events(struct PlUi *ui);

#endif // PL_BT_H
