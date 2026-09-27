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
// CONNECTED only becomes reachable once a2dp.c's
// A2DP_SUBEVENT_STREAM_ESTABLISHED fires (moved off STREAM_STARTED by bead
// pico-link-4vb.2 -- see that bead's bug 2), so it is exposed here rather
// than duplicated.
void pl_bt_push_link_state_connected(void);

// Pushes Event::LinkStateChanged{state: Idle} -- the disconnected
// counterpart of pl_bt_push_link_state_connected above. Bead
// pico-link-4vb.5: before this, nothing in firmware told core when a
// connected sink went away (power-off, out of range, etc.) -- BtModel's
// link_state stayed Connected forever and Home kept rendering a live
// link to hardware that was no longer there. Call from
// A2DP_SUBEVENT_SIGNALING_CONNECTION_RELEASED (a2dp.c), the point BTstack
// itself treats as the authoritative "this A2DP session is over" signal
// -- NOT from STREAM_SUSPENDED/STREAM_RELEASED, which fire on an ordinary
// pause and must not read as a disconnect.
void pl_bt_push_link_state_disconnected(void);

// Pushes Event::ConnectStepChanged(step). `step` is the raw wire value of
// ui-ffi's PlConnectStep (Connecting=0, Pairing=1, SettingUpAudio=2,
// NegotiatingCodec=3 -- see ui-ffi/src/lib.rs; cbindgen does not emit
// C constants for this enum because no FFI struct field is typed as it,
// only as a plain u32, so a2dp.c/a2dp.h define their own PL_CONNECT_STEP_*
// constants matching those discriminants exactly). `seq` (ADA DESIGN v2,
// bead pico-link-chc3) is the attempt this step belongs to -- see
// a2dp.h's PL_SEQ_ANY doc comment for the shared seq convention.
void pl_bt_push_connect_step(uint32_t step, uint16_t seq);

// Pushes Event::ConnectSucceeded{addr, degraded, seq}. `addr` added by bead
// pico-link-cz0.6 (M5 persistence, PL_EVENT_ABI_VERSION bumped 1 -> 2) so
// core's auto-reconnect persistence policy always knows which device
// succeeded, including via the PL_DEBUG_REMOTE bypass path (which never
// drives the wizard, core's only other source for this) -- see
// PlConnectSucceededPayload's doc comment in ui-ffi/src/lib.rs. `seq`
// (ADA DESIGN v2) is 0 for a session core did not initiate (remote/
// PL_DEBUG_REMOTE), else the owning attempt's seq.
void pl_bt_push_connect_succeeded(const uint8_t *addr, bool degraded, uint16_t seq);

// Pushes Event::ConnectFailed{addr, reason, seq}. `reason` is the raw wire
// value of ui-ffi's PlFailureReason (PL_FAILURE_REASON_* constants,
// generated into pico_link_ui.h since PlConnectFailedPayload::reason IS a
// real FFI field of that numeric type). `seq` (ADA DESIGN v2) is the
// owning attempt's seq, 0 if none.
void pl_bt_push_connect_failed(const uint8_t *addr, uint32_t reason, uint16_t seq);

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

// Pushes Event::VolumeChanged{level, muted, source} (bead pico-link-4v2.5,
// VT5, design section 7). `level` is volume.c's canonical 0..127 AVRCP-
// domain value, `source` is volume.h's PlVolumeSource raw value
// (0=host/1=sink/2=device -- never PL_VOLUME_SOURCE_CONSOLE=3, design
// section 7 excludes it). Call only from volume.c's apply_and_propagate,
// and only when `emit` is true.
void pl_bt_push_volume_changed(uint8_t level, bool muted, uint8_t source);

// pl_bt_push_levels_changed used to live here (bead pico-link-du0). Deleted
// by pico-link-nli.5 (G4) -- see a2dp.c's pl_a2dp_poll_levels and
// s_level_snapshot doc comments for the seqlock that replaced this ring
// push, and .planning/decisions/2026-09-03-ldac-encoder-on-core1.md sec 5
// for why (a level is not an event; the ring's drop-newest policy is wrong
// for it).

// Pushes Event::WizardAutoDismiss (no payload). Bead pico-link-4vb.2 (bug
// 3): PL_EVENT_TAG_WIZARD_AUTO_DISMISS existed in the FFI with core-side
// handling already wired (pops the wizard back to Home, but only on a
// plain non-degraded success) yet had NO producer anywhere in firmware.
// Call from a2dp.c's one-shot wizard-dismiss timer, armed a couple of
// seconds after a ConnectSucceeded push.
void pl_bt_push_wizard_auto_dismiss(void);

