// Pico Link firmware -- M5 persistence (bead pico-link-cz0.6). See
// persist.h's module doc for the design summary; full design of record is
// pico-link-cz0.6.1's closed-bead comment.
#include "persist.h"

#include <stddef.h>
#include <string.h>

#include "hardware/flash.h"
#include "hardware/sync.h"
#include "pico/btstack_flash_bank.h"
#include "pico/time.h"

#include "btstack.h"
#include "btstack_tlv.h"
#include "btstack_tlv_flash_bank.h"
#include "classic/btstack_link_key_db_tlv.h"

#include "a2dp.h"
#include "bt.h"
#include "usb_audio.h"
#include "usb_pump.h"

// This project's schema version for the PL:M:0 marker's stored value --
// bumped only when pl_persist_device_record_t's on-flash shape changes in a
// way old readers can't tolerate. A mismatch drops OUR records (device,
// future preset) but never touches BTstack's own BTL/BTD/BTC link-key tags
// -- separate namespace, this module never calls delete_tag on anything but
// its own PL:* tags.
#define PL_PERSIST_SCHEMA_VERSION 1u

// Settle + rate-limit timers (design point 4): a save staged by a fresh
// ConnectSucceeded waits 2s before it's eligible to actually write (in case
// a second event supersedes it almost immediately -- e.g. a fast
// reconnect-to-a-different-device churn), and no more than one write happens
// per 10s regardless of how many saves get staged in between.
#define PL_PERSIST_SETTLE_US (2ull * 1000ull * 1000ull)
#define PL_PERSIST_MIN_INTERVAL_US (10ull * 1000ull * 1000ull)

// PL_PERSIST_DEVICE_SLOTS (persist.h) is the slot count -- widened from a
// single slot (index 0 only) to 8 by bead pico-link-4vb.6 (T1), per design
// `.planning/design/2026-09-01-remembered-devices.md` section 6. The record
// shape (pl_persist_device_record_t below) is unchanged -- it already
// carried every field this needed; only how many slots get used, and which
// fields a write actually populates, changed.

static inline uint32_t pl_persist_tag(uint8_t kind, uint8_t index) {
    return ((uint32_t)'P' << 24) | ((uint32_t)'L' << 16) | ((uint32_t)kind << 8) | (uint32_t)index;
}

// --- On-flash record shapes ---

typedef struct __attribute__((packed)) {
    uint8_t schema_version;
} pl_persist_marker_t;

// ~52 bytes -- design point 4's "a 52-byte record write is up to 3
// blackouts of ~3ms" figure. `preset_id` is a REFERENCE into a future
// global preset store (design point 2, Andreas's pico-link-ryw constraint:
// "if I forget a device then the EQ will not be lost") -- 0 means "no
// preset assigned", not implemented on the read/write side yet (that store
// doesn't exist this bead), but the field's presence now is what avoids a
// format migration later.
typedef struct __attribute__((packed)) {
    uint8_t addr[6];
    uint8_t name[32];
    uint8_t name_len;
    uint8_t codec_id;
    uint8_t ldac_quality;
    uint8_t volume;
    uint8_t flags;
    uint32_t mru_seq;
    uint16_t preset_id;
    uint16_t crc16;
} pl_persist_device_record_t;

// CRC16/CCITT-FALSE (poly 0x1021, init 0xFFFF) over every field of
// pl_persist_device_record_t EXCEPT crc16 itself. A bit-loop, not a table --
// records are tiny (50 bytes) and written at most once per ~10s, so table
// memory isn't worth spending on a bare-metal build.
static uint16_t pl_persist_crc16(const uint8_t *data, size_t len) {
    uint16_t crc = 0xFFFF;
    for (size_t i = 0; i < len; i++) {
        crc ^= (uint16_t)data[i] << 8;
        for (int bit = 0; bit < 8; bit++) {
            crc = (crc & 0x8000) ? (uint16_t)((crc << 1) ^ 0x1021) : (uint16_t)(crc << 1);
        }
    }
    return crc;
}

static btstack_tlv_flash_bank_t s_tlv_context;
static const btstack_tlv_t *s_tlv_impl;

