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

#include "device/usbd_pvt.h" // usbd_edpt_busy -- bead pico-link-okx D9
#include "tusb.h"

#include "pcm_ring.h"
#include "usb_audio.h"
#include "usb_descriptors.h"

// EP1 OUT, the isochronous audio endpoint -- matches usb_descriptors.c's
// EPNUM_AUDIO_OUT and usb_pump.c's PL_EP_AUDIO_OUT (bead pico-link-okx D9).
#define PL_EP_AUDIO_OUT 0x01u

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

// Bead pico-link-4v2.1 (VT1, volume-sync risk gate): "VOL WATCH" toggle.
// Plain volatile flag, no locking -- the only writer is debug_remote.c's
// thread-context console handler. The reader is ALSO thread context
// (debug_remote.c's poll, once per superloop iteration -- see that file):
// logging from inside this file's 0xC0 IRQ callbacks via pl_log() was
// tried first and measured unreliable under this firmware's background
// log-ring congestion (log_drops in the tens of thousands within seconds
// of boot, pl_prio.h's module doc measured the same thing for other
// counters). Polling fu_set_calls()'s counter from thread context and
// publishing through pl_prio.h's spare slot 4 (thread-context-only by
// that module's own contract) is the reliable path -- see debug_remote.c.
static volatile bool s_watch_enabled = false;

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
// Bead pico-link-pbv/pico-link-6vv (C2-8): s_fb_sends/tud_audio_feedback_interval_isr
// is REMOVED here -- Ada found it structurally dead on this TinyUSB version.
// tud_audio_feedback_interval_isr is reached only from audiod_sof_isr, and
// audiod_set_interface disables the SOF consumer unless the feedback method
// is one of the FREQUENCY_* ones (audio_device.c:2013-2025); this build uses
// AUDIO_FEEDBACK_METHOD_DISABLED (see tud_audio_feedback_params_cb below),
// so that ISR can only ever read 0. tud_audio_fb_done_cb (weak,
// audio_device.c:505, invoked per completed feedback transfer at
// audio_device.c:2239) is the only counter that actually answers "is the
// host consuming our feedback" -- see the override below.
static volatile uint32_t s_fb_done;

// --- Instrumentation (bead pico-link-okx D7/D9). Plain volatile increments
// only, same rule as everything else in this file's callbacks (0xC0 worker
// IRQ context via tud_task()). ---
static volatile uint32_t s_rx_bytes_total; // D7: sum of n_bytes_received, EVERY packet -- feeds the sof_isr/packets/rx_bytes conservation table
static volatile uint32_t s_rx_short_packets; // D7: packets where n_bytes_received != 192 (one full 1ms 48kHz/16-bit/stereo UAC2 packet)
static volatile uint32_t s_ep_out_busy_at_alt1_entry; // D9: usbd_edpt_busy(EP1 OUT) was already true the moment alt 1 was selected -- non-fatal capture of the panic precondition

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
            // Bead pico-link-okx D9: non-fatal capture of the panic
            // precondition (rp2040_usb.c:108's "ep %02X was already
            // available", guarded by if (ep->active) in
            // hw_endpoint_xfer_start) at the one other site besides the
            // steady-state worker tick where an arm is attempted -- alt-1
            // entry itself. Andreas's crashed board and Ruby's earlier
            // round-3 panic both happened at/near this transition.
            if (usbd_edpt_busy(rhport, PL_EP_AUDIO_OUT)) {
                s_ep_out_busy_at_alt1_entry++;
            }
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

