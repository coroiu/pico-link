// Pico Link firmware -- interrupt-driven USB servicing pump. See
// usb_pump.h's module doc for the why; this file is the implementation.
#include "usb_pump.h"

#include <stdarg.h>
#include <stdio.h>

#include "device/usbd_pvt.h" // usbd_edpt_busy -- bead pico-link-okx D8/D9/D13
#include "hardware/irq.h"
#include "pico/mutex.h"
#include "pico/time.h"
#include "tusb.h"

#include "pcm_ring.h"
#include "pl_log_ring.h"
#include "usb_audio.h"
#include "watchdog_sup.h"

// The USB address of the isochronous OUT audio endpoint (EP1 OUT, direction
// bit clear) -- bead pico-link-okx. Matches the endpoint named in the
// crashed board's panic record ("ep 01 was a[lready available]",
// rp2040_usb.c:108) and in usb_descriptors.c's descriptor table.
#define PL_EP_AUDIO_OUT 0x01u

// The re-arm deadline is ~1ms (audiod_xfer_cb only re-arms the ISO OUT
// endpoint from inside tud_task()), not the ~4ms the 784-byte software
// FIFO's capacity alone would suggest -- see usb_pump.h's module doc.
#define PL_USB_PUMP_INTERVAL_US 1000
// Strictly between USBCTRL_IRQ (PICO_DEFAULT_IRQ_PRIORITY, 0x80) and the
// cyw43/BTstack background IRQ (PICO_LOWEST_IRQ_PRIORITY, 0xFF).
#define PL_USB_PUMP_IRQ_PRIORITY 0xC0

// The ONLY console lock in this firmware -- see pl_log's doc comment.
static mutex_t pl_usb_mutex;

static int s_pump_irq_num = -1;
static struct repeating_timer s_pump_timer;

// --- Instrumentation (bead pico-link-tfj: "do not flash without it") ---
static volatile uint16_t s_avail_high_water; // tud_audio_available() high water, WINDOWED -- see D6 below
static volatile uint32_t s_avail_ge_576; // D6 tripwire: ticks this window where avail reached >=576 (734x the 784-byte FIFO)
static volatile uint32_t s_worst_interval_us; // worst gap between worker invocations
static uint64_t s_last_worker_us;

// --- Instrumentation (bead pico-link-okx, D1-D13 -- see this bead's design
// comments, Ada 2026-08-29, for the discriminator these feed). Counters
// only; nothing here formats a string or blocks, so all of it is safe at
// PL_USB_PUMP_IRQ_PRIORITY. ---
static volatile uint32_t s_pump_ticks_run; // D1: mutex_try_enter succeeded, tud_task() ran
static volatile uint32_t s_pump_ticks_skipped; // D2: mutex_try_enter failed, tick skipped entirely
static volatile uint32_t s_ep_out_idle_ticks; // D8: usbd_edpt_busy(EP1 OUT) read false this tick -- endpoint was NOT armed
static volatile uint32_t s_ep_out_state_flipped_in_task; // D13: usbd_edpt_busy(EP1 OUT) differed before vs after tud_task()

