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
// pl_persist_service() (called from the superloop, thread context, every
// iteration) is the only place an actual flash write happens, and it gates
// on both pl_usb_audio_streaming() and pl_a2dp_streaming() being false, plus
// a 2s settle timer and a 10s global rate limit. pl_persist_flush_now()
// (forced, same streaming gate, no settle/rate-limit) is for the "flush on
// stream stop, flush before arming a stream" call sites (a2dp.c).
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

// Set by pl_persist_init() to the last-loaded device's address; only
// meaningful when pl_persist_init()'s own return-by-out-param `has_device`
// (see below) is true. Kept as file-scope query functions rather than
// threading a struct through bt.c, matching this codebase's existing
// pl_usb_audio_streaming()-style small-getter convention.
pl_persist_status_t pl_persist_boot_status(void);
bool pl_persist_boot_has_device(void);
void pl_persist_boot_device_addr(uint8_t out_addr[6]);

// Stages `addr` to be persisted as the last-used device -- called from
// bt.c's PL_COMMAND_TAG_PERSIST_DEVICE handler (Rust's auto-reconnect
// POLICY decides *when* a device is worth remembering -- on
// Event::ConnectSucceeded -- and tells C via this command; C only stages,
// gates and flushes, per design point 7). Does not write flash itself --
// see this header's module doc. Thread-context only (bt.c's poll loop).
void pl_persist_request_save_device(const uint8_t addr[6]);

// Called once per superloop iteration, thread context, unconditionally
// cheap when nothing is pending (a few volatile reads) -- performs the
// actual flash write when a save is staged AND it is safe to do so (see
// this header's module doc for the exact gate). No-op if nothing is
// pending.
void pl_persist_service(void);

// Flags any pending save as urgent -- pl_persist_service() (thread context,
// the superloop) will then write it on its very next iteration once it is
// actually safe to do so (not streaming), skipping the normal 2s settle /
// 10s rate-limit gates but NOT the streaming gate ("NO flash write while
// streaming, of any size" has no exception). This function itself does NOT
// touch flash and is safe to call from IRQ context -- a2dp.c's call sites
// (design point 4's "flush on stream stop" / "flush before arming a
// stream") run in the cyw43/BTstack background IRQ (pico-link-ouw), and the
// design's own rule is that a real flash write is "serviced from the
// superloop in THREAD context, never a packet handler" -- so this is a
// same-instant-cheap flag set, not the write itself. A no-op if nothing is
// pending.
void pl_persist_request_urgent_flush(void);

// Tag namespace, exposed so a future preset-store implementation (hardening,
// not MVP -- design point 8/pico-link-ryw) reuses this exact scheme rather
// than inventing a second one. tag = ('P'<<24)|('L'<<16)|(kind<<8)|index.
#define PL_PERSIST_KIND_MARKER 0x4Du // 'M'
#define PL_PERSIST_KIND_DEVICE 0x44u // 'D'
#define PL_PERSIST_KIND_PRESET 0x50u // 'P' -- reserved, not yet implemented

#endif // PL_PERSIST_H
