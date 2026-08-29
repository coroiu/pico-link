// Pico Link firmware -- the SBC codec table row (M4 S1, bead
// pico-link-cz0.5.2). See codec_table.h's module doc for the table this
// plugs into.
//
// SBC is mandatory in the A2DP spec, so this row is always present once
// the table has more than one entry (S4 prepends LDAC). The capability
// bytes, the AVDTP media codec capabilities BTstack advertises to the
// sink, mirror BlueKitchen's own example/a2dp_source_demo.c
// (media_sbc_codec_capabilities) -- read directly from the pico-sdk's
// vendored btstack submodule, not USBPods.
#ifndef PICO_LINK_CODEC_SBC_H
#define PICO_LINK_CODEC_SBC_H

#include "codec_table.h"

// The row itself -- referenced by codec_table.c's PL_CODECS array. Defined
// (not just declared) in codec_sbc.c, statically allocated, never malloc'd.
extern pl_codec_t pl_codec_sbc;

// a2dp.c decodes A2DP_SUBEVENT_SIGNALING_MEDIA_CODEC_SBC_CONFIGURATION's
// fields into one of these and passes a pointer to it as pl_codec_sbc's
// init()'s `configuration` argument (cast to `const uint8_t *`,
// configuration_len == sizeof(pl_codec_sbc_negotiated_t)) -- this is this
// row's own private negotiated-config shape, not raw AVDTP wire bytes; see
// codec_table.h's doc comment on pl_codec::init for why that's fine. Field
// shape and semantics mirror BlueKitchen's a2dp_source_demo.c's
// media_codec_configuration_sbc_t exactly (down to allocation_method being
// pre-adjusted -1 from the AVDTP wire value -- see a2dp.c's decode site).
typedef struct {
    int num_channels;
    int sampling_frequency;
    int block_length;
    int subbands;
    int min_bitpool_value;
    int max_bitpool_value;
    uint8_t channel_mode;      // btstack_sbc_channel_mode_t
    uint8_t allocation_method; // btstack_sbc_allocation_method_t
} pl_codec_sbc_negotiated_t;

#endif // PICO_LINK_CODEC_SBC_H