// D12 (lead instrument, Ada's design addendum 2026-08-29): SOF-to-worker-
// tick phase histogram. HOOK SUBSTITUTION FROM THE DESIGN AS WRITTEN, see
// this file's tud_sof_cb override below for why: the design named
// tud_audio_feedback_interval_isr (usb_audio.c) as "already ISR context,
// already exists", but that function is UNREACHABLE in this build --
// audiod_sof_isr only calls it for AUDIO_FEEDBACK_METHOD_FREQUENCY_* (see
// pico-sdk 2.1.1 audio_device.c:2016-2025), this build uses
// AUDIO_FEEDBACK_METHOD_DISABLED (usb_audio.c's
// tud_audio_feedback_params_cb), and with no OTHER SOF consumer registered
// either, usbd_sof_enable() ends up calling dcd_sof_enable(rhport, false),
// which clears USB_INTS_DEV_SOF_BITS outright (dcd_rp2040.c:446-459) -- the
// RP2350's SOF hardware interrupt is disabled entirely, so neither
// audiod_sof_isr nor tud_audio_feedback_interval_isr would ever run.
// Verified by reading pico-sdk 2.1.1's usbd.c/audio_device.c/dcd_rp2040.c
// directly, not assumed. tud_sof_cb (also TU_ATTR_WEAK, also "already
// exists", enabled via the public tud_sof_cb_enable(true) in main.c) is the
// nearest equivalent that this build can actually reach: same phase
// question (SOF arrival time vs worker tick time), same "no TinyUSB patch"
// property, at the cost of turning the SOF hardware interrupt ON for the
// duration of this measurement (it is normally off in this build) and one
// extra queued-event hop (DCD_EVENT_SOF -> queue_event -> drained at the
// top of the next tud_task() call, i.e. still inside this same worker,
// typically within one tick).
//
// UNRESOLVED as of this bead's first hardware run (2026-08-29): sof_isr
// read 0 for the entire ~19s capture (16000+ worker ticks, streaming never
// started) despite this hook and tud_sof_cb_enable(true) both being
// present and linked (confirmed via nm -- tud_sof_cb is a strong symbol,
// not the weak default). The dispatch chain was re-verified by reading
// dcd_rp2040.c/usbd.c directly (dcd_rp2040_irq's SOF handling, the raw ISR
// dispatch loop, usbd_task()'s queue drain) and looks structurally sound;
// no root cause identified yet. Do not assume this hook is proven working
// -- check sof_isr on the NEXT hardware run before trusting sof_phase_hist
// at all. A one-off register-level diagnostic (usb_hw->inte/ints/sie_ctrl)
// was tried and pulled back out of this commit (see the bead's comments)
// without a conclusive answer; the board went silent after that flash for
// unrelated-looking reasons before the diagnostic read anything useful.
static volatile uint64_t s_last_sof_us;
static volatile uint32_t s_sof_isr_count; // D4: real SOF ISR count (fb_sends' original, dead-on-this-build intent)
#define PL_SOF_PHASE_BUCKETS 8u
static volatile uint32_t s_sof_phase_hist[PL_SOF_PHASE_BUCKETS];

// TU_ATTR_WEAK override -- fires once per SOF event, dispatched from inside
// tud_task() (usbd.c's event-queue drain), i.e. from within this file's
// worker. See s_last_sof_us's doc comment above for why this hook was
// chosen over the one the design named.
void tud_sof_cb(uint32_t frame_count) {
    (void)frame_count;
    s_last_sof_us = time_us_64();
    s_sof_isr_count++;
}

// TU_ATTR_WEAK override -- fires from inside tud_task() right after
// usbd.c's process_set_config() on a successful SET_CONFIGURATION
// (usbd.c:793-809). Bead pico-link-1av: main.c's original
// tud_sof_cb_enable(true) at boot (before enumeration) does not survive
// enumeration -- usbd.c:793 calls configuration_reset(), which
// tu_varclr()s the whole _usbd_dev struct (usbd.c:552) including the
// sof_consumer bitfield the boot-time call set, and configuration_reset()
// runs on EVERY DCD_EVENT_BUS_RESET (via usbd_reset(), usbd.c:559) and
// EVERY SET_CONFIGURATION (usbd.c:793), i.e. on every real enumeration.
// tud_mount_cb() is called at usbd.c:809, strictly AFTER that reset, so
// re-issuing the enable here is the first point after each (re-)mount
// where it sticks. This is the actual fix for D12's sof_isr reading 0 for
// an entire capture -- the boot-time call in main.c was clobbered before
// the host ever got a chance to see it.
//
// SET_INTERFACE (alt-setting change, e.g. entering/leaving the audio
// streaming alt) does NOT call configuration_reset() -- usbd.c's
// process_set_interface() only resets/opens the driver's own endpoints,
// it never touches _usbd_dev via tu_varclr(). So sof_consumer survives
// alt-set changes and no tud_umount_cb/resume_cb re-arm is needed for
// that case. A real bus reset (physical replug, hub reset, or the host
// deliberately resetting the port) DOES go through usbd_reset() ->
// configuration_reset(), clearing sof_consumer again -- but that always
// culminates in a fresh SET_CONFIGURATION and therefore a fresh
// tud_mount_cb() call, which re-arms it. So mounting alone is sufficient;
// no separate tud_umount_cb/tud_resume_cb override is needed.
void tud_mount_cb(void) {
    tud_sof_cb_enable(true);
}

