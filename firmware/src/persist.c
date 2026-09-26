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
// blackouts of ~3ms" figure. `preset_id` is a REFERENCE into the PL:P
// global preset store (design point 2, Andreas's pico-link-ryw constraint:
// "if I forget a device then the EQ will not be lost"; bead pico-link-ryw.6
// wires the read/write side -- see PL_PERSIST_DEVICE_FIELD_PRESET_ID in
// persist.h) -- 0 (PL_PERSIST_PRESET_ID_NONE) means "no preset assigned",
// and so does any id the PL:P store no longer holds (a dangling reference
// after a delete, design `.planning/design/2026-09-25-dsp-effects-
// stage.md` sec 2.4) -- `core` resolves both cases as Off, this module
// never does.
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
//   (LDACBT_EQMID_SQ), 3 = 330 kbps (LDACBT_EQMID_MQ), 4 = Adaptive --
//   IMPLEMENTED, bead pico-link-7jol.3, see .planning/design/2026-09-07-
//   ldac-abr-control-loop.md sec 0.1: the reachable ladder is 5 rungs, not
//   3, and the controller walks it with ldacBT_alter_eqmid_priority. The
//   EQMID mapping itself lives in exactly one place, codec_ldac.c -- this
//   module stores and moves the byte, never interprets it.
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

// Bead pico-link-qivj.5 (S11): PL:S:0, the global display-settings record.
// Self-versioned SEPARATELY from PL_PERSIST_SCHEMA_VERSION (the device/
// marker schema) -- design point D10: a device-schema mismatch must not
// wipe display prefs, and this record's own version byte must not force a
// device-schema bump either. `screensaver_mode`/`screensaver_timeout_s` are
// the raw wire bytes core's `DisplaySettings::to_wire`/`from_wire` already
// define (1=Off, 2=Dim; timeout_s 0=Never/30/60/120/300) -- this module
// stores and moves the bytes, never interprets them (same discipline as
// ldac_quality above).
#define PL_PERSIST_SETTINGS_VERSION 1u
typedef struct __attribute__((packed)) {
    uint8_t version;
    uint8_t screensaver_mode;
    uint16_t screensaver_timeout_s;
    uint16_t crc16;
} pl_persist_display_settings_record_t;

// Bead pico-link-8pp1.4 (S3): PL:S:1, the global congestion-cushion-policy
// record -- design `.planning/design/2026-09-24-congestion-cushion.md`
// sec 4. Own version byte, SEPARATE from PL_PERSIST_SETTINGS_VERSION (see
// PL_PERSIST_INDEX_CUSHION_POLICY's doc comment in persist.h for why).
// `policy` is the raw wire byte `pico_link_core::audio::CushionPolicy::
// to_wire`/`from_wire` already define (0=unset, 1=Low, 2=Stable, 3
// reserved for a possible future Super-stable mode) -- this module stores
// and moves the byte, never interprets it (same discipline as
// screensaver_mode above / ldac_quality in the device record).
#define PL_PERSIST_CUSHION_POLICY_VERSION 1u
typedef struct __attribute__((packed)) {
    uint8_t version;
    uint8_t policy;
    uint16_t crc16;
} pl_persist_cushion_policy_record_t;

// Bead pico-link-d42g.3 (F3): PL:S:2, the global LDAC Adaptive-floor
// record -- design `.planning/design/2026-09-25-adaptive-floor.md` sec 2.
// Own version byte, SEPARATE from PL_PERSIST_SETTINGS_VERSION/
// PL_PERSIST_CUSHION_POLICY_VERSION, same reasoning as PL:S:1 above.
// `floor` is the raw wire byte `pico_link_core::audio::AbrFloor::
// to_wire`/`from_wire` already define (0=unset->330, 1=330, 2=246, 3=198)
// -- this module stores and moves the byte, never interprets it (same
// discipline as `policy` above). Uses the shared
// pl_persist_load_u8_setting/pl_persist_store_u8_setting helpers below
// rather than its own bespoke read/write pair.
#define PL_PERSIST_ABR_FLOOR_VERSION 1u

// Bead pico-link-ryw.6, design `.planning/design/2026-09-25-dsp-effects-
// stage.md` sec 2.2: PL:P:<slot>, one DSP-preset record. `blob` is entirely
// OPAQUE to C -- Rust owns the wire format (to_wire/from_wire, its own
// per-field-fallback version byte inside the blob itself); this module
// only ever stores and moves the `blob_len`-byte prefix, exactly the same
// "C never parses" discipline `pl_persist_display_settings_record_t`'s
// `screensaver_mode` etc. apply to a single byte, just extended to a whole
// buffer. `preset_id` is never 0 in a valid record (0 is
// PL_PERSIST_PRESET_ID_NONE, reserved) and is never reused across the
// store's lifetime (design sec 2.2's monotonic id allocator, s_next_preset_id
// below).
#define PL_PERSIST_PRESET_VERSION 1u
typedef struct __attribute__((packed)) {
    uint8_t version;
    uint16_t preset_id;
    uint8_t blob_len;
    uint8_t blob[PL_PERSIST_PRESET_BLOB_LEN];
    uint16_t crc16;
} pl_persist_preset_record_t;

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

// Bead pico-link-d42g.3 (F3), design `.planning/design/2026-09-25-
// adaptive-floor.md` sec 2: shared load/store helpers for the growing
// family of one-byte PL:S:<i> settings records (PL:S:1 cushion policy,
// PL:S:2 Adaptive floor) -- named as debt in that design ("this is the
// third copy of the 1-byte settings-record boilerplate"). Same wire shape
// as pl_persist_cushion_policy_record_t: {u8 version; u8 value; u16
// crc16}. `name` is a short label for the pl_log lines only (e.g. "abr
// floor") -- it is never written to flash. PL:S:0 (display settings) is
// NOT moved onto this: its layout has an extra u16 field, so it isn't a
// one-byte setting.
typedef struct __attribute__((packed)) {
    uint8_t version;
    uint8_t value;
    uint16_t crc16;
} pl_persist_u8_setting_record_t;

// Returns false, leaving `*out_value` untouched, on any of: absent record,
// wrong length, wrong version, bad CRC -- same "no migration, just fall
// back to the caller's default" contract every PL:S:<i> loader in this
// file already follows (see pl_persist_init's PL:S:0/PL:S:1 blocks).
static bool pl_persist_load_u8_setting(uint8_t index, uint8_t version, const char *name, uint8_t *out_value) {
    pl_persist_u8_setting_record_t rec;
    int rec_len = s_tlv_impl->get_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_SETTINGS, index), (uint8_t *)&rec, sizeof(rec));
    if (rec_len != (int)sizeof(rec)) {
        pl_log("persist: no PL:S:%u %s record -- using default\r\n", (unsigned)index, name);
        return false;
    }
    if (rec.version != version) {
        pl_log(
            "persist: PL:S:%u %s version mismatch (got %u, expected %u) -- using default\r\n", (unsigned)index, name,
            (unsigned)rec.version, (unsigned)version
        );
        return false;
    }
    uint16_t crc = pl_persist_crc16((const uint8_t *)&rec, offsetof(pl_persist_u8_setting_record_t, crc16));
    if (crc != rec.crc16) {
        pl_log(
            "persist: PL:S:%u %s CRC mismatch (got 0x%04x, computed 0x%04x) -- using default\r\n", (unsigned)index, name, rec.crc16,
            crc
        );
        return false;
    }
    *out_value = rec.value;
    pl_log("persist: loaded PL:S:%u %s=%u\r\n", (unsigned)index, name, (unsigned)rec.value);
    return true;
}

