#ifndef PL_PERSIST_H
#define PL_PERSIST_H

#include <stdbool.h>
#include <stdint.h>

// Pico Link firmware -- M5 persistence (bead pico-link-cz0.6), design of
// record: pico-link-cz0.6.1's closed-bead comment (Ada, 2026-08-30).
//
// Owns flash-backed storage for BTstack's link-key DB and this project's own
// device/preset records, ALL through one shared btstack_tlv_flash_bank
// instance over an 8KB window near (not at) the end of flash --
// 0x00FFD000-0x00FFF000 on this board's 16MB flash, two 4KB banks. NOT
// pico_flash_bank_instance()'s literal default (the last 8KB,
// 0x00FFE000-0x01000000): firmware/CMakeLists.txt overrides
// PICO_FLASH_BANK_STORAGE_OFFSET one sector earlier than that, deliberately
// leaving the true last 4KB sector (0x00FFF000-0x01000000) untouched -- see
// that override's own comment for why (a 256-byte picotool/RP2350 UF2
// metadata block lives at a fixed address inside that last sector on every
// build and IS rewritten by every reflash, which the stock default's second
// bank would otherwise share a sector with). See
// btstack_flash_bank.c/pico/btstack_flash_bank.h for the mechanism. Distinct tag
// namespace from BTstack's own (BTL/BTD/BTC): ours are 'P','L',kind,index --
// collision-impossible by construction (design point 1).
//
// TWO STORES, not one blob (design point 2, Andreas's constraint from
// pico-link-ryw 2026-09-01): a global preset store (PL_PERSIST_KIND_PRESET,
// bead pico-link-ryw.6, design `.planning/design/2026-09-25-dsp-effects-
// stage.md` sec 2) and per-device records that hold a preset id REFERENCE
// (pl_persist_device_record_t::preset_id), so forgetting a device never
// destroys its EQ. Per-record tags, not one blob -- wear + CRC failure
// isolation + no MRU-bump rewrite storm (design point 6).
//
// TIMING is the hard constraint here, not wear (design point 4): a flash
// write is up to 3 blackouts of ~3ms each (save_and_disable_interrupts on
// this core, plus -- as of bead pico-link-nli.2 -- a bounded
// multicore_lockout handshake with core1 if and when core1 is running; see
// flash_lockout.c. Single-core today: core1 is not launched until
// pico-link-nli.4 (G3), so the handshake is inert and this reduces to
// exactly the pre-multicore behaviour, see pico-sdk's flash.c), and
// usb_audio.c's ISO-OUT re-arm has a 2ms bar with
// exactly ONE missed re-arm being PERMANENT (audio_device.c:759-762). So
// this module NEVER writes flash while USB audio or A2DP is streaming --
// gated on both pl_usb_audio_streaming() and pl_a2dp_streaming() being
// false, plus a 2s settle timer and a 10s global rate limit for an ordinary
// save, or just the streaming gate for an urgent one (see
// pl_persist_request_urgent_flush()).
//
// REENTRANCY (code-review finding 1, 2026-09-01, fixed on bd-pico-link-cz0.6):
// this module shares its one btstack_tlv_flash_bank instance with BTstack's
// own link-key DB, which BTstack writes to synchronously from inside its
// own HCI event dispatch (hci.c's put_link_key) -- running on the
// cyw43/BTstack background async_context, a real IRQ, not a poll. The TLV
// store's own store_tag/get_tag/delete_tag have no locking of their own
// (btstack_tlv_flash_bank.c), so ANY call into them from thread context
// races that IRQ. Every actual write in this module therefore happens on
// that SAME async_context -- never thread context -- via one of two paths:
// pl_persist_execute_pending_write() (bt.c's pending-queue drain, the
// pico-link-ouw idiom, for low-value deferred writes) or
// pl_persist_save_device_now() (called directly from a2dp.c, which already
// runs on that context -- see the ORDERING section below and that
// function's own doc comment). Thread context (pl_persist_service(), the
// superloop) only ever DECIDES when a deferred write is due and enqueues a
// request via pl_bt_enqueue_persist_write() (bt.h); it never touches flash
// itself. pl_persist_init() is the one exception to "async_context only",
// and is safe for a structural reason, not a lock -- see its own doc
// comment in persist.c.
//
// ORDERING (Andreas's ruling, 2026-09-01, follow-up to finding 1's fix):
// the device record is written as part of ESTABLISHING the connection, not
// staged in RAM for a quiet window that may never come. pl_persist_save_device_now()
// is called synchronously from a2dp.c's A2DP_SUBEVENT_STREAM_ESTABLISHED
// handler, BEFORE the stream state flips to PRIMING -- i.e. before
// pl_a2dp_streaming() can become true for this connection and before any
// audio has started toward the headphones. A short delay before first
// sound (this write's ~9ms worst case) is invisible; a lost pairing costs
// a physical headphone factory reset (pico-link-7ur), which is why this
// bead was promoted to P1. THE ONE CARVE-OUT: if pl_usb_audio_streaming()
// is already true at that moment (the USB host was already sending
// isochronous audio when pairing completed), ISO-OUT is live and a missed
// re-arm is PERMANENT (audio_device.c:759-762) -- that case falls back to
// the conservative RAM-staged path instead of writing immediately,
// tracked as pico-link-lmf (not fixed here, deliberately not made worse).
// The 2s settle / 10s PL_PERSIST_MIN_INTERVAL_US rate limits still apply
// to LATER, low-value updates (volume, codec, MRU bumps) via
// pl_persist_service() -- they no longer gate the pairing-time record.
//
// KNOWN GAP, not fixed here (code review finding 2, 2026-09-01): nothing in
// core (home.rs/app.rs's Bluetooth-menu-row and Scan-row activation) gates
// opening the pairing wizard on the current link_state, so a user CAN scan
// and pair a second device while already connected and streaming to a
// first. BTstack's own put_link_key call for that new pairing is
// unconditional and NOT gated by pl_a2dp_streaming() -- this bead turned it
// from a no-op (hci_set_link_key_db was never wired before) into a real
// flash write, so this is new exposure this bead introduces, not
// pre-existing. Fixing it is a UX/product decision (block or warn on
// scan-while-connected), not a persistence-layer one -- left for a
// follow-up bead.
void pl_persist_init(void);