// Runs at PL_USB_PUMP_IRQ_PRIORITY (0xC0) as a claimed user IRQ, pended by
// the 1ms timer callback below -- never invoked directly from the timer
// itself, see usb_pump.h's module doc for why.
static void pl_usb_pump_worker_irq(void) {
    uint64_t now_us = time_us_64();
    if (s_last_worker_us != 0) {
        uint32_t interval = (uint32_t)(now_us - s_last_worker_us);
        if (interval > s_worst_interval_us) {
            s_worst_interval_us = interval;
        }
    }
    s_last_worker_us = now_us;

    // Bead pico-link-ufh: proves the 1ms timer -> 0xC0 IRQ path itself is
    // still firing, independent of whether tud_task() below is reached --
    // see watchdog_sup.h. Kept ABOVE the mutex_try_enter so a tick that
    // skips tud_task() still proves the IRQ path alive.
    pl_wdt_kick(PL_WDT_USB_TIMER);

    // D12: phase between the most recent SOF timestamp and THIS worker
    // invocation, computed before mutex_try_enter/tud_task() below ("worker
    // top", matching the design's placement) so a skipped tick still gets a
    // sample. s_last_sof_us == 0 means no SOF has ever been observed yet
    // (tud_sof_cb never fired) -- skip rather than bucket a huge bogus
    // phase.
    if (s_last_sof_us != 0) {
        uint32_t ph = (uint32_t)(now_us - s_last_sof_us);
        if (ph < 1000) {
            s_sof_phase_hist[ph >> 7]++;
        }
    }

    // tud_task() is not reentrant -- this guard now exists purely for that
    // property in the abstract (bead pico-link-okx F1 removed pl_log's own
    // contention on pl_usb_mutex, so this is expected to never fail again
    // in practice; D2 below is the counter that proves it).
    if (!mutex_try_enter(&pl_usb_mutex, NULL)) {
        s_pump_ticks_skipped++;
        return;
    }
    s_pump_ticks_run++;

    // D13: sample immediately around tud_task() -- a near-miss (the
    // endpoint's busy state changing) is visible without waiting for the
    // panic that a full double-arm produces.
    bool busy_before = usbd_edpt_busy(0, PL_EP_AUDIO_OUT);
    tud_task();
    bool busy_after = usbd_edpt_busy(0, PL_EP_AUDIO_OUT);
    if (busy_before != busy_after) {
        s_ep_out_state_flipped_in_task++;
    }
    // D8: was the ISO OUT endpoint NOT armed at all when the worker looked?
    // In a healthy stream this should be true almost every tick.
    if (!busy_after) {
        s_ep_out_idle_ticks++;
    }

    // Bead pico-link-ufh: proves tud_task() is actually being reached, not
    // just that the timer/IRQ plumbing is alive.
    pl_wdt_kick(PL_WDT_USB_TASK);

    // Peek (does not consume) before pl_usb_audio_task()'s drain loop, so
    // the high-water mark reflects the fill level tud_task() just left
    // behind, before this tick's own drain reduces it. D6: WINDOWED now --
    // reset every report (pl_usb_pump_report below) -- round 3's
    // lifetime-max version saturated at 784 before streaming even started
    // and was blind for the rest of the run (Ada, design comment).
    uint16_t avail = tud_audio_available();
    if (avail > s_avail_high_water) {
        s_avail_high_water = avail;
    }
    if (avail >= 576) {
        s_avail_ge_576++;
    }
    pl_usb_audio_task();

    // M4 S1 (bead pico-link-cz0.5.2), design sec 2.1: "in the 0xC0 worker
    // after the drain (we are already there; ring fill is two loads and a
    // mask)".
    //
    // Bead pico-link-pbv/pico-link-6vv (C2-7), guarded on
    // pl_usb_audio_streaming(): Ada found that on this TinyUSB (pico-sdk
    // 2.1.1), tud_audio_fb_set() does NOT merely store a value for the SOF
    // ISR (design sec 2.1's original claim was wrong for this version, now
    // corrected there too) -- tud_audio_n_fb_set only guards on p_desc !=
    // NULL (true from SET_CONFIGURATION onward) and then unconditionally
    // calls usbd_edpt_claim()/usbd_edpt_xfer() on audio->ep_fb. That field
    // is 0 while alt 0 is selected (explicitly reset to 0 on alt 0,
    // audio_device.c:1837), and neither usbd_edpt_claim nor usbd_edpt_xfer
    // guards epnum != 0 -- so calling this unconditionally claims EP0-OUT
    // and queues a 3-byte OUT transfer on the CONTROL endpoint about a
    // thousand times a second whenever the streaming alt-setting isn't
    // selected. That is a plausible mechanism for the "EP0 half-open, board
    // goes deaf" wedge this project already spent a session chasing. Only
    // call it once the streaming alt-setting (alt 1) is actually selected
    // and ep_fb is a real endpoint.
    if (pl_usb_audio_streaming()) {
        pl_usb_audio_feedback_task();
    }

    mutex_exit(&pl_usb_mutex);
}

