// Pico Link firmware -- M3: UAC2 speaker application logic.
//
// Own code, not derived from any vendored example (the descriptor and
// class-driver plumbing IS derived -- see usb_descriptors.h/c's provenance
// notes; this file is just the small amount of glue TinyUSB's audio class
// driver needs from the application: draining the OUT FIFO, answering
// clock/volume/mute control requests, and counting bytes for verification).
#ifndef PICO_LINK_USB_AUDIO_H
#define PICO_LINK_USB_AUDIO_H

#include <stdbool.h>
#include <stdint.h>

// Call every main-loop iteration, after tud_task(). Drains whatever PCM the
// host has written into the OUT FIFO so far (non-blocking) and advances
// pl_usb_audio_pcm_bytes_total.
void pl_usb_audio_task(void);

// M4 S1 (bead pico-link-cz0.5.2), design sec 2.1: computes and applies the
// explicit USB audio feedback value from the PCM ring's own fill level.
// MUST be called from the same 0xC0 worker IRQ as pl_usb_audio_task(),
// after it -- see usb_audio.c's doc comment on this function and on
// tud_audio_feedback_params_cb (AUDIO_FEEDBACK_METHOD_DISABLED tells
// TinyUSB the application supplies this, not the class driver). This is a
// hard dependency of M4, not optional instrumentation: without it, the
// host runs the ISO OUT stream at an uncontrolled rate and the PCM ring
// overruns within seconds once a consumer (a2dp.c) exists (design sec 2).
void pl_usb_audio_feedback_task(void);

// True once the host has selected the streaming alternate setting (alt 1)
// on the audio streaming interface -- i.e. audio is actually flowing, not
// just enumerated.
bool pl_usb_audio_streaming(void);

// Cumulative bytes of PCM received from the host since boot. Free-running;
// main.c prints the delta over a fixed window to report a measured byte/frame
// rate, per this bead's acceptance criteria (not merely "it enumerated").
uint32_t pl_usb_audio_pcm_bytes_total(void);

// The currently negotiated sample rate (Hz) -- only one is offered for M3
// (48000), but this reflects whatever the host's clock SET_CUR actually set,
// which is the honest thing to report rather than the compile-time constant.
uint32_t pl_usb_audio_sample_rate(void);

// Cumulative count of tud_audio_rx_done_pre_read_cb firings -- one per
// received isochronous OUT packet, BEFORE pl_usb_audio_task's drain loop
// reads the bytes out. Bead pico-link-tfj instrumentation: distinguishes
// "packets never arriving at all" from "packets arriving but the software
// FIFO not draining fast enough" (see pl_usb_pump_report).
uint32_t pl_usb_audio_packet_count(void);

// --- Instrumentation (bead pico-link-icb probe 2) ---
// These answer whether SET_INTERFACE (for ANY interface) or any audio
// control-entity request ever reaches this firmware at all, before
// packet_count==0 / streaming==false is trusted as evidence the host
// never selected the streaming alt-setting. All are plain integer
// counters updated from callbacks that already run inside the 0xC0
// worker IRQ (see usb_pump.h) -- no formatting happens in that context;
// pl_usb_pump_report reads and prints them from outside it.

// Cumulative count of tud_audio_set_itf_cb firings, for ANY interface
// number -- not just ITF_NUM_AUDIO_STREAMING.
uint32_t pl_usb_audio_set_itf_calls(void);

// The interface number (wIndex low byte) from the most recent
// SET_INTERFACE the class driver routed to tud_audio_set_itf_cb.
uint8_t pl_usb_audio_last_set_itf(void);

// The alternate setting (wValue low byte) from the most recent
// SET_INTERFACE the class driver routed to tud_audio_set_itf_cb.
uint8_t pl_usb_audio_last_set_alt(void);

// Cumulative count of feature_unit_get_request calls (UAC2_ENTITY_FEATURE_UNIT
// GET, any control selector/request).
uint32_t pl_usb_audio_fu_get_calls(void);

// Cumulative count of feature_unit_set_request calls (UAC2_ENTITY_FEATURE_UNIT
// SET, any control selector/request).
uint32_t pl_usb_audio_fu_set_calls(void);