// Blank vs corrupt, distinguished at this layer (design point 5 -- upstream
// BTstack's own get_tag return value conflates them): PL_PERSIST_STATUS_FIRST_BOOT
// means the PL:M:0 marker tag was never written (a genuinely fresh store);
// PL_PERSIST_STATUS_RECORD_CORRUPT means the marker was fine but the device
// record's CRC16 failed (that ONE record is dropped, not the whole store);
// PL_PERSIST_STATUS_VERSION_MISMATCH means the marker's schema version byte
// doesn't match this firmware's (our own records are dropped, but BTstack's
// link-key tags are UNTOUCHED -- separate tag namespace, nothing here ever
// deletes a BT* tag); PL_PERSIST_STATUS_LOADED means a valid device record
// was found. Surfaced to the UI (via bt.c's PlEventTag::StoreLoaded) so a
// reset store never renders identically to a healthy one that just has no
// device yet.
typedef enum {
    PL_PERSIST_STATUS_FIRST_BOOT = 0,
    PL_PERSIST_STATUS_LOADED = 1,
    PL_PERSIST_STATUS_RECORD_CORRUPT = 2,
    PL_PERSIST_STATUS_VERSION_MISMATCH = 3,
} pl_persist_status_t;

// Result of an actual (attempted) flash write -- bead pico-link-4vb.6 (T1),
// widening the store from 1 slot to PL_PERSIST_DEVICE_SLOTS with NO eviction
// (design `.planning/design/2026-09-01-remembered-devices.md` section 6):
// once every slot is occupied by a DIFFERENT address than the one being
// saved, the write is refused outright rather than evicting anything. This
// return value is how that refusal is signalled up to a caller -- today
// `pl_persist_execute_pending_write`/`pl_persist_save_device_now` are that
// caller and both bt.c and a2dp.c currently discard it (their call sites are
// bare statements, which still compiles fine against a non-void return); the
// deferred T3 bt.c wiring is what will actually inspect this and push
// `PlEventTag::PairedStoreFull` to core. Until then a STORE_FULL result is
// only observable via the `pl_log` line `pl_persist_do_write` emits.
typedef enum {
    PL_PERSIST_WRITE_OK = 0,
    PL_PERSIST_WRITE_STORE_FULL = 1,
} pl_persist_write_result_t;

pl_persist_status_t pl_persist_boot_status(void);

// Bead pico-link-4vb.7 (T3), design section 5.2/8: replaces the old
// single-device `pl_persist_boot_has_device()`/`pl_persist_boot_device_addr()`
// pair (deleted -- they were a SINGLE-DEVICE view that made "auto-reconnect
// target" C's decision; that policy now belongs to `core`, computed as
// `paired.iter().max_by_key(|d| d.mru_seq)` once every loaded record has
// been folded into `core`'s `paired` list). bt.c's boot sequence
// (`pl_bt_init`'s `BTSTACK_EVENT_STATE`/`HCI_STATE_WORKING` case) calls
// these to push one `PairedDeviceUpserted` per surviving record, THEN
// `StoreLoaded{status, count}` as the terminator -- see
// `pl_persist_init()`'s doc comment for where this snapshot is populated.
uint8_t pl_persist_boot_device_count(void);
// `out_ldac_quality` added by bead pico-link-7jol.5: the persisted 1-based
// quality pick (0 = unset) from this record's slot mirror, same value
// pl_persist_get_device_settings would return for this address -- needed
// so bt.c's boot-restore PairedDeviceUpserted echo carries the real value
// instead of silently zero-initialising it. `out_preset_id` added by bead
// pico-link-ryw.6, same reasoning: 0 or an id the preset store no longer
// holds both mean Off, resolved by `core`, never by this module (design
// `.planning/design/2026-09-25-dsp-effects-stage.md` sec 2.3).
void pl_persist_boot_device_at(uint8_t index, uint8_t out_addr[6], uint8_t out_name[32], uint8_t *out_name_len, uint32_t *out_mru_seq, uint8_t *out_ldac_quality, uint16_t *out_preset_id);

