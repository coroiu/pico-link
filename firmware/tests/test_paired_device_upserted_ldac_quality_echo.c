// Pico Link firmware -- host-buildable test for bead pico-link-7jol.5's
// code-review finding: the PairedDeviceUpserted echo silently dropped
// ldac_quality on the wire, reverting the QUALITY row's check and Home's
// ADAPTIVE tag after every pick and every boot even though the flash write
// itself was correct.
//
// MODEL test, not a link test -- same convention as
// test_ldac_abr_controller.c and test_a2dp_tx_ring_count.c.
// firmware/src/bt.c and persist.c cannot be linked on host (BTstack,
// pico-sdk). The functions below are copied verbatim from:
//   - bt.c's pl_bt_push_paired_device_upserted (the event-construction
//     helper the review found unwidened)
//   - bt.c's boot-restore loop body inside BTSTACK_EVENT_STATE/
//     HCI_STATE_WORKING (the boot_addr/boot_name/.../pl_bt_push_... call
//     sequence)
//   - persist.c's pl_persist_boot_device_at (the slot-mirror -> out-param
//     read the boot-restore loop depends on)
//   - persist.c's pl_persist_rmw tail (the write-echo call this bead's own
//     quality-write path drives)
// with pl_bt_ring_push replaced by a test-injectable capture (model_ring_push)
// so this file needs no BTstack ring buffer. If any of the four real
// functions change, update both here and there.
//
// What this proves: a non-zero ldac_quality staged in a persist slot
// mirror survives BOTH call sites that push PairedDeviceUpserted --
// boot-restore (bt.c:620) and the quality-write echo (persist.c's
// pl_persist_rmw tail) -- all the way into the pushed PlEvent's payload.
// Before the fix, pl_bt_push_paired_device_upserted took no ldac_quality
// parameter at all and always zero-initialised the field; this test would
// fail to compile against that old signature, and would fail its asserts
// against a version that accepted but dropped the argument.
//
// Build + run (no CMake target -- same convention as the other firmware/
// tests/test_*.c files):
//   cc -std=c11 -Wall -Wextra -I firmware/include \
//      firmware/tests/test_paired_device_upserted_ldac_quality_echo.c \
//      -o /tmp/test_paired_device_upserted_ldac_quality_echo && \
//      /tmp/test_paired_device_upserted_ldac_quality_echo
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "pico_link_ui.h"

// --- Capture stub replacing bt.c's real pl_bt_ring_push ---
static struct PlEvent g_last_event;
static bool g_last_event_valid;

static void model_ring_push(struct PlEvent event) {
    g_last_event = event;
    g_last_event_valid = true;
}

// --- Copied verbatim (signature + body) from bt.c's
// pl_bt_push_paired_device_upserted, post-fix ---
static void model_pl_bt_push_paired_device_upserted(
    const uint8_t addr[6], const uint8_t name[32], uint8_t name_len, uint32_t mru_seq, uint8_t ldac_quality
) {
    struct PlEvent event = {
        .version = PL_EVENT_ABI_VERSION,
        .tag = PL_EVENT_TAG_PAIRED_DEVICE_UPSERTED,
        .payload = {.paired_device_upserted = {.name_len = name_len, .mru_seq = mru_seq, .ldac_quality = ldac_quality}},
    };
    memcpy(event.payload.paired_device_upserted.addr, addr, 6);
    memcpy(event.payload.paired_device_upserted.name, name, sizeof(event.payload.paired_device_upserted.name));
    model_ring_push(event);
}

// --- Copied verbatim (shape) from persist.c's pl_persist_slot_t mirror,
// trimmed to the fields this test exercises ---
typedef struct {
    bool occupied;
    uint8_t addr[6];
    uint8_t name[32];
    uint8_t name_len;
    uint32_t mru_seq;
    uint8_t ldac_quality;
} model_slot_t;

#define MODEL_SLOTS 4
static model_slot_t s_slots[MODEL_SLOTS];

