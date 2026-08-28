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

// Cumulative count of clock_get_request calls (UAC2_ENTITY_CLOCK GET,
// any control selector/request).
uint32_t pl_usb_audio_clock_get_calls(void);

// Cumulative count of feature_unit_get_request calls (UAC2_ENTITY_FEATURE_UNIT
// GET, any control selector/request).
uint32_t pl_usb_audio_fu_get_calls(void);

// Cumulative count of feature_unit_set_request calls (UAC2_ENTITY_FEATURE_UNIT
// SET, any control selector/request).
uint32_t pl_usb_audio_fu_set_calls(void);

#endif // PICO_LINK_USB_AUDIO_H