// Stages `addr` (and, optionally, `name`/`name_len`) to be persisted as the
// last-used device -- called from bt.c's PL_COMMAND_TAG_PERSIST_DEVICE
// handler (Rust's auto-reconnect POLICY decides *when* a device is worth
// remembering -- on Event::ConnectSucceeded -- and tells C via this
// command; C only stages, gates and flushes, per design point 7). Does not
// write flash itself -- see this header's module doc.
//
// `name`/`name_len` added by bead pico-link-4vb.7 (T3) so the
// pico-link-lmf carve-out in `pl_persist_save_device_now()` below doesn't
// lose the in-flight connect target's name when it falls back to this
// staged path -- `name_len == 0` means "no name to contribute, leave
// whatever is already on record" (same RMW convention as
// `pl_persist_do_write`). bt.c's own PL_COMMAND_TAG_PERSIST_DEVICE handler
// (the MRU-bump-only caller, design section 7 hazard 3) passes NULL/0.
//
// # Calling contract
//
// TWO valid callers, in two different contexts: bt.c's
// PL_COMMAND_TAG_PERSIST_DEVICE handler (thread context, the superloop) and
// pl_persist_save_device_now()'s pico-link-lmf carve-out (IRQ/async_context,
// when USB audio is already streaming at pairing time -- see that
// function's doc comment). The function body is wrapped in
// save_and_disable_interrupts()/restore_interrupts() specifically so both
// contexts can call it safely; do not add a third caller without checking
// that guard still suffices.
void pl_persist_request_save_device(const uint8_t addr[6], const uint8_t *name, uint8_t name_len);

// Called once per superloop iteration, thread context, unconditionally
// cheap when nothing is pending (a few volatile reads) -- decides whether a
// staged save is due (see this header's module doc for the exact gate) and,
// if so, enqueues a write request via pl_bt_enqueue_persist_write() (bt.h).
// Never touches flash directly -- see the module doc's Reentrancy section.
// No-op if nothing is pending or a request is already enqueued and awaiting
// the heartbeat.
void pl_persist_service(void);

// Flags any pending save as urgent -- the next time pl_persist_service()
// runs (thread context, the superloop) it will enqueue the write
// immediately once it is actually safe to do so (not streaming), skipping
// the normal 2s settle / 10s rate-limit gates but NOT the streaming gate
// ("NO flash write while streaming, of any size" has no exception). This
// function itself does NOT touch flash and is safe to call from IRQ
// context -- a2dp.c's call sites (design point 4's "flush on stream stop" /
// "flush before arming a stream") run in the cyw43/BTstack background IRQ
// (pico-link-ouw) -- so this is a same-instant-cheap flag set, not the
// write itself. A no-op if nothing is pending.
void pl_persist_request_urgent_flush(void);

// Performs the actual flash write for whatever save is currently staged --
// writes the marker and the device record, or bails (leaving the request
// pending) if the streaming gate has flipped true again since it was
// enqueued. See this header's module doc (Reentrancy) for the full
// rationale.
//
// # Calling contract
//
// MUST be called ONLY from bt.c's pending-queue drain (pl_bt_pending_service,
// itself only ever called from the cyw43/BTstack background async_context)
// -- never from thread context, and never directly by anything other than
// bt.c's PL_BT_PENDING_PERSIST_WRITE case. Calling this from thread context
// reintroduces the exact race code-review finding 1 closed.
//
// Returns PL_PERSIST_WRITE_OK both when a write actually happened and when
// this call was a no-op (nothing pending, or the streaming gate bounced the
// request back to pending) -- PL_PERSIST_WRITE_STORE_FULL is returned only
// when a write was actually attempted and every slot was occupied by a
// different address.
//
// Bead pico-link-4vb.7 (T3): on an actual write, pushes
// PlEventTag::PairedDeviceUpserted (via bt.h's pl_bt_push_paired_device_upserted);
// on PL_PERSIST_WRITE_STORE_FULL, pushes PlEventTag::PairedStoreFull instead
// -- see pl_persist_do_write's doc comment in persist.c for where this is
// centralized.
pl_persist_write_result_t pl_persist_execute_pending_write(void);