// --- Instrumentation (bead pico-link-icb probe 3, revision 2 of the fix) ---
// Same rules as above: plain counters, updated from callbacks that already
// run inside the 0xC0 worker IRQ; formatting happens only in
// pl_usb_pump_report, outside that context.

// Cumulative count of tud_audio_set_itf_cb firings specifically for
// ITF_NUM_AUDIO_STREAMING with alt == 1 -- the streaming alt setting being
// selected. This is the primary pass criterion for revision 2 of the fix:
// pl_usb_audio_set_itf_calls() alone cannot distinguish "some SET_INTERFACE
// arrived" from "the streaming alt setting was actually chosen".
uint32_t pl_usb_audio_set_itf_alt1_calls(void);

// Cumulative count of clock_set_request calls (UAC2_ENTITY_CLOCK SET, any
// control selector/request) -- answers whether macOS ever sets the sample
// rate, separate from whether it merely reads it.
uint32_t pl_usb_audio_clock_set_calls(void);

// Cumulative count of clock_get_request calls answering AUDIO_CS_CTRL_SAM_FREQ
// / AUDIO_CS_REQ_CUR specifically (split out of the former combined
// clock_get counter).
uint32_t pl_usb_audio_clk_get_freq_cur(void);

// Cumulative count of clock_get_request calls answering AUDIO_CS_CTRL_SAM_FREQ
// / AUDIO_CS_REQ_RANGE specifically.
uint32_t pl_usb_audio_clk_get_freq_range(void);

// Cumulative count of clock_get_request calls answering AUDIO_CS_CTRL_CLK_VALID
// specifically.
uint32_t pl_usb_audio_clk_get_valid(void);

// Bead pico-link-pbv/pico-link-6vv (C2-8): cumulative count of
// tud_audio_fb_done_cb firings -- one per completed feedback OUT transfer.
// REPLACES pl_usb_audio_fb_sends()/tud_audio_feedback_interval_isr, which
// Ada found structurally dead on this TinyUSB version with
// AUDIO_FEEDBACK_METHOD_DISABLED (see usb_audio.c's doc comment). This is
// the only counter that answers "is the host actually consuming our
// feedback" -- pbv's falsifier F4 (reads 0 while streaming) depends on it.
uint32_t pl_usb_audio_fb_done(void);

// --- Instrumentation (bead pico-link-okx D7/D9) ---
// rx_bytes_total: cumulative n_bytes_received summed over every completed
// ISO-OUT packet (tud_audio_rx_done_pre_read_cb), including short ones.
// rx_short_packets: count of those packets where n_bytes_received != 192
// (one full 1ms 48kHz/16-bit/stereo UAC2 packet) -- a genuinely
// host-reduced send, as opposed to a whole packet going missing.
uint32_t pl_usb_audio_rx_bytes_total(void);
uint32_t pl_usb_audio_rx_short_packets(void);
// Cumulative count of SET_INTERFACE(streaming, alt=1) calls that found
// usbd_edpt_busy(EP1 OUT) already true -- the precondition for the
// EP1-OUT double-arm panic (rp2040_usb.c:108), captured non-fatally at the
// one transition point besides the steady-state worker tick where an arm
// is attempted.
uint32_t pl_usb_audio_ep_out_busy_at_alt1_entry(void);

// --- Instrumentation (bead pico-link-pbv, C6) ---
// Both read the state pl_usb_audio_feedback_task() already maintains at
// its own ~1ms cadence -- the finest-grained sampling of ring fill
// anywhere in this firmware. Safe to call from thread context
// (pl_a2dp_report); plain aligned reads, no locking needed.

// The EMA fill level pl_usb_audio_feedback_task's P controller already
// computes (design sec 2.1) -- the correct value to evaluate the
// closed-loop pass criterion against, not the raw/instantaneous fill.
int32_t pl_usb_audio_fb_fill_ema(void);