static pl_persist_status_t s_boot_status = PL_PERSIST_STATUS_FIRST_BOOT;
static bool s_boot_has_device;
static uint8_t s_boot_device_addr[6];
// Next mru_seq to stamp on a save -- seeded from whatever was loaded at
// boot (if anything) so a fresh save's mru_seq is monotonic across a
// reflash, not just within one power-on session. Bead pico-link-4vb.6 (T1):
// now the max mru_seq loaded across ALL PL_PERSIST_DEVICE_SLOTS slots, plus
// one (design section 6) -- not just slot 0's.
static uint32_t s_next_mru_seq = 1;

// In-RAM mirror of "which slots are occupied, and by which address" --
// populated by pl_persist_init()'s load loop and kept in sync by every
// write/forget after that. Exists purely to avoid re-reading flash (up to
// 8 get_tag calls) on every slot-selection decision in pl_persist_do_write/
// pl_persist_forget_device -- bead pico-link-4vb.6 (T1). Never itself
// written to flash; reconstructed fresh from the real on-flash records
// every boot in pl_persist_init(), so it can never drift from what's
// actually stored (a stale cache can only cause a spurious "occupied" read,
// self-correcting the moment get_tag itself is consulted inside
// pl_persist_do_write's own read-modify-write).
static bool s_slot_occupied[PL_PERSIST_DEVICE_SLOTS];
static uint8_t s_slot_addr[PL_PERSIST_DEVICE_SLOTS][6];

// Staged-save state (design point 4's "staged, gated, flushed" split).
static bool s_pending;
static uint8_t s_pending_addr[6];
static uint64_t s_pending_since_us;
static uint64_t s_last_write_us;
static bool s_have_last_write;
// Set by pl_persist_request_urgent_flush() (IRQ-context-safe: a plain
// monotonic bool write, no read-modify-write hazard -- see that function's
// doc comment), cleared only by pl_persist_execute_pending_write() after it
// actually writes. Lets a "flush on stream stop"/"flush before arming a
// stream" call site skip the settle/rate-limit gates without itself
// touching flash.
static volatile bool s_urgent;
// Code-review finding 1 (2026-09-01, on bd-pico-link-cz0.6): set the moment
// pl_persist_service() enqueues a write request onto bt.c's pending queue,
// cleared by pl_persist_execute_pending_write() once that write actually
// runs (or bails). Without this, pl_persist_service() -- called every
// superloop iteration while s_pending stays true -- would re-enqueue a
// PL_BT_PENDING_PERSIST_WRITE entry on every single iteration until the
// heartbeat (up to 100ms later) finally drains one, flooding bt.c's 8-slot
// pending queue and potentially crowding out a real scan/connect request.
static volatile bool s_write_enqueued;

// Unconditional (NOT #ifndef NDEBUG-gated) firmware/storage-region collision
// check -- replaces btstack_flash_bank.c:53-58's assert, which pico-sdk's
// forced-Release build (CMAKE_BUILD_TYPE unset -> NDEBUG defined) silently
// elides. Checks against PICO_FLASH_BANK_STORAGE_OFFSET as OVERRIDDEN by
// firmware/CMakeLists.txt (one sector earlier than pico_flash_bank's own
// default -- see that override's comment for the picotool/RP2350 UF2
// metadata-block collision it avoids). A collision here means the .uf2
// itself would overwrite the store on every reflash, defeating the entire
// point of this bead (the reflash requirement, not just power-cycle) -- so
// this halts loudly rather than letting persistence quietly corrupt itself.
static void pl_persist_check_no_firmware_collision(void) {
    extern char __flash_binary_end;
    uint32_t flash_binary_end_offset = (uint32_t)((uintptr_t)&__flash_binary_end - XIP_BASE);
    uint32_t storage_offset = PICO_FLASH_BANK_STORAGE_OFFSET;
    if (flash_binary_end_offset > storage_offset) {
        pl_log(
            "persist: FATAL firmware image ends at flash offset 0x%lx, past the storage region start "
            "0x%lx -- halting (see persist.c's collision check)\r\n",
            (unsigned long)flash_binary_end_offset, (unsigned long)storage_offset
        );
        while (true) {
            tight_loop_contents();
        }
    }
}

