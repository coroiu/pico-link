# OFF-build ring starvation is USB OUT packet loss, not a clock problem (pico-link-9ziq, = pico-link-0gtk)

Ada, 2026-09-23. Status: design for Ruby.

## Finding

On the OFF build (PL_ENCODER_ON_CORE1 undefined) about **1.5 ISO OUT packets per
second are lost inside TinyUSB**: they are counted as received, then never reach
`tud_audio_read`. The ring is short by about 1500 ppm. The feedback loop can only
add 500 ppm, so it sits railed while the ring stays on the floor.

Conservation, measured on the board (eef24c8 OFF, 990 kbps, passive 150 s capture,
per report block, RP clock):

| quantity | value | source |
|---|---|---|
| SOF rate | 1000.005 /s | sof_streaming delta / a2dp report_dt_us, 150 block pairs |
| host sent | 192 096 B/s = 192.095 B/SOF (+494 ppm) | rx_bytes_total (tud_audio_rx_done_pre_read_cb, usb_audio.c:333) |
| into our code | 191 807 B/s | `measured=` (pcm_bytes_total after tud_audio_read, usb_audio.c:366; exact-us windows, main.c:811-822) |
| consumed | 191 804 B/s | ctr line e= (enc_frames_total) x 512 / u= (one atomic line, a2dp.c:4126) |
| resync trim, flush, ring overrun, misaligned | ~0 | counters flat |
| **lost between the rx callback and tud_audio_read** | **~289 B/s (1500 ppm, 1.5 packets/s)** | the difference |
| fb_rail_ticks | 0.976 per SOF (railed almost every tick) | usb_audio.c:497 |

The same arithmetic on fe8ce44 OFF at 660 kbps (/tmp/nli7_logs/armOFF) gives rx 192.09-192.10 B/SOF,
railed about 1.0 per SOF, consumption ~191 935 B/s: **the bug predates the SRAM change and quzf**,
but is roughly half the size (~700 ppm). ON (on_active.log): rx 192.006 B/SOF, rail 0.35 per SOF,
measured 191 978 B/s, loss within noise.

## Rulings on the hypotheses

- **(a) The consumer runs ahead of real time: refuted.** Credit accrues exactly
  `elapsed_us * 48000` (a2dp.c:1854-1862) and is only ever clamped down (a2dp.c:1902-1910).
  CATCHUP_K limits dwell, not credit (a2dp.c:1525-1543). Consumed plus clamped is
  191 804 + ~20 B/s, i.e. nominal. What drains the ring is the input, not the output.
- **(b) The two clocks disagree: refuted.** SOF is 1000.005/s on the RP clock (5 ppm).
  Underfill already has a correction: the nxf PI loop (usb_audio.c:456-501, ±500 ppm).
  It works, and the host honours it (+494 ppm observed). It is just out of range.
- **(c) OFF-specific: yes, but not in pl_a2dp_fill.** It is a priority inversion on
  `pl_usb_mutex`:
  1. The 0xC0 worker drains the TinyUSB OUT FIFO only when it wins
     `mutex_try_enter(&pl_usb_mutex)`. Otherwise it skips the whole tick (usb_pump.c:242-245).
  2. Thread-mode code takes the same mutex: the log drain (pl_log_ring.c:294, which runs
     all the time because the backlog sits at 8192), the FU status push (main.c:605),
     media_keys.c:127 and debug_remote.c:258.
  3. On OFF, the 0xFF BTstack IRQ runs the whole LDAC fill loop, with dwell_max_us of
     4.5 ms now and 8.3 ms before the SRAM change. If that IRQ preempts a thread that
     holds the mutex, the worker loses every tick for that long.
  4. The EP-OUT software FIFO holds only 4 packets, 784 B, about 4 ms
     (tusb_config.h:157). `audiod_xfer_isr` writes it with `tu_fifo_write_n`
     (lib/tinyusb audio_device.c:819). That call is non-overwritable, so on a full FIFO
     the packet tail is truncated silently. Nothing counts it.
  5. On ON, core0's 0xFF bursts are short, so the same inversion costs single ticks and
     loses nothing. That is why only OFF starves.
  Supporting evidence: the gap between sof_isr and pump_ticks_run is 4.75/s on OFF,
  about what ~1 collision/s of 4-5 skipped ticks would give. This is consistent, not
  proven, because pump_ticks_skipped is not printed.

