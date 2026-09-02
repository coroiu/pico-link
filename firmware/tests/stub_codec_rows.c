// Pico Link firmware -- host-buildable stub definitions of the two codec
// rows, for firmware/tests/test_codec_id_stability.c ONLY.
//
// The real pl_codec_sbc/pl_codec_ldac (codec_sbc.c/codec_ldac.c) pull in
// BTstack's classic/avdtp.h and Sony's ldacBT.h, neither host-buildable, so
// this test cannot link the real row definitions. What it CAN and does link
// is the real firmware/src/codec_table.c unmodified -- the actual shipped
// pl_codec_table_find_by_id[_in] search logic under test -- against stand-in
// rows that carry the same PL_CODEC_ID_* identity values the real rows do
// (codec_sbc.c/codec_ldac.c's own `.codec_id = PL_CODEC_ID_*` assignments).
// If those real assignments ever drift from PL_CODEC_ID_SBC/PL_CODEC_ID_LDAC,
// this file's own values below would need updating to match -- there is no
// automatic link between the two beyond both reading the same header
// constants, which is exactly what this test exists to keep honest at the
// table-shape level (identity is a pinned id, not a position).
#include "codec_table.h"

pl_codec_t pl_codec_sbc = {
    .display_name = "SBC",
    .avdtp_codec_type = 0,
    .vendor_id = 0,
    .vendor_codec_id = 0,
    .codec_id = PL_CODEC_ID_SBC,
    .preference = 0,
    .capabilities = NULL,
    .capabilities_len = 0,
    .configuration = NULL,
    .configuration_len = 0,
    .local_seid = 0,
    .init = NULL,
    .encode = NULL,
    .deinit = NULL,
    .state = NULL,
};

pl_codec_t pl_codec_ldac = {
    .display_name = "LDAC",
    .avdtp_codec_type = 0,
    .vendor_id = 0x0000012D,
    .vendor_codec_id = 0x00AA,
    .codec_id = PL_CODEC_ID_LDAC,
    .preference = 0,
    .capabilities = NULL,
    .capabilities_len = 0,
    .configuration = NULL,
    .configuration_len = 0,
    .local_seid = 0,
    .init = NULL,
    .encode = NULL,
    .deinit = NULL,
    .state = NULL,
};