// --- Copied verbatim (body) from persist.c's pl_persist_boot_device_at,
// post-fix, re-scoped to MODEL_SLOTS ---
static void model_pl_persist_boot_device_at(
    uint8_t index, uint8_t out_addr[6], uint8_t out_name[32], uint8_t *out_name_len, uint32_t *out_mru_seq, uint8_t *out_ldac_quality
) {
    uint8_t seen = 0;
    for (uint8_t i = 0; i < MODEL_SLOTS; i++) {
        if (!s_slots[i].occupied) {
            continue;
        }
        if (seen == index) {
            memcpy(out_addr, s_slots[i].addr, 6);
            memcpy(out_name, s_slots[i].name, 32);
            *out_name_len = s_slots[i].name_len;
            *out_mru_seq = s_slots[i].mru_seq;
            *out_ldac_quality = s_slots[i].ldac_quality;
            return;
        }
        seen++;
    }
    memset(out_addr, 0, 6);
    memset(out_name, 0, 32);
    *out_name_len = 0;
    *out_mru_seq = 0;
    *out_ldac_quality = 0;
}

// --- Copied verbatim (shape) from bt.c's boot-restore loop body inside
// BTSTACK_EVENT_STATE/HCI_STATE_WORKING ---
static void model_boot_restore_one(uint8_t index) {
    uint8_t boot_addr[6];
    uint8_t boot_name[32];
    uint8_t boot_name_len;
    uint32_t boot_mru_seq;
    uint8_t boot_ldac_quality;
    model_pl_persist_boot_device_at(index, boot_addr, boot_name, &boot_name_len, &boot_mru_seq, &boot_ldac_quality);
    model_pl_bt_push_paired_device_upserted(boot_addr, boot_name, boot_name_len, boot_mru_seq, boot_ldac_quality);
}

// --- Copied verbatim (the one relevant line) from persist.c's
// pl_persist_rmw tail -- the quality-write echo path ---
static void model_write_echo(const model_slot_t *sl) {
    model_pl_bt_push_paired_device_upserted(sl->addr, sl->name, sl->name_len, sl->mru_seq, sl->ldac_quality);
}

static void test_boot_restore_carries_ldac_quality(void) {
    memset(s_slots, 0, sizeof(s_slots));
    s_slots[0].occupied = true;
    memcpy(s_slots[0].addr, (uint8_t[6]){0x94, 0xDB, 0x56, 0x54, 0x7C, 0xF2}, 6);
    s_slots[0].name_len = 0;
    s_slots[0].mru_seq = 3;
    s_slots[0].ldac_quality = 3; // 330 kbps, "most reliable"

    g_last_event_valid = false;
    model_boot_restore_one(0);

    assert(g_last_event_valid);
    assert(g_last_event.tag == PL_EVENT_TAG_PAIRED_DEVICE_UPSERTED);
    assert(g_last_event.payload.paired_device_upserted.mru_seq == 3);
    assert(g_last_event.payload.paired_device_upserted.ldac_quality == 3);
    printf("test_boot_restore_carries_ldac_quality: PASS\n");
}

static void test_quality_write_echo_carries_ldac_quality(void) {
    model_slot_t sl = {0};
    memcpy(sl.addr, (uint8_t[6]){0x94, 0xDB, 0x56, 0x54, 0x7C, 0xF2}, 6);
    sl.name_len = 0;
    sl.mru_seq = 7;
    sl.ldac_quality = 1; // 990 kbps, "best audio"

    g_last_event_valid = false;
    model_write_echo(&sl);

    assert(g_last_event_valid);
    assert(g_last_event.payload.paired_device_upserted.ldac_quality == 1);
    printf("test_quality_write_echo_carries_ldac_quality: PASS\n");
}

int main(void) {
    test_boot_restore_carries_ldac_quality();
    test_quality_write_echo_carries_ldac_quality();
    printf("All tests passed.\n");
    return 0;
}
