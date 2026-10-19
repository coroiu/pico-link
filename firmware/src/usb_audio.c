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
#include "volume.h" // pl_volume_notify_host_raw -- T3 (pico-link-4v2.3)

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
// Bead pico-link-ycm2.7.2: s_fb_done/tud_audio_fb_done_cb/
// tud_audio_feedback_params_cb are REMOVED here -- there is no feedback
// endpoint at all any more (design comment on pico-link-ycm2.7, sec 3/4),
// so there is nothing for a feedback-transfer-completed counter to count.
// The prior history (s_fb_sends/tud_audio_feedback_interval_isr found
// structurally dead by Ada, then tud_audio_fb_done_cb added as the
// replacement "is the host consuming our feedback" signal -- bead
// pico-link-pbv/pico-link-6vv C2-8) is moot now that the feedback mechanism
// itself is gone.

// --- Instrumentation (bead pico-link-okx D7/D9). Plain volatile increments
// only, same rule as everything else in this file's callbacks (0xC0 worker
// IRQ context via tud_task()). ---
static volatile uint32_t s_rx_bytes_total; // D7: sum of n_bytes_received, EVERY packet -- feeds the sof_isr/packets/rx_bytes conservation table
static volatile uint32_t s_rx_short_packets; // D7: packets where n_bytes_received != 192 (one full 1ms 48kHz/16-bit/stereo UAC2 packet)
static volatile uint32_t s_ep_out_busy_at_alt1_entry; // D9: usbd_edpt_busy(EP1 OUT) was already true the moment alt 1 was selected -- non-fatal capture of the panic precondition