// Calls s_tlv_impl's get_tag/delete_tag directly (thread context -- called
// from pl_bt_init, itself called synchronously from main() before the
// superloop even starts) -- the ONE place in this file that's exempt from
// pl_persist_execute_pending_write's "async_context only" rule (code-review
// finding 1). Safe by construction, not by convention: this function's own
// caller (pl_bt_init) calls it BEFORE hci_power_control(HCI_POWER_ON), so
// no HCI event has fired yet and BTstack cannot have dispatched anything --
// including a put_link_key call -- through the cyw43/BTstack background
// async_context. There is nothing yet running on that queue for this call
// to race with. Every write after this point (pl_persist_execute_pending_write)
// goes through the deferred queue; this function does not, because at the
// moment it runs there is no concurrent writer to serialize against.
void pl_persist_init(void) {
    pl_persist_check_no_firmware_collision();

    s_tlv_impl = btstack_tlv_flash_bank_init_instance(&s_tlv_context, pico_flash_bank_instance(), NULL);
    btstack_tlv_set_instance(s_tlv_impl, &s_tlv_context);

    // Shares this same TLV instance with BTstack's own link-key DB, per
    // design point 1 -- distinct tag namespace (BTL/BTD/BTC vs our
    // 'P','L',kind,index), so no collision is possible.
    hci_set_link_key_db(btstack_link_key_db_tlv_get_instance(s_tlv_impl, &s_tlv_context));

    // --- Marker: distinguishes first-boot (tag absent) from a version we
    // understand vs. one we don't (design point 5) ---
    pl_persist_marker_t marker;
    int marker_len = s_tlv_impl->get_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_MARKER, 0), (uint8_t *)&marker, sizeof(marker));
    if (marker_len <= 0) {
        pl_log("persist: no PL:M:0 marker -- first boot, empty store\r\n");
        s_boot_status = PL_PERSIST_STATUS_FIRST_BOOT;
        return;
    }
    if (marker_len != (int)sizeof(marker) || marker.schema_version != PL_PERSIST_SCHEMA_VERSION) {
        pl_log(
            "persist: PL:M:0 marker version mismatch (got %d byte(s), version=%u, expected %u) -- "
            "dropping our records, link keys untouched\r\n",
            marker_len, marker_len == (int)sizeof(marker) ? marker.schema_version : 0xFFu, PL_PERSIST_SCHEMA_VERSION
        );
        for (uint8_t i = 0; i < PL_PERSIST_DEVICE_SLOTS; i++) {
            s_tlv_impl->delete_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_DEVICE, i));
        }
        s_tlv_impl->delete_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_MARKER, 0));
        s_boot_status = PL_PERSIST_STATUS_VERSION_MISMATCH;
        return;
    }

    // --- Device records: loop every slot, CRC-verified independently, drop
    // only the bad ones (design section 6's per-record isolation -- one
    // corrupt slot must not affect the others). Bead pico-link-4vb.6 (T1):
    // widened from slot 0 only to all PL_PERSIST_DEVICE_SLOTS slots.
    // PL_PERSIST_SCHEMA_VERSION stays 1, so an old single-slot store's slot 0
    // record loads here exactly as it always did -- slots 1..7 simply read
    // back "absent" (rec_len != sizeof(rec)), same as a slot that was never
    // written.
    bool any_loaded = false;
    bool any_corrupt = false;
    uint32_t max_mru_seq = 0;
    for (uint8_t slot = 0; slot < PL_PERSIST_DEVICE_SLOTS; slot++) {
        pl_persist_device_record_t rec;
        int rec_len = s_tlv_impl->get_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_DEVICE, slot), (uint8_t *)&rec, sizeof(rec));
        if (rec_len != (int)sizeof(rec)) {
            // Slot never written -- not an error, just unoccupied.
            continue;
        }
        uint16_t crc = pl_persist_crc16((const uint8_t *)&rec, offsetof(pl_persist_device_record_t, crc16));
        if (crc != rec.crc16) {
            pl_log(
                "persist: device record CRC mismatch in slot %u (got 0x%04x, computed 0x%04x) -- dropping this "
                "record only\r\n",
                slot, rec.crc16, crc
            );
            s_tlv_impl->delete_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_DEVICE, slot));
            any_corrupt = true;
            continue;
        }

        s_slot_occupied[slot] = true;
        memcpy(s_slot_addr[slot], rec.addr, 6);
        any_loaded = true;
        if (rec.mru_seq > max_mru_seq) {
            max_mru_seq = rec.mru_seq;
            // Bead pico-link-4vb.6 (T1): the highest-mru_seq loaded record
            // is still exposed as "the boot device" via
            // pl_persist_boot_has_device()/pl_persist_boot_device_addr()
            // purely for bt.c's (T3, deferred) existing single-device
            // reconnect call site -- see those getters' doc comments in
            // persist.h.
            memcpy(s_boot_device_addr, rec.addr, 6);
            s_boot_has_device = true;
        }
        pl_log(
            "persist: loaded device slot=%u %02x:%02x:%02x:%02x:%02x:%02x (mru_seq=%lu, name_len=%u)\r\n", slot, rec.addr[0], rec.addr[1],
            rec.addr[2], rec.addr[3], rec.addr[4], rec.addr[5], (unsigned long)rec.mru_seq, rec.name_len
        );
    }

    s_next_mru_seq = max_mru_seq + 1;
    s_boot_status = any_corrupt ? PL_PERSIST_STATUS_RECORD_CORRUPT : PL_PERSIST_STATUS_LOADED;
    if (!any_loaded) {
        pl_log("persist: marker present but no device records -- valid store, no device yet\r\n");
    }
}

