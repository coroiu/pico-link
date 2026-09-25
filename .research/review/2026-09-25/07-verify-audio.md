# 07 — Adversarial verification of 01-audio-pipeline.md

Verifier pass, 2026-09-25, tree `1104a58` (review commits only on top of `2e37164`; no source change).
Method: every claim re-derived from source. The SDK is not on this machine, so I fetched
**upstream TinyUSB 0.18.0** (`hathach/tinyusb@0.18.0`: `audio_device.c`, `usbd.c`, `usbd_pvt.h`,
`audio_device.h`, `dcd_rp2040.c`, `rp2040_usb.c`, `tusb_fifo.[ch]`) and **BTstack v1.6.2**
`avdtp_source.c` (identical to v1.6.1) into the scratchpad. I then ran `tools/apply-sdk-patches.sh` against
a fake SDK tree built from those files: **all nine patches (01, 02, 03, 04a-d, 05, 06) applied
cleanly and a second run reported all nine "already applied"**. Every STOCK string matched, and the
design doc's `audio_device.c:819` citation lands on the same line in my patched copy. So this *is*
the vendored file, byte for byte in the patched regions. The reviewer did not have it. Scratch artefacts
are in `/tmp/claude-0/.../scratchpad/{fakesdk,stock018,bts,fbsim.py}`.

| Finding | Verdict | Severity (reviewer → mine) | Tier |
|---|---|---|---|
| F-audio-01 | **REFUTED** as stated. Residual ZLP path → F-audio-V01 | P1 → (withdrawn) | — |
| F-audio-02 | **PARTIALLY CONFIRMED**: gap real, numbers and one fix idea wrong | P1 → P2 | Sonnet |
| F-audio-03 | **CONFIRMED** (numbers reproduced); fix option (a) is wrong | P2 → P2 | Sonnet |
| F-audio-04 (spot-check) | **CONFIRMED** | P2 → P2 | Haiku |
| F-audio-06 (spot-check) | **CONFIRMED** (mechanism); one sub-claim wrong, trigger easier than stated | P2 → P2 | Sonnet |
| F-audio-08 (follows from 01) | **REFUTED**: no double count. Real instrument defect is F-audio-V02 | P2 → (withdrawn) | — |
| "SPSC + core1 handshake correct" | Rings: **hold**. Handshake: **one real hole** → F-audio-V03 | — | — |

---

## F-audio-01: ISR ISO-OUT patch defers a full-FIFO packet into the stock path, which double-counts it
**Verdict: REFUTED.** The premise that a full FIFO makes `tu_fifo_write_n` return 0 is false for this SDK.

Evidence. The post-patch control flow, all line numbers from my patched 0.18.0 copy:
```
USBCTRL_IRQ -> hw_handle_buff_status (dcd_rp2040.c, patch 05: reset_transfer, THEN notify)
  dcd_event_xfer_complete -> dcd_event_handler case XFER_COMPLETE (usbd.c, patch 04b)
    busy=claimed=0; send = !audiod_xfer_isr(...); if send {busy=claimed=1}; if send -> queue for tud_task
audiod_xfer_isr (audio_device.c:796):
  if !AS_interface_index         -> return false        # alt teardown; stock path fails identically
  pre_read_cb  [packet_count++, rx_bytes += n]          # usb_audio.c:333-347, always returns true
  written = tu_fifo_write_n(ep_out_ff, lin_buf_out, n)  # :824
  if written < n -> shortfall += n-written
  if written == 0 -> return false                       # <-- the reviewer's deferral
  if !usbd_edpt_xfer(re-arm) -> return false            # unreachable: busy was just cleared, rp2040 dcd_edpt_xfer returns true
  post_read_cb (weak default, true); return true
```
But `ep_out_ff` is configured **overwritable**: `tu_fifo_config(&audio->ep_out_ff, ..., 1, true)`
(stock `audio_device.c:1324`, patched `:1402`). In `_tu_fifo_write_n` (`tusb_fifo.c:469-545`) the
`!f->overwritable` clamp is skipped. When `n < depth` (192 < 784), it writes all `n` bytes over the oldest
data and returns `n`. So for any packet with `n > 0`, `written == n`: the shortfall counter never moves,
the ISR never returns false, and it always re-arms. The full-FIFO → deferral → double count → dead
endpoint chain **cannot happen**. `written == 0` happens only when `n == 0`, a zero-length packet. That real
residual is filed as F-audio-V01. The reviewer's hardware verification step (b), which expects
"`fifo_shortfall_bytes` rises by 4-5 packets", would also fail, because that counter cannot rise.

