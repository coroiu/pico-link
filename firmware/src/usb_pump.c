// Pico Link firmware -- interrupt-driven USB servicing pump. See
// usb_pump.h's module doc for the why; this file is the implementation.
#include "usb_pump.h"

#include <stdarg.h>
#include <stdio.h>

#include "hardware/irq.h"
#include "pico/mutex.h"
#include "pico/time.h"
#include "tusb.h"

#include "pcm_ring.h"
#include "usb_audio.h"

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
static volatile uint16_t s_avail_high_water; // tud_audio_available() high water, vs the 784-byte FIFO
static volatile uint32_t s_worst_interval_us; // worst gap between worker invocations
static volatile uint32_t s_log_drop_count; // pl_log calls skipped, mutex contended
static uint64_t s_last_worker_us;
static uint64_t s_last_report_us;

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

    // tud_task() is not reentrant. If pl_log() is mid-vprintf (holding
    // pl_usb_mutex) on the other side of this same mutex, skip this tick
    // entirely rather than block -- it runs again in ~1ms regardless.
    if (!mutex_try_enter(&pl_usb_mutex, NULL)) {
        return;
    }

    tud_task();

    // Peek (does not consume) before pl_usb_audio_task()'s drain loop, so
    // the high-water mark reflects the fill level tud_task() just left
    // behind, before this tick's own drain reduces it.
    uint16_t avail = tud_audio_available();
    if (avail > s_avail_high_water) {
        s_avail_high_water = avail;
    }
    pl_usb_audio_task();

    // M4 S1 (bead pico-link-cz0.5.2), design sec 2.1: "in the 0xC0 worker
    // after the drain (we are already there; ring fill is two loads and a
    // mask)". Must run every tick this worker runs, not just while a
    // consumer is attached -- the feedback loop has to already be correct
    // before a2dp.c's stream exists, or the ring overruns the instant one
    // does (see usb_audio.c's doc comment on tud_audio_feedback_params_cb).
    pl_usb_audio_feedback_task();

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

void pl_log(const char *fmt, ...) {
    if (!mutex_try_enter(&pl_usb_mutex, NULL)) {
        s_log_drop_count++;
        return;
    }
    va_list args;
    va_start(args, fmt);
    vprintf(fmt, args);
    va_end(args);
    mutex_exit(&pl_usb_mutex);
}

// Bead pico-link-l60: see usb_pump.h's doc comment on the declaration --
// this is for callers that already hold pl_usb_mutex (currently just
// usb_reset.c, called from inside tud_task() inside the worker IRQ). No
// mutex_try_enter/exit, no drop counting: the caller has already paid for
// exclusive access.
void pl_log_locked(const char *fmt, ...) {
    va_list args;
    va_start(args, fmt);
    vprintf(fmt, args);
    va_end(args);
}

void pl_usb_pump_report(void) {
    uint64_t now_us = time_us_64();
    if (s_last_report_us != 0 && now_us - s_last_report_us < 1000000) {
        return;
    }
    s_last_report_us = now_us;
    pl_log(
        "usb-pump: packets=%lu avail_hwm=%u/784 worst_interval_us=%lu ring_drops=%lu log_drops=%lu misaligned=%lu\r\n",
        (unsigned long)pl_usb_audio_packet_count(),
        (unsigned)s_avail_high_water,
        (unsigned long)s_worst_interval_us,
        (unsigned long)pl_pcm_overrun_frames(),
        (unsigned long)s_log_drop_count,
        (unsigned long)pl_pcm_misaligned()
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
    pl_log(
        "usb-audio-fix: set_itf_alt1_calls=%lu clock_set_calls=%lu clk_get_freq_cur=%lu clk_get_freq_range=%lu clk_get_valid=%lu fb_sends=%lu\r\n",
        (unsigned long)pl_usb_audio_set_itf_alt1_calls(),
        (unsigned long)pl_usb_audio_clock_set_calls(),
        (unsigned long)pl_usb_audio_clk_get_freq_cur(),
        (unsigned long)pl_usb_audio_clk_get_freq_range(),
        (unsigned long)pl_usb_audio_clk_get_valid(),
        (unsigned long)pl_usb_audio_fb_sends()
    );
}
