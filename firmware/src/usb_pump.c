// Pico Link firmware -- interrupt-driven USB servicing pump. See
// usb_pump.h's module doc for the why; this file is the implementation.
#include "usb_pump.h"

#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>

#include "device/usbd_pvt.h" // usbd_edpt_busy -- bead pico-link-okx D9/D13
#include "hardware/irq.h"
#include "hardware/structs/usb.h" // bead pico-link-2ap: raw ISO-OUT buffer-control AVAIL bit at SOF
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
static volatile uint32_t s_ep_out_state_flipped_in_task; // D13: usbd_edpt_busy(EP1 OUT) differed before vs after tud_task()

// Bead pico-link-wbq: D8 (s_ep_out_idle_ticks) and D12's sof_phase_hist/
// hist2 (the SOF-to-worker-tick phase histograms) were DELETED here, not
// merely trimmed from the report -- both were structurally dead
// instruments, not just noisy ones:
//   * D8 (usbd_edpt_busy(EP1 OUT) read false this tick) read 0 at every one
//     of >700,000 SOFs in every state, in both health and collapse, because
//     usbd's busy flag stays true from queue until tud_task() processes the
//     completion -- it can never observe "idle" at the sampling point this
//     worker used.
//   * sof_phase_hist/hist2 measured the gap between s_last_sof_us and the
//     worker's own invocation time, but s_last_sof_us was written from
//     INSIDE this same worker (via tud_sof_cb, dispatched from the tud_task
//     queue drain -- see pl_usb_sof_isr_sample's doc comment below for the
//     dispatch chain this superseded). So the "phase" it measured was the
//     1ms timer's own period, not anything about the USB bus. The 99% in
//     bucket 7 the pre-wbq soak reported was that artifact, not a bus
//     measurement -- do not cite either histogram again.
// See pico-link-2ap's design-of-record comment (Ada, 2026-08-31, sections 4
// and 7-E2) for the full reasoning.
static volatile uint32_t s_sof_isr_count; // D4: real SOF ISR count, now sampled in true ISR context (see below)

// --- Bead pico-link-2ap: THE discriminator between "the host is sending
// fewer ISO-OUT packets" and "we are failing to ingest what it sends".
// Sampled inside pl_usb_sof_isr_sample (see below), called from the
// vendored usbd.c patch in TRUE ISR context -- bead pico-link-wbq moved
// this off the worker-dispatched tud_sof_cb it originally used. Plain
// volatile increments only, ISR-safe.
//
// The instrument is the RAW hardware buffer-control AVAIL bit for EP1 OUT,
// NOT usbd_edpt_busy(): usbd's busy flag stays true from the moment a
// transfer is queued until tud_task() processes the completion event, so it
// reads "armed" during exactly the window where the hardware buffer is
// already full and the controller can NOT accept another packet. AVAIL set
// == the controller will accept this frame's packet; AVAIL clear == this
// frame's packet is dropped by hardware no matter what the host does.
//
// Reading of the counters:
//   sof_miss_avail  -- frame passed with the endpoint READY and no packet
//                      turned up  => the HOST sent nothing.
//   sof_miss_unavail-- frame passed with the endpoint NOT ready => WE lost
//                      it (re-arm too late).
// Whichever of those two tracks (sof_streaming - packets) is the answer.
static volatile uint32_t s_sof_streaming;      // denominator: SOF interrupts while alt 1 is selected
static volatile uint32_t s_sof_hw_unavail;     // AVAIL bit clear at SOF (endpoint could not accept this frame)
static volatile uint32_t s_sof_got;            // packet_count advanced since the previous SOF
static volatile uint32_t s_sof_miss_avail;     // no arrival AND AVAIL set   -> host sent nothing
static volatile uint32_t s_sof_miss_unavail;   // no arrival AND AVAIL clear -> we were not ready
static volatile uint32_t s_sof_last_pkt_count; // packet_count as of the previous SOF

// Bead pico-link-2ap.4: TRUE packet delta accumulator, alongside (not
// replacing) the got/miss_avail/miss_unavail boolean counters above. Those
// counters are a SATURATING BOOLEAN per SOF interval -- two packets
// accounted between consecutive SOF samples count as one "got" and the
// NEIGHBOURING interval reads as a miss, aliasing a healthy 2-then-0
// pattern down to exactly 0.500 and never lower. s_sof_pkts sums the raw
// packet_count delta every interval instead, so sof_pkts/sof_streaming is
// a true ratio uncontaminated by that aliasing. Report BOTH ratios side by
// side -- do not remove the boolean counters, the comparison IS the point.
static volatile uint32_t s_sof_pkts;