// M4 S1 (bead pico-link-cz0.5.2), design sec 2/2.1 -- REPLACES M3's
// AUDIO_FEEDBACK_METHOD_FIFO_COUNT. That method derived feedback from the
// fill of TinyUSB's own 784-byte ISO-OUT software FIFO, but
// pl_usb_audio_task() (below) drains that FIFO to empty every millisecond
// -- TinyUSB therefore always saw a permanently-empty FIFO and commanded
// the host toward maximum deviation, continuously. Harmless in M3 (nothing
// downstream); fatal the moment a real consumer (a2dp.c's media timer)
// exists, because the host then runs fast and the PCM ring overruns within
// seconds. Confirmed on hardware before this change (bd pico-link-cz0.5.2's
// dispatch comment): M3's `usb-audio: ... measured=194148 B/s` against a
// 192000 nominal is exactly the signature of a host already reacting to
// SOME feedback signal -- so the fix here is to make that signal correct
// (computed from the PCM ring's own fill, design sec 2.1), not to prove
// the endpoint is reachable at all.
//
// AUDIO_FEEDBACK_METHOD_DISABLED tells TinyUSB "the application computes
// and calls tud_audio_fb_set() itself" -- see pl_usb_audio_feedback_task()
// below, called from the 0xC0 worker (usb_pump.c) right after this file's
// own pl_usb_audio_task() drain, per design sec 2.1's comment "we are
// already there; ring fill is two loads and a mask".
void tud_audio_feedback_params_cb(uint8_t func_id, uint8_t alt_itf, audio_feedback_params_t *feedback_param) {
    (void)func_id;
    (void)alt_itf;
    feedback_param->method = AUDIO_FEEDBACK_METHOD_DISABLED;
    feedback_param->sample_freq = current_sample_rate;
}

// Bead pico-link-pbv/pico-link-6vv (C2-8): replaces the dead
// tud_audio_feedback_interval_isr override (see s_fb_done's doc comment
// above). tud_audio_fb_done_cb fires once per completed feedback OUT
// transfer -- the only signal that answers "is the host actually consuming
// our feedback", and pbv's falsifier F4 (fb_done reads 0 while streaming)
// depends on it. Called from inside the 0xC0 worker IRQ (TinyUSB's device
// task); plain volatile increment only, no formatting here.
void tud_audio_fb_done_cb(uint8_t func_id) {
    (void)func_id;
    s_fb_done++;
}