pl_persist_status_t pl_persist_boot_status(void) {
    return s_boot_status;
}

bool pl_persist_boot_has_device(void) {
    return s_boot_has_device;
}

void pl_persist_boot_device_addr(uint8_t out_addr[6]) {
    memcpy(out_addr, s_boot_device_addr, 6);
}

// Code-review finding (bd-pico-link-cz0.6, 2026-09-01, CONFIRMED): this
// function has TWO callers in two different contexts -- bt.c:814's
// PL_COMMAND_TAG_PERSIST_DEVICE handler (thread context, the superloop) and
// pl_persist_save_device_now()'s pico-link-lmf carve-out below (IRQ /
// cyw43-BTstack background async_context, when USB audio is already
// streaming at pairing time). The staging state it writes
// (s_pending/s_pending_addr/s_pending_since_us/s_write_enqueued) is plain,
// non-atomic memory with no lock of its own. On this single core, disabling
// interrupts for the body is sufficient mutual exclusion between the two
// contexts -- it makes the async_context caller unable to preempt a
// thread-context write in progress (and vice versa: the write itself can't
// be re-entered), closing the torn-MAC-address write. Same short-critical-
// section idiom as bt.c's pl_bt_pending_push (bt.c:613) -- kept deliberately
// tiny (plain memory writes only, no flash access) per that idiom.
void pl_persist_request_save_device(const uint8_t addr[6]) {
    uint32_t irq_state = save_and_disable_interrupts();
    memcpy(s_pending_addr, addr, 6);
    s_pending = true;
    s_pending_since_us = time_us_64();
    // Code-review finding 1: a freshly staged save supersedes whatever the
    // heartbeat may already be about to write for a STALE prior request
    // (e.g. a quick reconnect-to-a-different-device churn) -- the enqueued
    // flag only gates against re-enqueueing the SAME request repeatedly,
    // not against a genuinely new one.
    s_write_enqueued = false;
    restore_interrupts(irq_state);
    pl_log(
        "persist: staged save for %02x:%02x:%02x:%02x:%02x:%02x\r\n", addr[0], addr[1], addr[2], addr[3], addr[4], addr[5]
    );
}

