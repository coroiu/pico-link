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

#include "pcm_ring.h"
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
static volatile uint32_t packet_count = 0;

// --- Instrumentation (bead pico-link-icb probe 2): lightweight integer
// counters only -- no printf/vsnprintf here, this file's callbacks run
// inside the 0xC0 worker IRQ via tud_task() (pl_usb_pump_worker_irq).
// Answers whether SET_INTERFACE (for ANY interface, matching or not) or
// any audio control-entity request ever arrives at all, before trusting
// packet_count==0 as evidence the streaming alt-setting was never
// selected. See pl_usb_pump_report for where these get surfaced.
static volatile uint32_t s_set_itf_calls;
static volatile uint8_t s_last_set_itf; // last wIndex low byte seen, any interface
static volatile uint8_t s_last_set_alt; // last wValue low byte seen, any interface
static volatile uint32_t s_fu_get_calls;
static volatile uint32_t s_fu_set_calls;

// --- Instrumentation (bead pico-link-icb probe 3, revision 2 of the fix):
// same rules as above -- plain volatile increments only, no formatting in
// this file's callbacks (they run inside the 0xC0 worker IRQ). These prove
// (a) the streaming alt setting 1 is actually selected, not just that SOME
// SET_INTERFACE arrived, and (b) that the feedback endpoint fixed in this
// revision is actually being serviced once alt 1 opens.
static volatile uint32_t s_set_itf_alt1_calls; // SET_INTERFACE(ITF_NUM_AUDIO_STREAMING, 1) specifically -- primary pass criterion
static volatile uint32_t s_clock_set_calls;    // clock_set_request calls -- does macOS ever set the rate?
static volatile uint32_t s_clk_get_freq_cur;   // clock_get_request, AUDIO_CS_CTRL_SAM_FREQ / AUDIO_CS_REQ_CUR
static volatile uint32_t s_clk_get_freq_range; // clock_get_request, AUDIO_CS_CTRL_SAM_FREQ / AUDIO_CS_REQ_RANGE
static volatile uint32_t s_clk_get_valid;      // clock_get_request, AUDIO_CS_CTRL_CLK_VALID
static volatile uint32_t s_fb_sends;           // tud_audio_feedback_interval_isr firings -- feedback EP actually serviced

//--------------------------------------------------------------------+
// Clock entity (UAC2_ENTITY_CLOCK)
//--------------------------------------------------------------------+
static bool clock_get_request(uint8_t rhport, audio_control_request_t const *request) {
    if (request->bControlSelector == AUDIO_CS_CTRL_SAM_FREQ) {
        if (request->bRequest == AUDIO_CS_REQ_CUR) {
            s_clk_get_freq_cur++;
            audio_control_cur_4_t cur = {(int32_t)tu_htole32(current_sample_rate)};
            return tud_audio_buffer_and_schedule_control_xfer(rhport, (tusb_control_request_t const *)request, &cur, sizeof(cur));
        }
        if (request->bRequest == AUDIO_CS_REQ_RANGE) {
            s_clk_get_freq_range++;
            audio_control_range_4_n_t(N_SAMPLE_RATES) range = {.wNumSubRanges = tu_htole16(N_SAMPLE_RATES)};
            for (uint8_t i = 0; i < N_SAMPLE_RATES; i++) {
                range.subrange[i].bMin = (int32_t)sample_rates[i];
                range.subrange[i].bMax = (int32_t)sample_rates[i];
                range.subrange[i].bRes = 0;
            }
            return tud_audio_buffer_and_schedule_control_xfer(rhport, (tusb_control_request_t const *)request, &range, sizeof(range));
        }
    } else if (request->bControlSelector == AUDIO_CS_CTRL_CLK_VALID && request->bRequest == AUDIO_CS_REQ_CUR) {
        s_clk_get_valid++;
        audio_control_cur_1_t cur_valid = {.bCur = 1};
        return tud_audio_buffer_and_schedule_control_xfer(rhport, (tusb_control_request_t const *)request, &cur_valid, sizeof(cur_valid));
    }
    return false;
}