// Fires once per received isochronous OUT packet, before this file's own
// drain loop below reads the bytes out -- see usb_audio.h's doc comment
// on pl_usb_audio_packet_count. Called by TinyUSB's audio class driver
// from inside tud_task(), i.e. from within the bd pico-link-tfj 0xC0
// worker IRQ.
bool tud_audio_rx_done_pre_read_cb(uint8_t rhport, uint16_t n_bytes_received, uint8_t func_id, uint8_t ep_out, uint8_t cur_alt_setting) {
    (void)rhport;
    (void)func_id;
    (void)ep_out;
    (void)cur_alt_setting;
    packet_count++;
    // Bead pico-link-okx D7: was previously discarded here. 192 bytes = 48
    // stereo 16-bit sample-frames = one full 1ms UAC2 packet at 48kHz --
    // see the bead's CORRECTION 2 for why "short packets" (macOS reducing
    // its own send size) and "missing whole packets" (an unarmed endpoint)
    // are mutually exclusive explanations this counter tells apart.
    s_rx_bytes_total += n_bytes_received;
    if (n_bytes_received != 192) {
        s_rx_short_packets++;
    }
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

// M4 S1, design sec 2.1: the explicit-feedback control loop. Called from
// the 0xC0 worker IRQ (usb_pump.c), right after pl_usb_audio_task()'s
// drain, once per ~1ms tick.
//
// P-only, deliberately (design sec 2.1's rationale): the plant (ring fill)
// is a pure integrator of rate error, so a proportional controller is
// stable with no anti-windup needed. +/-500ppm authority is ~10x a
// realistic crystal mismatch (two decent crystals differ by well under
// +/-100ppm) -- enough to correct sustained drift, deliberately too slow
// to react to a transient (a 4.6KB fill error at full authority takes
// ~48s to correct): transients are absorbed by the ring's capacity
// (design sec 3.3), sustained drift by this loop. Conflating the two jobs
// would give an oscillating buffer.
//
// The EMA exists because the consumer (a2dp.c's media timer, ~13ms
// cadence once ring-fed) drains in bursts, so raw fill sawtooths by
// roughly one tick's worth -- comparable to the target itself. Feeding raw
// fill into a P controller would inject ~100Hz ripple into the feedback
// value; the ~64ms EMA (>>6 each ~1ms tick) filters that out while still
// tracking real drift within a couple of tick periods.
//
// PL_FB_NOMINAL_Q16 hardcodes 48.0 samples/frame (16.16 fixed point) --
// correct because this build offers exactly one sample rate (48000Hz, see
// sample_rates[] above); a future multi-rate build would need this
// proportional to current_sample_rate/1000 instead.
#define PL_FB_NOMINAL_Q16 (48u << 16)
#define PL_FB_MAX_PPM 500

// Bead pico-link-nxf: PROPORTIONAL GAIN, previously conflated with the
// output clamp. It was PL_FB_MAX_PPM, which made the two impossible to
// tune independently -- they are different quantities that happened to
// share a number.
#define PL_FB_KP_PPM 500

// Integral scale: s_fb_i_accum sums -err_bytes once per ~1ms tick, and
// this divides it down to ppm. At a steady 222ppm crystal offset the loop
// converges in roughly 11s from a cold start, which is slow enough not to
// fight the EMA (~64ms) or the ~11ms media-timer sawtooth, and fast
// enough that a listener never reaches the dry-ring regime.
#define PL_FB_KI_DIV 100000

// Anti-windup bound on the integral term alone, in the accumulator's own
// units, so the I contribution can never exceed the total output clamp.
#define PL_FB_I_ACCUM_MAX ((int32_t)PL_FB_MAX_PPM * PL_FB_KI_DIV)

static int32_t s_fb_fill_ema;
// Bead pico-link-nxf: the integral term. WHY IT IS REQUIRED, not a
// refinement: a pure P controller parks at whatever error produces the
// correction it needs, so it CANNOT null a constant offset. The RP2350
// and the host crystal differ by a fixed ~200-270ppm, and with
// ppm = -(err * 500) / 4608 commanding +222ppm demands err = -2046, i.e.
// the ring settles ~2050 bytes BELOW target and stays there by design.
// Measured on hardware over 94s (.research/captures/2026-08-31-ldac-dwell-fix/
// residual.log): fill_ema fell 5664 -> 1764 against a 4608 setpoint,
// fill_min reached 740 bytes (3.9ms of audio, less than one tick's drain),
// stop_ring_empty went 0 -> 7 and credit_clamp_events climbed by 113.
// That dry-ring regime is the residual crackle Andreas reported after the
// dwell-cap fix. The integral term drives the steady-state error to zero
// so the ring sits AT target instead of parked below it.
static int32_t s_fb_i_accum;
// Bead pico-link-pbv (C6): running minimum of the raw (non-EMA'd) fill
// level, sampled at this function's own ~1ms cadence -- the finest-grained
// sampling of pl_pcm_fill_bytes() anywhere in this firmware, so the true
// sawtooth trough is far more likely to be caught here than at a2dp.c's
// coarser ~11ms media-timer cadence. UINT32_MAX sentinel means "never
// sampled yet" (link not up / no streaming started).
static uint32_t s_fill_min = 0xFFFFFFFFu;

void pl_usb_audio_feedback_task(void) {
    uint32_t fill_now = pl_pcm_fill_bytes();
    if (fill_now < s_fill_min) {
        s_fill_min = fill_now;
    }
    s_fb_fill_ema += ((int32_t)fill_now - s_fb_fill_ema) >> 6;
    int32_t err_bytes = s_fb_fill_ema - (int32_t)PL_PCM_TARGET_FILL_BYTES;
    int32_t p_ppm = -(err_bytes * PL_FB_KP_PPM) / (int32_t)PL_PCM_TARGET_FILL_BYTES;

    // Integrate, then clamp the accumulator itself (not just the output) --
    // clamping only the output is the classic windup bug: the accumulator
    // keeps growing while saturated and then has to unwind before the loop
    // responds at all.
    s_fb_i_accum -= err_bytes;
    if (s_fb_i_accum > PL_FB_I_ACCUM_MAX) {
        s_fb_i_accum = PL_FB_I_ACCUM_MAX;
    }
    if (s_fb_i_accum < -PL_FB_I_ACCUM_MAX) {
        s_fb_i_accum = -PL_FB_I_ACCUM_MAX;
    }
    int32_t ppm = p_ppm + (s_fb_i_accum / PL_FB_KI_DIV);
    if (ppm > PL_FB_MAX_PPM) {
        ppm = PL_FB_MAX_PPM;
    }
    if (ppm < -PL_FB_MAX_PPM) {
        ppm = -PL_FB_MAX_PPM;
    }
    tud_audio_fb_set((uint32_t)((int32_t)PL_FB_NOMINAL_Q16 + (int32_t)((int64_t)PL_FB_NOMINAL_Q16 * ppm / 1000000)));
}

// Bead pico-link-pbv (C6): exposes the EMA pl_usb_audio_feedback_task
// already computes every ~1ms, for pl_a2dp_report (thread context) to
// print -- see design sec 2.1's doc comment on why the EMA, not raw fill,
// is the correct thing to evaluate the closed-loop pass criterion against
// (raw fill sawtooths by roughly one tick's worth, comparable to the
// target itself).
int32_t pl_usb_audio_fb_fill_ema(void) {
    return s_fb_fill_ema;
}

// Bead pico-link-pbv round 2 (C2-9): s_fill_min is now a WINDOWED minimum,
// reset every time it is read -- round 1's lifetime minimum was guaranteed
// to latch at 0 the first time pl_pcm_reset() ran (a2dp.c's STREAM_ESTABLISHED/
// SUSPENDED/RELEASED handlers all call it) and stay there forever,
// regardless of how the loop actually behaved afterward, which is why
// "fill_min hit 0" falsified nothing in round 1. Each read (pl_a2dp_report,
// ~1s cadence) now returns the true minimum fill seen only since the
// previous read, then starts a fresh window. Same benign-race convention as
// every other plain counter here -- see this file's module doc.
uint32_t pl_usb_audio_fill_min(void) {
    uint32_t result = s_fill_min;
    s_fill_min = 0xFFFFFFFFu;
    return result;
}

// Bead pico-link-pbv round 2 (C2-9): called from a2dp.c's STREAM_STARTED
// handler. Seeds the EMA to the CURRENT ring fill (rather than letting it
// coast in from whatever it read during priming/idle) so the P controller
// starts streaming at its actual operating point instead of commanding a
// large one-shot ppm correction through the priming transient, and resets
// the windowed fill_min so a stale reading from a previous stream (or from
// before this one started) never gets attributed to this run.
void pl_usb_audio_fb_reset(void) {
    s_fb_fill_ema = (int32_t)pl_pcm_fill_bytes();
    s_fill_min = 0xFFFFFFFFu;
    // Bead pico-link-nxf: the integral term MUST be cleared here too.
    // Carrying an old stream's accumulated correction into a fresh one
    // starts the loop mid-windup against a ring that was just re-primed,
    // which is exactly the transient this reset exists to prevent.
    s_fb_i_accum = 0;
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

uint32_t pl_usb_audio_fb_done(void) {
    return s_fb_done;
}

uint32_t pl_usb_audio_rx_bytes_total(void) {
    return s_rx_bytes_total;
}

uint32_t pl_usb_audio_rx_short_packets(void) {
    return s_rx_short_packets;
}

uint32_t pl_usb_audio_ep_out_busy_at_alt1_entry(void) {
    return s_ep_out_busy_at_alt1_entry;
}

// Bead pico-link-4v2.1 (VT1, volume-sync risk gate).
int16_t pl_usb_audio_fu_volume(uint8_t ch) {
    if (ch >= CFG_TUD_AUDIO_FUNC_1_N_CHANNELS_RX + 1) {
        return 0;
    }
    return fu_volume[ch];
}

int8_t pl_usb_audio_fu_mute(uint8_t ch) {
    if (ch >= CFG_TUD_AUDIO_FUNC_1_N_CHANNELS_RX + 1) {
        return 0;
    }
    return fu_mute[ch];
}

uint8_t pl_usb_audio_fu_channel_count(void) {
    return CFG_TUD_AUDIO_FUNC_1_N_CHANNELS_RX + 1;
}

void pl_usb_audio_set_watch(bool enabled) {
    s_watch_enabled = enabled;
}

bool pl_usb_audio_watch_enabled(void) {
    return s_watch_enabled;
}