// Code-review finding 1 (2026-09-01, on bd-pico-link-cz0.6, CONFIRMED
// critical): persist.c and BTstack share ONE btstack_tlv_flash_bank
// instance with NO locking of its own
// (btstack_tlv_flash_bank_store_tag/get_tag/delete_tag are a multi-step
// sequence over plain, non-atomic struct fields -- check space, maybe
// migrate, write value, write header, delete old entries, THEN mutate
// self->write_offset). BTstack writes link keys into that SAME instance
// synchronously from inside its own HCI event dispatch (hci.c's
// put_link_key), which runs on the cyw43/BTstack background
// async_context -- a real low-priority hardware IRQ, not a cooperative
// poll. A THREAD-CONTEXT TLV call preempted mid-sequence by that IRQ (or
// vice versa) corrupts write_offset and the bank bookkeeping.
//
// FIX: every actual flash write in this file goes through this one static
// helper, and every caller of it is on the SAME async_context BTstack's own
// put_link_key runs on -- never thread context. That queue
// (async_context_threadsafe_background) serializes every callback
// registered on it to completion before starting the next, so once both
// sides run through it, they cannot preempt each other -- the race is
// closed by construction, not by a lock. Two callers, both IRQ/async_context:
// pl_persist_execute_pending_write() (bt.c's pending-queue drain, the
// pico-link-ouw idiom, for LOW-VALUE deferred writes -- volume/codec/MRU
// bumps) and pl_persist_save_device_now() (a2dp.c's STREAM_ESTABLISHED
// handler, called directly and synchronously -- see that function's own
// doc comment for why a queue+wait round-trip isn't needed there).
// Finds which slot currently holds `addr`, if any -- returns the slot index
// or -1. Consults the in-RAM `s_slot_occupied`/`s_slot_addr` cache, not
// flash (see that cache's doc comment).
static int pl_persist_find_slot_for_addr(const uint8_t addr[6]) {
    for (uint8_t i = 0; i < PL_PERSIST_DEVICE_SLOTS; i++) {
        if (s_slot_occupied[i] && memcmp(s_slot_addr[i], addr, 6) == 0) {
            return (int)i;
        }
    }
    return -1;
}

// Finds the first unoccupied slot, or -1 if every slot is in use. Bead
// pico-link-4vb.6 (T1) / design section 6: NO eviction -- this is the only
// fallback pl_persist_do_write tries after a same-address match fails; if
// this also returns -1, the write is refused outright
// (PL_PERSIST_WRITE_STORE_FULL).
static int pl_persist_find_free_slot(void) {
    for (uint8_t i = 0; i < PL_PERSIST_DEVICE_SLOTS; i++) {
        if (!s_slot_occupied[i]) {
            return (int)i;
        }
    }
    return -1;
}

