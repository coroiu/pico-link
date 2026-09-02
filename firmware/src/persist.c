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
//
// `codec_id` and `ldac_quality` (design findings 1.1/1.2,
// .planning/design/2026-09-02-device-page-seam.md, bead pico-link-ay0.1),
// confirmed against the existing (pre-this-bead) shape: PL_PERSIST_SCHEMA_VERSION
// stays 1 -- both fields already existed and already rode through
// pl_persist_rmw's read-modify-write untouched before this bead populated
// them, so an existing record's zero bytes are already exactly the
// sentinels these encodings want. Neither is written by anything before
// this bead lands, so every record in the field today reads back 0 for
// both.
//
//   codec_id: codec_table.h's PL_CODEC_ID_* -- a PINNED per-row identity,
//   NEVER the PL_CODECS array index (index order is the negotiation
//   preference order and is designed to change). 0 (PL_CODEC_ID_AUTOMATIC)
//   means "no pin, follow the normal preference walk" and is also the
//   correct reading of an old/never-written record.
//
//   ldac_quality: 1-BASED, NOT a raw LDACBT_EQMID_* value --
//   LDACBT_EQMID_HQ is literally 0 (ldacBT.h:129), so storing the raw
//   EQMID would make "never chosen" and "explicitly chose 990 kbps" the
//   same byte forever. 0 = unset (use whatever codec_ldac.c's init()
//   configures by default), 1 = 990 kbps (LDACBT_EQMID_HQ), 2 = 660 kbps
//   (LDACBT_EQMID_SQ), 3 = 330 kbps (LDACBT_EQMID_MQ), 4 = Adaptive
//   (reserved -- not implementable with the vendored libldac, design sec
//   5). The EQMID mapping itself lives in exactly one place, codec_ldac.c
//   (a later task) -- this module stores and moves the byte, never
//   interprets it.
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

// Design finding 1.4 (.planning/design/2026-09-02-device-page-seam.md sec
// 1.4, bead pico-link-ay0.1): the LIVE in-RAM mirror of every occupied slot
// -- supersedes two things that used to be separate and, critically, went
// stale after boot: the old s_boot_devices snapshot (bead pico-link-4vb.7,
// filled ONLY by pl_persist_init()'s load loop and never touched again) and
// the old s_slot_occupied/s_slot_addr slot-selection cache (bead
// pico-link-4vb.6, which WAS kept live by every write but only carried
// occupancy + address, not settings). This struct is now the one place
// that answers "what does slot N currently hold" for every purpose --
// pl_persist_find_slot_for_addr/find_free_slot's slot selection,
// pl_persist_boot_device_count/at's boot snapshot (computed live from this,
// see those functions below), and pl_persist_get_device_settings's reader.
//
// Populated by pl_persist_init()'s load loop AND updated by the shared RMW
// core (pl_persist_rmw) on every successful write, in the same
// cyw43/BTstack async_context critical section that performs the flash
// write itself -- so codec_id/ldac_quality here can never lag what was
// actually written. Never itself written to flash; a stale read here can
// only ever be corrected by the next get_tag inside pl_persist_rmw's own
// read-modify-write, exactly as the old occupancy cache's doc comment
// argued.
typedef struct {
    bool occupied;
    uint8_t addr[6];
    uint8_t name[32];
    uint8_t name_len;
    uint32_t mru_seq;
    // Design findings 1.1/1.2: codec_id is codec_table.h's PL_CODEC_ID_*
    // (0 = Automatic), ldac_quality is 1-based (0 = unset; NOT a raw
    // LDACBT_EQMID_* value -- see pl_persist_write_device_settings's doc
    // comment in persist.h).
    uint8_t codec_id;
    uint8_t ldac_quality;
} pl_persist_slot_t;
static pl_persist_slot_t s_slots[PL_PERSIST_DEVICE_SLOTS];

// Next mru_seq to stamp on a save -- seeded from whatever was loaded at
// boot (if anything) so a fresh save's mru_seq is monotonic across a
// reflash, not just within one power-on session. Bead pico-link-4vb.6 (T1):
// now the max mru_seq loaded across ALL PL_PERSIST_DEVICE_SLOTS slots, plus
// one (design section 6) -- not just slot 0's.
static uint32_t s_next_mru_seq = 1;

