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
// pico-link-ryw 2026-09-01): a future global preset store (PL_PERSIST_KIND_PRESET,
// declared below but not yet implemented -- hardening, not MVP-blocking) and
// per-device records that hold a preset id REFERENCE
// (pl_persist_device_record_t::preset_id), so forgetting a device never
// destroys its EQ. Per-record tags, not one blob -- wear + CRC failure
// isolation + no MRU-bump rewrite storm (design point 6).
//
// TIMING is the hard constraint here, not wear (design point 4): a flash
// write is up to 3 blackouts of ~3ms each (save_and_disable_interrupts,
// since no multicore is linked -- flash_safe_execute reduces to that, see
// pico-sdk's flash.c), and usb_audio.c's ISO-OUT re-arm has a 2ms bar with
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

// Set by pl_persist_init() to the highest-`mru_seq` device loaded across all
// PL_PERSIST_DEVICE_SLOTS slots (the "most recently used" one); only
// meaningful when `pl_persist_boot_has_device()` is true. Kept as file-scope
// query functions rather than threading a struct through bt.c, matching this
// codebase's existing pl_usb_audio_streaming()-style small-getter
// convention.
//
// bead pico-link-4vb.6 (T1): this is a SINGLE-DEVICE view over what is now
// an 8-slot store, kept only because bt.c (T3, deferred) still calls it to
// build today's `PlStoreLoadedPayload{status, has_device, addr}` -- picking
// the MRU-max record preserves "auto-reconnect to the last-used device"
// exactly as before slot widening. `PlStoreLoadedPayload` itself has already
// been reshaped to `{status, count}` in this same bead (dropping
// has_device/addr) -- see ui-ffi/src/lib.rs -- so as soon as T3 lands and
// stops calling these two functions and bt.c's `pl_bt_push_store_loaded`
// stops referencing the payload's now-removed fields, these two getters
// become dead code ready to delete. Do not build new call sites on them.
pl_persist_status_t pl_persist_boot_status(void);
bool pl_persist_boot_has_device(void);
void pl_persist_boot_device_addr(uint8_t out_addr[6]);

// Stages `addr` to be persisted as the last-used device -- called from
// bt.c's PL_COMMAND_TAG_PERSIST_DEVICE handler (Rust's auto-reconnect
// POLICY decides *when* a device is worth remembering -- on
// Event::ConnectSucceeded -- and tells C via this command; C only stages,
// gates and flushes, per design point 7). Does not write flash itself --
// see this header's module doc.
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
void pl_persist_request_save_device(const uint8_t addr[6]);

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
// different address. bt.c does not yet inspect this (T3, deferred).
pl_persist_write_result_t pl_persist_execute_pending_write(void);

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
// address other than `addr` (a2dp.c does not yet inspect this, T3
// deferred); returns PL_PERSIST_WRITE_OK both when the write actually
// happened and when it was deferred to the staged/pending path instead
// (the pico-link-lmf carve-out below).
pl_persist_write_result_t pl_persist_save_device_now(const uint8_t addr[6]);

// Forgets a remembered device (bead pico-link-4vb.6 / T1, design section 6's
// "Forget" bullet): deletes its PL:D:<slot> tag AND its BTstack link key
// (S18 -- "forgetting removes the link key too"; a record without its key is
// a row that says Paired but cannot connect without re-pairing). Returns
// false, doing nothing, if no slot in the store currently holds `addr`.
//
// NOT YET WIRED into bt.c's pending queue -- that's
// PL_BT_PENDING_FORGET_DEVICE, tracked as T3 (bt.c producer work,
// deferred). This is the primitive T3's handler will call; T3 also owns
// getting `Command::ForgetDevice`'s async_context requirement satisfied
// (queueing onto bt.c's existing pending-queue/heartbeat mechanism, the
// same pico-link-ouw idiom `pl_persist_execute_pending_write` already
// uses) before ever calling this.
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

// Tag namespace, exposed so a future preset-store implementation (hardening,
// not MVP -- design point 8/pico-link-ryw) reuses this exact scheme rather
// than inventing a second one. tag = ('P'<<24)|('L'<<16)|(kind<<8)|index.
#define PL_PERSIST_KIND_MARKER 0x4Du // 'M'
#define PL_PERSIST_KIND_DEVICE 0x44u // 'D'
#define PL_PERSIST_KIND_PRESET 0x50u // 'P' -- reserved, not yet implemented

// Number of PL:D:<i> device slots the store holds, i in [0, PL_PERSIST_DEVICE_SLOTS).
// Widened from a single slot (index 0 only) to 8 by bead pico-link-4vb.6
// (T1), per design section 6 -- "PL:D:0 .. PL:D:7". PL_PERSIST_SCHEMA_VERSION
// stays 1: an existing single-slot (v1) store's slot-0 record loads cleanly
// under the new 8-slot reader (slots 1-7 simply read as absent/unoccupied).
#define PL_PERSIST_DEVICE_SLOTS 8u

#endif // PL_PERSIST_H