static void pl_persist_store_u8_setting(uint8_t index, uint8_t version, uint8_t value) {
    pl_persist_u8_setting_record_t rec = {.version = version, .value = value, .crc16 = 0};
    rec.crc16 = pl_persist_crc16((const uint8_t *)&rec, offsetof(pl_persist_u8_setting_record_t, crc16));
    s_tlv_impl->store_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_SETTINGS, index), (const uint8_t *)&rec, sizeof(rec));
}

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
    // LDACBT_EQMID_* value -- see persist.h's doc comment on
    // pl_persist_request_device_settings).
    uint8_t codec_id;
    uint8_t ldac_quality;
    // Bead pico-link-ryw.6: the PL:P preset id this device references (0 =
    // PL_PERSIST_PRESET_ID_NONE = Off). Same "kept live by every write,
    // never just a boot-time snapshot" discipline as codec_id/ldac_quality
    // above.
    uint16_t preset_id;
} pl_persist_slot_t;
static pl_persist_slot_t s_slots[PL_PERSIST_DEVICE_SLOTS];

// Next mru_seq to stamp on a save -- seeded from whatever was loaded at
// boot (if anything) so a fresh save's mru_seq is monotonic across a
// reflash, not just within one power-on session. Bead pico-link-4vb.6 (T1):
// now the max mru_seq loaded across ALL PL_PERSIST_DEVICE_SLOTS slots, plus
// one (design section 6) -- not just slot 0's.
static uint32_t s_next_mru_seq = 1;

// Bead pico-link-ryw.6, design sec 2.2: the LIVE in-RAM mirror of every
// occupied PL:P preset slot -- same role as s_slots above, just for
// presets. Populated by pl_persist_init()'s preset load loop and kept live
// by pl_persist_execute_pending_save_preset_write/pl_persist_execute_
// pending_delete_preset_write, in the same async_context call that
// performs the flash write/delete itself.
typedef struct {
    bool occupied;
    uint16_t id;
    uint8_t blob_len;
    uint8_t blob[PL_PERSIST_PRESET_BLOB_LEN];
} pl_persist_preset_slot_t;
static pl_persist_preset_slot_t s_preset_slots[PL_PERSIST_PRESET_SLOTS];

// Next preset id to allocate -- seeded from the max id loaded at boot, plus
// one (same discipline as s_next_mru_seq above). Design sec 2.2: "ids are
// monotonic and never reused... at boot, next_id = max(stored) + 1". 0
// (PL_PERSIST_PRESET_ID_NONE) is reserved and never allocated, so this
// always starts at least at 1.
static uint16_t s_next_preset_id = 1;

static pl_persist_status_t s_preset_boot_status = PL_PERSIST_STATUS_FIRST_BOOT;

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

// Bead pico-link-7jol.5, generalized by pico-link-ryw.6: a SECOND,
// independent staging slot for per-device SETTINGS writes (codec_id,
// ldac_quality, preset_id) -- see persist.h's doc comment on
// pl_persist_request_device_settings for why this is not folded into
// s_pending/s_pending_addr above, and for the field-mask coalescing
// contract. `s_device_settings_pending_mask` is an OR of every field a
// not-yet-drained call has staged for the CURRENT `s_device_settings_
// pending_addr` -- a call for a different address resets it (see
// pl_persist_request_device_settings's body).
static bool s_device_settings_pending;
static uint8_t s_device_settings_pending_addr[6];
static uint8_t s_device_settings_pending_mask;
static uint8_t s_device_settings_pending_codec_id;
static uint8_t s_device_settings_pending_ldac_quality;
static uint16_t s_device_settings_pending_preset_id;
static volatile bool s_device_settings_write_enqueued;

// Bead pico-link-ryw.14, Ada's preset-id-allocation contract: an ORDERED,
// per-id staging table for PL:P preset saves/deletes -- replaces the old
// single-slot s_preset_save_pending_id/s_preset_delete_pending_id (which
// silently lost whichever request wasn't staged when two different ids
// landed before the drain, code-review finding on pico-link-ryw.14).
// `core` is now the sole allocator of preset ids, so C's job on a save is
// an UPSERT, not an allocate-or-overwrite decision -- see
// pl_persist_request_save_preset's doc comment for the full contract.
typedef enum {
    PL_PERSIST_PRESET_OP_SAVE,
    PL_PERSIST_PRESET_OP_DELETE,
} pl_persist_preset_op_t;

typedef struct {
    uint16_t id;
    pl_persist_preset_op_t op;
    uint8_t blob_len; // meaningful for PL_PERSIST_PRESET_OP_SAVE only
    uint8_t blob[PL_PERSIST_PRESET_BLOB_LEN];
} pl_persist_preset_stage_entry_t;

// `PL_PERSIST_PRESET_SLOTS * 2`: generous headroom over the flash slot
// budget itself (design comment's own sizing) -- a burst of edits to
// DIFFERENT presets between heartbeat drains is the only way this fills,
// and each drain only ever removes at most one edit's worth of headroom
// per preset actually being edited.
#define PL_PERSIST_PRESET_STAGE_SLOTS (PL_PERSIST_PRESET_SLOTS * 2u)

// Dense array: occupied entries live at indices [0, s_preset_stage_len),
// in FIFO (insertion) order -- draining the head and shifting the rest
// down by one is cheap at this size (at most 16 entries of ~86 bytes
// each), and keeping FIFO order matters because `core` allocates
// monotonically and its SavePreset commands arrive FIFO, so two distinct
// new ids reach the executor's upsert rule (b) in ascending order (see
// pl_persist_execute_pending_save_preset_write's doc comment).
static pl_persist_preset_stage_entry_t s_preset_stage[PL_PERSIST_PRESET_STAGE_SLOTS];
static uint8_t s_preset_stage_len;
static volatile bool s_preset_op_write_enqueued;

// Bead pico-link-qivj.5 (S11): a THIRD, independent staging slot -- global
// display settings (screensaver mode + idle timeout), not per-device, so it
// shares neither the pairing-write slot above nor the per-device-settings
// slot above it. Same short-critical-section RAM-only staging idiom; no
// addr, since this is a singleton record (PL:S:0).
static bool s_display_pending;
static uint8_t s_display_pending_mode;
static uint16_t s_display_pending_timeout_s;
static volatile bool s_display_write_enqueued;

// Bead pico-link-qivj.5 (S11): the PL:S:0 record loaded at boot (see
// pl_persist_init's load, which runs before the PL:M:0 marker check) --
// mirrors s_boot_status's role for the device store, but independent of it.
static bool s_display_settings_loaded;
static uint8_t s_display_settings_mode;
static uint16_t s_display_settings_timeout_s;