// Bead pico-link-4vb.6 (T1) -- THE MOST IMPORTANT CHANGE IN THIS SECTION,
// per the design doc: this is now READ-MODIFY-WRITE, not
// construct-from-scratch. Previously this memset a fresh record and filled
// only addr/mru_seq/crc16, so EVERY OTHER FIELD (name, codec_id,
// ldac_quality, volume, flags, preset_id) was silently zeroed on every save
// -- a bug that was invisible while nothing but addr/mru_seq was ever
// populated, and would have become a real, hard-to-diagnose defect ("looks
// like flash corruption") the moment a per-device-setting write landed on
// top of this. Fix: if the target slot already holds a valid record, start
// from IT (not a zeroed one); only overwrite the fields THIS call actually
// carries. Today only `addr` and (optionally) `name`/`name_len` are
// call-supplied -- `name_len == 0` means "this caller has no name to
// contribute, leave whatever is already stored." `codec_id`/
// `ldac_quality`/`volume`/`flags`/`preset_id` are never touched by this
// function at all (no caller populates them yet -- those are Tier 2 work,
// design section 2 point 5) and so ride through RMW unchanged for free.
// `mru_seq` is ALWAYS bumped -- every write, by construction, means "this
// device was just used."
//
// `name`/`name_len` are NULL/0 from every call site in this bead (T3,
// deferred, is what will thread a real name through
// PlCommandTag::Connect's payload -> bt.c's in-flight connect-target cache
// -> here) -- the parameters exist now so T3 only has to change call
// sites, not this function's RMW logic.
//
// Slot selection (design section 6): match by `addr` against an existing
// slot (a re-pairing of an already-remembered device updates that same
// slot rather than consuming a new one) -- else the first free slot -- else
// NO EVICTION, refuse the write and return PL_PERSIST_WRITE_STORE_FULL.
static pl_persist_write_result_t pl_persist_do_write(const uint8_t addr[6], const uint8_t *name, uint8_t name_len) {
    int slot = pl_persist_find_slot_for_addr(addr);
    if (slot < 0) {
        slot = pl_persist_find_free_slot();
    }
    if (slot < 0) {
        pl_log(
            "persist: store full (%u/%u slots used) -- refusing to write %02x:%02x:%02x:%02x:%02x:%02x, no "
            "eviction\r\n",
            (unsigned)PL_PERSIST_DEVICE_SLOTS, (unsigned)PL_PERSIST_DEVICE_SLOTS, addr[0], addr[1], addr[2], addr[3], addr[4], addr[5]
        );
        return PL_PERSIST_WRITE_STORE_FULL;
    }

    pl_persist_marker_t marker = {.schema_version = PL_PERSIST_SCHEMA_VERSION};
    s_tlv_impl->store_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_MARKER, 0), (const uint8_t *)&marker, sizeof(marker));

    // Read-modify-write: start from the slot's existing record if it has
    // one and it's valid; otherwise (brand new slot, or a corrupt existing
    // record we're about to overwrite anyway) start from zeroed fields --
    // same fresh-record shape the old construct-from-scratch code always
    // produced, just no longer the ONLY path.
    pl_persist_device_record_t rec;
    memset(&rec, 0, sizeof(rec));
    int existing_len = s_tlv_impl->get_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_DEVICE, (uint8_t)slot), (uint8_t *)&rec, sizeof(rec));
    if (existing_len == (int)sizeof(rec)) {
        uint16_t existing_crc = pl_persist_crc16((const uint8_t *)&rec, offsetof(pl_persist_device_record_t, crc16));
        if (existing_crc != rec.crc16) {
            // Corrupt existing record in this slot -- don't propagate
            // garbage fields forward, start clean instead.
            memset(&rec, 0, sizeof(rec));
        }
    } else {
        memset(&rec, 0, sizeof(rec));
    }

    memcpy(rec.addr, addr, 6);
    if (name != NULL && name_len > 0) {
        uint8_t copy_len = name_len > (uint8_t)sizeof(rec.name) ? (uint8_t)sizeof(rec.name) : name_len;
        memcpy(rec.name, name, copy_len);
        if (copy_len < (uint8_t)sizeof(rec.name)) {
            memset(rec.name + copy_len, 0, sizeof(rec.name) - copy_len);
        }
        rec.name_len = copy_len;
    }
    // else: leave rec.name/rec.name_len exactly as read (or zeroed, for a
    // brand new slot) -- this call has no name to contribute.
    rec.mru_seq = s_next_mru_seq++;
    // codec_id/ldac_quality/volume/flags/preset_id: untouched above --
    // whatever was in `rec` (from the existing record, or zeroed for a new
    // slot) rides through unchanged. No call site populates these yet
    // (Tier 2, design section 2 point 5).
    rec.crc16 = pl_persist_crc16((const uint8_t *)&rec, offsetof(pl_persist_device_record_t, crc16));

    s_tlv_impl->store_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_DEVICE, (uint8_t)slot), (const uint8_t *)&rec, sizeof(rec));

    s_slot_occupied[(uint8_t)slot] = true;
    memcpy(s_slot_addr[(uint8_t)slot], addr, 6);

    s_last_write_us = time_us_64();
    s_have_last_write = true;
    pl_log(
        "persist: wrote device record slot=%d %02x:%02x:%02x:%02x:%02x:%02x (mru_seq=%lu, name_len=%u)\r\n", slot, rec.addr[0], rec.addr[1],
        rec.addr[2], rec.addr[3], rec.addr[4], rec.addr[5], (unsigned long)rec.mru_seq, rec.name_len
    );
    return PL_PERSIST_WRITE_OK;
}