// Bead pico-link-ryw.6, design `.planning/design/2026-09-25-dsp-effects-
// stage.md` sec 2.3: ONE field-masked staged per-device SETTINGS write,
// replacing bead pico-link-7jol.5's bespoke `pl_persist_request_ldac_quality`
// / `pl_persist_execute_pending_ldac_quality_write` pair. The design named
// the quick fix ("a fifth bespoke staging slot plus execute_pending pair"
// for preset assignment, on top of the four -- ldac_quality, display,
// cushion, abr floor -- persist.h already had) and rejected it: this
// function absorbs what used to be the ldac_quality-only pair AND carries
// `preset_id`, so a future per-device field needs no sixth pair either,
// just one more `PL_PERSIST_DEVICE_FIELD_*` bit.
//
// `field_mask` is an OR of `PL_PERSIST_DEVICE_FIELD_*` (below) -- only the
// masked-in fields of `codec_id`/`ldac_quality`/`preset_id` are staged to
// overwrite the on-flash record; every other field (including ones NOT in
// this call's mask but already staged by an earlier, not-yet-drained call
// for the SAME address) rides through untouched, same RMW discipline
// `pl_persist_rmw` (persist.c) already applies at write time. A call for a
// DIFFERENT address supersedes whatever was staged before, same "freshest
// wins" behaviour `pl_persist_request_save_device` already has.
//
// A SEPARATE staging slot from `pl_persist_request_save_device`'s
// pairing-write one above, deliberately: mixing a settings pick into the
// pairing slot's settle/rate-limit timers would delay a "no confirm,
// applies live" UI action behind unrelated pairing-write debouncing it has
// no reason to inherit. Same short-critical-section pattern (RAM only, no
// flash) -- thread-context safe. No settle delay: a user-initiated pick or
// edit is already the debounced event (design sec 2.5: "save and assign
// are user-initiated, so they are not stream-gated"; Andreas's ryw.6
// ruling extends this to "save immediately on every value change" for the
// effects editor -- a burst of stage calls before the heartbeat drains
// coalesces to the LATEST value, same as every other staging slot in this
// file, so this never queues unboundedly no matter how fast the caller
// stages). `pl_persist_service()` (below) picks this up on the same "not
// streaming" gate every other user-initiated write in this file uses
// (design sec 2.5 / D11 precedent).
void pl_persist_request_device_settings(const uint8_t addr[6], uint8_t field_mask, uint8_t codec_id, uint8_t ldac_quality, uint16_t preset_id);

// Performs the actual flash write for whatever settings save is currently
// staged by pl_persist_request_device_settings -- same calling contract as
// pl_persist_execute_pending_write (bt.c's pending-queue drain, async_
// context ONLY). Unlike the old ldac_quality-only pair, this never needs to
// pre-read the existing record to avoid clobbering a field this call didn't
// touch -- `pl_persist_rmw`'s RMW core already starts from the existing
// record and only applies `field_mask`'s bits, so an unmasked field simply
// rides through.
void pl_persist_execute_pending_device_settings_write(void);

// Bead pico-link-qivj.5 (S11), design `.planning/design/2026-09-24-
// screensaver-dim-and-timeout.md` (bead pico-link-qivj.1 closed comment):
// reads the PL:S:0 display-settings record loaded at boot -- see
// pl_persist_init's doc comment for why this load happens BEFORE the
// PL:M:0 marker check, independently of the device-store lifecycle.
// Returns false (leaving the outputs untouched) if the record was never
// written, was the wrong length, failed its version check, or failed CRC
// -- callers (main.c) treat false as "use core's own default", same
// fallback shape as core's `DisplaySettings::from_wire`'s per-field
// fallback for an invalid mode/timeout byte.
bool pl_persist_boot_display_settings(uint8_t *mode, uint16_t *timeout_s);

// Stages a display-settings write -- called from bt.c's
// PL_COMMAND_TAG_SET_DISPLAY_SETTINGS handler (thread context, the
// superloop). Same short-critical-section RAM-only staging idiom as
// pl_persist_request_device_settings above; a SEPARATE staging slot from both
// the pairing-write slot and the per-device-settings slot (design point 11
// D9/D11: this is a global, not per-device, record).
void pl_persist_request_display_settings(uint8_t mode, uint16_t timeout_s);

// Performs the actual flash write for whatever display-settings save is
// currently staged by pl_persist_request_display_settings -- same calling
// contract as pl_persist_execute_pending_device_settings_write (bt.c's
// pending-queue drain, async_context ONLY).
//
// Bead pico-link-xcmx / this design's D11 (Andreas's ruling): deliberately
// NOT gated on pl_usb_audio_streaming()/pl_a2dp_streaming() -- this is a
// user-initiated write (the user just picked a Settings row) and, like the
// LDAC-quality write above, is allowed to skip audio rather than silently
// delay the picker's own feedback.
void pl_persist_execute_pending_display_settings_write(void);