// Bead pico-link-8pp1.4 (S3): a FOURTH, independent staging slot -- the
// global cushion policy (PL:S:1), same shape as s_display_pending/
// s_display_settings_loaded above, just one field instead of two.
static bool s_cushion_pending;
static uint8_t s_cushion_pending_policy;
static volatile bool s_cushion_write_enqueued;
static bool s_cushion_policy_loaded;
static uint8_t s_cushion_policy;

// Bead pico-link-d42g.3 (F3): a FIFTH, independent staging slot -- the
// global LDAC Adaptive floor (PL:S:2), same shape as s_cushion_pending/
// s_cushion_policy_loaded above.
static bool s_abr_floor_pending;
static uint8_t s_abr_floor_pending_floor;
static volatile bool s_abr_floor_write_enqueued;
static bool s_abr_floor_loaded;
static uint8_t s_abr_floor;

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

    // --- Bead pico-link-qivj.5 (S11), design D10: load PL:S:0 (display
    // settings) HERE -- BEFORE the PL:M:0 marker check below, and
    // unconditionally (not gated on that check's outcome) -- so neither a
    // first-boot early-return nor a device-schema version-mismatch
    // early-return (both a few lines down) can ever skip it. Self-versioned
    // independently of PL_PERSIST_SCHEMA_VERSION; a bad length, wrong
    // version or failed CRC just leaves s_display_settings_loaded false
    // (pl_persist_boot_display_settings returns false, main.c falls back to
    // core's own default) -- this record is never deleted here even when
    // invalid, unlike the device-store mismatch path, since a truncated/
    // corrupt PL:S:0 isn't evidence the whole store needs resetting.
    {
        pl_persist_display_settings_record_t rec;
        int rec_len = s_tlv_impl->get_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_SETTINGS, 0), (uint8_t *)&rec, sizeof(rec));
        if (rec_len != (int)sizeof(rec)) {
            pl_log("persist: no PL:S:0 display-settings record -- using core defaults\r\n");
        } else if (rec.version != PL_PERSIST_SETTINGS_VERSION) {
            pl_log(
                "persist: PL:S:0 version mismatch (got %u, expected %u) -- using core defaults\r\n", rec.version,
                PL_PERSIST_SETTINGS_VERSION
            );
        } else {
            uint16_t crc = pl_persist_crc16((const uint8_t *)&rec, offsetof(pl_persist_display_settings_record_t, crc16));
            if (crc != rec.crc16) {
                pl_log(
                    "persist: PL:S:0 CRC mismatch (got 0x%04x, computed 0x%04x) -- using core defaults\r\n", rec.crc16, crc
                );
            } else {
                s_display_settings_loaded = true;
                s_display_settings_mode = rec.screensaver_mode;
                s_display_settings_timeout_s = rec.screensaver_timeout_s;
                pl_log(
                    "persist: loaded display settings mode=%u timeout_s=%u\r\n", (unsigned)rec.screensaver_mode,
                    (unsigned)rec.screensaver_timeout_s
                );
            }
        }
    }

    // --- Bead pico-link-8pp1.4 (S3), design `.planning/design/2026-09-24-
    // congestion-cushion.md` sec 4: load PL:S:1 (cushion policy) HERE too --
    // same "before the PL:M:0 marker check, unconditionally" placement as
    // PL:S:0 above, and for the same reason (neither a first-boot nor a
    // device-schema-mismatch early-return below may skip it). Self-
    // versioned independently of both PL_PERSIST_SCHEMA_VERSION AND
    // PL_PERSIST_SETTINGS_VERSION -- a bad length, wrong version or failed
    // CRC just leaves s_cushion_policy_loaded false (pl_persist_boot_
    // cushion_policy returns false, main.c falls back to the compiled-in
    // default, Low) -- this record is never deleted here even when
    // invalid, same reasoning as PL:S:0's own load.
    {
        pl_persist_cushion_policy_record_t rec;
        int rec_len = s_tlv_impl->get_tag(
            &s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_SETTINGS, PL_PERSIST_INDEX_CUSHION_POLICY), (uint8_t *)&rec, sizeof(rec)
        );
        if (rec_len != (int)sizeof(rec)) {
            pl_log("persist: no PL:S:1 cushion-policy record -- using default (Low)\r\n");
        } else if (rec.version != PL_PERSIST_CUSHION_POLICY_VERSION) {
            pl_log(
                "persist: PL:S:1 version mismatch (got %u, expected %u) -- using default (Low)\r\n", rec.version,
                PL_PERSIST_CUSHION_POLICY_VERSION
            );
        } else {
            uint16_t crc = pl_persist_crc16((const uint8_t *)&rec, offsetof(pl_persist_cushion_policy_record_t, crc16));
            if (crc != rec.crc16) {
                pl_log("persist: PL:S:1 CRC mismatch (got 0x%04x, computed 0x%04x) -- using default (Low)\r\n", rec.crc16, crc);
            } else {
                s_cushion_policy_loaded = true;
                s_cushion_policy = rec.policy;
                pl_log("persist: loaded cushion policy=%u\r\n", (unsigned)rec.policy);
            }
        }
    }

    // --- Bead pico-link-d42g.3 (F3), design `.planning/design/2026-09-25-
    // adaptive-floor.md` sec 2: load PL:S:2 (Adaptive floor) HERE too --
    // same "before the PL:M:0 marker check, unconditionally" placement as
    // PL:S:0/PL:S:1 above, and for the same reason. Uses the shared
    // pl_persist_load_u8_setting helper -- a bad length, wrong version or
    // failed CRC just leaves s_abr_floor_loaded false
    // (pl_persist_boot_abr_floor returns false, main.c falls back to the
    // compiled-in default, 330 kbps); this record is never deleted here
    // even when invalid, same reasoning as PL:S:0/PL:S:1's own load.
    {
        uint8_t floor;
        if (pl_persist_load_u8_setting(PL_PERSIST_INDEX_ABR_FLOOR, PL_PERSIST_ABR_FLOOR_VERSION, "abr floor", &floor)) {
            s_abr_floor_loaded = true;
            s_abr_floor = floor;
        }
    }

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
        // Bead pico-link-ryw.6: presets are "our own records" too (this
        // header's module doc) -- wiped alongside devices on a schema
        // mismatch, same as every PL:D tag above.
        for (uint8_t i = 0; i < PL_PERSIST_PRESET_SLOTS; i++) {
            s_tlv_impl->delete_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_PRESET, i));
        }
        s_tlv_impl->delete_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_MARKER, 0));
        s_boot_status = PL_PERSIST_STATUS_VERSION_MISMATCH;
        s_preset_boot_status = PL_PERSIST_STATUS_VERSION_MISMATCH;
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
        sl->preset_id = rec.preset_id;
        any_loaded = true;
        if (rec.mru_seq > max_mru_seq) {
            max_mru_seq = rec.mru_seq;
        }
        pl_log(
            "persist: loaded device slot=%u %02x:%02x:%02x:%02x:%02x:%02x (mru_seq=%lu, name_len=%u, codec_id=%u, "
            "ldac_quality=%u, preset_id=%u)\r\n",
            slot, rec.addr[0], rec.addr[1], rec.addr[2], rec.addr[3], rec.addr[4], rec.addr[5], (unsigned long)rec.mru_seq, rec.name_len,
            rec.codec_id, rec.ldac_quality, rec.preset_id
        );
    }

    s_next_mru_seq = max_mru_seq + 1;
    s_boot_status = any_corrupt ? PL_PERSIST_STATUS_RECORD_CORRUPT : PL_PERSIST_STATUS_LOADED;
    if (!any_loaded) {
        pl_log("persist: marker present but no device records -- valid store, no device yet\r\n");
    }

    // --- Preset records: same per-record-isolation loop shape as the
    // device loop above (design sec 2.2's "load every valid PL:P record
    // before the marker check" is satisfied trivially here -- this runs
    // AFTER the marker check has already confirmed PL_PERSIST_SCHEMA_VERSION
    // matches, same as the device loop it mirrors).
    bool any_preset_loaded = false;
    bool any_preset_corrupt = false;
    uint16_t max_preset_id = 0;
    for (uint8_t slot = 0; slot < PL_PERSIST_PRESET_SLOTS; slot++) {
        pl_persist_preset_record_t rec;
        int rec_len = s_tlv_impl->get_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_PRESET, slot), (uint8_t *)&rec, sizeof(rec));
        if (rec_len != (int)sizeof(rec)) {
            // Slot never written -- not an error, just unoccupied.
            continue;
        }
        if (rec.version != PL_PERSIST_PRESET_VERSION) {
            pl_log(
                "persist: preset record version mismatch in slot %u (got %u, expected %u) -- dropping this record "
                "only\r\n",
                slot, rec.version, PL_PERSIST_PRESET_VERSION
            );
            s_tlv_impl->delete_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_PRESET, slot));
            any_preset_corrupt = true;
            continue;
        }
        uint16_t crc = pl_persist_crc16((const uint8_t *)&rec, offsetof(pl_persist_preset_record_t, crc16));
        if (crc != rec.crc16) {
            pl_log(
                "persist: preset record CRC mismatch in slot %u (got 0x%04x, computed 0x%04x) -- dropping this "
                "record only\r\n",
                slot, rec.crc16, crc
            );
            s_tlv_impl->delete_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_PRESET, slot));
            any_preset_corrupt = true;
            continue;
        }
        if (rec.preset_id == PL_PERSIST_PRESET_ID_NONE) {
            // Should never happen (only pl_persist_execute_pending_save_
            // preset_write ever writes this tag, and it never allocates
            // id 0) -- treat as corrupt rather than trusting a sentinel
            // value as a real id.
            pl_log("persist: preset record in slot %u has id=0 (reserved) -- dropping this record only\r\n", slot);
            s_tlv_impl->delete_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_PRESET, slot));
            any_preset_corrupt = true;
            continue;
        }

        pl_persist_preset_slot_t *psl = &s_preset_slots[slot];
        psl->occupied = true;
        psl->id = rec.preset_id;
        psl->blob_len = rec.blob_len > (uint8_t)sizeof(psl->blob) ? (uint8_t)sizeof(psl->blob) : rec.blob_len;
        memcpy(psl->blob, rec.blob, sizeof(psl->blob));
        any_preset_loaded = true;
        if (rec.preset_id > max_preset_id) {
            max_preset_id = rec.preset_id;
        }
        pl_log("persist: loaded preset slot=%u id=%u blob_len=%u\r\n", slot, rec.preset_id, rec.blob_len);
    }
    s_next_preset_id = (uint16_t)(max_preset_id + 1);
    s_preset_boot_status = any_preset_corrupt ? PL_PERSIST_STATUS_RECORD_CORRUPT : PL_PERSIST_STATUS_LOADED;
    if (!any_preset_loaded) {
        pl_log("persist: marker present but no preset records -- valid store, no presets yet\r\n");
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

void pl_persist_boot_device_at(uint8_t index, uint8_t out_addr[6], uint8_t out_name[32], uint8_t *out_name_len, uint32_t *out_mru_seq, uint8_t *out_ldac_quality, uint16_t *out_preset_id) {
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
            *out_ldac_quality = s_slots[i].ldac_quality;
            *out_preset_id = s_slots[i].preset_id;
            return;
        }
        seen++;
    }
    memset(out_addr, 0, 6);
    memset(out_name, 0, 32);
    *out_name_len = 0;
    *out_mru_seq = 0;
    *out_ldac_quality = 0;
    *out_preset_id = 0;
}