static bool clock_set_request(uint8_t rhport, audio_control_request_t const *request, uint8_t const *buf) {
    (void)rhport;
    s_clock_set_calls++;
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
    s_fu_get_calls++;
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
    s_fu_set_calls++;
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
    // Record regardless of match -- bead pico-link-icb probe 2 needs to
    // know whether SET_INTERFACE arrives for ANY interface at all, not
    // just the one we expect.
    s_set_itf_calls++;
    s_last_set_itf = itf;
    s_last_set_alt = alt;
    if (itf == ITF_NUM_AUDIO_STREAMING) {
        streaming = (alt != 0);
        if (alt == 1) {
            // Bead pico-link-icb revision 2, primary pass criterion: this is
            // specifically the streaming alt setting being selected, not
            // just any SET_INTERFACE for any interface (s_set_itf_calls
            // above already covers that broader question).
            s_set_itf_alt1_calls++;
        }
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

// Bead pico-link-icb revision 2: TinyUSB's weak default (audio_device.c:520)
// does nothing. Overriding it with a plain counter proves the feedback
// endpoint fixed in this revision is actually being serviced once alt 1
// opens -- the difference between "macOS opened the pipe" and "macOS opened
// it and we are feeding it". Fires from inside the 0xC0 worker IRQ
// (TU_ATTR_FAST_FUNC, called from audio_device.c's SOF handling) -- plain
// volatile increment only, no formatting here.
TU_ATTR_FAST_FUNC void tud_audio_feedback_interval_isr(uint8_t func_id, uint32_t frame_number, uint8_t interval_shift) {
    (void)func_id;
    (void)frame_number;
    (void)interval_shift;
    s_fb_sends++;
}

// Fires once per received isochronous OUT packet, before this file's own
// drain loop below reads the bytes out -- see usb_audio.h's doc comment
// on pl_usb_audio_packet_count. Called by TinyUSB's audio class driver
// from inside tud_task(), i.e. from within the bd pico-link-tfj 0xC0
// worker IRQ.
bool tud_audio_rx_done_pre_read_cb(uint8_t rhport, uint16_t n_bytes_received, uint8_t func_id, uint8_t ep_out, uint8_t cur_alt_setting) {
    (void)rhport;
    (void)n_bytes_received;
    (void)func_id;
    (void)ep_out;
    (void)cur_alt_setting;
    packet_count++;
    return true;
}

//--------------------------------------------------------------------+
// Application-facing API
//--------------------------------------------------------------------+
// Bead pico-link-tfj: called from the 0xC0 worker IRQ
// (pl_usb_pump_worker_irq), right after tud_task(), instead of once per
// superloop iteration -- the drain has to keep pace with the ~1ms re-arm
// deadline, not the render loop's frame time. See usb_pump.h's module doc.
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
        // Feed the USB<->Bluetooth PCM ring so a future consumer (M4's A2DP
        // media timer) can drain it -- nothing drains it yet in M3, see
        // pl_pcm_push's doc comment on frame-granular drop-when-full.
        pl_pcm_push(scratch, n);
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

uint32_t pl_usb_audio_packet_count(void) {
    return packet_count;
}

uint32_t pl_usb_audio_set_itf_calls(void) {
    return s_set_itf_calls;
}

uint8_t pl_usb_audio_last_set_itf(void) {
    return s_last_set_itf;
}

uint8_t pl_usb_audio_last_set_alt(void) {
    return s_last_set_alt;
}

uint32_t pl_usb_audio_fu_get_calls(void) {
    return s_fu_get_calls;
}

uint32_t pl_usb_audio_fu_set_calls(void) {
    return s_fu_set_calls;
}

uint32_t pl_usb_audio_set_itf_alt1_calls(void) {
    return s_set_itf_alt1_calls;
}

uint32_t pl_usb_audio_clock_set_calls(void) {
    return s_clock_set_calls;
}

uint32_t pl_usb_audio_clk_get_freq_cur(void) {
    return s_clk_get_freq_cur;
}

uint32_t pl_usb_audio_clk_get_freq_range(void) {
    return s_clk_get_freq_range;
}

uint32_t pl_usb_audio_clk_get_valid(void) {
    return s_clk_get_valid;
}

uint32_t pl_usb_audio_fb_sends(void) {
    return s_fb_sends;
}