// Bead pico-link-8pp1.4 (S3), design `.planning/design/2026-09-24-
// congestion-cushion.md` sec 4: reads the PL:S:1 cushion-policy record
// loaded at boot -- own kind index (1, under PL_PERSIST_KIND_SETTINGS),
// own version byte, loaded independently of the device-store AND of
// PL:S:0's own lifecycle (same "load before the PL:M:0 marker check"
// discipline as pl_persist_boot_display_settings above, so neither a
// first-boot early-return nor a device-schema mismatch can skip it).
// Returns false (leaving `*policy` untouched) if the record was never
// written, was the wrong length, failed its version check, or failed CRC
// -- callers (main.c) treat false as "use the compiled-in default (Low)",
// same fallback shape as pl_persist_boot_display_settings above.
bool pl_persist_boot_cushion_policy(uint8_t *policy);

// Stages a cushion-policy write -- called from bt.c's
// PL_COMMAND_TAG_SET_CUSHION_POLICY handler (thread context, the
// superloop). Same short-critical-section RAM-only staging idiom as
// pl_persist_request_display_settings above; a SEPARATE staging slot (this
// is also a global, not per-device, record -- Andreas's 2026-09-24
// ruling).
void pl_persist_request_cushion_policy(uint8_t policy);

// Performs the actual flash write for whatever cushion-policy save is
// currently staged by pl_persist_request_cushion_policy -- same calling
// contract as pl_persist_execute_pending_display_settings_write (bt.c's
// pending-queue drain, async_context ONLY).
//
// Andreas's 2026-09-24 ruling (bead pico-link-8pp1's design sec 4, "not
// gated on streaming, D11 precedent"): deliberately NOT gated on
// pl_usb_audio_streaming()/pl_a2dp_streaming() -- a user-initiated write
// may stall audio briefly and that is accepted, same discipline as
// pl_persist_execute_pending_display_settings_write above.
void pl_persist_execute_pending_cushion_policy_write(void);

// Bead pico-link-d42g.3 (F3), design `.planning/design/2026-09-25-
// adaptive-floor.md` sec 2: reads the PL:S:2 Adaptive-floor record loaded
// at boot -- own kind index (2, under PL_PERSIST_KIND_SETTINGS), own
// version byte, loaded independently of PL:S:0/PL:S:1's own lifecycles
// (same "load before the PL:M:0 marker check" discipline as
// pl_persist_boot_cushion_policy above). Returns false (leaving `*floor`
// untouched) if the record was never written, was the wrong length,
// failed its version check, or failed CRC -- callers (main.c) treat false
// as "use the compiled-in default (330 kbps)".
bool pl_persist_boot_abr_floor(uint8_t *floor);

// Stages an Adaptive-floor write -- called from bt.c's
// PL_COMMAND_TAG_SET_ABR_FLOOR handler (thread context, the superloop).
// Same short-critical-section RAM-only staging idiom as
// pl_persist_request_cushion_policy above; a SEPARATE staging slot (this
// is also a global, not per-device, record).
void pl_persist_request_abr_floor(uint8_t floor);

// Performs the actual flash write for whatever Adaptive-floor save is
// currently staged by pl_persist_request_abr_floor -- same calling
// contract as pl_persist_execute_pending_cushion_policy_write (bt.c's
// pending-queue drain, async_context ONLY).
//
// Design sec 4/D11 precedent: deliberately NOT gated on
// pl_usb_audio_streaming()/pl_a2dp_streaming() -- a user-initiated write
// may stall audio briefly and that is accepted, same discipline as
// pl_persist_execute_pending_cushion_policy_write above.
void pl_persist_execute_pending_abr_floor_write(void);

// Andreas's ruling, 2026-09-01: writes the device record SYNCHRONOUSLY, as
// part of establishing the connection -- see this header's module doc
// (ORDERING) for the full rationale and the one carve-out
// (pl_usb_audio_streaming() already true, tracked pico-link-lmf). Stages
// via pl_persist_request_save_device() and falls back to the normal
// deferred/gated path instead of writing when that carve-out applies;
// otherwise writes immediately and clears any pending/urgent/enqueued
// state for the record just written.
//
// # Calling contract
//
// MUST be called ONLY from the cyw43/BTstack background async_context --
// today that means exclusively from a2dp.c's
// A2DP_SUBEVENT_STREAM_ESTABLISHED case, BEFORE the stream state advances
// to PRIMING. Calling this from thread context reintroduces the exact race
// code-review finding 1 closed.
//
// Returns PL_PERSIST_WRITE_STORE_FULL if every slot is occupied by an
// address other than `addr` (a2dp.c does not yet inspect this); returns
// PL_PERSIST_WRITE_OK both when the write actually happened and when it was
// deferred to the staged/pending path instead (the pico-link-lmf carve-out
// below).
//
// Bead pico-link-4vb.7 (T3): reads the device's name from bt.c's in-flight
// connect-target cache (`pl_bt_get_connect_target_name`, bt.h) before
// writing -- this is the whole reason a record could never have a name
// before this bead: at the moment this function runs, C has an address and
// (until now) nothing else. On an actual write, pushes
// PlEventTag::PairedDeviceUpserted; on STORE_FULL, pushes
// PlEventTag::PairedStoreFull -- see pl_persist_do_write's doc comment in
// persist.c.
pl_persist_write_result_t pl_persist_save_device_now(const uint8_t addr[6]);