// Bead pico-link-06m: incremented from INSIDE the patched pico-sdk TinyUSB
// (rp2040_usb.c, _hw_endpoint_buffer_control_update32) where stock TinyUSB
// instead calls panic("ep %02X was already available") -- an endpoint buffer
// re-armed while USB_BUF_CTRL_AVAIL was still set, i.e. the arm/complete race.
// Stock behaviour is a hard lockup from an IRQ path; counting it instead keeps
// the board alive AND makes the race's rate observable for the first time,
// which is the actual question bead pico-link-okx is asking. Indexed
// [ep_num << 1 | dir], dir 1 = IN. volatile: written from IRQ context.
// tools/apply-sdk-patches.sh installs the SDK side; firmware/CMakeLists.txt
// fails the configure if it is missing.
volatile unsigned int pl_ep_double_arm_count[32];

// Bead pico-link-06m, SECOND patched site: hw_endpoint_xfer_continue() in the
// same SDK file, where stock TinyUSB panics with "Can't continue xfer on
// inactive ep". A buffer-status completion arriving for an endpoint that was
// torn down mid-transfer -- exactly what an abrupt alt0 teardown produces, and
// the likelier door the historical teardown wedges went through, since the
// double-arm site fires at stream START. Deliberately a SEPARATE counter from
// pl_ep_double_arm_count so the two sites are distinguishable in one report.
volatile unsigned int pl_ep_inactive_xfer_count[32];

// Bead pico-link-9ziq (F2): bytes tu_fifo_write_n() (audio_device.c's
// audiod_xfer_isr, sdk-patches/06) failed to write into the ISO-OUT FIFO --
// both the full-FIFO (written==0) case and, critically, the PARTIAL-write
// case the original `if (!tu_fifo_write_n(...))` test alone could not see.
// Written from true USB ISR context by the patched SDK file; read only from
// pl_usb_pump_report below. volatile, no lock needed for a monotonic
// counter read/written by a single producer.
volatile uint32_t pl_usb_fifo_shortfall_bytes;