// Bead pico-link-ryw.6, design sec 2.2: same "computed live from the slot
// mirror, called once at boot before any write can run" contract as
// pl_persist_boot_device_count above, just for the preset store.
uint8_t pl_persist_boot_preset_count(void) {
    uint8_t count = 0;
    for (uint8_t i = 0; i < PL_PERSIST_PRESET_SLOTS; i++) {
        if (s_preset_slots[i].occupied) {
            count++;
        }
    }
    return count;
}

// Bead pico-link-ryw.6, design sec 2.2: same "seen == index" walk as
// pl_persist_boot_device_at above.
void pl_persist_boot_preset_at(uint8_t index, uint16_t *out_id, uint8_t *out_blob_len, uint8_t out_blob[PL_PERSIST_PRESET_BLOB_LEN]) {
    uint8_t seen = 0;
    for (uint8_t i = 0; i < PL_PERSIST_PRESET_SLOTS; i++) {
        if (!s_preset_slots[i].occupied) {
            continue;
        }
        if (seen == index) {
            *out_id = s_preset_slots[i].id;
            *out_blob_len = s_preset_slots[i].blob_len;
            memcpy(out_blob, s_preset_slots[i].blob, PL_PERSIST_PRESET_BLOB_LEN);
            return;
        }
        seen++;
    }
    *out_id = 0;
    *out_blob_len = 0;
    memset(out_blob, 0, PL_PERSIST_PRESET_BLOB_LEN);
}

