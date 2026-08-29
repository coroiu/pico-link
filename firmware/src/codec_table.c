// Pico Link firmware -- the codec table itself (M4 S1, bead
// pico-link-cz0.5.2). See codec_table.h's module doc.
//
// S1's whole table is { &pl_codec_sbc }. S4 (a separate bead) prepends
// &pl_codec_ldac. No call site anywhere -- including this file -- switches
// on codec identity; a2dp.c only ever iterates PL_CODECS and reads through
// the vtable.
#include "codec_table.h"

#include "codec_sbc.h"

pl_codec_t *const PL_CODECS[] = {
    &pl_codec_sbc,
};

const size_t PL_CODEC_COUNT = sizeof(PL_CODECS) / sizeof(PL_CODECS[0]);
