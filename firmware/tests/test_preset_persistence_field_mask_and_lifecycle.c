// Pico Link firmware -- host-buildable test for bead pico-link-ryw.6's
// persistence work: the field-masked device-settings RMW (persist.c's
// pl_persist_rmw, field-mask application at persist.c:993-1010) and the
// PL:P preset store's save/allocate/overwrite and delete lifecycle
// (pl_persist_execute_pending_save_preset_write at persist.c:1488,
// pl_persist_execute_pending_delete_preset_write at persist.c:1568).
//
// MODEL test, not a link test -- same convention as
// test_paired_device_upserted_ldac_quality_echo.c and every other
// firmware/tests/test_*.c file (firmware/src/persist.c cannot be linked on
// host: BTstack, pico-sdk -- see pico-link-6cho, still open). The functions
// below are copied/simplified from the real ones cited above, with
// s_tlv_impl's flash calls replaced by a plain in-memory "occupied" bool
// per slot (same substitution the real module's own in-RAM slot mirrors
// already make for reads -- see s_slots/s_preset_slots's own doc comments
// in persist.c). If any of the real functions change, update both here and
// there -- nothing enforces that they stay in sync (see LIMITATION below).
//
// What this proves:
//   1. A field-masked write that only sets PL_PERSIST_DEVICE_FIELD_PRESET_ID
//      does not clobber an already-stored codec_id/ldac_quality, and vice
//      versa (design sec 2.3's whole reason for existing).
//   2. PairedDeviceUpserted's echo carries preset_id (design sec 2.3 /
//      bt.h's ABI 5->6 note).
//   3. Save with preset_id==0 allocates a fresh, monotonically increasing
//      id; a later save with a non-zero id overwrites in place without
//      bumping the allocator (design sec 2.2).
//   4. Delete removes only the PL:P slot -- a device record's preset_id
//      field is never touched by a delete (design sec 2.4's "does NOT
//      rewrite device records").
//   5. Forget-device touches no preset slot at all (design sec 2.4's
//      "forget knows nothing about presets" -- holds by construction, this
//      test pins it down).
//
// LIMITATION (same one test_paired_device_upserted_ldac_quality_echo.c's
// own header documents): this is a hand-copied model, not a link test.
// Nothing here compiles or links firmware/src/persist.c itself, so a
// revert of the real fix would NOT fail this test -- it only documents and
// pins down the intended behaviour of the copy. A real link test needs the
// firmware/tests infrastructure pico-link-6cho is tracking.
//
// Build + run (no CMake target -- same convention as the other firmware/
// tests/test_*.c files; -I firmware/src is needed because
// firmware/include/pico_link_ui.h #includes dsp.h from there):
//   cc -std=c11 -Wall -Wextra -I firmware/include -I firmware/src \
//      firmware/tests/test_preset_persistence_field_mask_and_lifecycle.c \
//      -o /tmp/test_preset_persistence_field_mask_and_lifecycle && \
//      /tmp/test_preset_persistence_field_mask_and_lifecycle
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "pico_link_ui.h"

// --- Field-mask bits -- copied verbatim from persist.h's
// pl_persist_device_field_mask_t. ---
#define MODEL_FIELD_CODEC_ID (1u << 0)
#define MODEL_FIELD_LDAC_QUALITY (1u << 1)
#define MODEL_FIELD_PRESET_ID (1u << 2)

// --- Copied (shape, trimmed) from persist.c's pl_persist_device_record_t /
// pl_persist_slot_t -- just the Tier-2 fields this test exercises. ---
typedef struct {
    bool occupied;
    uint8_t addr[6];
    uint8_t codec_id;
    uint8_t ldac_quality;
    uint16_t preset_id;
} model_device_slot_t;

#define MODEL_DEVICE_SLOTS 4
static model_device_slot_t s_device_slots[MODEL_DEVICE_SLOTS];

static struct PlEvent g_last_event;
static bool g_last_event_valid;

static void model_ring_push(struct PlEvent event) {
    g_last_event = event;
    g_last_event_valid = true;
}

// --- Copied verbatim (signature + body), widened for preset_id, from
// bt.c's pl_bt_push_paired_device_upserted (post-ryw.6). ---
static void model_pl_bt_push_paired_device_upserted(
    const uint8_t addr[6], uint8_t ldac_quality, uint16_t preset_id
) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_PAIRED_DEVICE_UPSERTED,
        .payload = {.paired_device_upserted = {.name_len = 0, .mru_seq = 1, .ldac_quality = ldac_quality, .preset_id = preset_id}},
    };
    memcpy(event.payload.paired_device_upserted.addr, addr, 6);
    model_ring_push(event);
}