// Bead pico-link-4vb.7 (T3), design section 5.3's "Why the name rides on
// Connect": PL_COMMAND_TAG_CONNECT's handler (and pl_bt_debug_connect, the
// PL_DEBUG_REMOTE bypass) cache `{addr, name, name_len}` as the in-flight
// connect target. persist.c calls this to read that cached name back when
// it writes the device record (pl_persist_save_device_now, a2dp.c's
// STREAM_ESTABLISHED handler) -- at that point C has only `addr`, and this
// cache is the sole place the name it was told about at Connect time still
// lives. Writes `*out_name_len = 0` (leave `out_name` untouched -- caller
// must not read it) if no cached target matches `addr` (a stale/mismatched
// connect, or the debug-connect bypass, which never caches a name) --
// callers treat that the same as "no name to contribute" (persist.c's RMW
// convention). Safe from any context: reads happen from the
// cyw43/BTstack background async_context (persist.c's callers), writes
// from thread context (bt.c's own command handlers) -- see
// pl_bt_set_connect_target's doc comment in bt.c for why a critical
// section still guards both sides despite the two never running
// concurrently in practice today.
void pl_bt_get_connect_target_name(const uint8_t addr[6], uint8_t out_name[32], uint8_t *out_name_len);

// Bead pico-link-4vb.7 (T3), design section 5.1: pushes
// PlEventTag::PairedDeviceUpserted -- called from persist.c's
// pl_persist_do_write (the single place a device record write actually
// lands, bt.c's PL_BT_PENDING_PERSIST_WRITE drain and a2dp.c's
// STREAM_ESTABLISHED handler both funnel through it) and from bt.c's own
// boot sequence (pl_bt_init's BTSTACK_EVENT_STATE case), once per record
// persist.c loaded at boot. `name`/`name_len` follow
// PlPairedDeviceUpsertedPayload's convention (fixed 32-byte buffer, copied
// by value). `ldac_quality` (bead pico-link-7jol.5, ABI 4->5) is the
// persisted 1-based quality pick, 0 = unset -- callers must pass whatever
// persist.c's slot mirror actually holds for this record (see
// pl_persist_get_device_settings/pl_persist_boot_device_at), never a
// literal 0. `preset_id` (bead pico-link-ryw.6, ABI 5->6) is the PL:P id
// this device references, 0 (or an id the preset store no longer holds)
// meaning Off -- resolved by `core`, never by this module. Safe from IRQ or
// thread context -- routes through pl_bt_ring_push, same as every other
// push helper in this header.
void pl_bt_push_paired_device_upserted(const uint8_t addr[6], const uint8_t name[32], uint8_t name_len, uint32_t mru_seq, uint8_t ldac_quality, uint16_t preset_id);

// Bead pico-link-4vb.7 (T3), design section 5.1: pushes
// PlEventTag::PairedDeviceForgotten. Called from persist.c's
// pl_persist_forget_device on success.
void pl_bt_push_paired_device_forgotten(const uint8_t addr[6]);

// Bead pico-link-4vb.7 (T3), design section 5.1: pushes
// PlEventTag::PairedStoreFull (no payload). Called from persist.c's
// pl_persist_do_write when every slot is occupied by a different address
// (S18 -- never silently evict, so the UI must hear about a refused
// write).
void pl_bt_push_paired_store_full(void);

// Bead pico-link-ryw.6, design `.planning/design/2026-09-25-dsp-effects-
// stage.md` sec 2.2/3.2: pushes PlEventTag::PresetLoaded -- either at boot
// (bt.c's own boot sequence, one push per surviving PL:P record) or as the
// PL_COMMAND_TAG_SAVE_PRESET echo (persist.c's
// pl_persist_execute_pending_save_preset_write, the single place a preset
// write actually lands). `id` is never 0 (PL_PERSIST_PRESET_ID_NONE is
// reserved). `blob` is copied by value into the pushed PlEvent's own fixed
// buffer, same convention as pl_bt_push_codec_changed's `name` -- only the
// first `blob_len` bytes are meaningful. `blob` is a plain pointer, not a
// PL_PERSIST_PRESET_BLOB_LEN-sized array -- persist.h's constant is not
// visible from every translation unit that includes this header (e.g.
// volume.c, debug_remote.c), same "name as a pointer, copied by value
// inside" convention pl_bt_push_codec_changed's `name` param uses; the
// destination array size comes from pico_link_ui.h's PlPresetLoadedPayload
// (already visible here via the include above), not persist.h.
void pl_bt_push_preset_loaded(uint16_t id, uint8_t blob_len, const uint8_t *blob);

