# 01 — Audio pipeline (USB host → ISO OUT → PCM ring → encoder/core1 → A2DP TX → BTstack)

Reviewed tree: `2e37164`. Seam prefix `audio`. Reviewer read every file in scope end to end
(`a2dp.c` in full, all 4492 lines), the three ADRs and fourteen design docs named in the
brief, `tools/apply-sdk-patches.sh` (the real patch source), the relevant slices of
`persist.c`, `flash_lockout.c`, `bt.c`, `btstack_config.h`, `main.c`, and the libldac
public header at the call boundary. No board, no ARM toolchain, no TinyUSB source in this
container: every claim about vendored TinyUSB is from the patch text plus the reviewer's
knowledge of 0.18.0 and is marked so.

**Host tests measured (all in `firmware/tests/`, built with `cc -std=c11 -Wall -Wextra`):
8 of 8 compile and pass.** Individual results: `test_a2dp_priming_cushion_frames_per_packet`
3 checks pass; `test_a2dp_tx_ring_count` ALL PASSED; `test_codec_id_stability` 3 checks pass;
`test_fault_evaluator` 4 tests pass; `test_ldac_abr_controller` 4 checks pass (ALL PASSED);
`test_ldac_frames_per_packet` 3 checks pass; `test_paired_device_upserted_ldac_quality_echo`
2 tests pass (needed `cbindgen` to generate `pico_link_ui.h` first — see F-audio-09k);
`test_pcm_ring_cross_core` ALL PASSED (8,192,000 bytes round-tripped). Every file emits one
`-Wcomment` warning from the backslash-continued build line in its own header comment.

---

## 1. Verdict

The pipeline is **structurally sound and unusually well instrumented**: every ring is
single-writer-per-index with explicit `__dmb()` at each cross-core publish, the credit clock
is bitrate-invariant, the resync trim and ABR controller both follow one decide/apply/complete
pattern, and nothing on the hot path logs, allocates, or calls Rust. No P0. The genuinely good
thing to leave alone is the SPSC discipline plus the "counters only in IRQ, print from thread"
rule — it is applied consistently across ~40 counters and it is what made every hardware bug
in the history diagnosable. The two P1s are at the edges: the vendored TinyUSB ISR patch
still hands a full-FIFO packet to the unpatched stock path that kills the endpoint (and
double-counts it on the way), and the LDAC row's fixed 679-byte MTU is never checked against
the sink-negotiated payload. The PI feedback loop is stable but underdamped (ζ≈0.19, 143 s
period) and its comment misstates its dynamics by ~40×.

Counts: **P0: 0 · P1: 2 · P2: 6 · P3: 1 batch (12 nits)**.

## 2. What is well done (do not touch)

- **`pcm_ring.c` ownership contract.** Producer writes only `s_head`, consumer only
  `s_tail`; overflow drops newest and never touches the consumer's index; misaligned
  lengths are rejected wholesale. `__dmb()` after the data write / before the data read.
  The host test proves integrity across 8 M bytes of wrap. Keep.
- **The tx ring after `nli.3`.** Deriving `tx_count` from `(head - tail) & MASK` with a
  power-of-two depth and one reserved slot removed the cross-core RMW; both the seal-side
  publish and the send-side consume have the correct barrier pairs
  (`a2dp.c:1468-1520`, `1884-1950`). The redundant queue-full guard *inside* the seal
  branches is provably dead (count can only fall between the loop-top check and the seal)
  but harmless.
- **Core1 lifecycle** (`a2dp.c:2371-2560`): `IDLE→RUNNING→DRAINING`, a bounded quiesce
  handshake before every core0 mutation of shared state, `__sev()`/`__wfe()` park with the
  latched-event argument written out, a compile-time assert tying the 2 ms fill budget to the
  5 ms quiesce timeout, an unconditional stack-overlap assert, and the six invariants
  (no Rust/alloc/IRQ-off/pl_log/bt-ring/BTstack) honoured at every call site I checked.
- **Decide/apply/complete split** used identically by the resync trim and the ABR
  controller: one writer per word, targets not deltas, reseed-last. The ABR host test
  covers rail handling, multi-rung convergence and the limit-cycle bound.
- **Credit clock** (`pl_a2dp_accrue_credit`): µs-resolution accrual with carried remainder,
  clamped to ring fill (not packet size), debited by the trim (`rzqd` Fix B), reset on the
  RUNNING entry edge (Fix A). Bitrate-invariant by construction, exactly as the ABR design
  §4.1 argues.
