# Cushioning audio against congested 2.4GHz air

Bead `pico-link-8pp1`. Ada, 2026-09-24. Desk design from source at `99b6d34`; no
hardware this pass. Every claim is tagged **[E]** established (code read or banked
measurement) or **[I]** inference.

## 1. Where audio is lost under congestion today

The buffering chain, sink-ward: USB -> PCM ring (32KB = 170ms, target 30ms) ->
LDAC encoder (core1, ~834us per 2.67ms frame, ~3.2x real time) -> tx queue
(7 usable slots; HQ packs 2 frames/packet, so 7 x 5.33ms = 37ms at HQ, ~112ms at
MQ) -> BTstack/L2CAP -> CYW43 controller ACL buffers -> air -> sink jitter buffer.

- **[E] The controller never drops.** BTstack only sets an automatic flush
  timeout via its setter (`hci.c:5237`); the firmware never calls it, so ACL
  packets are retried until delivered or the link supervision timeout. Congestion
  shows up as a *stall* (no `CAN_SEND_MEDIA_PACKET_NOW` grants), not as air loss.
- **[E] A stall backs up into the PCM ring, not the encoder.** Once the tx queue
  is at 7, `pl_a2dp_fill` stops with `stop_queue_full` (a2dp.c:1628) while credit
  keeps accruing; PCM accumulates in the ring.
- **[E] The resync trim is where our side discards audio.** When `fill_ema`
  exceeds target+15ms (2s lockout), decide/apply/complete cuts the ring to target
  (a2dp.c:1997-2076), counted in `resync_drops`/`resync_events`. Measured on
  hardware in the `pico-link-rzqd` log: 4 trims following a foreign-page tx stall.
  Fix B (owed credit debited) closed the follow-on collapse.
- **[E] The trim fires almost immediately after a stall longer than ~15ms.** The
  feedback EMA is shift-6 (`usb_audio.c:461`, tau ~64 samples), so it tracks a
  stall-sized step within tens of ms; the trim does not give the radio any time to
  drain the backlog first.
- **[I] Drop-newest overflow** (`ovr_frames`) needs raw fill > 170ms before the
  trim catches it: a single stall of >~140ms. Plausible for a page train, never
  attributed in a banked log.
- **[I] `pkt_fail`** (encoded packet discarded on a failed send) should stay 0 —
  sends are grant-gated.
- **[I] The audible loss is mostly at the sink.** A stall longer than the sink's
  jitter buffer B underruns it (gap). A shorter stall is inaudible *if* we later
  burst the backlog in; today's trim instead throws that backlog away, so the sink
  runs at B-T until it rebuffers and the next stall gaps sooner. The trim trades
  a skip now for freshness; it never restores the sink's cushion.
- **[I] Catch-up headroom is the real limit, not the headset's acceptance of a
  burst.** Draining a backlog T takes T/(h-1) where h = link capacity / codec
  rate. The 679B MTU implies 2-DH5 (~1.4Mbps): h ~1.4 at HQ in clean air, <1 in
  congestion (it never drains); h ~4 at MQ. The sink's baseband acks independently
  of its jitter buffer (L2CAP basic mode, no flow control), so if a burst
  overflows the sink it is dropped invisibly there — judge by ear only.
- **[E] Instrument gaps:** no stall-duration counter (grant gap), no per-trim
  size, no fill_ema peak, and AVDTP delay reports are received and ignored
  (a2dp.c:3204) — the sink's own stated buffer is sitting unread.

## 2. Options

| Option | Verdict |
|---|---|
| A. Backlog tolerance before trim (setting) | **Yes.** Pre-encoder, rate-agnostic, reuses the trim machinery; costs latency only transiently after a stall. |
| B. Post-encoder packet queue with catch-up | **No — the quick fix.** Commits bitrate at encode time (ABR cannot help queued packets), duplicates buffering, costs ~700B SRAM/slot, and the encoder is not the bottleneck (3.2x real time) — the radio is. |
| C. LDAC ABR | **Already built** (7jol, Adaptive only). It is the thing that creates headroom h>1 so a held backlog can drain. It does not prevent stalls. |
| **A + C** | **Recommended.** A holds the backlog long enough; C drains it. In pinned HQ, A only delays the trim (the backlog cannot drain in congestion) — say so in the UI copy. |

Separately **[I]**: the dongle stays page-scannable while streaming, and the
09-23 stalls were a foreign device paging it. Turning page scan off while
streaming would remove one stall source outright. Out of scope here (Andreas is
troubleshooting the tx stall himself); flagged as a follow-up.

## 3. The mechanism: a hold timer, not a bigger target

Raising the target fill buys nothing against radio stalls (the ring grows during
a stall regardless) and adds permanent latency. What resilience needs is *time*
before the trim gives up. Replace the single trip with two:

- **soft trip:** fill_ema > target + 15ms (today's band) **continuously for
  `hold_ms`**;
- **hard trip:** fill_ema > target + `hard_band_ms` (overflow guard, immediate).

Trim-to stays `target` (one clean cut back to fresh). Low latency = `hold 0,
hard 15ms`, i.e. bit-identical to today. Resilient = `hold 3000ms, hard 70ms`.
The hard band is capped by the ring: target 30 + hard 70 + ~64ms EMA ramp lag
~= 164ms < 170ms, so drop-newest stays unreachable at 32KB. A larger hard band
needs a 64KB ring (+32KB SRAM, contested by libldac-in-SRAM and the coming
effects DSP) — only if the round shows stalls >70ms are common and drainable.

In the steady state latency is unchanged (30ms). After a stall it is +T for at
most `hold_ms`, then either drained (ABR/headroom) or trimmed.

### Invariants
- `hold_ms`/`hard_band` and the hold-start timestamp are **core0-private**, read
  only by `pl_a2dp_resync_decide`. APPLY and COMPLETE are untouched; no new
  cross-core word; nothing moves into the IRQ. Core1 code unchanged, so no
  libldac-in-SRAM impact.
- `pl_pcm_target_fill_bytes` is **not** touched (still written once at
  STREAM_ESTABLISHED) — its "stable while core1 runs" contract holds.
- The policy is one core0 struct written by the command handler and read by the
  media timer, both on core0 in the same async_context; aligned words, no lock.
  Takes effect live.
- The 2s min-interval lockout stays.
- Check during implementation: `fb_rail_ticks` will rise during a hold (the
  feedback loop pushes toward target at rail). Confirm no fault-strip key is
  driven by it, or exclude hold time.

## 4. Setting and persistence

- Global, not per-device (Andreas's call; recommend global — the air, not the
  headset, is what varies). Two values: **Low latency** / **Stable** (Uma names
  them). Wire: 1 byte, 0 = unset -> default, 1 = low, 2 = stable.
- **New record `PL:S:1`** (kind `PL_PERSIST_KIND_SETTINGS`, index 1), own
  version byte, loaded at boot independently of the device store — same shape as
  `PL:S:0` (`pl_persist_boot_display_settings` / `_request_` /
  `_execute_pending_`). Do not bump `PL:S:0`'s version: that would reset the
  screensaver prefs.
- Core: an `AudioSettings` model with `from_wire` per-field fallback, a Settings
  row plus the existing single-select picker, `PL_COMMAND_TAG_SET_AUDIO_SETTINGS`
  out over the FFI; `main.c` passes the boot value in, as for display settings.
- User-initiated write is not gated on streaming (D11 precedent).

## 5. Measurement round (one round, Tess, one agent on the board)

**Build:** branch with bead S1 (below), PL_DEBUG_REMOTE=ON, core1 default. The
policy is switchable live over the CDC debug channel, so arms A1..B2 need no
reflash and share the RF environment.

**Source:** one continuous 60-min file (e.g. `sox` pink noise or music), played
once, never looped. Validate duty >= 99% (USB rx bytes vs 192000 B/s) before
reading any ratio.

**Congestion:** (a) deterministic stall injection: a second Bluetooth device
(the Mac's own radio is simplest) pages the dongle every 30s, which reproduces
the 09-23 foreign-page stall; (b) if Andreas has a 2.4GHz AP, sustained iperf3
load near the dongle for the "real air" arms. Headphones (94:DB:56:54:7C:F2) at
a fixed ~3m position throughout.

**Arms (10 min each, interleaved):** A0 main HQ-pinned (control) -> A1 branch
low-latency HQ -> B1 branch stable HQ -> A2 branch low-latency Adaptive ->
B2 branch stable Adaptive -> B1 -> A1 (repeat tail catches RF drift). A0 vs A1
must agree within noise, which proves the instrument is inert.

**Per-arm deltas** (parse per block — the console is not line-atomic):
stall episodes and max grant gap, stall histogram (>20/50/100ms),
`stop_queue_full`, `resync_events`/`resync_drops`, max single trim, `ovr_frames`,
`pkt_fail`, `und`/`starved_us`, `fill_ema` peak, time from stall end back inside
the soft band, ABR steps, `fb_rail_ticks`, last sink delay report. Andreas's ear
optional: tally gaps vs skips.

**Validity:** an arm with zero recorded stall episodes did not test anything —
discard it, do not read it as a pass.

**Decision bars:** Stable is worth shipping if B2 shows `resync_events` <= 25%
of A2, `ovr_frames` 0, `und` not higher, and drains within `hold_ms`. If B1
(pinned HQ) never drains, Stable only helps with Adaptive and the UI copy says
so. The stall length where gaps start, together with the delay report, gives the
sink's buffer B empirically.

## 6. Implementation beads (one Sonnet implementer each)

- **S1 (Ruby, firmware):** instrumentation (grant-gap stall episodes, max gap,
  histogram, fill_ema peak, max single trim, store and print the sink delay
  report) **plus** the hold/hard-band policy in `pl_a2dp_resync_decide`, with
  defaults bit-identical to today, and a PL_DEBUG_REMOTE CDC command to set it.
  Gate: host tests, firmware cross-compile, and resync behaviour unchanged at
  defaults.
- **S2 (Tess, hardware):** the round in section 5. Reports numbers and a
  recommendation for `hold_ms`/`hard_band`.
- **S3 (Ruby, firmware):** `PL:S:1` persistence plus the
  `PL_COMMAND_TAG_SET_AUDIO_SETTINGS` handler, boot load into the policy.
  Depends on S2 (values).
- **S4 (Uma, then Ruby, core):** name and copy for the Settings row (Uma, small),
  then the `AudioSettings` model, Settings row and picker, FFI command, and
  headless screenshots. Depends on S3.
- **Follow-up, separate:** page scan off while streaming (tx-stall source), and
  `pico-link-dge6` before relying on ABR step-up (**[I]** the manual raise bug may
  share the upward rung-apply path).

## 7. Product calls for Andreas

1. **Default:** recommend **Low latency** (today's behaviour) until S2 shows
   Stable cuts audible events; then flip it. Stable's steady-state latency is
   identical, so this is a smaller trade against the meetings concern than it
   sounds.
2. **Global vs per-device:** recommend global.
3. **Ring growth to 64KB** only if S2 shows >70ms stalls that do drain.
