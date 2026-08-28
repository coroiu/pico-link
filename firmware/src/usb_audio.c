// Pico Link firmware -- M3: UAC2 speaker application logic.
//
// Own code (see usb_audio.h's provenance note -- the callback SHAPES here
// mirror what every TinyUSB UAC2 example does, since these callback
// signatures are dictated by TinyUSB's audio class driver, but the bodies
// are written fresh for this project's minimal M3 scope: one fixed format,
// no I2S/LDAC consumer yet -- draining the FIFO into a scratch buffer and
// counting bytes IS the "consumer" for this milestone, matching the bead's
// scope boundary (audio arriving and being verifiably correct, not fed
// anywhere yet).

#include <string.h>

#include "tusb.h"

#include "usb_audio.h"
#include "usb_descriptors.h"

//--------------------------------------------------------------------+
// State
//--------------------------------------------------------------------+
static uint32_t current_sample_rate = 48000;
static const uint32_t sample_rates[] = {48000};
#define N_SAMPLE_RATES (sizeof(sample_rates) / sizeof(sample_rates[0]))

// +1 for the master channel (channel 0), per UAC2 feature unit convention.
static int8_t fu_mute[CFG_TUD_AUDIO_FUNC_1_N_CHANNELS_RX + 1];
static int16_t fu_volume[CFG_TUD_AUDIO_FUNC_1_N_CHANNELS_RX + 1];

static volatile bool streaming = false;
static volatile uint32_t pcm_bytes_total = 0;

//--------------------------------------------------------------------+
// Clock entity (UAC2_ENTITY_CLOCK)
//--------------------------------------------------------------------+
static bool clock_get_request(uint8_t rhport, audio_control_request_t const *request) {
    if (request->bControlSelector == AUDIO_CS_CTRL_SAM_FREQ) {
        if (request->bRequest == AUDIO_CS_REQ_CUR) {
            audio_control_cur_4_t cur = {(int32_t)tu_htole32(current_sample_rate)};
            return tud_audio_buffer_and_schedule_control_xfer(rhport, (tusb_control_request_t const *)request, &cur, sizeof(cur));
        }
        if (request->bRequest == AUDIO_CS_REQ_RANGE) {
            audio_control_range_4_n_t(N_SAMPLE_RATES) range = {.wNumSubRanges = tu_htole16(N_SAMPLE_RATES)};
            for (uint8_t i = 0; i < N_SAMPLE_RATES; i++) {
                range.subrange[i].bMin = (int32_t)sample_rates[i];
                range.subrange[i].bMax = (int32_t)sample_rates[i];
                range.subrange[i].bRes = 0;
            }
            return tud_audio_buffer_and_schedule_control_xfer(rhport, (tusb_control_request_t const *)request, &range, sizeof(range));
        }
    } else if (request->bControlSelector == AUDIO_CS_CTRL_CLK_VALID && request->bRequest == AUDIO_CS_REQ_CUR) {
        audio_control_cur_1_t cur_valid = {.bCur = 1};
        return tud_audio_buffer_and_schedule_control_xfer(rhport, (tusb_control_request_t const *)request, &cur_valid, sizeof(cur_valid));
    }
    return false;
}

static bool clock_set_request(uint8_t rhport, audio_control_request_t const *request, uint8_t const *buf) {
    (void)rhport;
    if (request->bRequest != AUDIO_CS_REQ_CUR || request->bControlSelector != AUDIO_CS_CTRL_SAM_FREQ) {
        return false;
    }
    if (request->wLength != sizeof(audio_control_cur_4_t)) {
        return false;
    }
    // M3 offers exactly one rate; accept whatever the host asks for (per
    // UAC2 hosts always SET the rate they just read from RANGE) without
    // re-validating against the table -- matches every upstream example.
    current_sample_rate = (uint32_t)((audio_control_cur_4_t const *)buf)->bCur;
    return true;
}

//--------------------------------------------------------------------+
// Feature unit entity (UAC2_ENTITY_FEATURE_UNIT) -- mute/volume
//--------------------------------------------------------------------+
static bool feature_unit_get_request(uint8_t rhport, audio_control_request_t const *request) {
    uint8_t ch = request->bChannelNumber;
    if (ch >= CFG_TUD_AUDIO_FUNC_1_N_CHANNELS_RX + 1) {
        return false;
    }
    if (request->bControlSelector == AUDIO_FU_CTRL_MUTE && request->bRequest == AUDIO_CS_REQ_CUR) {
        audio_control_cur_1_t cur = {.bCur = fu_mute[ch]};
        return tud_audio_buffer_and_schedule_control_xfer(rhport, (tusb_control_request_t const *)request, &cur, sizeof(cur));
    }
    if (request->bControlSelector == AUDIO_FU_CTRL_VOLUME) {
        if (request->bRequest == AUDIO_CS_REQ_RANGE) {
            audio_control_range_2_n_t(1) range = {
                .wNumSubRanges = tu_htole16(1),
                .subrange[0] = {.bMin = tu_htole16((int16_t)-12800), .bMax = tu_htole16(0), .bRes = tu_htole16(256)},
            };
            return tud_audio_buffer_and_schedule_control_xfer(rhport, (tusb_control_request_t const *)request, &range, sizeof(range));
        }
        if (request->bRequest == AUDIO_CS_REQ_CUR) {
            audio_control_cur_2_t cur = {.bCur = tu_htole16(fu_volume[ch])};
            return tud_audio_buffer_and_schedule_control_xfer(rhport, (tusb_control_request_t const *)request, &cur, sizeof(cur));
        }
    }
    return false;
}