// Staged-save state (design point 4's "staged, gated, flushed" split).
static bool s_pending;
static uint8_t s_pending_addr[6];
// Bead pico-link-4vb.7 (T3): optional name for the staged save, threaded
// through so the pico-link-lmf carve-out in pl_persist_save_device_now
// doesn't lose the in-flight connect target's name when it falls back to
// this staged path. s_pending_name_len == 0 means "no name staged, leave
// whatever is on record" -- same RMW convention pl_persist_do_write already
// uses for its own name/name_len parameters.
static uint8_t s_pending_name[32];
static uint8_t s_pending_name_len;
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

        // Design finding 1.4: populate the live slot mirror directly, not a
        // separate boot-only snapshot -- see s_slots's doc comment above.
        pl_persist_slot_t *sl = &s_slots[slot];
        sl->occupied = true;
        memcpy(sl->addr, rec.addr, 6);
        memcpy(sl->name, rec.name, sizeof(sl->name));
        sl->name_len = rec.name_len;
        sl->mru_seq = rec.mru_seq;
        sl->codec_id = rec.codec_id;
        sl->ldac_quality = rec.ldac_quality;
        any_loaded = true;
        if (rec.mru_seq > max_mru_seq) {
            max_mru_seq = rec.mru_seq;
        }
        pl_log(
            "persist: loaded device slot=%u %02x:%02x:%02x:%02x:%02x:%02x (mru_seq=%lu, name_len=%u, codec_id=%u, "
            "ldac_quality=%u)\r\n",
            slot, rec.addr[0], rec.addr[1], rec.addr[2], rec.addr[3], rec.addr[4], rec.addr[5], (unsigned long)rec.mru_seq, rec.name_len,
            rec.codec_id, rec.ldac_quality
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

// Design finding 1.4: computed live from s_slots rather than a fixed
// boot-time count -- these two functions are called exactly once, at boot,
// before pl_persist_service() or any write can run, so the live-vs-snapshot
// distinction doesn't change their observed behaviour; iterating the same
// PL_PERSIST_DEVICE_SLOTS-sized array in slot order, counting/indexing only
// occupied entries, reproduces the old s_boot_devices ordering exactly.
uint8_t pl_persist_boot_device_count(void) {
    uint8_t count = 0;
    for (uint8_t i = 0; i < PL_PERSIST_DEVICE_SLOTS; i++) {
        if (s_slots[i].occupied) {
            count++;
        }
    }
    return count;
}

void pl_persist_boot_device_at(uint8_t index, uint8_t out_addr[6], uint8_t out_name[32], uint8_t *out_name_len, uint32_t *out_mru_seq) {
    uint8_t seen = 0;
    for (uint8_t i = 0; i < PL_PERSIST_DEVICE_SLOTS; i++) {
        if (!s_slots[i].occupied) {
            continue;
        }
        if (seen == index) {
            memcpy(out_addr, s_slots[i].addr, 6);
            memcpy(out_name, s_slots[i].name, 32);
            *out_name_len = s_slots[i].name_len;
            *out_mru_seq = s_slots[i].mru_seq;
            return;
        }
        seen++;
    }
    memset(out_addr, 0, 6);
    memset(out_name, 0, 32);
    *out_name_len = 0;
    *out_mru_seq = 0;
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
void pl_persist_request_save_device(const uint8_t addr[6], const uint8_t *name, uint8_t name_len) {
    uint32_t irq_state = save_and_disable_interrupts();
    memcpy(s_pending_addr, addr, 6);
    if (name != NULL && name_len > 0) {
        uint8_t copy_len = name_len > (uint8_t)sizeof(s_pending_name) ? (uint8_t)sizeof(s_pending_name) : name_len;
        memcpy(s_pending_name, name, copy_len);
        s_pending_name_len = copy_len;
    } else {
        s_pending_name_len = 0;
    }
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
// or -1. Consults the in-RAM `s_slots` mirror, not flash (see its doc
// comment).
static int pl_persist_find_slot_for_addr(const uint8_t addr[6]) {
    for (uint8_t i = 0; i < PL_PERSIST_DEVICE_SLOTS; i++) {
        if (s_slots[i].occupied && memcmp(s_slots[i].addr, addr, 6) == 0) {
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
        if (!s_slots[i].occupied) {
            return (int)i;
        }
    }
    return -1;
}

// Design finding 1.3 (.planning/design/2026-09-02-device-page-seam.md sec
// 1.3, bead pico-link-ay0.1): what a particular RMW call carries and how it
// should behave, so pl_persist_do_write (pairing/reconnect writes) and
// pl_persist_write_device_settings (a codec pin) share ONE
// read-modify-write core (pl_persist_rmw below) instead of forking the RMW
// logic -- two independently written RMW paths over the same on-flash
// struct is exactly how a CRC-checked store starts producing "corruption"
// nobody can reproduce.
typedef struct {
    // NULL (or non-NULL with name_len == 0) => leave rec.name/rec.name_len
    // exactly as already on record (or zeroed, for a brand-new slot) -- the
    // same RMW convention pl_persist_do_write always used.
    const uint8_t *name;
    uint8_t name_len;
    // true => overwrite rec.codec_id/rec.ldac_quality with the values
    // below. false => leave them exactly as already on record. A pairing
    // write (pl_persist_do_write) never sets this -- codec/quality are Tier
    // 2 settings, not pairing facts.
    bool set_codec_settings;
    uint8_t codec_id;
    uint8_t ldac_quality;
    // Design finding 1.3: a settings write ("I pinned a codec") is NOT "I
    // used this device" -- only a pairing/reconnect write bumps mru_seq.
    // Bumping on a settings write would make a pinned-but-unconnected
    // device the boot auto-reconnect target
    // (core::paired.iter().max_by_key(|d| d.mru_seq)).
    bool bump_mru;
    // true (pl_persist_do_write): the original find-existing-slot else
    // first-free-slot else refuse policy (design section 6, NO eviction).
    // false (pl_persist_write_device_settings): never claim a fresh slot --
    // refuse (return false from pl_persist_rmw, writing nothing) if no
    // existing slot holds the target address.
    bool allow_create_slot;
} pl_persist_rmw_fields_t;

// THE shared read-modify-write core (design finding 1.3) -- every actual
// flash write to a device record funnels through here. Previously this was
// pl_persist_do_write's own body (bead pico-link-4vb.6, T1's fix for the
// original construct-from-scratch bug that silently zeroed every
// unpopulated field on each save): if the target slot already holds a
// valid record, start from IT, not a zeroed one, and only overwrite the
// fields `fields` actually carries.
//
// Returns false, writing nothing, if `fields->allow_create_slot` is false
// and no slot currently holds `addr` (pl_persist_write_device_settings's
// "refuses to create a slot" contract) -- `*out_result` is left untouched
// in that case. Returns true otherwise, with `*out_result` set to
// PL_PERSIST_WRITE_OK or PL_PERSIST_WRITE_STORE_FULL (STORE_FULL only
// reachable when allow_create_slot is true and pl_persist_find_free_slot
// also fails -- structurally unreachable when allow_create_slot is false,
// since that path never calls find_free_slot at all).
//
// # Calling contract
//
// Same as pl_persist_do_write's / this file's module doc (Reentrancy
// section): cyw43/BTstack background async_context ONLY.
static bool pl_persist_rmw(const uint8_t addr[6], const pl_persist_rmw_fields_t *fields, pl_persist_write_result_t *out_result) {
    int slot = pl_persist_find_slot_for_addr(addr);
    if (slot < 0) {
        if (!fields->allow_create_slot) {
            return false;
        }
        slot = pl_persist_find_free_slot();
    }
    if (slot < 0) {
        pl_log(
            "persist: store full (%u/%u slots used) -- refusing to write %02x:%02x:%02x:%02x:%02x:%02x, no "
            "eviction\r\n",
            (unsigned)PL_PERSIST_DEVICE_SLOTS, (unsigned)PL_PERSIST_DEVICE_SLOTS, addr[0], addr[1], addr[2], addr[3], addr[4], addr[5]
        );
        // Bead pico-link-4vb.7 (T3): S18 -- "must never silently evict", so
        // the UI must hear about a refused write too.
        pl_bt_push_paired_store_full();
        *out_result = PL_PERSIST_WRITE_STORE_FULL;
        return true;
    }

    pl_persist_marker_t marker = {.schema_version = PL_PERSIST_SCHEMA_VERSION};
    s_tlv_impl->store_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_MARKER, 0), (const uint8_t *)&marker, sizeof(marker));

    // Read-modify-write: start from the slot's existing record if it has
    // one and it's valid; otherwise (brand new slot, or a corrupt existing
    // record we're about to overwrite anyway) start from zeroed fields.
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
    if (fields->name != NULL && fields->name_len > 0) {
        uint8_t copy_len = fields->name_len > (uint8_t)sizeof(rec.name) ? (uint8_t)sizeof(rec.name) : fields->name_len;
        memcpy(rec.name, fields->name, copy_len);
        if (copy_len < (uint8_t)sizeof(rec.name)) {
            memset(rec.name + copy_len, 0, sizeof(rec.name) - copy_len);
        }
        rec.name_len = copy_len;
    }
    // else: leave rec.name/rec.name_len exactly as read (or zeroed, for a
    // brand new slot) -- this call has no name to contribute.

    if (fields->set_codec_settings) {
        rec.codec_id = fields->codec_id;
        rec.ldac_quality = fields->ldac_quality;
    }
    // else: leave rec.codec_id/rec.ldac_quality exactly as read -- a
    // pairing write must not clobber a previously pinned preference.

    if (fields->bump_mru) {
        rec.mru_seq = s_next_mru_seq++;
    }
    // else: leave rec.mru_seq exactly as read -- design finding 1.3, a
    // settings write is not a use.

    // volume/flags/preset_id: untouched -- no caller populates them yet
    // (Tier 2, design section 2 point 5). Rides through RMW unchanged.
    rec.crc16 = pl_persist_crc16((const uint8_t *)&rec, offsetof(pl_persist_device_record_t, crc16));

    s_tlv_impl->store_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_DEVICE, (uint8_t)slot), (const uint8_t *)&rec, sizeof(rec));

    // Design finding 1.4: this mirror IS the live source of truth for
    // pl_persist_get_device_settings and boot-snapshot reads now, not just
    // a slot-occupancy cache -- update it fully from what was actually
    // written, in the same async_context call that performed the write, so
    // a pin set now is visible immediately, not after the next power cycle.
    pl_persist_slot_t *sl = &s_slots[(uint8_t)slot];
    sl->occupied = true;
    memcpy(sl->addr, rec.addr, 6);
    memcpy(sl->name, rec.name, sizeof(sl->name));
    sl->name_len = rec.name_len;
    sl->mru_seq = rec.mru_seq;
    sl->codec_id = rec.codec_id;
    sl->ldac_quality = rec.ldac_quality;

    s_last_write_us = time_us_64();
    s_have_last_write = true;
    pl_log(
        "persist: wrote device record slot=%d %02x:%02x:%02x:%02x:%02x:%02x (mru_seq=%lu, name_len=%u, codec_id=%u, "
        "ldac_quality=%u)\r\n",
        slot, rec.addr[0], rec.addr[1], rec.addr[2], rec.addr[3], rec.addr[4], rec.addr[5], (unsigned long)rec.mru_seq, rec.name_len,
        rec.codec_id, rec.ldac_quality
    );
    // Bead pico-link-4vb.7 (T3): echo the write that actually landed --
    // design section 3's single-writer rule ("no echo means no row") means
    // this is the ONLY place PlEventTag::PairedDeviceUpserted is pushed for
    // a save (pl_persist_do_write and pl_persist_write_device_settings both
    // funnel through this one function).
    pl_bt_push_paired_device_upserted(rec.addr, rec.name, rec.name_len, rec.mru_seq);
    *out_result = PL_PERSIST_WRITE_OK;
    return true;
}

