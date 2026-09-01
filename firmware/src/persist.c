// Pico Link firmware -- M5 persistence (bead pico-link-cz0.6). See
// persist.h's module doc for the design summary; full design of record is
// pico-link-cz0.6.1's closed-bead comment.
#include "persist.h"

#include <stddef.h>
#include <string.h>

#include "hardware/flash.h"
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

// MVP: exactly one device slot (index 0). The record shape already carries
// mru_seq so a future bead can add slots 1..7 (design point 2's "8
// per-device records") purely by widening the loop this module's load/save
// use -- no format change needed.
#define PL_PERSIST_DEVICE_SLOT 0u

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
// reflash, not just within one power-on session.
static uint32_t s_next_mru_seq = 1;

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
        s_tlv_impl->delete_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_DEVICE, PL_PERSIST_DEVICE_SLOT));
        s_tlv_impl->delete_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_MARKER, 0));
        s_boot_status = PL_PERSIST_STATUS_VERSION_MISMATCH;
        return;
    }

    // --- Device record: CRC-verified, drop only this record on failure ---
    pl_persist_device_record_t rec;
    int rec_len =
        s_tlv_impl->get_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_DEVICE, PL_PERSIST_DEVICE_SLOT), (uint8_t *)&rec, sizeof(rec));
    if (rec_len != (int)sizeof(rec)) {
        pl_log("persist: marker present but no device record (rec_len=%d) -- valid store, no device yet\r\n", rec_len);
        s_boot_status = PL_PERSIST_STATUS_LOADED;
        return;
    }
    uint16_t crc = pl_persist_crc16((const uint8_t *)&rec, offsetof(pl_persist_device_record_t, crc16));
    if (crc != rec.crc16) {
        pl_log("persist: device record CRC mismatch (got 0x%04x, computed 0x%04x) -- dropping this record only\r\n", rec.crc16, crc);
        s_tlv_impl->delete_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_DEVICE, PL_PERSIST_DEVICE_SLOT));
        s_boot_status = PL_PERSIST_STATUS_RECORD_CORRUPT;
        return;
    }

    memcpy(s_boot_device_addr, rec.addr, 6);
    s_boot_has_device = true;
    s_next_mru_seq = rec.mru_seq + 1;
    s_boot_status = PL_PERSIST_STATUS_LOADED;
    pl_log(
        "persist: loaded device %02x:%02x:%02x:%02x:%02x:%02x (mru_seq=%lu)\r\n", rec.addr[0], rec.addr[1], rec.addr[2], rec.addr[3],
        rec.addr[4], rec.addr[5], (unsigned long)rec.mru_seq
    );
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

void pl_persist_request_save_device(const uint8_t addr[6]) {
    memcpy(s_pending_addr, addr, 6);
    s_pending = true;
    s_pending_since_us = time_us_64();
    // Code-review finding 1: a freshly staged save supersedes whatever the
    // heartbeat may already be about to write for a STALE prior request
    // (e.g. a quick reconnect-to-a-different-device churn) -- the enqueued
    // flag only gates against re-enqueueing the SAME request repeatedly,
    // not against a genuinely new one.
    s_write_enqueued = false;
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
// self->write_offset). Before this fix, persist.c called store_tag/get_tag
// directly from THREAD context (pl_persist_service, the superloop) while
// BTstack writes link keys into that SAME instance synchronously from
// inside its own HCI event dispatch (hci.c's put_link_key), which runs on
// the cyw43/BTstack background async_context -- a real low-priority
// hardware IRQ, not a cooperative poll. A thread-context TLV call
// preempted mid-sequence by that IRQ (or vice versa) corrupts write_offset
// and the bank bookkeeping -- reachable on the exact acceptance path,
// since pl_a2dp_connect() (a2dp.c) requests an urgent flush immediately
// before establishing the very stream whose pairing just triggered
// BTstack's own link-key write for the same device.
//
// FIX: reuse pico-link-ouw's established idiom (bt.c's pl_bt_pending_push/
// pl_bt_pending_service -- see that bead's module doc in bt.c) rather than
// inventing a second mechanism. This function -- the only place that
// actually touches s_tlv_impl for a WRITE after boot -- is now called
// EXCLUSIVELY from pl_bt_pending_service (bt.c), which itself only ever
// runs from pl_bt_wdt_heartbeat_handler: a btstack_run_loop timer callback
// dispatched through the SAME async_context_threadsafe_background work
// queue that runs BTstack's own HCI event dispatch (including put_link_key).
// That queue serializes every callback registered on it to completion
// before starting the next -- so once both sides run through it, they
// cannot preempt each other, closing the race by construction rather than
// by adding a lock. pl_persist_service() (thread context, unchanged
// responsibility: streaming/settle/rate-limit gating) now only DECIDES
// when a write is due and enqueues a request via pl_bt_enqueue_persist_write()
// (bt.h) -- it never touches s_tlv_impl itself.
//
// # Safety / calling contract
//
// MUST be called only from bt.c's pending-queue drain (async_context/IRQ
// context). Calling this from thread context reintroduces exactly the race
// this fix closes.
void pl_persist_execute_pending_write(void) {
    if (!s_pending) {
        s_write_enqueued = false;
        return;
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
        return;
    }

    pl_persist_marker_t marker = {.schema_version = PL_PERSIST_SCHEMA_VERSION};
    s_tlv_impl->store_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_MARKER, 0), (const uint8_t *)&marker, sizeof(marker));

    pl_persist_device_record_t rec;
    memset(&rec, 0, sizeof(rec));
    memcpy(rec.addr, s_pending_addr, 6);
    rec.mru_seq = s_next_mru_seq++;
    // name/codec_id/ldac_quality/volume/flags/preset_id: left zeroed for
    // this MVP slice (design's explicit scope: "MVP slice = marker + one
    // device record + the link key"). A follow-up bead threading the
    // discovered name and negotiated codec through PlCommandTag::PersistDevice's
    // payload can populate these without a format change.
    rec.crc16 = pl_persist_crc16((const uint8_t *)&rec, offsetof(pl_persist_device_record_t, crc16));

    s_tlv_impl->store_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_DEVICE, PL_PERSIST_DEVICE_SLOT), (const uint8_t *)&rec, sizeof(rec));

    s_pending = false;
    s_urgent = false;
    s_write_enqueued = false;
    s_last_write_us = time_us_64();
    s_have_last_write = true;
    pl_log(
        "persist: wrote device record %02x:%02x:%02x:%02x:%02x:%02x (mru_seq=%lu)\r\n", rec.addr[0], rec.addr[1], rec.addr[2],
        rec.addr[3], rec.addr[4], rec.addr[5], (unsigned long)rec.mru_seq
    );
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