The "host throughput is healthy, host ruled out" conclusion in pico-link-0gtk was wrong
for the same reason. `measured=` is taken **after** the loss, and 0.15% is invisible at
a glance.

## Fix design

**F1 (the fix): take the audio data path out of `pl_usb_mutex`.** With
PL_USB_ISO_XFER_ISR on (the default), `tud_task()` no longer handles ISO OUT: the USB
ISR writes the FIFO and re-arms. The mutex exists because `tud_task()` and the CDC
calls are not reentrant. `tud_audio_available`/`tud_audio_read` are a single-consumer
`tu_fifo` read against a single ISR producer. `pl_usb_audio_feedback_task` is a plain
EMA plus `tud_audio_fb_set`.
- In `pl_usb_pump_worker_irq`, run `pl_usb_audio_task()` and
  `pl_usb_audio_feedback_task()` **before and regardless of** the try_enter.
  `tud_task()` stays gated as it is today.
- The one hazard to close: `tud_task()` handles SET_INTERFACE, which resets or clears
  the FIFO, and today that runs in the same worker. Keep it that way. Nothing outside
  the worker may call `tud_audio_read` or clear the FIFO. Add an assert or a comment
  that makes this ownership explicit, and put the audio drain in the worker **after**
  `tud_task()` when the lock was won, or alone when it was not.
- The data path is unchanged in OFF-irrelevant builds (PL_USB_ISO_XFER_ISR=OFF must
  keep today's gated behaviour: there, `tud_task` is what fills the FIFO).

**F2 (instrument, same change):**
- Count the bytes `audiod_xfer_isr` fails to write: `n_bytes_received - written`, where
  written is `tu_fifo_write_n`'s return value, and also count the full-FIFO false return.
  This is a small edit to the already-patched SDK file, so also record it in
  firmware/sdk-patches.
- Print that count and `pump_ticks_skipped` in the `usb-pump-race` line.
- Print `rx_bytes_total - pcm_bytes_total` as `usb_lost_bytes`. That is the direct
  conservation check, so no more inferring loss from three separate lines.

**Not recommended as the fix:** raising EP_OUT_SW_BUF_SZ from 4 to 16 packets. It
hides this symptom for about 2.3 KB of SRAM, but the inversion stays: CDC and
`tud_task` are still blocked for the whole encode stretch. It is fine as a follow-up
safety margin, not as the fix.

**Also not:** an underfill "resync" on the a2dp side. The ring is not drifting. Input
is being dropped, and inserting silence would only move the crackle somewhere else.

## Success numbers (OFF, 990 kbps, 15 min, music playing, after the ~60 s settle)

1. `usb_lost_bytes` delta = 0, and the new FIFO-shortfall counter delta = 0.
2. fill_ema within 5760 ± 1500 for minutes 1-15. Least-squares slope under 2 B/s in magnitude.
3. fb_rail_ticks per SOF under 0.05, where it is 0.976 today. The loop should settle
   near the true clock offset (a few tens of ppm), and rx/SOF should read 192.00 ± 0.02.
4. stop_ring_empty delta ≤ 2 over the 15 min, where it is about 17 per minute today.
   credit_clamped_samples delta under 1% of today's.
5. No regression on ON (same 15-min soak: loss 0, fill in band). No ISO-OUT death, no
   new panics, pump_ticks_skipped reported (a nonzero value is fine now: skips no longer
   cost audio).
6. Andreas: no crackle over 15 min at 990 kbps.

## Does this block flipping PL_ENCODER_ON_CORE1 to default ON?

**No.** ON does not show the loss: rx 192.006 B/SOF, measured 191 978 B/s, rail 0.35.
The quzf B-ON slope of -0.56 B/s checks out from its 15 logged samples; minutes 2-15
alone give +0.14 B/s. But the inversion is **latent** on ON: any long core0 0xFF
stretch, such as a flash write or a BTstack burst, would expose it. So F1 should land
regardless of the flip.

Separate anomaly, unverified, not this bead: on_active.log (ON) shows ctr-derived
consumption of 191 652 B/s (-0.18%) against 191 978 B/s into the ring, with no trims
and no overflow. That does not conserve. Either ON's enc_frames_total under-counts or
the log is mixed. Check it in the ON soak before the flip.