// Fires every 1ms from pico-sdk's default alarm pool
// (PICO_DEFAULT_IRQ_PRIORITY, 0x80 -- the same priority as USBCTRL_IRQ).
// Deliberately does nothing but pend the real worker: calling tud_task()
// directly from here would mean the actual DCD interrupt could never
// preempt it (same priority never preempts same priority on this NVIC).
static bool pl_usb_pump_timer_callback(struct repeating_timer *t) {
    (void)t;
    irq_set_pending((uint)s_pump_irq_num);
    return true; // keep repeating
}

void pl_usb_pump_init(void) {
    mutex_init(&pl_usb_mutex);

    s_pump_irq_num = user_irq_claim_unused(true);
    irq_set_exclusive_handler((uint)s_pump_irq_num, pl_usb_pump_worker_irq);
    irq_set_priority((uint)s_pump_irq_num, PL_USB_PUMP_IRQ_PRIORITY);
    irq_set_enabled((uint)s_pump_irq_num, true);

    // Negative interval: fire every PL_USB_PUMP_INTERVAL_US measured from
    // the previous scheduled time, not from when the callback finished --
    // same convention input.c's sampler uses.
    add_repeating_timer_us(-PL_USB_PUMP_INTERVAL_US, pl_usb_pump_timer_callback, NULL, &s_pump_timer);
}

// Bead pico-link-okx (F1): formats into a stack scratch buffer, then hands
// the bytes to pl_log_ring_push() -- see usb_pump.h's doc comment on
// pl_log for the full rationale. 256 bytes covers every report line in
// this firmware today with headroom (the longest, usb-audio-fix's line, is
// under 160 chars); vsnprintf truncates safely if a future line is longer,
// it never overflows.
void pl_log(const char *fmt, ...) {
    char scratch[256];
    va_list args;
    va_start(args, fmt);
    int n = vsnprintf(scratch, sizeof(scratch), fmt, args);
    va_end(args);
    if (n <= 0) {
        return;
    }
    uint32_t len = (uint32_t)n;
    if (len > sizeof(scratch) - 1) {
        len = sizeof(scratch) - 1; // vsnprintf's return value can exceed what it actually wrote
    }
    pl_log_ring_push(scratch, len);
}