static bool feature_unit_set_request(uint8_t rhport, audio_control_request_t const *request, uint8_t const *buf) {
    (void)rhport;
    uint8_t ch = request->bChannelNumber;
    if (ch >= CFG_TUD_AUDIO_FUNC_1_N_CHANNELS_RX + 1 || request->bRequest != AUDIO_CS_REQ_CUR) {
        return false;
    }
    if (request->bControlSelector == AUDIO_FU_CTRL_MUTE && request->wLength == sizeof(audio_control_cur_1_t)) {
        fu_mute[ch] = ((audio_control_cur_1_t const *)buf)->bCur;
        return true;
    }
    if (request->bControlSelector == AUDIO_FU_CTRL_VOLUME && request->wLength == sizeof(audio_control_cur_2_t)) {
        fu_volume[ch] = ((audio_control_cur_2_t const *)buf)->bCur;
        return true;
    }
    return false;
}

//--------------------------------------------------------------------+
// TinyUSB audio class driver entry points
//--------------------------------------------------------------------+
bool tud_audio_get_req_entity_cb(uint8_t rhport, tusb_control_request_t const *p_request) {
    audio_control_request_t const *request = (audio_control_request_t const *)p_request;
    if (request->bEntityID == UAC2_ENTITY_CLOCK) {
        return clock_get_request(rhport, request);
    }
    if (request->bEntityID == UAC2_ENTITY_FEATURE_UNIT) {
        return feature_unit_get_request(rhport, request);
    }
    return false;
}

bool tud_audio_set_req_entity_cb(uint8_t rhport, tusb_control_request_t const *p_request, uint8_t *buf) {
    audio_control_request_t const *request = (audio_control_request_t const *)p_request;
    if (request->bEntityID == UAC2_ENTITY_FEATURE_UNIT) {
        return feature_unit_set_request(rhport, request, buf);
    }
    if (request->bEntityID == UAC2_ENTITY_CLOCK) {
        return clock_set_request(rhport, request, buf);
    }
    return false;
}

bool tud_audio_set_itf_cb(uint8_t rhport, tusb_control_request_t const *p_request) {
    (void)rhport;
    uint8_t itf = tu_u16_low(tu_le16toh(p_request->wIndex));
    uint8_t alt = tu_u16_low(tu_le16toh(p_request->wValue));
    if (itf == ITF_NUM_AUDIO_STREAMING) {
        streaming = (alt != 0);
    }
    return true;
}

bool tud_audio_set_itf_close_EP_cb(uint8_t rhport, tusb_control_request_t const *p_request) {
    (void)rhport;
    uint8_t itf = tu_u16_low(tu_le16toh(p_request->wIndex));
    uint8_t alt = tu_u16_low(tu_le16toh(p_request->wValue));
    if (itf == ITF_NUM_AUDIO_STREAMING && alt == 0) {
        streaming = false;
    }
    return true;
}

// The "hard part" this bead calls out: tell TinyUSB's audio class driver to
// compute the explicit feedback value itself from OUT-FIFO fill level
// (AUDIO_FEEDBACK_METHOD_FIFO_COUNT) rather than hand-rolling SOF-counting
// clock math. This is the same method uac2_speaker_fb (the vendored
// reference for the feedback endpoint) uses.
void tud_audio_feedback_params_cb(uint8_t func_id, uint8_t alt_itf, audio_feedback_params_t *feedback_param) {
    (void)func_id;
    (void)alt_itf;
    feedback_param->method = AUDIO_FEEDBACK_METHOD_FIFO_COUNT;
    feedback_param->sample_freq = current_sample_rate;
}

//--------------------------------------------------------------------+
// Application-facing API
//--------------------------------------------------------------------+
void pl_usb_audio_task(void) {
    uint16_t avail = tud_audio_available();
    if (avail == 0) {
        return;
    }
    static uint8_t scratch[256];
    while (avail > 0) {
        uint16_t chunk = avail > sizeof(scratch) ? (uint16_t)sizeof(scratch) : avail;
        uint16_t n = tud_audio_read(scratch, chunk);
        pcm_bytes_total += n;
        if (n < chunk) {
            break;
        }
        avail = tud_audio_available();
    }
}

bool pl_usb_audio_streaming(void) {
    return streaming;
}

uint32_t pl_usb_audio_pcm_bytes_total(void) {
    return pcm_bytes_total;
}

uint32_t pl_usb_audio_sample_rate(void) {
    return current_sample_rate;
}
