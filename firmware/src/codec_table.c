// Pico Link firmware -- the codec table itself (M4 S1, bead
// pico-link-cz0.5.2). See codec_table.h's module doc.
//
// S1's whole table was { &pl_codec_sbc }. S4 (bead pico-link-cz0.5.6)
// prepends &pl_codec_ldac -- ARRAY ORDER is the preference order a2dp.c's
// CAPABILITIES_COMPLETE handler actually walks (it iterates PL_CODECS in
// order, first match wins; each row's own .preference field is
// documentation of that same order, not something re-sorted at runtime --
// see codec_table.h's doc comment). LDAC first, SBC permanently last as
// the mandatory fallback floor (codec_table.h:97-99). No call site
// anywhere -- including this file -- switches on codec identity; a2dp.c
// only ever iterates PL_CODECS and reads through the vtable.
#include "codec_table.h"

#include "codec_ldac.h"
#include "codec_sbc.h"

pl_codec_t *const PL_CODECS[] = {
    &pl_codec_ldac,
    &pl_codec_sbc,
};

const size_t PL_CODEC_COUNT = sizeof(PL_CODECS) / sizeof(PL_CODECS[0]);

pl_codec_t *pl_codec_table_find_by_id_in(pl_codec_t *const *table, size_t count, uint8_t codec_id) {
    if (codec_id == PL_CODEC_ID_AUTOMATIC) {
        return NULL;
    }
    for (size_t i = 0; i < count; i++) {
        if (table[i]->codec_id == codec_id) {
            return table[i];
        }
    }
    return NULL;
}

pl_codec_t *pl_codec_table_find_by_id(uint8_t codec_id) {
    return pl_codec_table_find_by_id_in(PL_CODECS, PL_CODEC_COUNT, codec_id);
}