Severity: none as filed. Tier: n/a. **Do not apply the fix sketch as written**: its stated premise is false,
and the one real case is handled in V01.

## F-audio-02: LDAC's fixed MTU is never checked against the sink's media payload
**Verdict: PARTIALLY CONFIRMED.** Confirmed: nothing checks it. Wrong: the byte counts, "corrupted frames", and the "re-init with a larger MTU" option.

Evidence:
- `a2dp.c:3429,3446`: `mtu = a2dp_max_media_payload_size(...)`, then `max_media_payload_size = min(mtu, 1029)`.
  In the self-packetising branch (`:3485-3486`), `frames_per_packet` comes from the row hint. There is **no
  comparison** against the LDAC packet size. The only capacity check in `pl_a2dp_fill` is the
  `frame_bytes > 0` seal (`:1706`), which is SBC-only. grep for `max_media_payload|679|PL_LDAC_INIT_MTU|mtu`
  in `firmware/src` finds no other check.
- Actual LDAC packet size: `ldacBT_api.c:166-170` sets `tx_size = LDACBT_MTU_REQUIRED(679) - LDACBT_TX_HEADER_SIZE(18) = 661`.
  It caps at `mtu-18` only when mtu is smaller, so **a larger MTU never grows the packet**. The AVDTP media
  payload is at most 1 + 661 = **662 B**, not the reviewer's 680.
- BTstack v1.6.2 `avdtp_source.c:205-207`: `if (12 + payload_size > remote_mtu) return ERROR_CODE_MEMORY_CAPACITY_EXCEEDED;`.
  The packet is **rejected whole**, never truncated. `a2dp.c:1925-1937` then counts `pkt_fail++` and frees the slot.
- The failure condition is therefore a sink L2CAP media MTU below 674. The 672-byte L2CAP default fails, by
  1-2 bytes, only on the rungs that fill the packet (HQ: 2×330 B frames). Result: sustained or intermittent
  **silence**, with `pkt_fail` climbing. It is not corrupt audio.

Severity: **P2** (reviewer P1). No LDAC sink is known to advertise < 674. libldac itself refuses
mtu < 679 (`ldacBT_internal.c:189-194`), so LDAC's transport assumes 2-DH5. The failure is loud (`pkt_fail`).
It is still worth closing before the next headphones.

Corrections to the fix sketch:
- (1) Compare `usable_payload` against **661** (`LDACBT_MTU_REQUIRED - 18`), not 679+1.
- (2) Drop "re-init libldac with the larger MTU for fewer, bigger packets". It is a no-op in this libldac version.
- (3) The verification step "log `max_media_payload_size` on every sink" is **already done** at `a2dp.c:3608`.
  Just collect it.
- (4) The nit 09d guard should be `out_cap < 661 + header`, not `< PL_LDAC_INIT_MTU`.

## F-audio-03: The PI feedback loop is underdamped and its comment is wrong; trims zero the integrator
**Verdict: CONFIRMED.** My numbers match the reviewer's. Fix option (a) is wrong.

Evidence (`usb_audio.c:456-501`, 1 ms tick in the 0xC0 worker, `usb_pump.c:272-283`):
- Controller: `u_ppm = -500·e/T + (Σ-e)/1e5`.
- Plant: fill' = 192 B/ms × 1e-6 per ppm = 1.92e-4 B/ms/ppm. The consumer is credit-clocked on the local
  crystal, so the plant is a pure integrator.
- Closed loop: `x'' + (1.92e-4·500/T)x' + (1.92e-4·1e-5)x = 0`.
- **T = 5760: ωn = 4.38e-5 rad/ms → period 143 s, ζ = 0.190, τ = 120 s.**
- The runtime target can be up to 8192 (`a2dp.c:3544-3561`, clamp to capacity/4). There ζ = 0.134 and τ = 171 s.
  The reviewer's figure is the best case.

My simulation (`scratchpad/fbsim.py`) models the exact integer code: C truncation, EMA>>6, both clamps, host
4-byte granularity, and 512-B consumer bursts. With a 222 ppm offset: peak error −765 B at 36 s, zero crossings
~78/151/224 s, range [−765, +418], 54 % overshoot. The reviewer had +745 B at 33 s; only the sign convention differs.