// Bead pico-link-l60 introduced this for callers already holding
// pl_usb_mutex from inside the worker (usb_reset.c). Bead pico-link-okx
// (F1): pl_log() no longer touches pl_usb_mutex at all, so there is no
// hazard left to route around -- this is now a plain alias. See
// usb_pump.h's doc comment on the declaration.
void pl_log_locked(const char *fmt, ...) {
    char scratch[256];
    va_list args;
    va_start(args, fmt);
    int n = vsnprintf(scratch, sizeof(scratch), fmt, args);
    va_end(args);
    if (n <= 0) {
        return;
    }
    uint32_t len = (uint32_t)n;
    if (len > sizeof(scratch) - 1) {
        len = sizeof(scratch) - 1;
    }
    pl_log_ring_push(scratch, len);
}

void pl_usb_pump_report(uint32_t report_dt_us) {
    pl_log(
        "usb-pump: packets=%lu avail_hwm=%u/784 avail_ge_576=%lu worst_interval_us=%lu ring_drops=%lu "
        "log_drops=%lu misaligned=%lu report_dt_us=%lu\r\n",
        (unsigned long)pl_usb_audio_packet_count(),
        (unsigned)s_avail_high_water,
        (unsigned long)s_avail_ge_576,
        (unsigned long)s_worst_interval_us,
        (unsigned long)pl_pcm_overrun_frames(),
        (unsigned long)pl_log_ring_bytes_dropped(),
        (unsigned long)pl_pcm_misaligned(),
        (unsigned long)report_dt_us
    );
    // D6: avail_hwm/avail_ge_576 above are WINDOWED -- reset now that this
    // report has read them, same convention as usb_audio.c's
    // pl_usb_audio_fill_min windowed minimum.
    s_avail_high_water = 0;
    s_avail_ge_576 = 0;
    // Bead pico-link-okx D1/D2/D8/D13: the arm/complete-race discriminator
    // counters. pump_ticks_run + pump_ticks_skipped should sum to
    // ~report_dt_us/1000 (one tick/ms). ep_out_idle_ticks/s is D8, the
    // instrument this bead's design calls out as the lead corroborator for
    // sof_phase_hist below. ep_out_state_flipped_in_task is D13, a
    // near-miss counter that does not require a crash to be informative.
    pl_log(
        "usb-pump-race: pump_ticks_run=%lu pump_ticks_skipped=%lu ep_out_idle_ticks=%lu "
        "ep_out_state_flipped_in_task=%lu sof_isr=%lu\r\n",
        (unsigned long)s_pump_ticks_run,
        (unsigned long)s_pump_ticks_skipped,
        (unsigned long)s_ep_out_idle_ticks,
        (unsigned long)s_ep_out_state_flipped_in_task,
        (unsigned long)s_sof_isr_count
    );
    // D12, the lead instrument (Ada's design addendum 2026-08-29): the SOF-
    // to-worker-tick phase histogram, 8 buckets of 128us each spanning the
    // full ~1ms SOF period. ROTATING bucket occupancy across successive
    // report lines is the free-running-timer-vs-SOF beat this bead's
    // leading hypothesis predicts; a STATIC bucket says the beat is not
    // happening (or F4, not built this round, would already be needed).
    // Cumulative, never reset -- deltas between report lines are what a
    // reader should look at when checking for rotation.
    pl_log(
        "usb-pump-phase: sof_phase_hist=%lu,%lu,%lu,%lu,%lu,%lu,%lu,%lu\r\n",
        (unsigned long)s_sof_phase_hist[0], (unsigned long)s_sof_phase_hist[1], (unsigned long)s_sof_phase_hist[2],
        (unsigned long)s_sof_phase_hist[3], (unsigned long)s_sof_phase_hist[4], (unsigned long)s_sof_phase_hist[5],
        (unsigned long)s_sof_phase_hist[6], (unsigned long)s_sof_phase_hist[7]
    );
    // Bead pico-link-icb probe 2: is SET_INTERFACE or any audio
    // control-entity request arriving at all? Answers the question
    // before trusting packets=0 above as "never streamed".
    pl_log(
        "usb-audio-ctl: set_itf_calls=%lu last_itf=%u last_alt=%u fu_get=%lu fu_set=%lu streaming=%u\r\n",
        (unsigned long)pl_usb_audio_set_itf_calls(),
        (unsigned)pl_usb_audio_last_set_itf(),
        (unsigned)pl_usb_audio_last_set_alt(),
        (unsigned long)pl_usb_audio_fu_get_calls(),
        (unsigned long)pl_usb_audio_fu_set_calls(),
        (unsigned)pl_usb_audio_streaming()
    );
    // Bead pico-link-icb probe 3 (revision 2 of the fix): the primary pass
    // criterion (set_itf_alt1_calls) plus the finer clock breakdown and the
    // feedback-endpoint service count -- proves the fixed feedback endpoint
    // is actually carrying traffic, not just that the alt setting opened.
    // Bead pico-link-pbv/pico-link-6vv (C2-8): fb_done replaces fb_sends --
    // see pl_usb_audio_fb_done's doc comment. Also the pbv acceptance
    // criterion A6 ("feedback alive"): fb_done climbing at ~1000/s while
    // streaming.
    pl_log(
        "usb-audio-fix: set_itf_alt1_calls=%lu clock_set_calls=%lu clk_get_freq_cur=%lu clk_get_freq_range=%lu clk_get_valid=%lu fb_done=%lu\r\n",
        (unsigned long)pl_usb_audio_set_itf_alt1_calls(),
        (unsigned long)pl_usb_audio_clock_set_calls(),
        (unsigned long)pl_usb_audio_clk_get_freq_cur(),
        (unsigned long)pl_usb_audio_clk_get_freq_range(),
        (unsigned long)pl_usb_audio_clk_get_valid(),
        (unsigned long)pl_usb_audio_fb_done()
    );
    // Bead pico-link-okx D7: rx_bytes_total/rx_short_packets, from
    // usb_audio.c's tud_audio_rx_done_pre_read_cb -- rx_short_packets/s > 0
    // with miss/s (sof_isr - packets, computed by the reader from the two
    // lines above) ~ 0 is the decision table's "host genuinely sends less"
    // branch; whole missing packets with rx_short_packets/s ~ 0 point the
    // other way. D9: ep_out_busy_at_alt1_entry -- was the endpoint already
    // marked busy the moment the streaming alt-setting was selected? Cheap
    // corroborator for the alt-1-entry instance of the double-arm panic,
    // never itself fatal.
    pl_log(
        "usb-audio-okx: rx_bytes_total=%lu rx_short_packets=%lu ep_out_busy_at_alt1_entry=%lu\r\n",
        (unsigned long)pl_usb_audio_rx_bytes_total(), (unsigned long)pl_usb_audio_rx_short_packets(),
        (unsigned long)pl_usb_audio_ep_out_busy_at_alt1_entry()
    );
    // D3, repointed (bead pico-link-okx): there is no pl_usb_mutex hold to
    // measure any more (F1 removed it from the logging path entirely) --
    // this is now the hold time of pl_log_ring's own interrupts-disabled
    // push critical section, which replaced it. Expected to be a handful
    // of microseconds always; if this ever climbs, the push itself (not
    // I/O, which is now outside every producer's critical path) has become
    // the hazard.
    pl_log(
        "usb-pump-logring: push_hold_us_total=%lu push_hold_us_max=%lu\r\n",
        (unsigned long)pl_log_ring_push_hold_us_total(), (unsigned long)pl_log_ring_push_hold_us_max()
    );
}