- **Counters as views** (`fault.c`): every fault is a delta of a monotonic counter with the
  negative-delta reset trap handled; the destructive `fill_min` reader has exactly one caller.
  The fault evaluator's host test exercises the pre-connect burst, host-silent gating,
  raise/clear/refresh cadence and dynamic severity.
- **Explicit-feedback USB**, gated on alt-1 so `tud_audio_fb_set` never claims EP0
  (`usb_pump.c:283-285`), 10.14/3-byte format for full-speed macOS, one setpoint shared by
  PRIMING, the feedback loop and the trim (`pl_pcm_set_target_fill_bytes`).
- **Flash safety while streaming**: `persist.c:791,938` refuse to write while
  `pl_usb_audio_streaming() || pl_a2dp_streaming()`, and `flash_lockout.c` uses the timeout
  lockout variants with START-timeout-benign / END-timeout-fatal semantics per ADR §7.1.
  `MAX_NR_HCI_CONNECTIONS 1` / `MAX_NR_AVDTP_CONNECTIONS 1` (`btstack_config.h:51,85`) also
  rule out the one path I looked for where `row->init()` could re-init the libldac handle
  under a still-RUNNING core1 (a second `establish_stream` fails at BTstack first).
- **The apply script's idempotent exact-string patching** with a "neither stock nor patched"
  failure, and CMake `FATAL_ERROR` on every missing marker.

## 3. Architecture assessment

| Doc | Code matches? | Divergence and who is right |
|---|---|---|
| ADR 2026-08-27 TinyUSB | Yes | — |
| ADR 2026-09-02 core1 allocation | Superseded by 09-03; the surviving decision (core1 = LDAC only, Rust single-core) holds. | — |
| ADR 2026-09-03 LDAC on core1 | §§2-4 implemented as written: two rings, verbatim `pl_a2dp_fill`, dwell bound retired under ON (`stop_dwell` kept, asserted 0), quiesce handshake, seqlock levels (§5), narrowed lockout (§6), failure table (§7). §12.4 F2 "park core1" landed for the **non-RUNNING** branch only; the RUNNING branch still spins between encodes (F-audio-05). | Code right where built; F2 half-done. |
| a2dp-source-pipeline (08-29) | §1 clock topology, §3 ring, §4 table, §5 IRQ contract, §7 report all hold. §2.1's P-only controller was replaced by PI (`nxf`) and the doc's own CORRECTION block already records the `fb_set`-on-alt-0 hazard. | Code right; doc §2.1 is stale but self-annotated. |
| usb-audio-alt1 (08-29) | Feedback EP 3 bytes, format correction on, `AUDIO_FEEDBACK_METHOD_DISABLED`: all present. | — |
| a2dp-drain (08-30) | D1-D7 present. D1's "CONCURRENCY: none" was correctly rewritten by `nli.3`. Depth 5→8. | — |
| ldac (08-30) | L0-L3 landed; Q4 "uniformise the vtable" taken (sustainable path). `PL_LDAC_INIT_MTU` follow-up ("once hardware proves the real negotiated MTU") never happened → F-audio-02. | Doc names the gap; code never closed it. |
| pcm-pacing (08-30) | Findings 1 & 2 fixed (clamp to ring, priming clamp to capacity/4, `worst_tick_interval_us` reset); trim wired (`fhf`). | — |
| 2ap tolerate-undersupply (09-01) | **Mostly unbuilt.** No `pl_usb_supply_q8`, no conditional integration (§3.3), no mute state machine (§2 regime B), no `AudioHealthChanged` tag. §3.3's rule "preserve the integrator across an episode; do not zero it" is contradicted by `pl_usb_audio_fb_reset` zeroing `s_fb_i_accum` on every resync trim (F-audio-04). `fault.c` later took the "USB SUPPLY LOW" level half a different way (bytes vs 192 B/ms). | Doc is a proposal that was overtaken; mark it Superseded-in-part or the next reader will build regime B on top of the fault strip. |
| audio-fault-model (09-07) | Implemented faithfully incl. §5.6's five traps. `PL_FAULT_CONGEST_MIN` is 2 (Tess-measured), doc says 8 provisional — code annotated. | Code right. |
| ldac-abr-control-loop (09-07) | §§2-3, 6 implemented; §4.2 "honest frames_per_packet for the floor rung" landed (`i6zn`). §5.2's reset-on-`ldac_quality`-change is not wired for a **live** pin→Adaptive switch (F-audio-09i). | Minor gap in code. |
| ldac-quality-selector (09-07) | Firmware side (`pin_now`, live kbps cache) present. | — |
| core1-encoder-default (09-23) | P1 (enc_mean/win_max) and P2 (objcopy libldac into `.time_critical`, post-link flash check) present; default flipped. | — |
| core1-resync-trim (09-23) | Implemented verbatim incl. cancel-at-arm, SKIPTICKS under ON, report additions. Its Test C expects `resync_events=0` after "stream started" — see F-audio-06 for why that can fail. | Doc's expectation may be wrong. |
| on-arm-ring-collapse (09-23) | Fixes A/B/C present and correctly placed. | — |
| usb-out-fifo-loss (09-23) | F1 (drain outside the mutex) and F2 (shortfall counter, `usb_lost_bytes`) present. | Instrument is skewed by F-audio-01's double count. |
| congestion-cushion (09-24) | S1 present (hold/hard band, stall histogram, delay report). | — |

