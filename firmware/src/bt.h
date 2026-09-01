// Pico Link firmware -- M2: Bluetooth Classic bring-up through pico-sdk's
// own HCI transport (pico_btstack_hci_transport_cyw43 -- see bt.c's module
// doc). This header is the seam main.c uses; everything BTstack-specific
// stays inside bt.c.
#ifndef PL_BT_H
#define PL_BT_H

#include <stdbool.h>
#include <stdint.h>

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

// --- M4 S1 additions (bead pico-link-cz0.5.2): let a2dp.c push events
// through this file's existing MPSC ring (see bt.c's pico-link-6o2 doc
// comment) rather than duplicating that machinery. a2dp.c's A2DP/AVRCP
// packet handler runs in the same IRQ context (cyw43/BTstack background,
// 0xFF) bt.c's own HCI packet handler does, so pushing into the same ring
// from there is exactly the MPSC case that ring already handles.

// Pushes Event::LinkStateChanged{state: Connected} -- PL_LINK_STATE_IDLE/
// SCANNING/CONNECTING are already reachable via bt.c's own call sites;
// CONNECTED only becomes reachable once a2dp.c's A2DP_SUBEVENT_STREAM_STARTED
// fires (design sec 4.3), so it is exposed here rather than duplicated.
void pl_bt_push_link_state_connected(void);

// Pushes Event::ConnectStepChanged(step). `step` is the raw wire value of
// ui-ffi's PlConnectStep (Connecting=0, Pairing=1, SettingUpAudio=2,
// NegotiatingCodec=3 -- see ui-ffi/src/lib.rs; cbindgen does not emit
// C constants for this enum because no FFI struct field is typed as it,
// only as a plain u32, so a2dp.c/a2dp.h define their own PL_CONNECT_STEP_*
// constants matching those discriminants exactly).
void pl_bt_push_connect_step(uint32_t step);

// Pushes Event::ConnectSucceeded{addr, degraded}. `addr` added by bead
// pico-link-cz0.6 (M5 persistence, PL_EVENT_ABI_VERSION bumped 1 -> 2) so
// core's auto-reconnect persistence policy always knows which device
// succeeded, including via the PL_DEBUG_REMOTE bypass path (which never
// drives the wizard, core's only other source for this) -- see
// PlConnectSucceededPayload's doc comment in ui-ffi/src/lib.rs.
void pl_bt_push_connect_succeeded(const uint8_t *addr, bool degraded);

// Pushes Event::ConnectFailed{addr, reason}. `reason` is the raw wire
// value of ui-ffi's PlFailureReason (PL_FAILURE_REASON_* constants,
// generated into pico_link_ui.h since PlConnectFailedPayload::reason IS a
// real FFI field of that numeric type).
void pl_bt_push_connect_failed(const uint8_t *addr, uint32_t reason);

// Pushes Event::CodecChanged{addr, word, nominal_bitrate_bps} (bead
// pico-link-1v5: the Home hero's live codec/bitrate). `name`/`name_len`
// are the codec table row's `display_name` (codec_table.h) and its
// length; truncated (never overrun) to PlCodecChangedPayload::name's fixed
// capacity if longer, matching every codec name this table declares
// today. Unlike pl_bt_push_device_discovered's borrowed pointer, this
// payload's name field is a fixed-size buffer copied by value into the
// PlEvent itself, so no separate ring name-buffer patching is needed at
// drain time -- see PlCodecChangedPayload's doc comment in
// pico_link_ui.h. Call only from the signaling codec-configuration
// handler (a2dp.c), never from the media timer path.
void pl_bt_push_codec_changed(const uint8_t *addr, const char *name, uint8_t name_len, uint32_t nominal_bitrate_bps);

#ifdef PL_DEBUG_REMOTE
// Bead pico-link-g48: debug-only direct connect to a host-supplied
// BD_ADDR, bypassing GAP inquiry/discovery entirely -- lets an unattended
// test reach a specific known headset without it being discoverable.
// Mirrors bt.c's own PL_COMMAND_TAG_CONNECT handler body exactly (log the
// target address, push PL_LINK_STATE_CONNECTING for UI feedback, call
// pl_a2dp_connect) rather than duplicating that logic; the address itself
// is never stored anywhere in this codebase -- it comes from
// debug_remote.c's "CONNECT <addr>" line, which comes from a host-side
// CLI argument (tools/usb-console/cdc_sender.py --connect), never a
// constant. Thread-context only (called from the superloop via
// pl_debug_remote_poll, same convention as the normal command path via
// pl_bt_poll_commands). Compiled only when PL_DEBUG_REMOTE is set (see
// firmware/CMakeLists.txt) -- entirely absent from a shipping build.
void pl_bt_debug_connect(const uint8_t *addr);

// Bead pico-link-nb6: debug-only direct disconnect of the current A2DP
// connection, no address needed (there is only ever one). Same context
// discipline as pl_bt_debug_connect -- thread-context caller, defers the
// real BTstack call to the heartbeat handler via the pending-action queue.
// Compiled only when PL_DEBUG_REMOTE is set; entirely absent from a
// shipping build.
void pl_bt_debug_disconnect(void);
#endif

// Bead pico-link-cz0.6 (M5 persistence), code-review finding 1: enqueues a
// deferred flash-write request onto this file's pending-action queue
// (pico-link-ouw's idiom), so it runs from pl_bt_pending_service's
// IRQ/async_context consumer -- the same serialized execution stream
// BTstack's own link-key writes run on -- rather than persist.c's own
// thread-context caller. Called from persist.c's pl_persist_service()
// (thread context, the superloop). See persist.h's module doc
// ("Reentrancy") for the full rationale.
void pl_bt_enqueue_persist_write(void);

#endif // PL_BT_H