// Bead pico-link-ryw.6, design sec 2.4/3.2: pushes
// PlEventTag::PresetDeleted -- the PL_COMMAND_TAG_DELETE_PRESET echo, a
// real deletion persist.c's flash store performed (no echo for a
// no-op delete of an id no slot holds).
void pl_bt_push_preset_deleted(uint16_t id);

// Bead pico-link-ryw.6, design sec 2.2: pushes
// PlEventTag::PresetStoreLoaded -- the terminator of bt.c's boot-time
// `count` x pl_bt_push_preset_loaded push sequence, same shape
// pl_bt_push_store_loaded is for PairedDeviceUpserted's boot sequence.
// `next_id` added by bead pico-link-ryw.14 -- see bt.c's own doc comment.
void pl_bt_push_preset_store_loaded(uint32_t status, uint16_t count, uint16_t next_id);

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
//
// Testability follow-up (bead pico-link-chc3, code review 2026-09-27): this
// now allocates a real, distinct seq from a2dp.h's
// `PL_A2DP_DEBUG_SEQ_MIN..=PL_A2DP_DEBUG_SEQ_MAX` C-only range (bt.c's
// static counter, wrapping within the range) instead of the seq-less
// `0` it used to pass -- `0` collided with core's "not core's attempt"
// sentinel, and every debug-established session's seq was previously
// tagged with `PL_SEQ_ANY` further downstream, which is what made H4 (a
// stale cancel against an already-live debug session) untestable: the
// cancel and the session shared the exact same sentinel. Logged so a
// capture can read back which seq a given debug CONNECT got.
void pl_bt_debug_connect(const uint8_t *addr);

// Bead pico-link-nb6: debug-only direct disconnect of the current A2DP
// connection, no address needed (there is only ever one). Same context
// discipline as pl_bt_debug_connect -- thread-context caller, defers the
// real BTstack call to the heartbeat handler via the pending-action queue.
// Compiled only when PL_DEBUG_REMOTE is set; entirely absent from a
// shipping build.
void pl_bt_debug_disconnect(void);

// Bead pico-link-chc3, testability follow-up: debug-only injection of
// Command::CancelConnect exactly as core/src/render/wizard.rs's own
// NavIntent::Back handler produces it -- lets an unattended hardware test
// drive cancel-at-stage S1-S5 (design .planning/design/2026-08-30-cancel-
// connect.md) without a human at the d-pad mid-Connecting, which the
// debug-remote CONNECT path alone could never reach (it bypasses the
// wizard screen entirely, so NAV BACK right after a debug CONNECT is a
// no-op -- see this bead's own hardware-round comment on the board). No
// address needed: mirrors the real PL_COMMAND_TAG_CANCEL_CONNECT handler's
// own addr (ignored by pl_a2dp_cancel_connect -- see its doc comment), and
// pl_bt_debug_disconnect's "there is only ever one" convention. Same
// context discipline as pl_bt_debug_connect/pl_bt_debug_disconnect above.
// Compiled only when PL_DEBUG_REMOTE is set; entirely absent from a
// shipping build.
void pl_bt_debug_cancel_connect(void);

// Bead pico-link-chc3, code review 2026-09-27, testability follow-up:
// CANCELCONNECT variant taking an explicit `seq` instead of always passing
// `PL_SEQ_ANY` -- lets a hardware test target the real seq a debug CONNECT
// (see pl_bt_debug_connect above) was allocated, so H4 (a stale cancel
// against an already-live session, S5's late-success race) can be driven
// headlessly: `--cancel-connect --seq N` targets a specific debug session's
// `session_seq` via `pl_a2dp_cancel_connect`'s Match 3, instead of `PL_SEQ_
// ANY` (which deliberately never matches a live session -- see that
// function's doc comment). `pl_bt_debug_cancel_connect()` above (no seq,
// PL_SEQ_ANY) is unchanged and still the right tool for "cancel whatever is
// HELD/IN_FLIGHT" with no live session in play. Same context discipline as
// every other debug entry point in this block. Compiled only when
// PL_DEBUG_REMOTE is set; entirely absent from a shipping build.
void pl_bt_debug_cancel_connect_seq(uint16_t seq);
#endif

// Bead pico-link-cz0.6 (M5 persistence), code-review finding 1: enqueues a
// deferred flash-write request onto this file's pending-action queue
// (pico-link-ouw's idiom), so it runs from pl_bt_pending_service's
// IRQ/async_context consumer -- the same serialized execution stream
// BTstack's own link-key writes run on -- rather than persist.c's own
// thread-context caller. Called from persist.c's pl_persist_service()
// (thread context, the superloop). See persist.h's module doc
// ("Reentrancy") for the full rationale.
// Bead pico-link-j5su: returns true if the request actually landed in the
// pending-action queue (capacity 8), false if the queue was full and the
// request was dropped -- the caller (persist.c) must only latch its own
// *_write_enqueued flag on true, or a drop wedges that write kind until
// reboot (see persist.h's module doc).
bool pl_bt_enqueue_persist_write(void);