- The "11 s" comment is the I-term ramp time assuming the P term keeps the error parked (22.2e6 / 2557 B ≈ 8.7 s).
  It is not a settling time. Confirmed.
- `pl_a2dp_resync_complete` → `pl_usb_audio_fb_reset()` (`a2dp.c:2252`) zeroes `s_fb_i_accum` (`usb_audio.c:550`).
  Confirmed. The 2ap §3.3 citation concerns undersupply episodes, not trims, so it is a stretch. The effect is real
  anyway: every trim discards the learned ~222 ppm and re-rings the loop.
- Minor correction: the loop is not "±750 B for minutes then done". A 2 % settle takes 400-560 s.

Severity **P2**, Tier **Sonnet**.

Corrections to the fix sketch:
- **Option (a), `PL_FB_KI_DIV → 1.3e6`, makes things worse.** τ = 2/Kp' does not depend on Ki. Simulated at
  250 ppm: peak −1845 B (2.2× worse, within ~1 KB of the trim band) and settling 584 s.
- **Use option (b).** `PL_FB_KP_PPM ≈ 1850` gives peak −513 B, ~4 % real overshoot, and |e| < 40 B by about 100 s,
  with no rail at 250 ppm.
- The proposed test threshold "2 % settling < 60 s" is unattainable under the consumer sawtooth for *any* tuning.
  Use "overshoot < 10 % and |e| < 100 B by 120 s at ±250 ppm".
- Split `fb_reset` as proposed.

## F-audio-04 (spot-check): `sdk-patches/` is not the source of truth it claims
**Verdict: CONFIRMED.**
- `01-*.patch` adds a bare `pl_ep_double_arm_count[...]++;` with no declaration. The script adds the `extern`.
- `.patch` files exist only for 01-03.
- Stale "default OFF" text: README `:107`, `tusb_config.h:177`, `CMakeLists.txt:68` and `:124-126`. Reality is
  `tusb_config.h:196` `1` and `CMakeLists.txt:128` `ON`.
- `tusb_config.h` contradicts itself: "EXPERIMENTAL, default OFF" at `:177`, then "DEFAULT ON" at `:189`.
- There is a phantom `04-tinyusb-audio-iso-out-isr.patch` at `:178`.

Additions:
- The comment that 04d injects *into the SDK* still says "default 0, a no-op".
- 06's injected comment says "clamped on a non-overwritable FIFO", which is false (see V02). Fix both when
  regenerating the patches.

P2, Haiku, as filed.

## F-audio-06 (spot-check): the START round-trip can make the first streaming tick trim
**Verdict: CONFIRMED (mechanism).** The trigger is easier than stated, and one sub-claim is wrong.

Evidence:
- PRIMING (`a2dp.c:2669-2673`) is evaluated once per ~10 ms media tick, so fill at the `start_stream` call is
  already target + 0…1920 B.
- STREAM_STARTED (`:3700-3784`) sets `last_resync_us = 0` and `s_soft_over_since_us = 0`, then `fb_reset` seeds the
  EMA to the current fill.
- `resync_decide` (`:2147-2185`) with the default hold 0 and hard band 2880 trips when EMA > target + 2880, and
  `now - 0 > 2 s` is always true.
- So a trim fires when tick overshoot + RTT×192 > 2880, i.e. **RTT > ~5-15 ms**, not "> 15 ms".
- Wrong sub-claim: "otherwise a permanent latency offset". The PI loop does pull the EMA back to target, just over
  the ~143 s ring of F-audio-03.
- RTT magnitude: unverifiable here. No capture in `.research/` shows a post-start `resync_events` value from
  hardware (the only hits are design-doc text).

P2, Sonnet. The fix sketch is fine.