// Bead pico-link-4vb.6 (T1), now a thin wrapper over the shared RMW core
// (pl_persist_rmw, design finding 1.3): pairing/reconnect writes always
// bump mru_seq and may claim a fresh slot (design section 6: match by
// `addr` against an existing slot, else the first free slot, else NO
// EVICTION -- refuse and return PL_PERSIST_WRITE_STORE_FULL). Never touches
// codec_id/ldac_quality -- those are Tier 2 settings, written only via
// pl_persist_write_device_settings.
//
// `name`/`name_len` are NULL/0 from most call sites -- `name_len == 0`
// means "this caller has no name to contribute, leave whatever is already
// stored" (pl_persist_save_device_now supplies a real name from bt.c's
// in-flight connect-target cache).
static pl_persist_write_result_t pl_persist_do_write(const uint8_t addr[6], const uint8_t *name, uint8_t name_len) {
    pl_persist_rmw_fields_t fields = {
        .name = name,
        .name_len = name_len,
        .set_codec_settings = false,
        .codec_id = 0,
        .ldac_quality = 0,
        .bump_mru = true,
        .allow_create_slot = true,
    };
    pl_persist_write_result_t result = PL_PERSIST_WRITE_OK;
    // allow_create_slot=true means pl_persist_rmw always attempts a write
    // (finds-or-creates a slot, or reports STORE_FULL) -- it can only
    // return false when allow_create_slot is false, which never applies
    // here.
    (void)pl_persist_rmw(addr, &fields, &result);
    return result;
}

