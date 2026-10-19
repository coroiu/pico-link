# Hardware acceptance capture: pico-link-ycm2.7.1 (drain-servo, step 1)

Branch `bd-pico-link-ycm2.7.1` @ `0ca5f0c`, flashed via the press-free
BOOTSEL iface-4 control transfer. Board: Pico Link Audio Dongle (2e8a:000c),
LDAC to test headphones (94:DB:56:54:7C:F2). Andreas played music through
"Pico Link Speaker" on macOS for the duration and listened.

## Files

- `summary.tsv` — one row per ~8s sample (`cdc_reader.py --duration 3` + 5s
  sleep), columns: `ts enc_frames_total alt fill_ema servo_ppm
  servo_learned_ppm stop_ring_empty resync_events ovr_frames
  credit_clamp_events fb_rail_ticks und`. Blank rows are an artifact of the
  console's non-line-atomic framing (a sample landed mid-block); ignore them.
- `rx_bytes.tsv` — `ts usb-audio-okx:rx_bytes_total`, used to attempt an
  independent host-rate cross-check (see caveat below).
- `raw.log.gz` — full console text for every sample.

## Timeline

- Flash + BT reconnect, USB alt 0 (idle): ts 1792408123-1792408396. `ovr_frames`
  already at 130097 here — accumulated during the idle gap before BT
  reconnected and before this test began (audio arriving over USB with
  nothing draining the ring). Flat for the rest of the capture: zero *new*
  overflows.
- **Streaming segment 1**: ts 1792408409 -> 1792408921 (512 s), `enc_frames_total`
  4387 -> 196277, confirming continuous LDAC encoding.
- **Stop/restart**: ts 1792408929-1792408954 (~25 s), alt drops to idle,
  `enc_frames_total` frozen at 197283 — Andreas paused/stopped playback near
  the end as requested.
- **Streaming segment 2**: ts 1792408962 -> 1792409070 (108 s), `enc_frames_total`
  198889 -> 239172.
- Total streaming time while `enc_frames_total` was advancing: 512+108 = 620 s
  (>= the 10-minute requirement).

## Acceptance criteria

1. **servo_ppm settles within +/-20ppm of the host offset (from usb-audio byte
   rate) over >=60s.** PARTIAL PASS, with a caveat:
   - The servo itself converges and *stays* within -2..+8 ppm for the last
     ~141s of segment 1 (ts 1792408780-1792408921), well past the 60s bar, and
     `servo_ppm` tracks `servo_learned_ppm` tightly throughout (self-consistent
     PI convergence, not oscillating).
   - I attempted an **independent** ppm estimate from `rx_bytes.tsv`
     (`rx_bytes_total` delta / elapsed vs the nominal 192000 B/s for 48kHz
     stereo16). Over a clean window (ts 423-648) this gives ~-9250ppm, two
     orders of magnitude larger than any real crystal offset. `usb-pump:
     log_drops` was climbing throughout (23583->34248) in the same window,
     so `rx_bytes_total` undercounts true host throughput by dropped-packet
     accounting, not clock drift — this counter is not a valid independent
     ppm reference in this build. I could not cross-validate the +/-20ppm
     claim against a source independent of the servo's own state. Flagging
     this rather than asserting a false independent pass.

2. **fill_ema within 3ms of target.** PASS. Target fill is 5760 bytes (30.0ms
   at 192 B/ms). Converged range in the back half of segment 1: 5631-5825
   bytes, i.e. -0.67ms to +0.34ms off target. Comfortably under 3ms for the
   whole capture.

3. **stop_ring_empty flat while streaming.** PASS. Flat at 8337 for all of
   segment 1 (512s) and flat at 11296 for all of segment 2 (108s). It only
   incremented once, exactly at the stop/restart transition (ts 1792408929),
   when `alt` was idle and `enc_frames_total` was NOT advancing — consistent
   with the design (ring drains on stop, not a bug).

4. **resync_events flat.** PASS. 0 before streaming; became 1 at the moment
   streaming started (ts 1792408409) and stayed at 1 for the remaining 620s,
   including across the stop/restart. Zero resyncs during steady playback.

5. **ovr_frames flat.** PASS. Constant 130097 across the entire capture
   (idle and both streaming segments) — zero new overflow frames once the
   test began.

6. **credit_clamp_events flat while streaming.** PASS. Flat at 2028 for all of
   segment 1, jumped once to 2746 at the same stop/restart transition as (3),
   flat at 2746 for all of segment 2.

7. **fb_rail_ticks flat.** PASS. Constant 2816 for the entire capture, no
   change at any point (feedback EP pinned at 48.0 as designed, never rails).

## servo_learned_ppm / carry-across-restart

`servo_learned_ppm` at the end of segment 1 (last sample before the stop,
ts 1792408921): **-2 ppm**. First sample of segment 2 after restart
(ts 1792408962): servo_ppm/learned both restart from that same -2 baseline
(not reset to 0), then transiently swing (-23 -> 56 -> -41) while the PI
re-settles against the freshly reconnected A2DP session — this is the
controller reacting to the restart transient, not a reset. `fill_ema` at the
first post-restart sample is 5951 bytes (**+1.0ms** off target) — no ~11s
low-fill dip of the kind the pre-servo design had. This confirms the learned
offset is carried into the next stream, per the design intent.

## Verdict

All seven flat/stable counters pass cleanly. The ppm-settling criterion is
self-consistently satisfied (servo stays in a tight +/-20ppm band for >60s)
but I could not independently corroborate it against a host-clock-derived ppm
from this capture — `rx_bytes_total` is confounded by packet loss, not usable
as the independent reference the criterion asks for. Recommend either
instrumenting a genuine independent offset counter (e.g. SOF-interval-based
host rate, not byte-count) for the next hardware round, or accepting
self-consistent PI convergence as sufficient evidence for step 1.