// --- Instrumentation (bead pico-link-2ue, VT4a): UAC2 status interrupt EP
// risk gate. Plain counters, same convention as the rest of this file.
static volatile uint32_t s_int_sent;  // tud_audio_int_n_write() accepted the send
static volatile uint32_t s_int_done;  // tud_audio_int_done_cb fired -- confirmed on the wire

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
            // Bead pico-link-4v2.2 (VT2), design sec 5: bMin=-12700,
            // bRes=100 gives exactly 127 steps, a BIJECTION with AVRCP's
            // 0..127 absolute-volume domain (volume.c's canonical
            // domain), which is what makes the domain round-trip
            // idempotent (design sec 4.1's termination proof depends on
            // it). Was bMin=-12800/bRes=256 (51 steps against AVRCP's 128
            // -- non-bijective). This is a class-specific GET_RANGE
            // response value, not part of the enumeration descriptor blob
            // in usb_descriptors.h -- same byte length, same interface
            // layout, no alt-setting change, so it carries none of that
            // file's enumeration/alt-setting risk history. VT1
            // (pico-link-4v2.1) measured macOS sending bCur as an exact
            // multiple of whatever bRes we declare with no off-grid
            // values against the OLD range, which is the load-bearing
            // assumption behind this change -- see that bead's RESULTS
            // comment.
            audio_control_range_2_n_t(1) range = {
                .wNumSubRanges = tu_htole16(1),
                .subrange[0] = {.bMin = tu_htole16((int16_t)-12700), .bMax = tu_htole16(0), .bRes = tu_htole16(100)},
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
        // T3 (pico-link-4v2.3), design sec 2/9: the real host->volume.c
        // edge. Only channel 0 (master) -- VT4a.1 (pico-link-rmp) measured
        // macOS writing fu_volume[0] on a host slider move, never a
        // per-channel index; volume.c's canonical value is a single
        // number, not per-channel. This callback runs inside
        // tud_task()/pl_usb_pump_worker_irq, the 0xC0 worker IRQ
        // (design sec 2's context table) -- pl_volume_notify_host_raw is
        // IRQ-safe by contract (volume.h).
        if (ch == 0) {
            pl_volume_notify_host_raw(fu_volume[0]);
        }
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

// Bead pico-link-ycm2.7.2: tud_audio_feedback_params_cb and
// tud_audio_fb_done_cb are REMOVED -- there is no feedback endpoint (see
// usb_descriptors.h) so TinyUSB never calls either; with
// CFG_TUD_AUDIO_ENABLE_FEEDBACK_EP 0 they would not even compile (both are
// behind that #if in audio_device.c/h). Their history: M3 shipped
// AUDIO_FEEDBACK_METHOD_FIFO_COUNT, fatal once a real PCM consumer existed
// (bead pico-link-cz0.5.2); that became AUDIO_FEEDBACK_METHOD_DISABLED plus
// an application-computed tud_audio_fb_set() (design sec 2.1); step 1 of
// this bead (pico-link-ycm2.7.1) moved the rate-matching math to
// drain_servo.c and pinned the feedback value to a constant nominal rate;
// step 2 (this bead) deletes the feedback mechanism entirely.

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

// Bead pico-link-ycm2.7.2: pl_usb_audio_feedback_task and
// PL_FB_NOMINAL_Q16 are REMOVED. Step 1 (pico-link-ycm2.7.1) had already
// moved the real rate-matching math to drain_servo.c and left this function
// pinning the feedback value to a constant nominal rate so pacing
// regressions would show up separately from enumeration; now that the
// feedback endpoint itself is gone (design sec 4), there is nothing left
// for it to pin -- tud_audio_fb_set no longer exists with
// CFG_TUD_AUDIO_ENABLE_FEEDBACK_EP 0. usb_pump.c no longer calls this.

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

// Bead pico-link-rmp (VT4a.1). See usb_audio.h's doc comment.
void pl_usb_audio_fu_set_volume(uint8_t ch, int16_t raw) {
    if (ch >= CFG_TUD_AUDIO_FUNC_1_N_CHANNELS_RX + 1) {
        return;
    }
    if (raw < -12700) {
        raw = -12700;
    }
    if (raw > 0) {
        raw = 0;
    }
    fu_volume[ch] = raw;
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

//--------------------------------------------------------------------+
// UAC2 status interrupt endpoint (bead pico-link-2ue, VT4a risk gate)
//--------------------------------------------------------------------+
// Fires once the interrupt EP transfer this file scheduled has actually
// gone out on the wire -- confirms the packet was sent, not just accepted
// into TinyUSB's buffer. Called from inside the 0xC0 worker IRQ (TinyUSB's
// device task), same context as every other weak-callback override in this
// file -- plain volatile increment only.
void tud_audio_int_done_cb(uint8_t rhport) {
    (void)rhport;
    s_int_done++;
}

bool pl_usb_audio_send_fu_status_interrupt(void) {
    // NO pl_usb_lock_try() here -- this is called ONLY from
    // debug_remote.c's pl_debug_remote_poll(), which already holds
    // pl_usb_mutex for its ENTIRE body (see that file's "Held for the
    // WHOLE poll" doc comment). pl_usb_mutex is pico-sdk's plain
    // (non-recursive) mutex_t: a second mutex_try_enter() from the same
    // thread context without an intervening exit fails every time, not
    // just under real contention -- this file's first revision did exactly
    // that and every call reported accepted=0 (measured, this bead). The
    // outer lock already excludes the 0xC0 worker for the duration of this
    // call, so re-entering it here would only ever deadlock-by-false, not
    // add safety. If a future caller invokes this from OUTSIDE the console
    // poll's locked region, it must take pl_usb_lock_try() itself first.
    //
    // UAC2 spec 6.1 "Interrupt Data Message Format": bInfo=0 (device
    // originated), bAttribute=AUDIO_CS_REQ_CUR ("current value of the
    // addressed Control has changed"). wValue selects the control on the
    // feature unit: channel 0 (master) + AUDIO_FU_CTRL_VOLUME. wIndex
    // identifies WHERE: low byte is the interface number (the audio
    // control interface owns this endpoint), high byte is the entity ID
    // that changed (UAC2_ENTITY_FEATURE_UNIT) -- this is what tells the
    // host WHICH unit/control to re-GET, per spec.
    audio_interrupt_data_t status = {
        .bInfo = 0,
        .bAttribute = (uint8_t)AUDIO_CS_REQ_CUR,
        .wValue_cn_or_mcn = 0, // master channel
        .wValue_cs = AUDIO_FU_CTRL_VOLUME,
        .wIndex_ep_or_int = ITF_NUM_AUDIO_CONTROL,
        .wIndex_entity_id = UAC2_ENTITY_FEATURE_UNIT,
    };
    bool sent = tud_audio_int_n_write(0, &status);
    if (sent) {
        s_int_sent++;
    }
    return sent;
}

uint32_t pl_usb_audio_int_sent(void) {
    return s_int_sent;
}

uint32_t pl_usb_audio_int_done(void) {
    return s_int_done;
}