// Forgets a remembered device (bead pico-link-4vb.6 / T1, design section 6's
// "Forget" bullet): deletes its PL:D:<slot> tag AND its BTstack link key
// (S18 -- "forgetting removes the link key too"; a record without its key is
// a row that says Paired but cannot connect without re-pairing). Returns
// false, doing nothing, if no slot in the store currently holds `addr`. On
// success, pushes PlEventTag::PairedDeviceForgotten (bead pico-link-4vb.7,
// T3).
//
// Wired into bt.c's pending queue via PL_BT_PENDING_FORGET_DEVICE (bead
// pico-link-4vb.7, T3) -- bt.c's PL_COMMAND_TAG_FORGET_DEVICE handler
// enqueues onto that entry, and pl_bt_pending_service's IRQ/async_context
// drain calls this.
//
// # Calling contract
//
// MUST be called ONLY from the cyw43/BTstack background async_context, for
// the exact same reason as pl_persist_execute_pending_write /
// pl_persist_save_device_now above -- this touches the shared
// btstack_tlv_flash_bank instance directly (delete_tag), and
// gap_drop_link_key_for_bd_addr touches BTstack's own link-key DB, which
// shares that same instance (see this header's module doc). Calling this
// from thread context reintroduces the exact race code-review finding 1
// closed.
bool pl_persist_forget_device(const uint8_t addr[6]);

// Design finding 1.3 (.planning/design/2026-09-02-device-page-seam.md sec
// 1.3, bead pico-link-ay0.1): updates per-device SETTINGS on an
// ALREADY-REMEMBERED device -- `codec_id` (codec_table.h's PL_CODEC_ID_*,
// 0 = Automatic) and `ldac_quality` (1-based: 0 = unset, 1 = 990 kbps, 2 =
// 660 kbps, 3 = 330 kbps, 4 = Adaptive -- IMPLEMENTED, bead pico-link-7jol.3;
// NOT a raw LDACBT_EQMID_* value, since LDACBT_EQMID_HQ is literally 0 and
// would make "never chosen" and "explicitly chose 990" the same byte
// forever). Bead pico-link-ryw.6 folded the actual write into a `static`
// helper inside persist.c (no longer a public function -- nothing outside
// persist.c ever called it directly; every caller goes through the staged
// pl_persist_request_device_settings/pl_persist_execute_pending_device_
// settings_write pair above) that shares ONE read-modify-write core with
// pl_persist_do_write (persist.c) rather than forking the RMW logic.
//
// Does NOT bump mru_seq: setting a preference is not using a device, and
// core's auto-reconnect policy is paired.iter().max_by_key(|d| d.mru_seq)
// -- bumping here would make a pinned-but-unconnected device the boot
// reconnect target.
//
// Does NOT create a slot: refuses, writing nothing, if no slot currently
// holds `addr`. The device page is only reachable for a remembered device
// or the connected one (a connected device is written at pairing time via
// pl_persist_save_device_now/pl_persist_request_save_device) -- there is no
// legitimate "pin a codec (or assign a preset) on a device we've never
// stored" path, and inventing one would let a pin consume one of
// PL_PERSIST_DEVICE_SLOTS slots without pairing.
//
// On an actual write, pushes PlEventTag::PairedDeviceUpserted, same as
// every other successful write in this file (design point 3's single-writer
// rule: no echo means no row) -- see pl_persist_do_write's doc comment.

// Design finding 1.4 (.planning/design/2026-09-02-device-page-seam.md sec
// 1.4, bead pico-link-ay0.1): reads the LIVE in-RAM slot mirror -- no flash
// access, no allocation. Kept live by every write (pl_persist_do_write and
// the field-masked device-settings write above both funnel through the
// same RMW core, which updates this mirror as part of the same write), NOT
// just populated once at boot -- so a pin set now is visible to a2dp.c's
// next connection attempt immediately, not only after a power cycle (the
// exact failure this bead exists to prevent). Returns false, leaving the
// outputs untouched, if `addr` is not currently remembered.
//
// Safe to call from the cyw43/BTstack background async_context (a2dp.c's
// CAPABILITIES_COMPLETE handler, design sec 3.3) -- the mirror is only ever
// mutated on that same async_context, so this is an uncontended same-context
// read, not a cross-context one.
bool pl_persist_get_device_settings(const uint8_t addr[6], uint8_t *out_codec_id, uint8_t *out_ldac_quality);