// Bead pico-link-wbq (E2, fix 1): called from the vendored usbd.c patch
// (firmware/sdk-patches/03-tinyusb-usbd-sof-isr-sample.patch), from INSIDE
// dcd_event_handler's DCD_EVENT_SOF case, in TRUE ISR context, BEFORE that
// function re-queues the event for tud_task() -- see
// firmware/sdk-patches/README.md for exactly why that placement matters.
//
// This replaces the previous approach of sampling from a TU_ATTR_WEAK
// tud_sof_cb() override: that hook is NOT an ISR hook in this build.
// usbd.c's DCD_EVENT_SOF case runs in ISR context but only calls tud_sof_cb
// after RE-QUEUING the event; usbd.c's tud_task() then dispatches it from
// its event-queue drain, which on this firmware runs inside
// pl_usb_pump_worker_irq (this file, below) -- i.e. up to ~1ms after the
// real start of frame, at a phase set by OUR OWN 1ms timer rather than by
// the bus. Reading usb_dpram->ep_buf_ctrl from there measured AVAIL at
// worker time, not AVAIL at SOF -- which is why hw_unavail read ~99.9% of
// samples in health and near-zero in collapse in the pre-wbq soak: the
// sample point moved relative to packet arrival, not the endpoint's actual
// readiness. Sampling directly in dcd_event_handler removes that phase
// error entirely.
//
// See the counter block above for how to read sof_streaming/got/
// miss_avail/miss_unavail/hw_unavail -- unchanged from before this bead;
// only the point in time they are sampled at has moved.
void pl_usb_sof_isr_sample(uint32_t frame_count) {
    (void)frame_count;
    s_sof_isr_count++;

    if (pl_usb_audio_streaming()) {
        s_sof_streaming++;
        bool hw_avail = (usb_dpram->ep_buf_ctrl[PL_EP_AUDIO_OUT & 0x0fu].out & USB_BUF_CTRL_AVAIL) != 0u;
        if (!hw_avail) {
            s_sof_hw_unavail++;
        }
        uint32_t pc = pl_usb_audio_packet_count();
        s_sof_pkts += (pc - s_sof_last_pkt_count);
        if (pc != s_sof_last_pkt_count) {
            s_sof_got++;
        } else if (hw_avail) {
            s_sof_miss_avail++;
        } else {
            s_sof_miss_unavail++;
        }
        s_sof_last_pkt_count = pc;
    }
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
//
// Bead pico-link-wbq: STILL REQUIRED after moving the actual sampling to
// pl_usb_sof_isr_sample (called from the vendored usbd.c patch, not from a
// tud_sof_cb override any more) -- this is what keeps SOF_CONSUMER_USER
// set, which is what keeps the RP2350's raw SOF hardware interrupt enabled
// at all (dcd_sof_enable). Without it, DCD_EVENT_SOF never fires and
// pl_usb_sof_isr_sample never runs, regardless of the usbd.c patch.
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

#if PL_USB_ISO_XFER_ISR
    // Bead pico-link-9ziq (F1): with PL_USB_ISO_XFER_ISR on (the default),
    // audiod_xfer_isr (sdk-patches/04+05) re-arms the ISO-OUT endpoint from
    // TRUE USB ISR context, not from tud_task() -- so the audio data path no
    // longer needs pl_usb_mutex at all. tud_audio_available()/
    // tud_audio_read() are a single-consumer tu_fifo read against that
    // single ISR producer, and pl_usb_audio_feedback_task() is a plain EMA
    // plus tud_audio_fb_set(); neither touches anything tud_task() or the
    // CDC calls mutate. Running this drain BEFORE AND REGARDLESS OF the
    // mutex_try_enter below is the actual fix: previously a thread-mode
    // pl_usb_mutex holder (the log drain, main.c's FU status push,
    // media_keys.c, debug_remote.c) preempted by the 0xFF BTstack/LDAC IRQ
    // made this ENTIRE tick -- audio drain included -- skip via D2 below,
    // and on the OFF build (encoder fill loop on core0, dwell_max ~4.5ms)
    // that starved the 784-byte/4-packet EP-OUT FIFO faster than TinyUSB's
    // re-arm could keep up, and audiod_xfer_isr's tu_fifo_write_n silently
    // truncated the overflow (see pl_usb_fifo_shortfall_bytes above and
    // sdk-patches/06). See
    // .planning/design/2026-09-23-usb-out-fifo-loss-off-build.md for the
    // full conservation-accounting root cause.
    //
    // Ownership invariant this depends on: nothing outside this worker may
    // ever call tud_audio_read() or otherwise touch the ISO-OUT FIFO -- see
    // usb_pump.h's module doc; that invariant is unchanged, only WHERE in
    // this function the drain runs relative to the mutex has moved.
    uint16_t avail = tud_audio_available();
    if (avail > s_avail_high_water) {
        s_avail_high_water = avail;
    }
    if (avail >= 576) {
        s_avail_ge_576++;
    }
    pl_usb_audio_task();
    // See the big comment block below (non-ISO_XFER_ISR branch) for why
    // this is guarded on pl_usb_audio_streaming() -- unchanged reasoning,
    // only the call site moved.
    if (pl_usb_audio_streaming()) {
        pl_usb_audio_feedback_task();
    }
#endif

    // tud_task() is not reentrant -- this guard now exists purely for that
    // property in the abstract (bead pico-link-okx F1 removed pl_log's own
    // contention on pl_usb_mutex, so this is expected to never fail again
    // in practice; D2 below is the counter that proves it). With
    // PL_USB_ISO_XFER_ISR on, a skipped tick here no longer costs audio (the
    // drain above already ran) -- only SET_INTERFACE/control-request
    // handling and other non-ISO tud_task() work waits for the next tick.
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
    // Bead pico-link-wbq: the D8 "was the endpoint idle" check that used to
    // live here (s_ep_out_idle_ticks) was DELETED, not trimmed from the
    // report -- usbd_edpt_busy() stays true from queue until tud_task()
    // processes the completion, so it read 0 at every one of >700,000 SOFs
    // in every state. See the doc comment above pl_usb_sof_isr_sample for
    // the full reasoning (shared with the phase histograms it was deleted
    // alongside).

    // Bead pico-link-ufh: proves tud_task() is actually being reached, not
    // just that the timer/IRQ plumbing is alive.
    pl_wdt_kick(PL_WDT_USB_TASK);

#if !PL_USB_ISO_XFER_ISR
    // Bead pico-link-9ziq: with PL_USB_ISO_XFER_ISR off, ISO-OUT re-arm is
    // still tud_task()'s job (audiod_xfer_cb, task context) -- so the drain
    // below genuinely does need to run after tud_task() and therefore still
    // needs to stay inside the mutex, exactly as before this bead. This is
    // "keep the current gated behaviour" from the design doc: an OFF build
    // (of PL_USB_ISO_XFER_ISR, not to be confused with PL_ENCODER_ON_CORE1)
    // is untouched by F1.
    //
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
#endif

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

// Bead pico-link-okx (F2/F2b): see usb_pump.h's doc comment. Never blocks;
// the 0xC0 worker's own tud_task() call (above) is the only other party
// that ever holds this mutex, and it never waits on the thread side, so
// there is no priority-inversion path here.
bool pl_usb_lock_try(void) {
    return mutex_try_enter(&pl_usb_mutex, NULL);
}

void pl_usb_unlock(void) {
    mutex_exit(&pl_usb_mutex);
}

// Bead pico-link-okx (F1): formats into a stack scratch buffer, then hands
// the bytes to pl_log_ring_push() -- see usb_pump.h's doc comment on
// pl_log for the full rationale. 384 bytes (raised from 256, bead
// pico-link-okx F4, to fit watchdog_sup.c's collapsed 8-entry ring-dump
// lines) covers every report line in this firmware today with headroom;
// vsnprintf truncates safely if a future line is longer, it never
// overflows.
// Bead pico-link-auh: __attribute__((noinline)) makes
// __builtin_return_address(0) below stable -- without it, LTO/inlining at
// call sites could fold this body into the caller and change what "the
// caller's PC" even means. This settles the wbq-vs-Ada producer dispute by
// measurement (pl_log_ring.c's attribution table) with ZERO edits to any
// of the ~144 call sites -- the return address IS the call site.
__attribute__((noinline)) void pl_log(const char *fmt, ...) {
    // Bead pico-link-okx (F4): raised from 256 to 384 to fit the collapsed
    // 8-entry-per-line wdt ring dump (watchdog_sup.c) without truncation.
    char scratch[384];
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
    void *pc = __builtin_return_address(0);
    pl_log_ring_push_attr(scratch, len, (uint32_t)(uintptr_t)pc);
}

// Bead pico-link-l60 introduced this for callers already holding
// pl_usb_mutex from inside the worker (usb_reset.c). Bead pico-link-okx
// (F1): pl_log() no longer touches pl_usb_mutex at all, so there is no
// hazard left to route around -- this is now a plain alias. See
// usb_pump.h's doc comment on the declaration.
// Bead pico-link-auh: see pl_log()'s comment just above on noinline +
// __builtin_return_address -- identical rationale applies here.
__attribute__((noinline)) void pl_log_locked(const char *fmt, ...) {
    // Bead pico-link-okx (F4): raised from 256 to 384 to fit the collapsed
    // 8-entry-per-line wdt ring dump (watchdog_sup.c) without truncation.
    char scratch[384];
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
    void *pc = __builtin_return_address(0);
    pl_log_ring_push_attr(scratch, len, (uint32_t)(uintptr_t)pc);
}

// Bead pico-link-wbq (E2, fix 2): the report set was cut from 11 pl_log()
// lines to 6. The pre-wbq 719s capture showed backlog_hwm=4095 (the ring's
// full 4096-byte capacity) and log_drops=41355 -- the console was
// saturated and competing with the 35ms display frames for the same bus,
// i.e. the instrument was loading the thing it measures. Everything below
// that isn't one of these six lines was DROPPED from the periodic report,
// not deleted outright -- the underlying counters (avail high-water,
// worst_interval_us, the double-arm/inactive-xfer totals, the
// usb-audio-ctl/usb-audio-fix clock diagnostics, the logring push-hold
// stats) still update; they answer questions from other beads (tfj, okx,
// icb, 06m) and can be temporarily re-added to a report line if one of
// those questions comes up again. What is gone for good (not just
// unprinted) is covered above pl_usb_sof_isr_sample and at the
// s_ep_out_state_flipped_in_task block: the two structurally dead
// instruments, D8 and the phase histograms.
//
// See pico-link-2ap's design-of-record comment (Ada, 2026-08-31) sections
// 7-E2 and 9.1 for the rationale.
void pl_usb_pump_report(uint32_t report_dt_us) {
    pl_log(
        "usb-pump: packets=%lu log_drops=%lu report_dt_us=%lu\r\n",
        (unsigned long)pl_usb_audio_packet_count(),
        (unsigned long)pl_log_ring_bytes_dropped(),
        (unsigned long)report_dt_us
    );
    // D6 (avail_hwm/avail_ge_576) was WINDOWED and is no longer printed, but
    // is still reset here so it doesn't silently accumulate stale state if
    // a future report line starts reading it again.
    s_avail_high_water = 0;
    s_avail_ge_576 = 0;

    // pump_ticks_run + pump_ticks_skipped should sum to ~report_dt_us/1000
    // (one tick/ms). sof_isr is now sampled in true ISR context -- see
    // pl_usb_sof_isr_sample's doc comment. Bead pico-link-9ziq (F2):
    // pump_ticks_skipped is now printed -- with PL_USB_ISO_XFER_ISR on, a
    // nonzero value is EXPECTED and harmless (the audio drain above already
    // ran regardless; only tud_task()'s non-audio work waited a tick), so
    // its meaning changed with this bead and it needs to be visible, not
    // just tracked.
    pl_log(
        "usb-pump-race: pump_ticks_run=%lu pump_ticks_skipped=%lu sof_isr=%lu\r\n",
        (unsigned long)s_pump_ticks_run,
        (unsigned long)s_pump_ticks_skipped,
        (unsigned long)s_sof_isr_count
    );

    // Bead pico-link-2ap: THE host-vs-ingest discriminator this bead exists
    // to fix the sampling point of. sof_streaming is the denominator;
    // got + miss_avail + miss_unavail == sof_streaming. hw_unavail is now
    // sampled in true ISR context (fix 1) -- its distribution is expected
    // to differ materially from the pre-wbq worker-context reading.
    // sof_pkts (pico-link-2ap.4): true summed packet-count delta, alongside
    // the boolean got/miss_avail/miss_unavail counters above -- see the
    // s_sof_pkts doc comment. sof_pkts/sof_streaming is the un-aliased
    // ratio; got/sof_streaming is the (possibly aliased) old one. Both are
    // cumulative since boot, same as the other counters on this line.
    pl_log(
        "usb-sof-2ap: sof_streaming=%lu got=%lu miss_avail=%lu miss_unavail=%lu hw_unavail=%lu sof_pkts=%lu\r\n",
        (unsigned long)s_sof_streaming, (unsigned long)s_sof_got,
        (unsigned long)s_sof_miss_avail, (unsigned long)s_sof_miss_unavail,
        (unsigned long)s_sof_hw_unavail, (unsigned long)s_sof_pkts
    );

    // Bead pico-link-pbv/pico-link-6vv (C2-8) acceptance criterion A6
    // ("feedback alive"): fb_done climbing at ~1000/s while streaming.
    pl_log(
        "usb-audio-fix: fb_done=%lu\r\n",
        (unsigned long)pl_usb_audio_fb_done()
    );

    // Bead pico-link-okx D7: rx_bytes_total/rx_short_packets -- distinguish
    // "the host genuinely sends less" (rx_short_packets/s ~ 0, whole
    // packets missing) from a partial-packet ingestion problem
    // (rx_short_packets/s > 0).
    pl_log(
        "usb-audio-okx: rx_bytes_total=%lu rx_short_packets=%lu\r\n",
        (unsigned long)pl_usb_audio_rx_bytes_total(), (unsigned long)pl_usb_audio_rx_short_packets()
    );

    // Bead pico-link-9ziq (F2): THE direct conservation check for this
    // bead's root cause -- no more inferring loss from three separate lines
    // by hand. fifo_shortfall_bytes is the SDK-side counter (patch 06,
    // audiod_xfer_isr's tu_fifo_write_n shortfall, both partial and
    // full-FIFO); usb_lost_bytes is rx_bytes_total (counted on arrival,
    // usb_audio.c:333) minus pcm_bytes_total (counted after
    // tud_audio_read(), usb_audio.c:366) -- the two should track each other
    // 1:1 once F1 lands, and both delta=0 over a soak is this bead's
    // success criterion.
    pl_log(
        "usb-pump-loss: fifo_shortfall_bytes=%lu usb_lost_bytes=%lu\r\n",
        (unsigned long)pl_usb_fifo_shortfall_bytes,
        (unsigned long)(pl_usb_audio_rx_bytes_total() - pl_usb_audio_pcm_bytes_total())
    );

    // Bead pico-link-okx (F2): backlog_hwm approaching PL_LOG_RING_SIZE
    // (4096, pl_log_ring.c) means the drain cannot keep up with the push
    // rate -- this is the OTHER half of fix 2's verification, alongside
    // log_drops above.
    pl_log(
        "usb-pump-logring: backlog_hwm=%lu\r\n",
        (unsigned long)pl_log_ring_backlog_hwm()
    );
}