pl_persist_status_t pl_persist_preset_boot_status(void) {
    return s_preset_boot_status;
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
// 1.3, bead pico-link-ay0.1), generalized by bead pico-link-ryw.6 from a
// single `set_codec_settings` bool to a real field mask (design
// `.planning/design/2026-09-25-dsp-effects-stage.md` sec 2.3): what a
// particular RMW call carries and how it should behave, so
// pl_persist_do_write (pairing/reconnect writes) and the field-masked
// device-settings write below (codec pin / LDAC quality pick / preset
// assignment) share ONE read-modify-write core (pl_persist_rmw below)
// instead of forking the RMW logic -- two independently written RMW paths
// over the same on-flash struct is exactly how a CRC-checked store starts
// producing "corruption" nobody can reproduce.
typedef struct {
    // NULL (or non-NULL with name_len == 0) => leave rec.name/rec.name_len
    // exactly as already on record (or zeroed, for a brand-new slot) -- the
    // same RMW convention pl_persist_do_write always used.
    const uint8_t *name;
    uint8_t name_len;
    // An OR of PL_PERSIST_DEVICE_FIELD_* (persist.h) -- only the masked-in
    // fields of codec_id/ldac_quality/preset_id below overwrite the
    // existing record; every other field rides through untouched. A
    // pairing write (pl_persist_do_write) always passes 0 here -- Tier 2
    // settings are never pairing facts.
    uint8_t field_mask;
    uint8_t codec_id;
    uint8_t ldac_quality;
    uint16_t preset_id;
    // Design finding 1.3: a settings write ("I pinned a codec", "I assigned
    // a preset") is NOT "I used this device" -- only a pairing/reconnect
    // write bumps mru_seq. Bumping on a settings write would make a
    // pinned-but-unconnected device the boot auto-reconnect target
    // (core::paired.iter().max_by_key(|d| d.mru_seq)).
    bool bump_mru;
    // true (pl_persist_do_write): the original find-existing-slot else
    // first-free-slot else refuse policy (design section 6, NO eviction).
    // false (the field-masked device-settings write): never claim a fresh
    // slot -- refuse (return false from pl_persist_rmw, writing nothing) if
    // no existing slot holds the target address.
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

    if (fields->field_mask & PL_PERSIST_DEVICE_FIELD_CODEC_ID) {
        rec.codec_id = fields->codec_id;
    }
    if (fields->field_mask & PL_PERSIST_DEVICE_FIELD_LDAC_QUALITY) {
        rec.ldac_quality = fields->ldac_quality;
    }
    if (fields->field_mask & PL_PERSIST_DEVICE_FIELD_PRESET_ID) {
        rec.preset_id = fields->preset_id;
    }
    // Every field NOT in fields->field_mask rides through exactly as read
    // -- a pairing write (field_mask == 0) must not clobber a previously
    // pinned preference or preset assignment, and an AssignPreset write
    // must not clobber a previously pinned codec/quality, etc.

    if (fields->bump_mru) {
        rec.mru_seq = s_next_mru_seq++;
    }
    // else: leave rec.mru_seq exactly as read -- design finding 1.3, a
    // settings write is not a use.

    // volume/flags: untouched -- no caller populates them yet (Tier 2,
    // design section 2 point 5). Rides through RMW unchanged.
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
    sl->preset_id = rec.preset_id;

    s_last_write_us = time_us_64();
    s_have_last_write = true;
    pl_log(
        "persist: wrote device record slot=%d %02x:%02x:%02x:%02x:%02x:%02x (mru_seq=%lu, name_len=%u, codec_id=%u, "
        "ldac_quality=%u, preset_id=%u)\r\n",
        slot, rec.addr[0], rec.addr[1], rec.addr[2], rec.addr[3], rec.addr[4], rec.addr[5], (unsigned long)rec.mru_seq, rec.name_len,
        rec.codec_id, rec.ldac_quality, rec.preset_id
    );
    // Bead pico-link-4vb.7 (T3), widened by pico-link-ryw.6: echo the write
    // that actually landed -- design section 3's single-writer rule ("no
    // echo means no row") means this is the ONLY place
    // PlEventTag::PairedDeviceUpserted is pushed for a save
    // (pl_persist_do_write and the field-masked device-settings write both
    // funnel through this one function).
    pl_bt_push_paired_device_upserted(rec.addr, rec.name, rec.name_len, rec.mru_seq, rec.ldac_quality, rec.preset_id);
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
        .field_mask = 0,
        .codec_id = 0,
        .ldac_quality = 0,
        .preset_id = 0,
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

// Bead pico-link-ryw.6: the field-masked write itself, `static` now --
// nothing outside persist.c ever calls this directly; every caller goes
// through pl_persist_request_device_settings/pl_persist_execute_pending_
// device_settings_write below. See persist.h's doc comment on
// pl_persist_request_device_settings for the full contract.
static bool pl_persist_write_device_settings(const uint8_t addr[6], uint8_t field_mask, uint8_t codec_id, uint8_t ldac_quality, uint16_t preset_id) {
    pl_persist_rmw_fields_t fields = {
        .name = NULL,
        .name_len = 0,
        .field_mask = field_mask,
        .codec_id = codec_id,
        .ldac_quality = ldac_quality,
        .preset_id = preset_id,
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
    // Bead pico-link-7jol.5, extended by pico-link-ryw.6: services every
    // staging slot below independently -- they share no settle timer and
    // no enqueued flag, see s_device_settings_pending's doc comment for why
    // they're not merged.
    if (s_pending && !s_write_enqueued && !(pl_usb_audio_streaming() || pl_a2dp_streaming())) {
        bool due;
        if (s_urgent) {
            due = true;
        } else {
            uint64_t now_us = time_us_64();
            due = (now_us - s_pending_since_us >= PL_PERSIST_SETTLE_US) &&
                  (!s_have_last_write || (now_us - s_last_write_us >= PL_PERSIST_MIN_INTERVAL_US));
        }
        if (due) {
            // Code-review finding 1: no direct flash access here any more
            // -- only enqueues onto bt.c's existing pending-queue/
            // heartbeat mechanism (see pl_persist_execute_pending_write's
            // doc comment above for the full rationale). s_write_enqueued
            // guards against flooding that queue on every subsequent
            // superloop iteration before the heartbeat (up to 100ms
            // later) actually drains this request.
            s_write_enqueued = true;
            pl_bt_enqueue_persist_write();
        }
    }
    if (s_device_settings_pending && !s_device_settings_write_enqueued) {
        // No settle/rate-limit window -- a manual pick is already the
        // debounced event (design sec 5: "applies live", the user pressed
        // A once); nothing to coalesce a burst of.
        //
        // Bead pico-link-xcmx, Andreas's ruling: unlike the general
        // pairing/link-key write above, this write is NOT gated behind
        // "not streaming". This is a user-initiated write -- the user just
        // pressed A on the quality picker, or a d-pad step in the
        // preset-assignment row -- and the checkmark's check-follows-echo
        // contract (design sec 5.1) means the echo, and therefore the UI
        // feedback, never arrives at all while gated, since the one moment
        // a user is guaranteed to be streaming is the moment they're
        // auditioning a pick by ear. Andreas: "just write. It's fine if
        // audio skips when I'm actively interacting with the device."
        // Background/periodic persistence (the write above, and
        // pl_persist_execute_pending_write) keeps the streaming gate.
        s_device_settings_write_enqueued = true;
        pl_bt_enqueue_device_settings_write();
    }
    if (s_preset_stage_len > 0 && !s_preset_op_write_enqueued) {
        // Bead pico-link-ryw.6, design sec 2.5 / Andreas's ryw.6 ruling:
        // "save immediately on every value change" -- same "no streaming
        // gate, user-initiated" treatment as the block above. Bead
        // pico-link-ryw.14: ONE shared flag/queue entry drains whichever
        // operation (save or delete) is oldest in the ordered table, same
        // "one entry in flight at a time" guard against flooding the
        // pending-action queue (capacity 8) the pre-ryw.14 two-flag version
        // had, now covering both operation kinds through a single flag
        // since they share one table.
        s_preset_op_write_enqueued = true;
        pl_bt_enqueue_save_preset_write();
    }
    if (s_display_pending && !s_display_write_enqueued) {
        // Bead pico-link-qivj.5 (S11), design D11 (Andreas's xcmx ruling,
        // extended to this record): same "just write, no streaming gate"
        // treatment as the per-device-settings block above -- a Settings
        // row pick is a user-initiated write, not background persistence.
        s_display_write_enqueued = true;
        pl_bt_enqueue_display_settings_write();
    }
    if (s_cushion_pending && !s_cushion_write_enqueued) {
        // Bead pico-link-8pp1's design sec 4 / Andreas's 2026-09-24 ruling
        // (D11 precedent): no streaming re-check here, matching the
        // display-settings block above -- this is a user-initiated write
        // that is allowed to skip audio rather than silently delay.
        s_cushion_write_enqueued = true;
        pl_bt_enqueue_cushion_policy_write();
    }
    if (s_abr_floor_pending && !s_abr_floor_write_enqueued) {
        // Bead pico-link-d42g.3's design sec 2/4 (D11 precedent, same as
        // the cushion-policy block above): no streaming re-check here --
        // this is a user-initiated write that is allowed to skip audio
        // rather than silently delay.
        s_abr_floor_write_enqueued = true;
        pl_bt_enqueue_abr_floor_write();
    }
}

// Bead pico-link-ryw.6. See persist.h's doc comment on
// pl_persist_request_device_settings for the full field-mask/coalescing
// contract.
void pl_persist_request_device_settings(const uint8_t addr[6], uint8_t field_mask, uint8_t codec_id, uint8_t ldac_quality, uint16_t preset_id) {
    uint32_t irq_state = save_and_disable_interrupts();
    if (s_device_settings_pending && memcmp(s_device_settings_pending_addr, addr, 6) != 0) {
        // A not-yet-drained request for a DIFFERENT address is staged --
        // this call supersedes it entirely (same "freshest wins" policy
        // pl_persist_request_save_device already has for the pairing
        // slot), rather than mixing field masks/values across two devices.
        s_device_settings_pending_mask = 0;
    }
    memcpy(s_device_settings_pending_addr, addr, 6);
    if (field_mask & PL_PERSIST_DEVICE_FIELD_CODEC_ID) {
        s_device_settings_pending_codec_id = codec_id;
    }
    if (field_mask & PL_PERSIST_DEVICE_FIELD_LDAC_QUALITY) {
        s_device_settings_pending_ldac_quality = ldac_quality;
    }
    if (field_mask & PL_PERSIST_DEVICE_FIELD_PRESET_ID) {
        s_device_settings_pending_preset_id = preset_id;
    }
    // OR in, don't overwrite: a field this call didn't touch but an
    // earlier, not-yet-drained call (for the SAME address) already staged
    // stays staged -- see this function's doc comment in persist.h.
    s_device_settings_pending_mask |= field_mask;
    s_device_settings_pending = true;
    restore_interrupts(irq_state);
}

// Bead pico-link-ryw.6. See persist.h's doc comment.
void pl_persist_execute_pending_device_settings_write(void) {
    if (!s_device_settings_pending) {
        s_device_settings_write_enqueued = false;
        return;
    }
    // Bead pico-link-xcmx / ryw.6: no streaming re-check here, matching the
    // enqueue side above -- this is the user-initiated write Andreas ruled
    // should just go through, streaming or not.

    uint8_t addr[6];
    uint8_t field_mask;
    uint8_t codec_id;
    uint8_t ldac_quality;
    uint16_t preset_id;
    uint32_t irq_state = save_and_disable_interrupts();
    memcpy(addr, s_device_settings_pending_addr, 6);
    field_mask = s_device_settings_pending_mask;
    codec_id = s_device_settings_pending_codec_id;
    ldac_quality = s_device_settings_pending_ldac_quality;
    preset_id = s_device_settings_pending_preset_id;
    s_device_settings_pending = false;
    s_device_settings_pending_mask = 0;
    restore_interrupts(irq_state);

    // pl_persist_rmw (via pl_persist_write_device_settings) starts from the
    // EXISTING record and only applies field_mask's bits -- no pre-read
    // needed here to avoid clobbering an unmasked field, unlike the old
    // ldac_quality-only pair this replaces.
    pl_persist_write_device_settings(addr, field_mask, codec_id, ldac_quality, preset_id);
    s_device_settings_write_enqueued = false;
}

// Bead pico-link-ryw.6, design sec 2.2. Finds which PL:P slot currently
// holds `id`, if any -- returns the slot index or -1. Consults the in-RAM
// s_preset_slots mirror, not flash, same discipline as
// pl_persist_find_slot_for_addr.
static int pl_persist_find_preset_slot_for_id(uint16_t id) {
    for (uint8_t i = 0; i < PL_PERSIST_PRESET_SLOTS; i++) {
        if (s_preset_slots[i].occupied && s_preset_slots[i].id == id) {
            return (int)i;
        }
    }
    return -1;
}

// Bead pico-link-ryw.6, design sec 2.2. Finds the first unoccupied PL:P
// slot, or -1 if every slot is in use -- same "no eviction" policy as
// pl_persist_find_free_slot.
static int pl_persist_find_free_preset_slot(void) {
    for (uint8_t i = 0; i < PL_PERSIST_PRESET_SLOTS; i++) {
        if (!s_preset_slots[i].occupied) {
            return (int)i;
        }
    }
    return -1;
}

// Bead pico-link-ryw.14. See persist.h's pl_persist_preset_next_id doc
// comment.
uint16_t pl_persist_preset_next_id(void) {
    return s_next_preset_id;
}

// Bead pico-link-ryw.14: pushes the truth echo for `id` -- PlEventTag::
// PresetLoaded if a slot currently holds it (the new blob on a successful
// upsert, the unchanged old one on a same-id refusal), else PlEventTag::
// PresetDeleted (a refused creation never had a slot, or a real deletion
// just removed one). Called after EVERY save/delete attempt, successful or
// refused -- see pl_persist_execute_pending_save_preset_write's doc
// comment.
static void pl_persist_push_preset_truth_echo(uint16_t id) {
    int slot = pl_persist_find_preset_slot_for_id(id);
    if (slot >= 0) {
        pl_bt_push_preset_loaded(id, s_preset_slots[(uint8_t)slot].blob_len, s_preset_slots[(uint8_t)slot].blob);
    } else {
        pl_bt_push_preset_deleted(id);
    }
}

// Bead pico-link-ryw.14: stages `op` for `id` into the ordered per-id
// table -- replace-in-place if `id` is already staged (latest wins, FIFO
// position kept), else append if there's room. Returns false (nothing
// staged) if the table is already full AND `id` wasn't already staged --
// the caller pushes the truth echo itself in that case (see
// pl_persist_request_save_preset/pl_persist_request_delete_preset).
static bool pl_persist_stage_preset_op(uint16_t id, pl_persist_preset_op_t op, uint8_t blob_len, const uint8_t *blob) {
    uint32_t irq_state = save_and_disable_interrupts();
    for (uint8_t i = 0; i < s_preset_stage_len; i++) {
        if (s_preset_stage[i].id == id) {
            s_preset_stage[i].op = op;
            s_preset_stage[i].blob_len = 0;
            if (op == PL_PERSIST_PRESET_OP_SAVE) {
                uint8_t copy_len = blob_len > (uint8_t)PL_PERSIST_PRESET_BLOB_LEN ? (uint8_t)PL_PERSIST_PRESET_BLOB_LEN : blob_len;
                memcpy(s_preset_stage[i].blob, blob, copy_len);
                if (copy_len < (uint8_t)PL_PERSIST_PRESET_BLOB_LEN) {
                    memset(s_preset_stage[i].blob + copy_len, 0, PL_PERSIST_PRESET_BLOB_LEN - copy_len);
                }
                s_preset_stage[i].blob_len = copy_len;
            }
            restore_interrupts(irq_state);
            return true;
        }
    }
    if (s_preset_stage_len >= PL_PERSIST_PRESET_STAGE_SLOTS) {
        restore_interrupts(irq_state);
        return false;
    }
    pl_persist_preset_stage_entry_t *entry = &s_preset_stage[s_preset_stage_len];
    entry->id = id;
    entry->op = op;
    entry->blob_len = 0;
    if (op == PL_PERSIST_PRESET_OP_SAVE) {
        uint8_t copy_len = blob_len > (uint8_t)PL_PERSIST_PRESET_BLOB_LEN ? (uint8_t)PL_PERSIST_PRESET_BLOB_LEN : blob_len;
        memcpy(entry->blob, blob, copy_len);
        if (copy_len < (uint8_t)PL_PERSIST_PRESET_BLOB_LEN) {
            memset(entry->blob + copy_len, 0, PL_PERSIST_PRESET_BLOB_LEN - copy_len);
        }
        entry->blob_len = copy_len;
    }
    s_preset_stage_len++;
    restore_interrupts(irq_state);
    return true;
}

// Bead pico-link-ryw.6, contract rewritten by pico-link-ryw.14. See
// persist.h's doc comment.
void pl_persist_request_save_preset(uint16_t preset_id, uint8_t blob_len, const uint8_t *blob) {
    if (preset_id == PL_PERSIST_PRESET_ID_NONE) {
        // Rule (d): `core` never sends 0 any more -- refuse and log only,
        // no echo (there is nothing meaningful to echo for the reserved
        // sentinel id).
        pl_log("persist: save_preset id=0 (reserved) -- refusing, core never sends this any more\r\n");
        return;
    }
    if (!pl_persist_stage_preset_op(preset_id, PL_PERSIST_PRESET_OP_SAVE, blob_len, blob)) {
        pl_log("persist: preset stage table full (%u entries) -- refusing id=%u\r\n", (unsigned)PL_PERSIST_PRESET_STAGE_SLOTS, (unsigned)preset_id);
        pl_persist_push_preset_truth_echo(preset_id);
    }
}

// Bead pico-link-ryw.6, contract rewritten by pico-link-ryw.14 -- drains
// the oldest staged preset operation (save OR delete; both
// pl_persist_request_save_preset and pl_persist_request_delete_preset feed
// the same table). See persist.h's doc comment on
// pl_persist_execute_pending_save_preset_write for the full upsert-rules/
// truth-echo contract this implements.
static void pl_persist_drain_preset_stage_head(void) {
    if (s_preset_stage_len == 0) {
        s_preset_op_write_enqueued = false;
        return;
    }

    pl_persist_preset_stage_entry_t entry = s_preset_stage[0];
    uint32_t irq_state = save_and_disable_interrupts();
    for (uint8_t i = 1; i < s_preset_stage_len; i++) {
        s_preset_stage[i - 1] = s_preset_stage[i];
    }
    s_preset_stage_len--;
    restore_interrupts(irq_state);

    if (entry.op == PL_PERSIST_PRESET_OP_DELETE) {
        // Design sec 2.4: deletes ONLY the PL:P tag -- never rewrites any
        // PL:D device record, so a device still referencing `entry.id`
        // keeps a dangling reference that `core` resolves as Off.
        int slot = pl_persist_find_preset_slot_for_id(entry.id);
        if (slot >= 0) {
            s_tlv_impl->delete_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_PRESET, (uint8_t)slot));
            memset(&s_preset_slots[(uint8_t)slot], 0, sizeof(s_preset_slots[(uint8_t)slot]));
            pl_log("persist: deleted preset slot=%d id=%u\r\n", slot, (unsigned)entry.id);
        } else {
            pl_log("persist: delete_preset id=%u but no slot holds it -- no-op\r\n", (unsigned)entry.id);
        }
        pl_persist_push_preset_truth_echo(entry.id);
        s_preset_op_write_enqueued = false;
        return;
    }

    // PL_PERSIST_PRESET_OP_SAVE -- Ada's upsert rules (a)-(e).
    int slot = pl_persist_find_preset_slot_for_id(entry.id);
    if (slot < 0) {
        if (entry.id < s_next_preset_id) {
            // Rule (c): `entry.id` was used before and deleted, or is
            // otherwise stale -- refuse. The truth echo (PresetDeleted,
            // since no slot holds it) tells `core` to drop it from its own
            // store rather than believing a save that never happened.
            pl_log("persist: save_preset id=%u is stale (< next_id=%u) -- refusing\r\n", (unsigned)entry.id, (unsigned)s_next_preset_id);
            pl_persist_push_preset_truth_echo(entry.id);
            s_preset_op_write_enqueued = false;
            return;
        }
        slot = pl_persist_find_free_preset_slot();
        if (slot < 0) {
            // Rule (e): a genuinely new (at-or-past-high-water-mark) id,
            // but every slot is occupied -- refuse, no eviction.
            pl_log(
                "persist: preset store full (%u/%u slots used) -- refusing id=%u, no eviction\r\n",
                (unsigned)PL_PERSIST_PRESET_SLOTS, (unsigned)PL_PERSIST_PRESET_SLOTS, (unsigned)entry.id
            );
            pl_persist_push_preset_truth_echo(entry.id);
            s_preset_op_write_enqueued = false;
            return;
        }
        // Rule (b): claim the free slot and raise the high-water mark past
        // this id -- ids are monotonic and never reused (design sec 2.2).
        s_next_preset_id = (uint16_t)(entry.id + 1u);
    }
    // Rule (a) (an existing slot already held `entry.id`) falls straight
    // through to the same write below -- an upsert writes identically
    // either way, only which slot differs.

    pl_persist_preset_record_t rec = {
        .version = PL_PERSIST_PRESET_VERSION,
        .preset_id = entry.id,
        .blob_len = entry.blob_len,
        .crc16 = 0,
    };
    memcpy(rec.blob, entry.blob, sizeof(rec.blob));
    rec.crc16 = pl_persist_crc16((const uint8_t *)&rec, offsetof(pl_persist_preset_record_t, crc16));
    s_tlv_impl->store_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_PRESET, (uint8_t)slot), (const uint8_t *)&rec, sizeof(rec));

    pl_persist_preset_slot_t *psl = &s_preset_slots[(uint8_t)slot];
    psl->occupied = true;
    psl->id = entry.id;
    psl->blob_len = entry.blob_len;
    memcpy(psl->blob, entry.blob, sizeof(psl->blob));

    pl_log("persist: wrote preset slot=%d id=%u blob_len=%u\r\n", slot, (unsigned)entry.id, (unsigned)entry.blob_len);
    pl_persist_push_preset_truth_echo(entry.id);
    s_preset_op_write_enqueued = false;
}

