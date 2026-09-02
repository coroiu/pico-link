// Pico Link firmware -- host-buildable test for design finding 1
// (.planning/design/2026-09-02-device-page-seam.md sec 1.1, bead
// pico-link-ay0.1): "a stored codec_id must be a pinned per-row identity,
// never the PL_CODECS array index, because array order is the preference
// order and is designed to change."
//
// Links the REAL firmware/src/codec_table.c unmodified (the actual shipped
// pl_codec_table_find_by_id/_find_by_id_in) against stub_codec_rows.c's
// stand-in rows -- see that file's own doc comment for why the real
// codec_sbc.c/codec_ldac.c can't be linked on a host build.
//
// Build + run (no CMake target exists for this -- firmware has no host test
// harness; this is a standalone host binary):
//   cc -std=c11 -Wall -Wextra -I firmware/src \
//      firmware/tests/test_codec_id_stability.c \
//      firmware/tests/stub_codec_rows.c \
//      firmware/src/codec_table.c \
//      -o /tmp/test_codec_id_stability && /tmp/test_codec_id_stability
#include <stdio.h>
#include <string.h>

#include "codec_table.h"
// Both header-only (no BTstack/libldac deps) -- see stub_codec_rows.c's doc
// comment. Needed only for the `extern pl_codec_t pl_codec_sbc/pl_codec_ldac`
// declarations so this TU can name the two rows by address.
#include "codec_ldac.h"
#include "codec_sbc.h"

static int g_failures = 0;

#define CHECK(cond, msg)                                                                                             \
    do {                                                                                                             \
        if (!(cond)) {                                                                                               \
            printf("FAIL: %s (%s:%d)\n", msg, __FILE__, __LINE__);                                                   \
            g_failures++;                                                                                            \
        } else {                                                                                                     \
            printf("ok:   %s\n", msg);                                                                               \
        }                                                                                                            \
    } while (0)

int main(void) {
    // --- Sanity: the real PL_CODECS table (codec_table.c's own
    // { &pl_codec_ldac, &pl_codec_sbc } order) resolves both real ids
    // through the shipped public API. ---
    pl_codec_t *ldac = pl_codec_table_find_by_id(PL_CODEC_ID_LDAC);
    pl_codec_t *sbc = pl_codec_table_find_by_id(PL_CODEC_ID_SBC);
    CHECK(ldac == &pl_codec_ldac, "find_by_id(LDAC) resolves the LDAC row in the shipped table order");
    CHECK(sbc == &pl_codec_sbc, "find_by_id(SBC) resolves the SBC row in the shipped table order");
    CHECK(pl_codec_table_find_by_id(PL_CODEC_ID_AUTOMATIC) == NULL, "find_by_id(AUTOMATIC) is always NULL (0 is never a row)");
    CHECK(pl_codec_table_find_by_id(99) == NULL, "find_by_id(unassigned id) is NULL, not a garbage row");

    // --- The whole point of finding 1: a codec_id pinned by a user must
    // keep meaning the same codec after PL_CODECS is reordered (e.g. a
    // future AAC row inserted ahead of LDAC). Simulate that reordering with
    // a local copy of the table -- exactly what would happen to the real
    // global array on a table-shape change -- and confirm the SAME shipped
    // search logic (pl_codec_table_find_by_id_in) still resolves each
    // pinned id to the SAME codec, not to whatever now sits at its old
    // array position. ---
    pl_codec_t *const shipped_order[] = {&pl_codec_ldac, &pl_codec_sbc};
    pl_codec_t *const reversed_order[] = {&pl_codec_sbc, &pl_codec_ldac};

    // Precondition for the test to mean anything: the two orderings really
    // do disagree about what sits at index 0/1.
    CHECK(shipped_order[0] != reversed_order[0], "the two orderings actually differ at index 0 (test precondition)");

    pl_codec_t *ldac_in_shipped = pl_codec_table_find_by_id_in(shipped_order, 2, PL_CODEC_ID_LDAC);
    pl_codec_t *ldac_in_reversed = pl_codec_table_find_by_id_in(reversed_order, 2, PL_CODEC_ID_LDAC);
    CHECK(ldac_in_shipped == &pl_codec_ldac, "find_by_id_in(shipped order, LDAC) == the LDAC row");
    CHECK(ldac_in_reversed == &pl_codec_ldac, "find_by_id_in(REVERSED order, LDAC) == the SAME LDAC row -- survives reordering");
    CHECK(
        ldac_in_shipped == ldac_in_reversed,
        "a stored codec_id resolves to the identical row regardless of PL_CODECS' array order (design finding 1.1)"
    );

    pl_codec_t *sbc_in_shipped = pl_codec_table_find_by_id_in(shipped_order, 2, PL_CODEC_ID_SBC);
    pl_codec_t *sbc_in_reversed = pl_codec_table_find_by_id_in(reversed_order, 2, PL_CODEC_ID_SBC);
    CHECK(sbc_in_shipped == &pl_codec_sbc, "find_by_id_in(shipped order, SBC) == the SBC row");
    CHECK(sbc_in_reversed == &pl_codec_sbc, "find_by_id_in(REVERSED order, SBC) == the SAME SBC row -- survives reordering");

    // --- The negative this bug would have produced under the rejected
    // "codec_id == array index" design: PL_CODECS[0] in the shipped order
    // is LDAC, but in the reversed order PL_CODECS[0] is SBC. An
    // index-keyed lookup of "index 0" would silently switch which codec a
    // stored preference means the moment the table reorders -- exactly the
    // defect finding 1 exists to prevent. Confirm the two orderings'
    // index-0 entries really are different codecs, so the id-keyed
    // resolution above is actually being tested against a real hazard, not
    // a vacuous one. ---
    CHECK(shipped_order[0]->codec_id != reversed_order[0]->codec_id, "index 0 means a DIFFERENT codec_id across the two orderings (the hazard this bead closes)");

    if (g_failures == 0) {
        printf("\nAll checks passed.\n");
        return 0;
    }
    printf("\n%d check(s) FAILED.\n", g_failures);
    return 1;
}