// Bead pico-link-7jol.5, generalized by pico-link-ryw.6: same idiom as
// pl_bt_enqueue_persist_write above, for persist.c's field-masked
// device-settings write (codec_id/ldac_quality/preset_id) instead of a
// pairing write -- see persist.h's doc comment on
// pl_persist_request_device_settings/pl_persist_execute_pending_device_
// settings_write for the full rationale. Called from persist.c's
// pl_persist_service() (thread context, the superloop).
// See pl_bt_enqueue_persist_write's doc comment above for the return-value
// contract (pico-link-j5su).
bool pl_bt_enqueue_device_settings_write(void);

// Bead pico-link-ryw.6: same idiom as pl_bt_enqueue_persist_write above,
// for persist.c's PL:P preset-save write -- see persist.h's doc comment on
// pl_persist_request_save_preset/pl_persist_execute_pending_save_preset_
// write for the full rationale. Called from persist.c's
// pl_persist_service() (thread context, the superloop).
// See pl_bt_enqueue_persist_write's doc comment above for the return-value
// contract (pico-link-j5su).
bool pl_bt_enqueue_save_preset_write(void);

// Bead pico-link-ryw.6: same idiom as pl_bt_enqueue_persist_write above,
// for persist.c's PL:P preset-delete write -- see persist.h's doc comment
// on pl_persist_request_delete_preset/pl_persist_execute_pending_delete_
// preset_write for the full rationale. Called from persist.c's
// pl_persist_service() (thread context, the superloop).
// See pl_bt_enqueue_persist_write's doc comment above for the return-value
// contract (pico-link-j5su).
bool pl_bt_enqueue_delete_preset_write(void);

// Bead pico-link-qivj.5 (S11): same idiom as pl_bt_enqueue_persist_write
// above, for persist.c's PL:S:0 display-settings write -- see persist.h's
// doc comment on pl_persist_request_display_settings/
// pl_persist_execute_pending_display_settings_write for the full
// rationale. Called from persist.c's pl_persist_service() (thread context,
// the superloop).
// See pl_bt_enqueue_persist_write's doc comment above for the return-value
// contract (pico-link-j5su).
bool pl_bt_enqueue_display_settings_write(void);

// Bead pico-link-8pp1.4 (S3): same idiom as pl_bt_enqueue_persist_write
// above, for persist.c's PL:S:1 cushion-policy write -- see persist.h's
// doc comment on pl_persist_request_cushion_policy/
// pl_persist_execute_pending_cushion_policy_write for the full rationale.
// Called from persist.c's pl_persist_service() (thread context, the
// superloop).
// See pl_bt_enqueue_persist_write's doc comment above for the return-value
// contract (pico-link-j5su).
bool pl_bt_enqueue_cushion_policy_write(void);

// Bead pico-link-d42g.3 (F3): same idiom as pl_bt_enqueue_persist_write
// above, for persist.c's PL:S:2 Adaptive-floor write -- see persist.h's
// doc comment on pl_persist_request_abr_floor/
// pl_persist_execute_pending_abr_floor_write for the full rationale.
// Called from persist.c's pl_persist_service() (thread context, the
// superloop).
// See pl_bt_enqueue_persist_write's doc comment above for the return-value
// contract (pico-link-j5su).
bool pl_bt_enqueue_abr_floor_write(void);

// Bead pico-link-oevr: current page-scan state and transition count, for
// a2dp.c's periodic debug report (pl_a2dp_report) to print -- so a
// hardware round reads scan state instead of inferring it. See bt.c's
// pl_bt_update_scan_mode doc comment for the ownership rule these reflect
// (connectable = no ACL up, discoverable = always off). Safe from any
// context -- recomputes from BTstack's own connection list / reads a
// single counter.
bool pl_bt_scan_connectable(void);
uint32_t pl_bt_scan_mode_changes(void);

// Bead pico-link-pigd (follow-up to pico-link-oevr's Q2): count of inbound
// classic connection requests refused by pl_bt_connection_filter because the
// remote address is not a device we have paired -- see bt.c's doc comment
// on pl_bt_connection_filter for the accept rule. For a2dp.c's periodic
// debug report (pl_a2dp_report), same idiom as pl_bt_scan_mode_changes
// above. Safe from any context -- reads a single counter.
uint32_t pl_bt_rejected_inbound_count(void);

#endif // PL_BT_H
