// Pico Link firmware -- the audio fault evaluator (bead pico-link-9eq2.3.2,
// design .planning/design/2026-09-07-audio-fault-model.md secs 5-7). Owns
// the fault-key enum, per-key edge/clear/refresh state, and
// pl_fault_evaluate() -- the ONLY thing in this firmware that reads
// usb_audio.c's destructive-on-read pl_usb_audio_fill_min() (design
// §5.6.5), turning a2dp.c/usb_audio.c/pcm_ring.c's since-boot cumulative
// counters into the six-key strip event ui-ffi's PlEventTag::AudioFault
// (wire tag 16) carries.
//
// Module ownership (design §7.2): this file may include a2dp.h,
// usb_audio.h, pcm_ring.h and pico_link_ui.h -- it consumes all four.
// NOTHING includes fault.h except main.c. In particular pcm_ring.c must
// NOT: it is owned by neither side of the seam this file crosses (the
// USB-audio domain and the A2DP/Bluetooth domain) and must stay that way,
// same reasoning as pcm_ring.h's own module doc on why it has no opinion
// about either side. Keeping the include graph one-directional (main.c ->
// fault.c -> {a2dp,usb_audio,pcm_ring}) means adding a fault, or retuning
// one, is never a change to any of those three modules' own public
// surface beyond the plain getters design §7.2 already lists.
//
// Context (design §7.1, the ruling): pl_fault_evaluate() runs from the
// main.c superloop in THREAD CONTEXT, at the same ~1Hz cadence
// pl_a2dp_report/pl_usb_pump_report already share -- so it may call
// pl_ui_push_event() directly. No IRQ-context work is added anywhere by
// this module; the counters it reads are the same `volatile uint32_t`
// single-aligned-word fields pl_a2dp_report has always read from thread
// context (a read concurrent with an IRQ-context producer's increment is
// benign -- old or new value, never a tear -- the identical convention
// that function has used since pico-link-pbv).
#ifndef PL_FAULT_H
#define PL_FAULT_H

#include <stdint.h>

#include "pico_link_ui.h"

// Runs the derived-edge evaluation for all six catalogue keys (design
// §3.1) and pushes Event::AudioFault (tag 16) through `ui` for any key
// that raises, refreshes, or (implicitly, by staying silent) clears this
// window. Call ONCE PER SECOND from main.c's existing shared-report block
// (design §5.1: "the same place and cadence pl_a2dp_report() and
// pl_a2dp_publish_counters() already run"), BEFORE pl_a2dp_report() -- this
// function is usb_audio.c's pl_usb_audio_fill_min()'s SOLE caller (design
// §5.6.5) and caches the result for pl_fault_last_fill_min() below, which
// pl_a2dp_report() needs for ITS OWN report line.
//
// `now_us` is the caller's time_us_64() reading, shared rather than
// re-queried, same discipline as report_dt_us already uses at this call
// site.
void pl_fault_evaluate(struct PlUi *ui, uint64_t now_us);

// The value pl_fault_evaluate()'s call (this window) to usb_audio.c's
// DESTRUCTIVE-ON-READ pl_usb_audio_fill_min() returned, cached for
// pl_a2dp_report to print without itself becoming a second reader of that
// accessor (design §5.6.5 -- two readers would steal each other's windows
// and both produce garbage). Returns 0 before the first evaluation.
uint32_t pl_fault_last_fill_min(void);

#endif // PL_FAULT_H