static int model_find_device_slot(const uint8_t addr[6]) {
    for (int i = 0; i < MODEL_DEVICE_SLOTS; i++) {
        if (s_device_slots[i].occupied && memcmp(s_device_slots[i].addr, addr, 6) == 0) {
            return i;
        }
    }
    return -1;
}

// --- Copied (behaviour), simplified from persist.c's pl_persist_rmw
// field-mask application (persist.c:993-1010) -- flash access replaced by
// direct mutation of the in-memory slot, same "start from the existing
// record, only overwrite masked fields" contract. ---
static bool model_write_device_settings(const uint8_t addr[6], uint8_t field_mask, uint8_t codec_id, uint8_t ldac_quality, uint16_t preset_id) {
    int slot = model_find_device_slot(addr);
    if (slot < 0) {
        return false; // allow_create_slot=false, same as the real function
    }
    model_device_slot_t *sl = &s_device_slots[slot];
    if (field_mask & MODEL_FIELD_CODEC_ID) {
        sl->codec_id = codec_id;
    }
    if (field_mask & MODEL_FIELD_LDAC_QUALITY) {
        sl->ldac_quality = ldac_quality;
    }
    if (field_mask & MODEL_FIELD_PRESET_ID) {
        sl->preset_id = preset_id;
    }
    model_pl_bt_push_paired_device_upserted(sl->addr, sl->ldac_quality, sl->preset_id);
    return true;
}

static void test_field_mask_preset_id_does_not_clobber_codec_settings(void) {
    memset(s_device_slots, 0, sizeof(s_device_slots));
    uint8_t addr[6] = {0x94, 0xDB, 0x56, 0x54, 0x7C, 0xF2};
    s_device_slots[0] = (model_device_slot_t){.occupied = true, .codec_id = 7, .ldac_quality = 3, .preset_id = 0};
    memcpy(s_device_slots[0].addr, addr, 6);

    g_last_event_valid = false;
    bool ok = model_write_device_settings(addr, MODEL_FIELD_PRESET_ID, 0, 0, 42);
    assert(ok);
    assert(s_device_slots[0].codec_id == 7); // untouched
    assert(s_device_slots[0].ldac_quality == 3); // untouched
    assert(s_device_slots[0].preset_id == 42);
    assert(g_last_event_valid);
    assert(g_last_event.payload.paired_device_upserted.preset_id == 42);
    assert(g_last_event.payload.paired_device_upserted.ldac_quality == 3);
    printf("test_field_mask_preset_id_does_not_clobber_codec_settings: PASS\n");
}

static void test_field_mask_ldac_quality_does_not_clobber_preset_id(void) {
    memset(s_device_slots, 0, sizeof(s_device_slots));
    uint8_t addr[6] = {0x94, 0xDB, 0x56, 0x54, 0x7C, 0xF2};
    s_device_slots[0] = (model_device_slot_t){.occupied = true, .codec_id = 0, .ldac_quality = 1, .preset_id = 9};
    memcpy(s_device_slots[0].addr, addr, 6);

    bool ok = model_write_device_settings(addr, MODEL_FIELD_LDAC_QUALITY, 0, 4, 0);
    assert(ok);
    assert(s_device_slots[0].ldac_quality == 4);
    assert(s_device_slots[0].preset_id == 9); // untouched
    printf("test_field_mask_ldac_quality_does_not_clobber_preset_id: PASS\n");
}

// --- Preset store model -- copied (behaviour), simplified from
// pl_persist_execute_pending_save_preset_write (persist.c:1488) and
// pl_persist_execute_pending_delete_preset_write (persist.c:1568). ---
typedef struct {
    bool occupied;
    uint16_t id;
} model_preset_slot_t;

#define MODEL_PRESET_SLOTS 4
static model_preset_slot_t s_preset_slots[MODEL_PRESET_SLOTS];
static uint16_t s_next_preset_id = 1;

static int model_find_preset_slot_for_id(uint16_t id) {
    for (int i = 0; i < MODEL_PRESET_SLOTS; i++) {
        if (s_preset_slots[i].occupied && s_preset_slots[i].id == id) {
            return i;
        }
    }
    return -1;
}

static int model_find_free_preset_slot(void) {
    for (int i = 0; i < MODEL_PRESET_SLOTS; i++) {
        if (!s_preset_slots[i].occupied) {
            return i;
        }
    }
    return -1;
}