void pl_persist_execute_pending_save_preset_write(void) {
    pl_persist_drain_preset_stage_head();
}

// Bead pico-link-ryw.6. See persist.h's doc comment.
void pl_persist_request_delete_preset(uint16_t preset_id) {
    if (!pl_persist_stage_preset_op(preset_id, PL_PERSIST_PRESET_OP_DELETE, 0, NULL)) {
        pl_log("persist: preset stage table full (%u entries) -- refusing delete id=%u\r\n", (unsigned)PL_PERSIST_PRESET_STAGE_SLOTS, (unsigned)preset_id);
        pl_persist_push_preset_truth_echo(preset_id);
    }
}

void pl_persist_execute_pending_delete_preset_write(void) {
    pl_persist_drain_preset_stage_head();
}

// Bead pico-link-qivj.5 (S11). See persist.h's doc comment.
bool pl_persist_boot_display_settings(uint8_t *mode, uint16_t *timeout_s) {
    if (!s_display_settings_loaded) {
        return false;
    }
    *mode = s_display_settings_mode;
    *timeout_s = s_display_settings_timeout_s;
    return true;
}

// Bead pico-link-qivj.5 (S11). See persist.h's doc comment.
void pl_persist_request_display_settings(uint8_t mode, uint16_t timeout_s) {
    uint32_t irq_state = save_and_disable_interrupts();
    s_display_pending_mode = mode;
    s_display_pending_timeout_s = timeout_s;
    s_display_pending = true;
    restore_interrupts(irq_state);
}