Two doc-hygiene facts that matter for a cheap model acting on this: (a) `.planning/progress.md`
is frozen at 2026-08-31 and describes a pre-core1 world; (b) dozens of `a2dp.c:NNNN` anchors
in the design docs are stale after the file grew. Neither is a code bug.

## 4. Findings

### F-audio-01: The ISR ISO-OUT patch still defers a full-FIFO packet into the unpatched stock path that kills the endpoint, and double-counts it on the way
- Severity: P1   Confidence: High (patch text) / Medium (stock 0.18.0 body, from memory + CLAUDE.md's own `audio_device.c:759-762` citation)   Effort: S   Tier: Sonnet
- Location: `tools/apply-sdk-patches.sh:326-334, 455-462` (the `PATCHED4D`/`PATCHED6` bodies, i.e. what is actually in the SDK); `firmware/src/usb_audio.c:333-347` (`tud_audio_rx_done_pre_read_cb`); `firmware/src/usb_pump.c:557-561` (`usb_lost_bytes`)
- Evidence: in `audiod_xfer_isr` the app callback runs first, then the FIFO write; a full FIFO returns `false`, which patch 04b turns into "queue the completion for `tud_task()`":
  ```
  if (!tud_audio_rx_done_pre_read_cb(...)) { return false; }      // packet_count++, rx_bytes_total += n  (usb_audio.c:337-341)
  uint16_t written = tu_fifo_write_n(...);
  if (written < xferred_bytes) { pl_usb_fifo_shortfall_bytes += ...; }
  if (written == 0) { return false; }                             // -> send = true in dcd_event_handler -> audiod_xfer_cb later
  ```
  Stock `audiod_xfer_cb → audiod_rx_done_cb` then (i) calls `tud_audio_rx_done_pre_read_cb` **again** (second `packet_count++`, second `rx_bytes_total += 192`), and (ii) does `TU_VERIFY(tu_fifo_write_n(...))` **before** `usbd_edpt_xfer(ep_out)` — the exact "one overflow returns before the re-arm" defect the project diagnosed on 2026-08-28. Nothing in `sdk-patches/` touches that stock line.
- Why it matters: (1) if the worker has still not drained when the deferred task-path write runs, the endpoint is never re-armed and audio is dead until a physical replug — the original M3 hang, now behind two rare conditions instead of one, but on the shipping path. (2) Every such event inflates `packet_count`/`rx_bytes_total` by one packet, so `usb_lost_bytes` (= rx − pcm) reads exactly `fifo_shortfall_bytes` **even when the packet was delivered late and nothing was lost**, `s_sof_pkts` over-counts, and `fault.c`'s USB SUPPLY LOW ratio is biased toward "healthy". The 9ziq conservation check therefore cannot distinguish "lost" from "deferred".
- Fix sketch: in `PATCHED4D`/`PATCHED6`, on `written == 0` do **not** return `false`: count the shortfall, drop the packet, still `usbd_edpt_xfer(ep_out)` to re-arm, return `true`. Same for the `pre_read_cb` false branch (unreachable today, same shape). Then `audiod_xfer_isr` never hands an already-counted packet to `audiod_rx_done_cb`, the endpoint can never die on a full FIFO, and `usb_lost_bytes == fifo_shortfall_bytes` becomes a true identity. Update `sdk-patches/README.md` §06 ("behaviour is unchanged") accordingly. Upstream status of the same defect in TinyUSB ≥0.19: **unknown** offline.
- Verification: (a) desk: re-read the applied `audio_device.c` and confirm no path from `audiod_xfer_isr` returns `false` for `audio->ep_out` after `pre_read_cb` ran; (b) hardware: a `PL_DEBUG_REMOTE` command that masks the 0xC0 worker IRQ for ~8 ms while streaming; pass = `fifo_shortfall_bytes` rises by 4-5 packets, `packets` keeps climbing afterwards, `usb_lost_bytes == fifo_shortfall_bytes`, no `miss_unavail` plateau.
- Related: `sdk-patches/README.md` §§04-06, beads `pico-link-tfj`, `pico-link-2ap.6`, `pico-link-9ziq`, design `2026-09-23-usb-out-fifo-loss-off-build.md`.

### F-audio-02: LDAC's fixed 679-byte MTU is never validated against the sink-negotiated media payload
- Severity: P1   Confidence: Medium   Effort: S   Tier: Sonnet
- Location: `firmware/src/codec_ldac.c:90` (`PL_LDAC_INIT_MTU 679`), `:539` (`(void)out_cap`); `firmware/src/a2dp.c:3446` (`max_media_payload_size`), `:3474-3490` (frames_per_packet for a self-packetising row — no MTU check)
- Evidence: `codec_ldac.c:90` comments that 679 is "always safely smaller than a2dp.c's generous PL_A2DP_PAYLOAD_SLOT_BYTES (1030) slot buffer regardless of what the remote actually negotiates" — but the binding limit is `pl_a2dp_usable_payload(max_media_payload_size, 1)`, the **peer's** L2CAP MTU minus RTP, which the same design doc says "is the headset's number" (`2026-09-03` ADR §11.3). The STREAM_ESTABLISHED handler computes `frames_per_packet` from the row's hint and never compares 679+1 against `usable_payload`. `LDACBT_MTU_REQUIRED` is 679 (`vendor/libldac/src/ldacBT_internal.h:56`), so libldac cannot be configured smaller.
- Why it matters: a sink whose media MTU yields a usable payload below 680 bytes (L2CAP's default 672-byte MTU gives 660) receives 680-byte AVDTP payloads. Whether BTstack truncates or rejects, the result is `pkt_fail` climbing or corrupted LDAC frames on a stream that negotiated fine. One sink has been tested; the product's next milestone is more headphones. This is the "follow-up once hardware proves the real negotiated MTU" that `codec_ldac.c:90`'s own comment defers and nothing tracks.
- Fix sketch: at STREAM_ESTABLISHED, when `frame.encoded_frame_bytes == 0`, compare `usable_payload` against the row's declared packet size (add `uint16_t self_packetising_packet_bytes` to `pl_codec_frame_info_t`, set to `PL_LDAC_INIT_MTU`). If smaller: log loudly and `pl_bt_push_connect_failed(NO_A2DP_SINK)`-style fallback, or re-run negotiation preferring SBC. Also add `_Static_assert(PL_LDAC_INIT_MTU + 1 <= PL_A2DP_PAYLOAD_SLOT_BYTES - 1)`. Optionally, when `usable_payload` is much larger, re-init libldac with the larger MTU (fewer, bigger packets — the congestion lever the ABR doc §4.4 names).
- Verification: extend `firmware/tests/test_a2dp_priming_cushion_frames_per_packet.c` with a model of the new check (usable 660 → refuse; 883 → accept); on hardware, log `max_media_payload_size` on every sink connected from now on.
- Related: `.planning/design/2026-08-30-ldac.md` Q4, `2026-09-07-ldac-abr-control-loop.md` §4.4, bead `pico-link-cz0.5.6`.

### F-audio-03: The USB feedback PI loop is underdamped and its comment misstates its dynamics; the resync trim zeroes the integrator the 2ap design says to preserve
- Severity: P2   Confidence: High (numerically simulated; see below)   Effort: S   Tier: Sonnet
- Location: `firmware/src/usb_audio.c:403-421` (gains), `:456-501` (`pl_usb_audio_feedback_task`), `:542-553` (`pl_usb_audio_fb_reset`); `firmware/src/a2dp.c:2229` (`resync_complete` calls `fb_reset`)
- Evidence: `p_ppm = -(err*500)/target`, `accum -= err` per 1 ms tick, `I = accum/100000`. With target 5760 and plant gain 192 B/ms: Kp' = 1.67e-5 /ms, Ki' = 1.92e-9 /ms², ωn = 4.4e-5 rad/ms → **period 143 s, ζ = 0.19, decay τ = 120 s**. A discrete simulation of the exact integer code (EMA, truncation, clamps) against a +222 ppm crystal gives peak error +745 B at t = 33 s, first zero-crossing at ~75 s, fill swinging 5319–6568 B for minutes. The comment at `:414` says "converges in roughly 11s" — that is the time for the I-term to *reach* 222 ppm while P is parked at −2046 B, not settling. Separately, `pl_usb_audio_fb_reset()` sets `s_fb_i_accum = 0` and is called from `pl_a2dp_resync_complete` after every trim, so each trim throws away the learned crystal offset and restarts the 143 s ring; `2026-09-01-2ap-tolerate-undersupply.md` §3.3 explicitly says "preserve the integrator … do not zero it".
- Why it matters: not audible today — the ±750 B swing sits inside the ±2880 B trim band and above the fill floor. But it halves the margin the trim band was sized for, adds a 2-4 minute fill wobble after every stream start, pause/resume and trim, and the congestion-cushion work (`8pp1`) is about to size hold times against `fill_ema` behaviour that this loop makes look like drift.
- Fix sketch: (1) split `fb_reset` into `fb_reseed_ema()` (EMA + `fill_min` only; used by the trim) and the full reset (used at STREAM_STARTED); (2) retune toward ζ ≈ 0.7: either `PL_FB_KI_DIV` 100000 → ~1.3e6 (Ki ÷13, keeps Kp) or raise `PL_FB_KP_PPM` ~3.7× (saturates the 500 ppm clamp beyond |err| ≈ 1550 B — acceptable); (3) fix the comment with the real numbers.
- Verification: add `firmware/tests/test_usb_feedback_loop.c` that copies the tick body and asserts overshoot < 20 % and 2 % settling < 60 s for ±250 ppm; on hardware, `fill_ema` after a SKIPTICKS trim returns to band without a second excursion.
- Related: beads `pico-link-nxf`, `pico-link-fhf`, `pico-link-quzf`; design `2026-09-01-2ap-tolerate-undersupply.md` §3.3.

### F-audio-04: `sdk-patches/` is not the source of truth it claims to be
- Severity: P2   Confidence: High   Effort: S   Tier: Haiku
- Location: `firmware/sdk-patches/01-tinyusb-rp2040-double-arm.patch:14`; `firmware/sdk-patches/README.md:1-14, 107, 134`; `tools/apply-sdk-patches.sh:115-470`; `firmware/src/tusb_config.h:177`; `firmware/CMakeLists.txt:65-70`
- Evidence: README: "every SDK change we depend on is vendored here … The .patch files … remain the human-readable source of truth". Patches 04a-d, 05 and 06 exist **only** as Python string literals in the script; there is no `.patch` for them. `01-*.patch` applies `pl_ep_double_arm_count[...]++;` with no declaration in scope (the script's version adds `extern volatile unsigned int pl_ep_double_arm_count[32];`) — applied with `patch(1)` it would not compile. README §04, `tusb_config.h:177` and `CMakeLists.txt:65-70` all say `PL_USB_ISO_XFER_ISR` "default OFF / defaults to 0"; `tusb_config.h:190` defaults it to `1` and `CMakeLists.txt:128` to `ON`. `tusb_config.h:178` names a file `04-tinyusb-audio-iso-out-isr.patch` that does not exist.
- Why it matters: the whole point of the directory is that a second machine or an SDK reinstall reproduces the exact vendored TinyUSB. Today only the script does, and a reviewer reading the `.patch` files (as this brief asked) sees half the change set. The stale "default OFF" comments will mislead the next person debugging ISO-OUT into thinking the ISR path is an experiment.
- Fix sketch: generate the six `.patch` files from a stock SDK checkout after running the script (`diff -u`), commit them, add a `--check` mode to the script that diffs its output against the `.patch` files; fix the four "default OFF" comments and the phantom filename.
- Verification: `tools/apply-sdk-patches.sh --check` exits 0; `grep -rn "default OFF\|defaults to 0" firmware/sdk-patches firmware/src/tusb_config.h firmware/CMakeLists.txt` returns nothing about `PL_USB_ISO_XFER_ISR`.
- Related: beads `pico-link-06m`, `pico-link-2ap.6`, `pico-link-9ziq`.

### F-audio-05: Core1 busy-spins between encodes while RUNNING (ADR §12.4 F2 is only half landed)
- Severity: P2   Confidence: Medium   Effort: S   Tier: Opus (touches core1 wake semantics)
- Location: `firmware/src/a2dp.c:2371-2493` (`pl_a2dp_core1_entry` RUNNING branch), `:1644-1647` (`stop_credit` break), `:1720-1724` (`stop_ring_empty` break)
- Evidence: the non-RUNNING branch does `__wfe()`; the RUNNING branch is `resync_apply → time_us_64 → accrue_credit → pl_a2dp_fill → heartbeat++ → loop` with no wait. At HQ one 128-frame encode (~834 µs measured) is owed every 2.67 ms, so ~65 % of core1's wall time is a tight loop of `time_us_64()` (APB), `pl_pcm_fill_bytes()`, `pl_a2dp_tx_count()` and `pl_a2dp_publish_levels()`'s `time_us_64()`; while starved it is ~100 k polls/s (the ADR and `rzqd` both measured this — it is why `underrun_events` had to be edge-counted).
- Why it matters: the 09-03 ADR §12.1(c) names this spin as a bus/APB contention source for core0 and §12.4 says F2 "should land regardless"; only the idle half did. It is power and contention, not correctness — but core0's render cost under streaming is the metric the epic missed its bar on.
- Fix sketch: on a `stop_credit` or `stop_ring_empty` exit, park with `best_effort_wfe_or_timeout(make_timeout_time_us(500))`, and add a bare `__sev()` after the `s_head` publish in `pl_pcm_push` (one instruction, wakes both cores, harmless spurious wakes). Interrupts stay enabled so the lockout handshake is unaffected; the quiesce timeout (5 ms) still covers a 500 µs park.
- Verification: `stop_ring_empty` rate while starved drops from ~1e5/s to ≤2e3/s; `stop_credit` rate drops from ~1e5/s to ~375-750/s; render p50 under LDAC HQ no worse (Tess A/B).
- Related: ADR `2026-09-03` §12.1(c), §12.4 F2; beads `pico-link-1n4.3`, `pico-link-rzqd`.

### F-audio-06: Stream start does not account for the START round-trip; the first STREAMING tick can fire a resync trim
- Severity: P2   Confidence: Medium (mechanism read end to end; RTT magnitude needs the board)   Effort: S   Tier: Sonnet
- Location: `firmware/src/a2dp.c:2676-2684` (PRIMING exit), `:3666-3760` (STREAM_STARTED: `last_resync_us = 0`, `pl_usb_audio_fb_reset()` seeds the EMA to raw fill, no trim), `:2147-2177` (`resync_decide`, `hold_ms` default 0)
- Evidence: PRIMING calls `a2dp_source_start_stream` when fill ≥ `priming_target_bytes` (≈5760). The ring keeps filling at 192 B/ms until `A2DP_SUBEVENT_STREAM_STARTED` arrives — an AVDTP round trip. STREAM_STARTED seeds `fill_ema` to the **current** fill and clears `last_resync_us`; on the next media tick `resync_decide` trips if `fill_ema > target + 2880`, i.e. whenever the START RTT exceeded 15 ms. The C2-3 comment says "the priming cushion is not overshoot" — true, but the RTT overshoot is, and nothing else removes it (the feedback loop needs ~50 s per 4.6 KB).
- Why it matters: either a resync trim (audible only as the first tens of ms of the track never playing, plus `resync_events=1` at every start, which breaks quzf Test C's `resync_events=0` expectation and pollutes the ENC RESYNC fault key), or, if RTT < 15 ms, a permanent latency offset the trim band never sees. Neither is what the priming design intended.
- Fix sketch: at STREAM_STARTED, `s_ctx.flush_frames += pl_pcm_trim_to(s_ctx.priming_target_bytes)` (consumer-side, core1 quiesced — under ON put it inside `pl_a2dp_core1_arm_running` before RUNNING is published) and only then reseed the EMA. Counted as flush, not resync.
- Verification: hardware — first `a2dp:` report after "stream started" shows `resync_events=0` and `fill` within ±1 tick of target; log the measured START RTT once per stream.
- Related: beads `pico-link-pbv` (C2-3), `pico-link-fhf`, `pico-link-quzf` (Test C); design `2026-09-23-core1-resync-trim.md`.

### F-audio-07: `a2dp.c` is five modules and 55 % bead archaeology
- Severity: P2   Confidence: High   Effort: L   Tier: Opus (moves IRQ/core1-owned state across files)
- Location: `firmware/src/a2dp.c` (4492 lines: 2478 comment, 256 blank, 1758 code; 274 `pico-link-` bead references)
- Evidence: five cohesive regions with distinct ownership: (1) AVRCP target/controller/volume service, `:1147-1400` — file statics only, zero `s_ctx` coupling; (2) OUT-meter accumulate + seqlock + `poll_levels`, `:934-1110` — touches three `s_ctx` counters; (3) the drain: tx ring, fill, seal, send, credit, resync, core1 section, `:1440-2560`; (4) signalling: negotiation, retry/wizard timers, the 700-line packet handler, init, `:2849-4050`; (5) getters, report, publish, `:4090-4492`. The report function alone is 300 lines of `pl_log`.
- Why it matters: a maintainability cost, not a defect: every change to the hot path is reviewed against a file where 3 of 5 lines are history, and the AVRCP and levels code share nothing with the encoder they sit between. It is the single file every audio bead touches, so merge conflicts and review load concentrate here.
- Fix sketch: (a) `a2dp_avrcp.c` first — pure extraction, `s_avrcp_*` statics move verbatim, one exported `pl_a2dp_avrcp_init()`; (b) `a2dp_levels.c` with three counter pointers or its own counters; (c) `a2dp_internal.h` exposing `s_ctx`'s type and the three tx/fill entry points so (d) `a2dp_drain.c` (+ core1 section) and `a2dp_signaling.c` can split. Move bead history into `.planning/design/` cross-references and keep only the *invariant* in the comment ("no pl_log here: pico-link-0d2"), which cuts the file roughly in half without losing a decision.
- Verification: `wc -l` per file ≤ ~1200; firmware cross-compiles both `PL_ENCODER_ON_CORE1` configs; `pl_a2dp_report` output byte-identical on a soak.
- Related: CLAUDE.md "no LDAC-or-SBC branch" ruling (see F-audio-09h).

### F-audio-08: The FIFO-loss instrument and the SUPPLY LOW fault inherit F-audio-01's double count
- Severity: P2   Confidence: High   Effort: S   Tier: Sonnet
- Location: `firmware/src/usb_pump.c:557-561`; `firmware/src/fault.c:297-323`
- Evidence: `usb_lost_bytes = rx_bytes_total − pcm_bytes_total`, `supply_q8 = d_bytes·256/nominal` with `d_bytes` from `rx_bytes_total`. Both are correct only if each ISO packet increments `rx_bytes_total` exactly once, which F-audio-01 breaks on every full-FIFO deferral.
- Why it matters: the 9ziq success criterion "`usb_lost_bytes` delta 0" can read non-zero with no loss, and "USB SUPPLY LOW" reads ~healthy when the host is short by one packet per deferral. Fixed for free by F-audio-01; listed separately so the synthesis knows the instruments are downstream of it.
- Fix sketch: none beyond F-audio-01; optionally count `packet_count` in the ISR only (patch 04d) and make the stock task path's `pre_read_cb` a no-op for `ep_out`.
- Verification: same as F-audio-01(b).
- Related: `2026-09-23-usb-out-fifo-loss-off-build.md` success numbers 1 and 3.

### F-audio-09: Nits (batched)
- Severity: P3   Confidence: High unless stated   Effort: S each   Tier: Haiku unless stated
- Location / evidence / fix, one line each:
  - **a.** `pcm_ring.c:120` publishes `s_tail` after the data loads with no `__dmb()`, and `:45-83` reads `s_tail` then stores data with none; the tx ring has both halves (`a2dp.c:1940`). Correct on an in-order M33, not by the ARMv8-M memory model; add the two barriers for symmetry. (Tier Sonnet.)
  - **b.** `a2dp.c:4246-4248` resets `enc_count/enc_sum_us/enc_win_max_us` from core0 while core1 increments them — cross-core RMW on a diagnostic; snapshot-and-subtract instead of reset.
  - **c.** `a2dp.c:1924` writes `slot->data[0] = (uint8_t)slot->frames` unmasked; the AVDTP media header's `num_frames` is 4 bits. LDAC's hint clamps to 15, so safe today; mask `& 0x0F` so a future codec cannot set the fragmentation bits by accident.
  - **d.** `codec_ldac.c:539` `(void)out_cap` — safe only because every LDAC emission seals immediately (so `head->len` is always `header_bytes` when libldac writes). Add `if (out_cap < PL_LDAC_INIT_MTU) return !ok` so the invariant is checked, not assumed.
  - **e.** `a2dp.c:2676-2684`: PRIMING calls `a2dp_source_start_stream` on every ~10 ms tick until STREAM_STARTED; BTstack rejects the repeats (`COMMAND_DISALLOWED`) but latch a `start_requested` flag anyway.
  - **f.** After a sink-initiated `STREAM_SUSPENDED` (→IDLE) the media timer keeps running at 100 Hz forever (`timer_armed` only cleared at RELEASED / CONNECTION_RELEASED, `a2dp.c:3797-3900`).
  - **g.** `PL_A2DP_HOST_SILENT_TICKS` (`a2dp.c:271-274`, "200 ms / 10 ms tick = 20 ticks") is stale under ON: `silent_ticks` counts core1 polls (~100 k/s), so 20 is ~200 µs. The real 200 ms gate is `host_silent`; either count on core0 or rewrite the comment.
  - **h.** Codec identity leaks into `a2dp.c` despite the 2026-08-29 ruling: pointer compares `s_ctx.codec == &pl_codec_ldac` at `:4119` and `:4129`, and the ABR decide (`:2715-2760`) calls `pl_codec_ldac_is_adaptive/applied_rung/request_rung` directly rather than via a vtable slot. The ABR design sanctions the calls; the pointer compares it does not. (Tier Sonnet.)
  - **i.** Live pin→Adaptive via `pl_codec_ldac_pin_now` (`codec_ldac.c:314-334`) does not reset `s_ctx.abr_q_ema/abr_last_step_us` (ABR design §5.2's fourth reset event); the frozen EMA from the last adaptive period is used for ~16 ticks. Harmless; wire a `pl_a2dp_abr_reset()` or accept and document.
  - **j.** `s_ldac_abr_steps_down/up/rail_hits/apply_fail` (`codec_ldac.c:151-154`) are plain `uint32_t` written on core1 and read on core0 while every sibling is `volatile`.
  - **k.** `test_paired_device_upserted_ldac_quality_echo.c`'s build line needs `firmware/include/pico_link_ui.h`, which only exists after CMake runs cbindgen; document `cbindgen --config cbindgen.toml --crate ui-ffi --output …` in the header. All eight test headers emit `-Wcomment` from their own backslash-continued example commands.
  - **l.** (Confidence Low) `A2DP_SUBEVENT_STREAM_STARTED` arriving from IDLE (sink-initiated resume after a sink-initiated suspend) starts STREAMING with an empty ring and no PRIMING; the 500 ppm loop then needs ~60 s to build a cushion. Consider transitioning IDLE→PRIMING on such a start instead.
- Verification: each is a one-site edit; existing host tests plus a firmware cross-compile of both `PL_ENCODER_ON_CORE1` configs.

## 5. Test coverage

Covered on the host: PCM ring push/read/wrap integrity; tx ring count arithmetic; codec-id lookup stability; LDAC frames-per-packet model; priming-cushion arithmetic; ABR controller model (rails, convergence, limit-cycle bound); fault evaluator (gating, cadence, dynamic severity); persist echo of `ldac_quality`. All are **models copied from the source** (constants and small functions re-declared in the test), not the compiled firmware functions — a divergence between the copy and `a2dp.c` is invisible. Only `test_pcm_ring_cross_core.c` and `test_codec_id_stability.c` link real source files.

Not covered anywhere runnable here: the USB feedback loop (F-audio-03 — the cheapest and most valuable addition: copy the 40-line tick into a host test and assert damping), `pl_a2dp_fill`'s stop-order/seal logic, `pl_a2dp_accrue_credit`'s clamp, the resync decide/apply/complete sequence, the ISR patch bodies, and everything in §7 below.

What a cheap model could add safely: (1) `test_usb_feedback_loop.c` per F-audio-03; (2) extend the priming-cushion test with the F-audio-02 MTU check; (3) a `test_a2dp_fill_model.c` that copies the loop's stop conditions and proves the inner queue-full guards are unreachable and that `head->len` is always `header_bytes` when a self-packetising codec emits (F-audio-09d); (4) a `tools/apply-sdk-patches.sh --check` (F-audio-04).

## 6. Open questions for Andreas

1. **Second sink.** F-audio-02 is only a real defect if some headset negotiates a media payload < 680 bytes. Do you have (or plan to buy) a second LDAC sink? If yes, the check should land before it is paired; if the product is single-sink for the foreseeable future it can wait.
2. **Sustained-undersupply mute (2ap regime B).** The design is written, nothing is built, and the fault strip took a different route. Keep the doc as a live plan or mark it superseded? (Affects only how F-audio-03's integrator fix is scoped.)
3. **Latency vs. start-of-stream skip (F-audio-06).** The proposed trim at STREAM_STARTED drops the first ~RTT of the track deliberately instead of carrying it as permanent latency. Your call which you prefer to hear.

## 7. Unverifiable here (needs the board or the SDK checkout)

- That the applied `audio_device.c` in `~/.pico-sdk/sdk/2.1.1` matches `PATCHED4D`/`PATCHED6` byte for byte, and that stock `audiod_rx_done_cb` still has `TU_VERIFY(tu_fifo_write_n(...))` before the re-arm (F-audio-01's second half). Medium confidence from memory of 0.18.0 and the project's own prior citation.
- The real AVDTP START round-trip on the WH-1000XM headset (F-audio-06).
- The actual `max_media_payload_size` per sink (F-audio-02).
- Whether TinyUSB upstream (≥0.19) has since fixed the full-FIFO/no-re-arm path: **unknown**.
- The tx-ring and pcm-ring barrier arguments assume RP2350's documented coherent SRAM with no data cache; the code and ADR state it, the board would confirm it.
- The core1 spin's measured contention cost after libldac moved to SRAM (F-audio-05's benefit size).
