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
// doc comment), cleared only by pl_persist_service() after it actually
// writes. Lets a "flush on stream stop"/"flush before arming a stream" call
// site skip the settle/rate-limit gates without itself touching flash.
static volatile bool s_urgent;

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
    pl_log(
        "persist: staged save for %02x:%02x:%02x:%02x:%02x:%02x\r\n", addr[0], addr[1], addr[2], addr[3], addr[4], addr[5]
    );
}

// The actual flash write -- writes the marker (idempotent, cheap to
// rewrite every time so a first save always leaves a consistent store even
// if a prior boot never wrote one) and the device record. Caller
// (pl_persist_service/pl_persist_flush_now) has already applied the
// streaming gate; this function itself re-checks it defensively (belt and
// suspenders -- "NO flash write while streaming, of any size" per design
// point 4 has no exception).
static void pl_persist_write_now(void) {
    if (pl_usb_audio_streaming() || pl_a2dp_streaming()) {
        // Should be unreachable (both call sites already gate on this),
        // but never write flash on a bad assumption -- leave it pending
        // for the next safe iteration instead.
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
    if (s_urgent) {
        pl_persist_write_now();
        return;
    }
    uint64_t now_us = time_us_64();
    if (now_us - s_pending_since_us < PL_PERSIST_SETTLE_US) {
        return;
    }
    if (s_have_last_write && (now_us - s_last_write_us < PL_PERSIST_MIN_INTERVAL_US)) {
        return;
    }
    pl_persist_write_now();
}

void pl_persist_request_urgent_flush(void) {
    if (!s_pending) {
        return;
    }
    s_urgent = true;
}