// Called EXCLUSIVELY from pl_bt_pending_service (bt.c), which itself only
// ever runs from pl_bt_wdt_heartbeat_handler -- see pl_persist_do_write's
// doc comment above for the full reentrancy rationale. Handles LOW-VALUE
// deferred writes (a staged save that missed the synchronous pairing-time
// path below, or a future volume/codec/MRU update) -- rate-limited and
// settle-gated by pl_persist_service(), thread context, which only decides
// WHEN this is due and enqueues the request; it never touches s_tlv_impl
// itself.
//
// # Safety / calling contract
//
// MUST be called only from bt.c's pending-queue drain (async_context/IRQ
// context). Calling this from thread context reintroduces exactly the race
// pl_persist_do_write's doc comment describes.
pl_persist_write_result_t pl_persist_execute_pending_write(void) {
    if (!s_pending) {
        s_write_enqueued = false;
        return PL_PERSIST_WRITE_OK;
    }
    if (pl_usb_audio_streaming() || pl_a2dp_streaming()) {
        // The gate could have flipped true again between
        // pl_persist_service() enqueueing this request and the heartbeat
        // draining it (up to ~100ms later) -- "NO flash write while
        // streaming, of any size" has no exception, so bail and leave the
        // request pending; pl_persist_service() will re-arm
        // s_write_enqueued the next time it sees a safe window (it does so
        // unconditionally on every call, see below).
        s_write_enqueued = false;
        return PL_PERSIST_WRITE_OK;
    }

    pl_persist_write_result_t result = pl_persist_do_write(s_pending_addr, NULL, 0);

    s_pending = false;
    s_urgent = false;
    s_write_enqueued = false;
    return result;
}

// Andreas's ruling, 2026-09-01 (follow-up to code-review finding 1's fix):
// writing the device record is part of ESTABLISHING the connection, not a
// chore staged in RAM for a quiet window that may never come -- see
// persist.h's module doc for the full ordering rationale. Called directly
// and SYNCHRONOUSLY from a2dp.c's A2DP_SUBEVENT_STREAM_ESTABLISHED handler,
// BEFORE s_ctx.state flips to PL_A2DP_MEDIA_PRIMING -- i.e. before
// pl_a2dp_streaming() can become true for this connection, and before any
// audio has started flowing toward the headphones. A short delay before
// first sound (this write's ~9ms worst case) is invisible; a lost pairing
// costs a physical headphone factory reset (pico-link-7ur).
//
// No queue+wait round-trip needed to reach the required async_context: the
// caller (a2dp.c's own AVDTP/A2DP packet handler) is ALREADY running on the
// cyw43/BTstack background async_context -- the exact same serialized
// execution stream pl_persist_execute_pending_write's callers use and
// BTstack's own put_link_key runs on (see a2dp.c's own module doc: "another
// IRQ-context producer, same IRQ context bt.c's own HCI packet handler
// does"). So this function calling pl_persist_do_write directly is already
// mutually exclusive with put_link_key by construction -- deferring onto
// bt.c's pending queue and then busy-waiting for it to drain would cross a
// context boundary that does not need crossing, and would only add latency
// audio would still have to wait on.
//
// THE ONE CARVE-OUT (pico-link-lmf, NOT fixed here, deliberately not made
// worse): if pl_usb_audio_streaming() is already true -- the USB host was
// already sending isochronous audio to this dongle when the pairing
// completed -- ISO-OUT is live and a single missed re-arm past this
// function's blackout is PERMANENT (audio_device.c:759-762), needing a
// physical unplug to recover. That is a materially worse failure than a
// delayed/lost device record, so this case falls back to the conservative
// RAM-staged path instead (pl_persist_request_save_device): the record is
// NOT written now, only staged, and pl_persist_service()'s normal
// streaming-gated path picks it up once safe -- same behaviour as before
// this ruling, for this one case only.
//
// # Safety / calling contract
//
// MUST be called only from the cyw43/BTstack background async_context --
// today that means exclusively from a2dp.c's A2DP_SUBEVENT_STREAM_ESTABLISHED
// case. Calling this from thread context reintroduces the exact race
// code-review finding 1 closed.
pl_persist_write_result_t pl_persist_save_device_now(const uint8_t addr[6]) {
    if (pl_usb_audio_streaming()) {
        pl_log(
            "persist: USB audio already live at pairing time -- staging %02x:%02x:%02x:%02x:%02x:%02x instead of "
            "writing now (pico-link-lmf carve-out)\r\n",
            addr[0], addr[1], addr[2], addr[3], addr[4], addr[5]
        );
        pl_persist_request_save_device(addr);
        // Not a real result -- deferred to the staged path, which does not
        // yet know whether the store will turn out to be full when it
        // finally writes. Bead pico-link-4vb.6 (T1) widens the store to 8
        // slots but doesn't change this carve-out's own behaviour
        // (pico-link-lmf, still open) -- see this function's doc comment.
        return PL_PERSIST_WRITE_OK;
    }

    pl_log(
        "persist: writing device record synchronously at pairing time for %02x:%02x:%02x:%02x:%02x:%02x\r\n", addr[0],
        addr[1], addr[2], addr[3], addr[4], addr[5]
    );
    pl_persist_write_result_t result = pl_persist_do_write(addr, NULL, 0);

    // The write attempt is now resolved (written, or refused as store-full)
    // -- clear any stale staged request for the same (or a different, e.g.
    // a fast device-switch) address so pl_persist_service() doesn't
    // redundantly re-enqueue it either way.
    s_pending = false;
    s_urgent = false;
    s_write_enqueued = false;
    return result;
}