// Bead pico-link-qivj.5 (S11). See persist.h's doc comment.
void pl_persist_execute_pending_display_settings_write(void) {
    if (!s_display_pending) {
        s_display_write_enqueued = false;
        return;
    }
    // Design D11: no streaming re-check here, matching the enqueue side
    // above (pl_persist_service) -- this is the user-initiated write
    // Andreas ruled should just go through, streaming or not.

    uint8_t mode;
    uint16_t timeout_s;
    uint32_t irq_state = save_and_disable_interrupts();
    mode = s_display_pending_mode;
    timeout_s = s_display_pending_timeout_s;
    s_display_pending = false;
    restore_interrupts(irq_state);

    pl_persist_display_settings_record_t rec = {
        .version = PL_PERSIST_SETTINGS_VERSION,
        .screensaver_mode = mode,
        .screensaver_timeout_s = timeout_s,
        .crc16 = 0,
    };
    rec.crc16 = pl_persist_crc16((const uint8_t *)&rec, offsetof(pl_persist_display_settings_record_t, crc16));
    s_tlv_impl->store_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_SETTINGS, 0), (const uint8_t *)&rec, sizeof(rec));
    // Keep the live boot-snapshot mirror in sync too, so a re-read within
    // the same session (there isn't one today, but matches the device
    // record's RMW discipline of never lagging what was actually written)
    // reflects this write immediately.
    s_display_settings_loaded = true;
    s_display_settings_mode = mode;
    s_display_settings_timeout_s = timeout_s;
    pl_log("persist: wrote display settings mode=%u timeout_s=%u\r\n", (unsigned)mode, (unsigned)timeout_s);
    s_display_write_enqueued = false;
}