## "SPSC rings and core1 handshake are correct" — attempted break
- **pcm ring** (`pcm_ring.c`):
  - Holds. Single writer per index: `s_head` is written only by the 0xC0 worker. `s_tail` is written by the
    consumer (core1 `pl_pcm_read`/`pl_pcm_trim_to` under ON) and by core0 `pl_pcm_reset`, but only after
    `quiesce_and_wait` (`a2dp.c:3444→3606`, `3806→3826`, `3847→3867`, `3883→3925`).
  - `__dmb()` is pico-sdk's `dmb` with a `"memory"` clobber, so it is also a compiler barrier. Producer order is
    data, dmb, head. Consumer order is head, dmb, data.
  - The missing consumer-side release before the `s_tail` store is real but benign on in-order M33 + uncached
    SRAM (the reviewer's 09a). Agree P3.
- **tx ring** (`a2dp.c:1468-1473`, `1911-1941`): holds. Seal is slot, dmb, head. Send is count, dmb, slot,
  clear, dmb, tail. One reserved slot.
- **Lost wakeup in the park**: none. `arm_running` does `__sev()` after publishing RUNNING (`:2601-2603`). If
  core1 read IDLE just before, the SEV latches the event register and the next `__wfe()` returns immediately.
- **Quiesce handshake: broken in a narrow window.** See F-audio-V03.

---

## New findings

### F-audio-V01: A zero-length ISO-OUT packet still takes the stock dead-endpoint path, and double-counts on the way
- Severity: P2   Confidence: High (code path) / Low (whether the host ever sends one)   Effort: S   Tier: Sonnet
- Location: `tools/apply-sdk-patches.sh:440-462` (PATCHED6 → `audio_device.c:824-832`); stock `audio_device.c:759-762`; `firmware/src/usb_audio.c:333-347`
- Evidence: for a completion of `xferred_bytes == 0`:
  - `rp2040_usb.c:246`: `0 < wMaxPacketSize` counts as a short packet, so the transfer completes with len 0.
  - `audiod_xfer_isr` calls `pre_read_cb` (packet_count++, rx_short_packets++).
  - `tu_fifo_write_n(..., 0)` returns 0 (`tusb_fifo.c:471`), so `written == 0` and the ISR returns false.
  - `usbd.c` reverts busy/claimed and queues the event. `tud_task` → `audiod_xfer_cb` → stock `audiod_rx_done_cb`.
  - `pre_read_cb` runs a **second** time.
  - `TU_VERIFY(tu_fifo_write_n(...,0))` fails, and the function returns **before** `usbd_edpt_xfer`.
  - Result: ISO-OUT is never re-armed, and audio stays dead until the host re-selects the alt setting or the cable is replugged.
  ```
  if (written == 0) {
    return false;          // n==0 is the only way to get here (FIFO is overwritable)
  }
  ```
- Why it matters: this is the only surviving route back to the "one bad completion kills the endpoint" defect the
  project diagnosed on 08-28. Stock 0.18.0 has the same flaw. Whether macOS CoreAudio ever sends a 0-byte ISO OUT
  (for example at stream start or under feedback-driven size modulation) is unknown. `rx_short_packets` runs to
  thousands in the logs, but those packets are non-zero sizes.
- Fix sketch: in PATCHED6, when `xferred_bytes == 0`, skip the FIFO write, re-arm, run post_read, and return true.
  More generally, never return false after `pre_read_cb` has run: count and re-arm instead. Update the patch-06
  comment and README §06.
- Verification: desk — re-run the script on a stock 0.18.0 tree and check that no `return false` follows the
  `pre_read_cb` call. Host — a test that compiles the patched `audiod_xfer_isr` body against stubs and feeds
  n = 0. Hardware — manual only (needs a host that emits ZLPs).
- Related: `pico-link-2ap.6`, `pico-link-9ziq`, reviewer F-audio-01.

### F-audio-V02: `pl_usb_fifo_shortfall_bytes` can never count: the EP-OUT FIFO is overwritable, and the 9ziq design rests on the opposite assumption
- Severity: P2   Confidence: High   Effort: S   Tier: Sonnet
- Location: stock `audio_device.c:1324` (`tu_fifo_config(..., 1, true)`); `tusb_fifo.c:483-529`; `tools/apply-sdk-patches.sh:421-462`; `firmware/src/usb_pump.c:136-143, 550-561`; `.planning/design/2026-09-23-usb-out-fifo-loss-off-build.md:50-52`
- Evidence: the design doc says `tu_fifo_write_n` "is non-overwritable, so on a full FIFO the packet tail is
  truncated silently". In 0.18.0 the FIFO is overwritable, so a full FIFO **overwrites the oldest unread bytes**
  and returns `n`. The reader then moves `rd_idx` forward (`_ff_correct_read_index`). Past about two depths
  unread (~8 ms of worker starvation), TinyUSB's own TODO says the read-back data is wrong
  (`tusb_fifo.c:516-519`). The ISR writer (0x80) can also preempt the reader's memcpy (0xC0), which gives torn samples.
- Why it matters:
  - The 9ziq success criterion "FIFO-shortfall counter delta = 0" is **vacuously true** and proves nothing.
  - `usb_lost_bytes = rx − pcm` is the only correct loss instrument, and it *is* correct: overwritten bytes are
    counted in rx and never in pcm.
  - Anyone reading patch 06 or the design doc will look for "truncated tails" when the real symptom is
    "oldest ~ms overwritten, sometimes garbage".
- Fix sketch:
  - Delete the shortfall counter, or redefine it as "bytes overwritten" computed as
    `max(0, count_before + n − depth)` from `tu_fifo_count` before the write.
  - Correct the design doc (mark §c.4 as wrong), the patch-06 comment and README §06.
  - Optionally raise `EP_OUT_SW_BUF_SZ` as the doc already suggests, to keep double overflow out of reach.
- Verification: after the fix, a hardware run that masks the 0xC0 worker for ~8 ms shows the new counter ≈
  `usb_lost_bytes` delta. Desk check: `grep -n "tu_fifo_config(&audio->ep_out_ff" $PICO_SDK_PATH/lib/tinyusb/src/class/audio/audio_device.c` shows `true`.
- Related: `pico-link-9ziq`; supersedes reviewer F-audio-08.

### F-audio-V03: core1 quiesce handshake can read a stale `s_enc_quiesced == true` while core1 is entering RUNNING
- Severity: P2   Confidence: Medium (mechanism) / Low (reachability: sub-µs window unless core1 takes an IRQ between two instructions)   Effort: S   Tier: Opus (core1 handshake)
- Location: `firmware/src/a2dp.c:2404-2420` (core1 loop head), `:2570-2581` (`pl_a2dp_core1_quiesce_and_wait`), `:2596-2603` (`pl_a2dp_core1_arm_running`)
- Evidence:
  ```
  core1: state = s_enc_state;            // reads RUNNING
         if (state != RUNNING) {...}      // not taken
         s_enc_quiesced = false;          // line 2420 -- store lands LATER
  core0: s_enc_state = DRAINING;          // quiesce_and_wait
         while (!s_enc_quiesced) ...      // reads the IDLE-era `true` -> returns at once
  ```
  Core0 then runs `pl_pcm_reset`, `tx_flush`, `rtp_next = 0` and codec re-init while core1 executes
  `resync_apply` and `pl_a2dp_fill`. That breaks the one-writer-per-index rule (`s_tail`, tx ring).
  - `arm_running` never clears `quiesced` itself.
  - Clearing it there is **not sufficient**: core1 in the IDLE loop can re-store `true` after core0's clear and
    before it reads RUNNING.
  - This is a Dekker pattern. Each side must store its own flag and then re-check the other side's flag, with a
    store→load barrier in between.
  - `quiesce_and_wait` has a dmb *before* the state store, but none between the store and the poll.
- Why it matters: if the window is hit, it is exactly the concurrent core0/core1 mutation the whole handshake
  exists to prevent: a corrupt ring, a double-sent or stale slot, or a crash inside libldac. Today it needs
  STREAM_STARTED followed by a quiesce (SUSPENDED/RELEASED) inside core1's ~10-20-cycle WFE-exit-to-store
  window. That is realistic only if core1 is interrupted there (for example by the flash-lockout FIFO IRQ). No
  counter would reveal it: `quiesce_timeouts` stays 0.
- Fix sketch:
  - Core1 RUNNING branch: `s_enc_quiesced = false; __dmb(); if (s_enc_state != PL_ENC_STATE_RUNNING) continue;`
    (re-check after publishing).
  - `quiesce_and_wait`: add `__dmb()` between `s_enc_state = DRAINING` and the poll loop.
  - Either side then sees the other's store, so a `true` observed by core0 implies core1 will re-read a
    non-RUNNING state before touching shared state.
- Verification: a host model test with two pthreads and injected delays between core1's state load and its
  flag store, asserting that "core0 passed quiesce" and "core1 inside fill" are never true together. The
  unfixed version fails with a forced delay. Firmware: cross-compile the ON config.
- Related: ADR 2026-09-03 §4 (quiesce handshake); beads `pico-link-nli.4`, `pico-link-nli.3`.

### Minor additions (P3)
- The core1 non-RUNNING branch stores `s_enc_quiesced = true` (`a2dp.c:2410`) with no preceding `__dmb()`. Core1's
  last `pl_a2dp_fill` stores to non-volatile `s_ctx` fields are ordered before it only by M33 in-order behaviour,
  not by the C or ARMv8-M memory model. This is the same class as 09a. Add a barrier when V03 is fixed.