// Bead pico-link-4vb.6 (T1). See persist.h's doc comment for the full
// contract (async_context-only, not yet wired into bt.c's pending queue --
// T3, deferred).
bool pl_persist_forget_device(const uint8_t addr[6]) {
    int slot = pl_persist_find_slot_for_addr(addr);
    if (slot < 0) {
        pl_log(
            "persist: forget requested for %02x:%02x:%02x:%02x:%02x:%02x but no slot holds it -- no-op\r\n", addr[0], addr[1], addr[2],
            addr[3], addr[4], addr[5]
        );
        return false;
    }

    s_tlv_impl->delete_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_DEVICE, (uint8_t)slot));
    s_slot_occupied[(uint8_t)slot] = false;
    memset(s_slot_addr[(uint8_t)slot], 0, 6);

    // S18: forgetting removes the link key too -- a PL:D record without its
    // key is a row that says "Paired" but can't connect without re-pairing.
    // Shares the same btstack_tlv_flash_bank instance as our own PL:* tags
    // (persist.h's module doc), so this MUST run on the same async_context
    // as every other write in this file -- see this function's calling
    // contract in persist.h.
    gap_drop_link_key_for_bd_addr((uint8_t *)addr);

    pl_log(
        "persist: forgot device slot=%d %02x:%02x:%02x:%02x:%02x:%02x (link key dropped too)\r\n", slot, addr[0], addr[1], addr[2], addr[3],
        addr[4], addr[5]
    );
    return true;
}

void pl_persist_service(void) {
    if (!s_pending) {
        return;
    }
    if (pl_usb_audio_streaming() || pl_a2dp_streaming()) {
        return;
    }
    bool due;
    if (s_urgent) {
        due = true;
    } else {
        uint64_t now_us = time_us_64();
        due = (now_us - s_pending_since_us >= PL_PERSIST_SETTLE_US) &&
              (!s_have_last_write || (now_us - s_last_write_us >= PL_PERSIST_MIN_INTERVAL_US));
    }
    if (!due || s_write_enqueued) {
        return;
    }
    // Code-review finding 1: no direct flash access here any more -- only
    // enqueues onto bt.c's existing pending-queue/heartbeat mechanism (see
    // pl_persist_execute_pending_write's doc comment above for the full
    // rationale). s_write_enqueued guards against flooding that queue on
    // every subsequent superloop iteration before the heartbeat (up to
    // 100ms later) actually drains this request.
    s_write_enqueued = true;
    pl_bt_enqueue_persist_write();
}

void pl_persist_request_urgent_flush(void) {
    if (!s_pending) {
        return;
    }
    s_urgent = true;
}