// Bead pico-link-8pp1.4 (S3). See persist.h's doc comment.
bool pl_persist_boot_cushion_policy(uint8_t *policy) {
    if (!s_cushion_policy_loaded) {
        return false;
    }
    *policy = s_cushion_policy;
    return true;
}

// Bead pico-link-8pp1.4 (S3). See persist.h's doc comment.
void pl_persist_request_cushion_policy(uint8_t policy) {
    uint32_t irq_state = save_and_disable_interrupts();
    s_cushion_pending_policy = policy;
    s_cushion_pending = true;
    restore_interrupts(irq_state);
}

// Bead pico-link-8pp1.4 (S3). See persist.h's doc comment.
void pl_persist_execute_pending_cushion_policy_write(void) {
    if (!s_cushion_pending) {
        s_cushion_write_enqueued = false;
        return;
    }

    uint8_t policy;
    uint32_t irq_state = save_and_disable_interrupts();
    policy = s_cushion_pending_policy;
    s_cushion_pending = false;
    restore_interrupts(irq_state);

    pl_persist_cushion_policy_record_t rec = {
        .version = PL_PERSIST_CUSHION_POLICY_VERSION,
        .policy = policy,
        .crc16 = 0,
    };
    rec.crc16 = pl_persist_crc16((const uint8_t *)&rec, offsetof(pl_persist_cushion_policy_record_t, crc16));
    s_tlv_impl->store_tag(&s_tlv_context, pl_persist_tag(PL_PERSIST_KIND_SETTINGS, PL_PERSIST_INDEX_CUSHION_POLICY), (const uint8_t *)&rec, sizeof(rec));
    // Keep the live boot-snapshot mirror in sync too, same discipline as
    // the display-settings write above.
    s_cushion_policy_loaded = true;
    s_cushion_policy = policy;
    pl_log("persist: wrote cushion policy=%u\r\n", (unsigned)policy);
    s_cushion_write_enqueued = false;
}

// Bead pico-link-d42g.3 (F3). See persist.h's doc comment.
bool pl_persist_boot_abr_floor(uint8_t *floor) {
    if (!s_abr_floor_loaded) {
        return false;
    }
    *floor = s_abr_floor;
    return true;
}

// Bead pico-link-d42g.3 (F3). See persist.h's doc comment.
void pl_persist_request_abr_floor(uint8_t floor) {
    uint32_t irq_state = save_and_disable_interrupts();
    s_abr_floor_pending_floor = floor;
    s_abr_floor_pending = true;
    restore_interrupts(irq_state);
}

// Bead pico-link-d42g.3 (F3). See persist.h's doc comment.
void pl_persist_execute_pending_abr_floor_write(void) {
    if (!s_abr_floor_pending) {
        s_abr_floor_write_enqueued = false;
        return;
    }

    uint8_t floor;
    uint32_t irq_state = save_and_disable_interrupts();
    floor = s_abr_floor_pending_floor;
    s_abr_floor_pending = false;
    restore_interrupts(irq_state);

    pl_persist_store_u8_setting(PL_PERSIST_INDEX_ABR_FLOOR, PL_PERSIST_ABR_FLOOR_VERSION, floor);
    // Keep the live boot-snapshot mirror in sync too, same discipline as
    // the cushion-policy write above.
    s_abr_floor_loaded = true;
    s_abr_floor = floor;
    pl_log("persist: wrote abr floor=%u\r\n", (unsigned)floor);
    s_abr_floor_write_enqueued = false;
}

void pl_persist_request_urgent_flush(void) {
    if (!s_pending) {
        return;
    }
    s_urgent = true;
}