// Bead pico-link-ryw.6, design sec 2.3: field-mask bits for
// pl_persist_request_device_settings/pl_persist_execute_pending_device_
// settings_write's shared staging slot -- which of
// pl_persist_device_record_t's Tier-2 fields a given staged write should
// overwrite. OR multiple bits together to write more than one field in the
// same flash write (e.g. a future combined codec+quality pick).
typedef enum {
    PL_PERSIST_DEVICE_FIELD_CODEC_ID = 1u << 0,
    PL_PERSIST_DEVICE_FIELD_LDAC_QUALITY = 1u << 1,
    // Bead pico-link-ryw.6: PL_COMMAND_TAG_ASSIGN_PRESET writes preset_id
    // through this same field-masked path -- see persist.h's doc comment
    // on pl_persist_request_device_settings.
    PL_PERSIST_DEVICE_FIELD_PRESET_ID = 1u << 2,
} pl_persist_device_field_mask_t;

// Tag namespace. tag = ('P'<<24)|('L'<<16)|(kind<<8)|index.
#define PL_PERSIST_KIND_MARKER 0x4Du // 'M'
#define PL_PERSIST_KIND_DEVICE 0x44u // 'D'
// Bead pico-link-ryw.6, design `.planning/design/2026-09-25-dsp-effects-
// stage.md` sec 2.2: PL:P:<slot>, the global DSP-preset store. Opaque to C
// (this module stores and moves the blob bytes, never parses them -- `core`
// owns the wire format, same discipline as every PL:S:<i> value byte
// above). Own version byte (PL_PERSIST_PRESET_VERSION in persist.c), own
// slot count (PL_PERSIST_PRESET_SLOTS below) -- independent of
// PL_PERSIST_DEVICE_SLOTS, and loaded/wiped alongside the device slots on a
// PL_PERSIST_SCHEMA_VERSION mismatch (both are "our own records", per this
// header's module doc).
#define PL_PERSIST_KIND_PRESET 0x50u // 'P'
// Bead pico-link-qivj.5 (S11): PL:S:0, the global display-settings record
// (screensaver mode + idle timeout) -- its own kind byte, its own version
// byte (see PL_PERSIST_SETTINGS_VERSION in persist.c), loaded independently
// of PL_PERSIST_KIND_MARKER/PL_PERSIST_KIND_DEVICE's lifecycle so a
// device-store first-boot or version-mismatch can never wipe it.
#define PL_PERSIST_KIND_SETTINGS 0x53u // 'S'
// Bead pico-link-8pp1.4 (S3): PL:S:1, a SECOND, independent record under
// the same PL_PERSIST_KIND_SETTINGS kind byte -- the global congestion-
// cushion policy. Its own index (1, not 0 -- PL:S:0 above is the display-
// settings record, unrelated), its own version byte
// (PL_PERSIST_CUSHION_POLICY_VERSION in persist.c). Do NOT bump
// PL_PERSIST_SETTINGS_VERSION for this record -- that would needlessly
// couple this record's format to the display-settings one's; each PL:S:<i>
// record versions itself independently, same discipline PL:S:0 already
// established relative to PL_PERSIST_SCHEMA_VERSION (the device/marker
// schema).
#define PL_PERSIST_INDEX_CUSHION_POLICY 1u
// Bead pico-link-d42g.3 (F3): PL:S:2, a THIRD, independent record under
// PL_PERSIST_KIND_SETTINGS -- the global LDAC Adaptive floor. Own index
// (2), own version byte (PL_PERSIST_ABR_FLOOR_VERSION in persist.c), same
// independent-versioning discipline as PL_PERSIST_INDEX_CUSHION_POLICY
// above.
#define PL_PERSIST_INDEX_ABR_FLOOR 2u

// Number of PL:D:<i> device slots the store holds, i in [0, PL_PERSIST_DEVICE_SLOTS).
// Widened from a single slot (index 0 only) to 8 by bead pico-link-4vb.6
// (T1), per design section 6 -- "PL:D:0 .. PL:D:7". PL_PERSIST_SCHEMA_VERSION
// stays 1: an existing single-slot (v1) store's slot-0 record loads cleanly
// under the new 8-slot reader (slots 1-7 simply read as absent/unoccupied).
#define PL_PERSIST_DEVICE_SLOTS 8u

// Bead pico-link-ryw.6, design sec 2.2: number of PL:P:<slot> preset slots
// -- independent budget from PL_PERSIST_DEVICE_SLOTS (a preset is not a
// device). "Live contents: ... 8 presets at about 90B ... for about 1.5KB
// of 4KB" (design sec 2.5).
#define PL_PERSIST_PRESET_SLOTS 8u

// Bead pico-link-ryw.6, design sec 2.2: the fixed on-flash preset blob
// width, matching ui-ffi's PL_DSP_PRESET_BLOB_LEN (80) -- an independent
// literal, not a shared constant, same "own reservation, `core`'s own wire
// length may grow within it without an ABI bump" convention
// PL_DSP_PRESET_BLOB_LEN's own doc comment documents on the Rust side.
#define PL_PERSIST_PRESET_BLOB_LEN 80u