// Bead pico-link-pbv round 2 (C2-9): WINDOWED minimum fill level -- reading
// this resets the window, so it answers "what was the true minimum fill
// since the last read" (~1s, pl_a2dp_report's cadence), not "since boot".
// Round 1's lifetime-minimum version was guaranteed to latch at 0 forever
// after the first pl_pcm_reset() and falsified nothing -- see usb_audio.c's
// doc comment.
uint32_t pl_usb_audio_fill_min(void);

// Bead pico-link-pbv round 2 (C2-9): call from a2dp.c's STREAM_STARTED
// handler. Seeds the fill EMA to the current ring fill (so the controller
// starts streaming at its real operating point rather than coasting in
// from priming) and resets the windowed fill_min so a stale reading can't
// be attributed to the stream that's about to start.
void pl_usb_audio_fb_reset(void);

// --- Bead pico-link-4v2.1 (VT1, volume-sync risk gate) ---
// debug_remote.c's "VOL GET"/"VOL WATCH" console commands read these to
// measure what macOS actually sends to the feature unit, per
// .planning/design/2026-09-02-volume-sync.md sec 9 (T1). All are plain
// reads of state already written from the 0xC0 worker IRQ callbacks above
// -- safe to call from thread context, same convention as every other
// accessor in this header.

// Current stored volume (raw UAC2 1/256 dB units, i.e. the range this
// device declares: bMin=-12800, bMax=0, bRes=256) for channel `ch` (0 =
// master, 1..N = per-channel). Returns 0 if `ch` is out of range.
int16_t pl_usb_audio_fu_volume(uint8_t ch);

// Current stored mute flag (host's raw bCur: 0 or 1, per
// audio_control_cur_1_t) for channel `ch`. Returns 0 if `ch` is out of
// range.
int8_t pl_usb_audio_fu_mute(uint8_t ch);

// Highest channel index this device's feature unit accepts (master + N
// audio channels) -- i.e. valid `ch` for the two accessors above is
// 0..pl_usb_audio_fu_channel_count()-1.
uint8_t pl_usb_audio_fu_channel_count(void);

// Enables/disables "VOL WATCH" mode. Does not itself log anything --
// debug_remote.c's poll (thread context, once per superloop iteration)
// checks this flag and, when set, watches pl_usb_audio_fu_set_calls() for
// changes and publishes the current fu_volume[]/fu_mute[] snapshot via
// pl_prio.h's non-starvable slot 4 when it does. See usb_audio.c's doc
// comment on s_watch_enabled for why logging directly from this file's
// 0xC0 IRQ callbacks via pl_log() was tried first and rejected (measured
// unreliable under log-ring congestion).
void pl_usb_audio_set_watch(bool enabled);
bool pl_usb_audio_watch_enabled(void);

// --- Bead pico-link-2ue (VT4a): UAC2 status interrupt endpoint risk gate ---
// debug_remote.c's "VOL INT" console command calls this directly (thread
// context) to send ONE feature-unit-volume-changed status packet on the AC
// interrupt endpoint and observe whether macOS reacts (its own output
// slider moving, and/or a GET_CUR/GET_RANGE follow-up bumping
// pl_usb_audio_fu_get_calls()). Does NOT take pl_usb_lock_try() itself --
// its only caller (pl_debug_remote_poll()) already holds pl_usb_mutex for
// its whole body, and pl_usb_mutex is non-recursive, so a second
// mutex_try_enter() from the same thread context would deadlock-by-false
// every time (measured on this bead's first revision). A future caller
// OUTSIDE that locked region must take the lock itself first. Returns
// false if TinyUSB rejected the send (endpoint not ready / already has a
// transfer pending) -- never blocks.
bool pl_usb_audio_send_fu_status_interrupt(void);

// Cumulative count of pl_usb_audio_send_fu_status_interrupt() calls that
// TinyUSB actually accepted (tud_audio_int_n_write() returned true), for
// "VOL INT" to report back to the console.
uint32_t pl_usb_audio_int_sent(void);

// Cumulative count of tud_audio_int_done_cb firings -- confirms the status
// packet was actually transmitted on the wire, not just accepted into
// TinyUSB's endpoint buffer.
uint32_t pl_usb_audio_int_done(void);

#endif // PICO_LINK_USB_AUDIO_H