bool pl_persist_write_device_settings(const uint8_t addr[6], uint8_t codec_id, uint8_t ldac_quality) {
    pl_persist_rmw_fields_t fields = {
        .name = NULL,
        .name_len = 0,
        .set_codec_settings = true,
        .codec_id = codec_id,
        .ldac_quality = ldac_quality,
        .bump_mru = false,
        .allow_create_slot = false,
    };
    pl_persist_write_result_t result = PL_PERSIST_WRITE_OK;
    if (!pl_persist_rmw(addr, &fields, &result)) {
        pl_log(
            "persist: write_device_settings for %02x:%02x:%02x:%02x:%02x:%02x but no slot holds it -- refusing "
            "(the device page is only reachable for a remembered or connected device)\r\n",
            addr[0], addr[1], addr[2], addr[3], addr[4], addr[5]
        );
        return false;
    }
    // allow_create_slot=false means STORE_FULL is structurally unreachable
    // here (pl_persist_rmw only calls pl_persist_find_free_slot, the one
    // path that can fail full, when allow_create_slot is true) -- but
    // treat it as failure anyway rather than asserting, in case that
    // invariant is ever weakened.
    return result == PL_PERSIST_WRITE_OK;
}

bool pl_persist_get_device_settings(const uint8_t addr[6], uint8_t *out_codec_id, uint8_t *out_ldac_quality) {
    int slot = pl_persist_find_slot_for_addr(addr);
    if (slot < 0) {
        return false;
    }
    *out_codec_id = s_slots[(uint8_t)slot].codec_id;
    *out_ldac_quality = s_slots[(uint8_t)slot].ldac_quality;
    return true;
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

    pl_persist_write_result_t result = pl_persist_do_write(s_pending_addr, s_pending_name_len > 0 ? s_pending_name : NULL, s_pending_name_len);

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
    // Bead pico-link-4vb.7 (T3): read the in-flight connect target's name
    // from bt.c's cache -- this is the whole reason a record could never
    // have a name before this bead: at the moment this function runs, C has
    // an address and (until now) nothing else. name_len == 0 (no cached
    // target matching this addr, or the debug-connect bypass which never
    // caches a name) means "leave whatever name is already on record" --
    // pl_persist_do_write's own RMW convention.
    uint8_t name[32];
    uint8_t name_len;
    pl_bt_get_connect_target_name(addr, name, &name_len);

    if (pl_usb_audio_streaming()) {
        pl_log(
            "persist: USB audio already live at pairing time -- staging %02x:%02x:%02x:%02x:%02x:%02x instead of "
            "writing now (pico-link-lmf carve-out)\r\n",
            addr[0], addr[1], addr[2], addr[3], addr[4], addr[5]
        );
        pl_persist_request_save_device(addr, name_len > 0 ? name : NULL, name_len);
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
    pl_persist_write_result_t result = pl_persist_do_write(addr, name_len > 0 ? name : NULL, name_len);

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
    // Design finding 1.4: clear the whole live mirror entry, not just
    // occupied/addr -- a subsequent pl_persist_get_device_settings for this
    // address is already refused via `occupied`, but a fully-zeroed entry
    // avoids leaving stale codec_id/ldac_quality/name bytes sitting in RAM
    // for a slot that no longer represents any device.
    memset(&s_slots[(uint8_t)slot], 0, sizeof(s_slots[(uint8_t)slot]));

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
    // Bead pico-link-4vb.7 (T3).
    pl_bt_push_paired_device_forgotten(addr);
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
