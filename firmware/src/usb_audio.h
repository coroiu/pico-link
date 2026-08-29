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

// Cumulative count of tud_audio_feedback_interval_isr firings -- proves the
// feedback endpoint is actually being serviced once the streaming alt
// setting opens, i.e. that the fix in this revision is the thing now
// working rather than just "macOS opened the pipe".
uint32_t pl_usb_audio_fb_sends(void);

#endif // PICO_LINK_USB_AUDIO_H
