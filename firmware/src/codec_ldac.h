// Pico Link firmware -- the LDAC codec table row (M4 S4, bead
// pico-link-cz0.5.6). See codec_table.h's module doc for the table this
// plugs into, and .planning/design/2026-08-30-ldac.md for the design of
// record this implements (stage L3).
//
// LDAC is a vendor-specific A2DP codec (AVDTP_CODEC_NON_A2DP) -- there is
// no BlueKitchen/BTstack-native support for it (unlike SBC), so this row
// owns the whole AVDTP media codec information shape itself: an 8-byte
// vendor blob (vendor ID + vendor codec ID + a one-byte sampling-frequency
// bitmap + a one-byte channel-mode bitmap), matching the well-known LDAC
// A2DP vendor codec layout cited in the design doc's Q2. The encoder
// itself is Sony/libldac (Apache-2.0), vendored under
// firmware/vendor/libldac (bead pico-link-cz0.5.4, L0) -- read directly
// from its own headers, never from USBPods.
#ifndef PICO_LINK_CODEC_LDAC_H
#define PICO_LINK_CODEC_LDAC_H

#include "codec_table.h"

// The row itself -- referenced by codec_table.c's PL_CODECS array. Defined
// (not just declared) in codec_ldac.c, statically allocated, never
// malloc'd (the row struct and its capability/configuration buffers; the
// libldac HANDLE_LDAC_BT itself is heap-allocated ONCE per init() call by
// ldacBT_get_handle() -- design doc Q1, negotiation-time only, confirmed
// never called from encode()).
extern pl_codec_t pl_codec_ldac;

// a2dp.c decodes A2DP_SUBEVENT_SIGNALING_MEDIA_CODEC_OTHER_CONFIGURATION's
// raw media_codec_information bytes (offsets 6/7 of the same 8-byte vendor
// blob shape the capability bytes use -- see codec_ldac.c's
// s_ldac_capabilities) into one of these and passes a pointer to it as
// pl_codec_ldac's init()'s `configuration` argument -- this row's own
// private negotiated-config shape, not raw AVDTP wire bytes (same pattern
// as codec_sbc.h's pl_codec_sbc_negotiated_t; see codec_table.h's doc
// comment on pl_codec::init for why that's fine).
typedef struct {
    uint8_t sampling_frequency; // single LDACBT_SAMPLING_FREQ_* bit (ldacBT.h)
    uint8_t channel_mode;       // single LDACBT_CHANNEL_MODE_* bit (ldacBT.h)
} pl_codec_ldac_negotiated_t;

// The ONE configuration this project ever offers or accepts for LDAC
// (48kHz/stereo -- see codec_ldac.c's doc comment on
// s_ldac_negotiated_info), exposed so a2dp.c's CAPABILITIES_COMPLETE
// handler (the AVDTP_CODEC_NON_A2DP arm of its preference-ordered table
// walk) can pass it straight to a2dp_source_set_config_other once it has
// checked the remote's discovered capability bitmaps accept it. STATIC
// LIFETIME -- a2dp_source_set_config_other stores this pointer, does not
// copy it (design doc trap #1); this buffer must outlive the connection,
// which a file-scope array in codec_ldac.c does by construction.
extern const uint8_t pl_codec_ldac_negotiated_info[8];

#endif // PICO_LINK_CODEC_LDAC_H