// Returns the id actually written, or 0 on refusal (store full / unknown
// id on an update) -- same "0 never allocated" sentinel the real module
// uses.
static uint16_t model_save_preset(uint16_t requested_id) {
    int slot;
    uint16_t id_to_write;
    if (requested_id == 0) {
        slot = model_find_free_preset_slot();
        if (slot < 0) {
            return 0;
        }
        id_to_write = s_next_preset_id++;
    } else {
        slot = model_find_preset_slot_for_id(requested_id);
        if (slot < 0) {
            return 0;
        }
        id_to_write = requested_id;
    }
    s_preset_slots[slot].occupied = true;
    s_preset_slots[slot].id = id_to_write;
    return id_to_write;
}

static bool model_delete_preset(uint16_t id) {
    int slot = model_find_preset_slot_for_id(id);
    if (slot < 0) {
        return false;
    }
    memset(&s_preset_slots[slot], 0, sizeof(s_preset_slots[slot]));
    return true;
}

static void test_save_allocates_monotonic_never_reused_ids(void) {
    memset(s_preset_slots, 0, sizeof(s_preset_slots));
    s_next_preset_id = 1;

    uint16_t id1 = model_save_preset(0);
    uint16_t id2 = model_save_preset(0);
    assert(id1 == 1);
    assert(id2 == 2);

    // Delete id1, then allocate a fresh one -- must NOT reuse id1.
    assert(model_delete_preset(id1));
    uint16_t id3 = model_save_preset(0);
    assert(id3 == 3);
    printf("test_save_allocates_monotonic_never_reused_ids: PASS\n");
}

static void test_save_with_existing_id_overwrites_in_place(void) {
    memset(s_preset_slots, 0, sizeof(s_preset_slots));
    s_next_preset_id = 1;

    uint16_t id1 = model_save_preset(0);
    assert(id1 == 1);
    uint16_t before_next = s_next_preset_id;

    uint16_t id1_again = model_save_preset(id1);
    assert(id1_again == id1);
    assert(s_next_preset_id == before_next); // allocator not bumped
    printf("test_save_with_existing_id_overwrites_in_place: PASS\n");
}

static void test_delete_preset_does_not_touch_device_record(void) {
    memset(s_device_slots, 0, sizeof(s_device_slots));
    memset(s_preset_slots, 0, sizeof(s_preset_slots));
    s_next_preset_id = 1;

    uint16_t id = model_save_preset(0);
    uint8_t addr[6] = {0x94, 0xDB, 0x56, 0x54, 0x7C, 0xF2};
    s_device_slots[0] = (model_device_slot_t){.occupied = true, .preset_id = id};
    memcpy(s_device_slots[0].addr, addr, 6);

    assert(model_delete_preset(id));
    // Design sec 2.4: the device record's preset_id is a dangling
    // reference now -- C never rewrites it. `core` resolves it as Off.
    assert(s_device_slots[0].preset_id == id);
    assert(model_find_preset_slot_for_id(id) < 0);
    printf("test_delete_preset_does_not_touch_device_record: PASS\n");
}

// --- Copied (behaviour) from pl_persist_forget_device (persist.c:1259) --
// forgetting a device only ever touches s_device_slots (the PL:D tag +
// link key); it never references s_preset_slots at all. This holds by
// construction (design sec 2.4: "forget knows nothing about presets"), so
// this test just documents it rather than exercising a real forget path. ---
static void model_forget_device(const uint8_t addr[6]) {
    int slot = model_find_device_slot(addr);
    if (slot < 0) {
        return;
    }
    memset(&s_device_slots[slot], 0, sizeof(s_device_slots[slot]));
    // No reference to s_preset_slots anywhere in this function -- that IS
    // the property under test.
}

static void test_forget_device_leaves_preset_store_untouched(void) {
    memset(s_device_slots, 0, sizeof(s_device_slots));
    memset(s_preset_slots, 0, sizeof(s_preset_slots));
    s_next_preset_id = 1;

    uint16_t id = model_save_preset(0);
    uint8_t addr[6] = {0x94, 0xDB, 0x56, 0x54, 0x7C, 0xF2};
    s_device_slots[0] = (model_device_slot_t){.occupied = true, .preset_id = id};
    memcpy(s_device_slots[0].addr, addr, 6);

    model_forget_device(addr);

    assert(!s_device_slots[0].occupied);
    assert(model_find_preset_slot_for_id(id) == 0); // preset survives
    printf("test_forget_device_leaves_preset_store_untouched: PASS\n");
}

int main(void) {
    test_field_mask_preset_id_does_not_clobber_codec_settings();
    test_field_mask_ldac_quality_does_not_clobber_preset_id();
    test_save_allocates_monotonic_never_reused_ids();
    test_save_with_existing_id_overwrites_in_place();
    test_delete_preset_does_not_touch_device_record();
    test_forget_device_leaves_preset_store_untouched();
    printf("All tests passed.\n");
    return 0;
}