// Bead pico-link-ryw.6, design sec 2.2: NO_PRESET_ID -- 0 always means "no
// preset assigned" (never allocated to a real preset), matching
// pico_link_core::dsp::store::NO_PRESET_ID and
// pl_persist_device_record_t::preset_id's own "0 means none" convention.
#define PL_PERSIST_PRESET_ID_NONE 0u

// Bead pico-link-ryw.6, design sec 2.2: how many valid PL:P records are
// currently loaded in the live slot mirror -- computed live, same
// "boot-time snapshot built once, before any write can run" contract as
// pl_persist_boot_device_count.
uint8_t pl_persist_boot_preset_count(void);

// Bead pico-link-ryw.6, design sec 2.2: the `index`-th (0-based, in slot
// order) occupied preset slot's id and blob -- same "seen == index" walk as
// pl_persist_boot_device_at. `out_blob` must point to
// PL_PERSIST_PRESET_BLOB_LEN bytes; `out_blob_len` is how many of them are
// meaningful, the rest is unspecified padding, same convention as
// PlPresetLoadedPayload's own doc comment. Out-of-range `index` zeroes
// every output.
void pl_persist_boot_preset_at(uint8_t index, uint16_t *out_id, uint8_t *out_blob_len, uint8_t out_blob[PL_PERSIST_PRESET_BLOB_LEN]);

// Bead pico-link-ryw.6: mirrors pl_persist_boot_status but for the PL:P
// store's own load pass -- see persist.c's pl_persist_init for exactly what
// this reflects (first-boot / loaded / a per-record CRC drop / a
// PL_PERSIST_SCHEMA_VERSION mismatch, same statuses as the device store,
// since presets are wiped alongside devices on a schema mismatch -- see
// PL_PERSIST_KIND_PRESET's doc comment above).
pl_persist_status_t pl_persist_preset_boot_status(void);

// Bead pico-link-ryw.6, design sec 2.2/2.5: stages a preset save --
// `preset_id == PL_PERSIST_PRESET_ID_NONE` (0) means "allocate a fresh,
// never-before-used id" (ids are monotonic and never reused, design sec
// 2.2: "a delete-then-create in the same slot gets a new id, and a stale
// device reference dangles rather than aliasing another EQ"); a non-zero
// id means "overwrite the existing preset with this id in place". Same
// short-critical-section RAM-only staging idiom as
// pl_persist_request_device_settings -- a burst of edits (Andreas's ryw.6
// ruling: "save immediately on every value change") coalesces to the
// LATEST blob, never queues unboundedly. `blob_len` bytes of `blob` are
// staged; the rest is not read. Called from bt.c's
// PL_COMMAND_TAG_SAVE_PRESET handler (thread context, the superloop).
void pl_persist_request_save_preset(uint16_t preset_id, uint8_t blob_len, const uint8_t *blob);

// Performs the actual flash write for whatever save is currently staged by
// pl_persist_request_save_preset -- same calling contract as
// pl_persist_execute_pending_write (bt.c's pending-queue drain, async_
// context ONLY). On an actual write (a fresh allocation, or overwriting an
// existing id in place), pushes PlEventTag::PresetLoaded with whichever id
// was actually used -- the SavePreset echo (design sec 2.2/3.2), same
// single-writer-echo discipline as every other write in this file. If
// `preset_id` was non-zero but no slot currently holds it, or a fresh
// allocation finds every PL_PERSIST_PRESET_SLOTS slot occupied, the write
// is refused and logged -- no echo, no row, same "no echo means no row"
// rule pl_persist_do_write's doc comment states for devices.
void pl_persist_execute_pending_save_preset_write(void);

// Bead pico-link-ryw.6, design sec 2.4: stages a preset delete. Same
// short-critical-section RAM-only staging idiom as
// pl_persist_request_save_preset. Called from bt.c's
// PL_COMMAND_TAG_DELETE_PRESET handler (thread context, the superloop).
void pl_persist_request_delete_preset(uint16_t preset_id);

// Performs the actual flash delete for whatever request is currently
// staged by pl_persist_request_delete_preset -- same calling contract as
// pl_persist_execute_pending_write (bt.c's pending-queue drain, async_
// context ONLY). Design sec 2.4: deletes ONLY the PL:P:<slot> tag -- never
// rewrites any PL:D device record, so a device still referencing this id
// keeps a dangling reference that `core` resolves as Off (never
// garbage-collected back onto another preset -- design sec 2.4: "NEVER
// garbage-collect a preset because its last device was forgotten" extends
// symmetrically to "because the preset itself was deleted"). Pushes
// PlEventTag::PresetDeleted on an actual deletion only -- a delete request
// for an id no slot currently holds is a silent no-op (matches
// pl_persist_forget_device's own "no slot holds it -- no-op" contract).
void pl_persist_execute_pending_delete_preset_write(void);

#endif // PL_PERSIST_H
