# M4 — A2DP source pipeline (bead pico-link-cz0.5)

**Author:** Ada (architect) · **Date:** 2026-08-29 · **Status:** design of record for M4
**Depends on:** `.planning/design/2026-08-29-usb-audio-alt1.md` §3 (async + explicit
feedback and its stated M4 consequence), `.planning/decisions/2026-08-27-c-first-pico-sdk-owns-main.md`,
`.planning/design/2026-08-28-on-device-ui.md` §7 (codec hero) and §11 (codec picker).

## 0. What this bead is

Get audio out of the dongle and into real headphones. PCM arriving from the UAC2
isochronous OUT endpoint (proven in M3) is buffered, encoded, and pushed over A2DP
to a paired sink, and the panel names the codec in use. **SBC first — it is mandatory
in the A2DP spec and every sink has it. LDAC after.** Andreas's ruling 2026-08-29: no
LDAC-or-SBC branch anywhere; a codec **table**, so AAC later is a new row plus an
encoder, not surgery.

## 1. Clock topology

Three clock domains touch this pipeline. Naming them wrongly is how you build the
wrong buffer.

| Domain | Rate source | Observable to us? |
|---|---|---|
| USB host | macOS's crystal, as SOF + samples per ISO packet | Yes — indirectly, as ring fill |
| Us (RP2350) | the board's 12 MHz crystal, via `btstack_run_loop` timers | Yes — it *is* our reference |
| Headphone DAC | the sink's own crystal | **No.** A2DP is push, with no rate-feedback channel |

```
USB host  --(ISO OUT, host-paced)-->  PCM ring  --(local-timer-paced)-->  encoder --> A2DP --> sink
    ^                                    |
    +----------- explicit feedback ---- fill level
```

The consumer side is time-paced against our own crystal, exactly as
`a2dp_source_demo.c` paces its synthetic source (`a2dp_demo_audio_timeout_handler`,
10 ms `AUDIO_TIMEOUT_MS`). The A2DP media stream has an intrinsic rate the sink
expects — 48000/128 = 375 SBC frames/s — and something must produce it. That
something is our timer.

**The RP2350 crystal is the reference; the feedback endpoint pulls the host onto it;
residual host-vs-sink drift is absorbed by the sink**, as for every A2DP source ever
built. That is not a compromise, it is what A2DP is.

A tempting alternative — "drain whatever is in the ring, let the send be data-driven"
— is **wrong and must not be built**. With a data-driven drain the ring has no
equilibrium fill (it empties whenever the radio can send), the feedback controller has
no setpoint, and it would command the host to maximum rate forever. Named here so
nobody rediscovers it.

We stay **asynchronous with explicit feedback**. Nothing here proposes adaptive.

## 2. The interaction with M3's feedback loop — a hard dependency, not a follow-up

`usb_audio.c:215-220` requests `AUDIO_FEEDBACK_METHOD_FIFO_COUNT`: TinyUSB derives
feedback from the fill of its own 784-byte ISO-OUT software FIFO. But
`pl_usb_audio_task()` drains that FIFO **to empty every millisecond**. TinyUSB
therefore sees a permanently-empty FIFO and commands the host toward maximum
deviation, continuously, today.

Harmless in M3 (nothing downstream). **Fatal the moment a real consumer exists**: the
host runs fast, our ring fills monotonically, and it overruns within seconds.

So **replacing the feedback source ships in the same change that adds the consumer.**
M3's own design already said so (§3, "Consequence for M4, stated plainly").

**Free check available before any M4 code lands:** M3's once-a-second
`usb-audio: ... measured=N B/s` line should read *slightly above* 192000 B/s, not
exactly 192000. If it reads bang on nominal, the host is ignoring our feedback and the
endpoint is decorative — which would change M4's plan materially.

### 2.1 The replacement loop

```c
// usb_audio.c
void tud_audio_feedback_params_cb(uint8_t func_id, uint8_t alt_itf, audio_feedback_params_t *p) {
    p->method      = AUDIO_FEEDBACK_METHOD_DISABLED;  // we compute it
    p->sample_freq = current_sample_rate;
}
```

and, in the 0xC0 worker after the drain (we are already there; ring fill is two loads
and a mask):

```c
#define PL_FB_NOMINAL_Q16   (48u << 16)   // 48.0 samples/frame, 16.16
#define PL_FB_MAX_PPM       500           // +/- 0.05 %

fill_ema += ((int32_t)pl_pcm_ring_fill_bytes() - fill_ema) >> 6;   // ~64 ms EMA
int32_t err_bytes = fill_ema - (int32_t)PL_PCM_TARGET_FILL_BYTES;
int32_t ppm = -(err_bytes * PL_FB_MAX_PPM) / (int32_t)PL_PCM_TARGET_FILL_BYTES;
if (ppm >  PL_FB_MAX_PPM) ppm =  PL_FB_MAX_PPM;
if (ppm < -PL_FB_MAX_PPM) ppm = -PL_FB_MAX_PPM;
tud_audio_fb_set((uint32_t)((int32_t)PL_FB_NOMINAL_Q16 +
                 (int32_t)((int64_t)PL_FB_NOMINAL_Q16 * ppm / 1000000)));
```

Proportional only, deliberately. The plant is a pure integrator (fill = the integral
of rate error), so P-only is stable and needs no anti-windup.

**Why ±500 ppm.** Two decent crystals differ by well under ±100 ppm. At 50 ppm the
fill drifts at 192 B/ms × 5e-5 = 0.0096 B/ms — about 4.6 KB per 8 minutes. Our
authority is 10x the disturbance, so the loop converges. It is *deliberately far too
slow to correct a transient* (a 4.6 KB error at full authority takes ~48 s):
**transients are absorbed by ring capacity, sustained drift by the controller.** Two
different jobs; conflating them gives an oscillating buffer.

**Why the EMA.** The consumer drains in 512-byte steps several times per 10 ms tick,
so raw fill sawtooths by roughly one tick's worth (~1.9 KB) — comparable to the target
itself. Raw fill into a P controller gives ~±200 ppm of 100 Hz ripple.

**CORRECTION (bead pico-link-pbv round 2 / pico-link-6vv, 2026-08-29):** the claim above
is WRONG for pico-sdk 2.1.1's vendored TinyUSB. `tud_audio_n_fb_set`
(`lib/tinyusb/src/class/audio/audio_device.c:2347-2358`) does not merely store a value --
its only guard is `p_desc != NULL` (true from `SET_CONFIGURATION` onward), and it then
calls `usbd_edpt_claim(rhport, audio->ep_fb)` and `audiod_fb_send()` ->
`usbd_edpt_xfer(rhport, audio->ep_fb, fb_buf, 3)`. `audio->ep_fb` is `0` until the
streaming alt-setting (alt 1) is selected and is explicitly reset to `0` on alt 0
(`audio_device.c:1837`); neither `usbd_edpt_claim` nor `usbd_edpt_xfer` guards
`epnum != 0`. So calling `tud_audio_fb_set()` while alt 0 is selected claims **EP0-OUT**,
marks it busy, and queues a 3-byte OUT transfer on the **control endpoint** roughly a
thousand times a second -- a plausible mechanism for the "EP0 half-open, board goes deaf"
wedge this project already spent a session chasing. The caller (`usb_pump.c`'s 0xC0
worker) MUST gate this call on `pl_usb_audio_streaming()` being true. See
`pico-link-6vv`.

## 3. The PCM ring

### 3.1 What is wrong with the one we have

`usb_pump.c:52-79` already has an SPSC ring, written in M3 with no consumer. Three
things must change before it carries audio:

1. **It drops individual BYTES on overflow** (`s_pcm_ring_drop_count++; continue;`).
   PCM here is 4-byte interleaved stereo frames. Dropping 1-3 bytes permanently swaps
   L/R and misaligns every subsequent sample — silent, permanent corruption that
   sounds like "the codec is broken". **Drops must be whole frames.**
2. **Byte-at-a-time copy on both sides**, ~192 + ~192 iterations per ms. Modest, but
   block copy is what makes a frame-granular ring natural.
3. **It lives in `usb_pump.c`.** The ring is the USB/Bluetooth seam; leaving it there
   forces the A2DP module to include `usb_pump.h` to get audio — a dependency pointing
   the wrong way through the seam. Move to `firmware/src/pcm_ring.{c,h}`, owned by
   neither side.

None are load-bearing hacks *yet* — which is exactly why to fix them now, while it is
a 100-line diff.

### 3.2 The design

```c
// firmware/src/pcm_ring.h
//
// The USB <-> Bluetooth seam. Producer: usb_audio.c's pl_usb_audio_task(), in the
// 0xC0 worker IRQ. Consumer: a2dp.c's media timer, in the cyw43/BTstack background
// IRQ (0xFF, lowest).
//
// SPSC and lock-free: each side writes only its own index, both indices are single
// aligned 32-bit words, so a 0xC0 preemption of the 0xFF consumer mid-read is safe.
// Same discipline input.c uses -- NOT bt.c's, which needed a critical section only
// because it has two producers.
//
// SRAM, not PSRAM. See sec 3.4.

#define PL_PCM_FRAME_BYTES        4u              // 16-bit stereo
#define PL_PCM_RING_CAPACITY      (32u * 1024u)   // power of two; 8192 frames = 170.7 ms
#define PL_PCM_TARGET_FILL_BYTES  (4608u)         // 1152 frames = 24 ms  <-- TUNABLE, sec 3.5

// Appends whole frames only. `len` MUST be a multiple of PL_PCM_FRAME_BYTES; a
// misaligned length is rejected wholesale and counted, never partially accepted.
// On overflow drops the NEWEST whole frames and counts them -- the producer must
// never touch `tail`.
void     pl_pcm_push(const uint8_t *data, uint32_t len);
uint32_t pl_pcm_read(uint8_t *out, uint32_t max);   // always a whole number of frames
uint32_t pl_pcm_fill_bytes(void);
void     pl_pcm_reset(void);                        // stream open/close only, consumer side
uint32_t pl_pcm_overrun_frames(void);
uint32_t pl_pcm_misaligned(void);
```

Two `memcpy`s (to the wrap and after it), power-of-two mask indexing, capacity-1
usable so full is distinguishable from empty (same convention as `input.c` and `bt.c`).

### 3.3 Sizing

48 kHz x 2 ch x 2 B = **192 B/ms**.

| Quantity | Value | Why |
|---|---|---|
| Capacity | 32 KiB = 170.7 ms | Target + transient headroom; SRAM cost negligible |
| Target fill | 4608 B = 24 ms | Covers one media packet plus radio scheduling jitter |
| One SBC frame (48k, 8 subbands x 16 blocks) | 128 PCM frames = 512 B in, ~119 B out, 2.67 ms | Fixes drain granularity |
| One media packet (~670 B L2CAP payload) | ~5 SBC frames = 13.3 ms | Fixes the minimum sensible target |
| Headroom above target | 146 ms | Absorbs a stalled radio |
| Headroom below target | 24 ms | Absorbs a late media tick |

**A 24 ms target is 24 ms of added end-to-end latency** on top of A2DP's inherent
~150-200 ms. That is the price; do not silently raise it to "be safe".

### 3.4 PSRAM: no

The board has 8 MB of PSRAM. **The audio path must not use it.**

- The working set is 32 KB against 520 KB of SRAM. Not a capacity problem.
- PSRAM is reached through the QMI/XIP path. An IRQ-context write there has cache-
  and arbitration-dependent latency and contends with XIP instruction fetch when code
  runs from flash. `usb_pump.h:80-83` already banked this reasoning for the M3 ring.
- The producer runs against a hard ~1 ms re-arm deadline that has already cost this
  project a wedged board once. A variable-latency store there to save 32 KB of a
  520 KB budget is a bad trade in both directions.

**Corollary as a rule:** if this pipeline ever appears to need >100 KB of buffering,
that is a scheduling defect being papered over with memory, and the fix is upstream of
the buffer. PSRAM stays for things that are actually large.

*Confirm at link time:* `arm-none-eabi-size` on the `.elf` before and after; check the
32 KB fits alongside the Rust arena, the 115 KB framebuffer inside it, and
`PICO_STACK_SIZE=0xC000` (48 KB).

### 3.5 Underrun and overrun policy

**Overrun** (producer finds the ring full). Drop the newest whole frames; count them.
The producer owns only `head` — advancing `tail` to drop-oldest is a cross-index write
into the consumer's variable and is forbidden. Once the feedback loop works, overrun
should be *structurally impossible*; therefore **any nonzero overrun count is a bug
signal, not a tuning signal**, and must be loud (sec 7).

**Underrun** — three distinct cases, three different answers:

1. **Transient** (< one media packet short, alt 1 active). Encode fewer frames; send a
   shorter packet or none. Count `underrun_events`. **Do not insert silence.** Padding
   makes the glitch inaudible-ish *and invisible*, which is how M3's "0 packets"
   reading nearly got read as a pass.
2. **Host silent** (alt 0, or alt 1 with no packets for > 200 ms). Not a fault — the
   user paused. `a2dp_source_pause_stream()`, reset the ring, report bitrate as
   **"idle"**, per the UI design's rule *never `0 kbps`*.
3. **Resume.** On alt 1 returning, **prime** — hold the media stream until
   fill >= `PL_PCM_TARGET_FILL_BYTES`, then `a2dp_source_start_stream()`. Starting an
   empty pipeline guarantees an underrun burst in the first second, precisely when a
   user is listening for whether it worked.

Also clamp `samples_ready` to one packet's worth. `a2dp_source_demo.c` accrues it from
elapsed time unbounded; across a long silence that becomes a huge backlog the pipeline
then tries to "catch up" on. We replace that accounting anyway — just don't reproduce
the latent bug.

## 4. The codec table

### 4.1 What BTstack already does — decisive for staging

From `a2dp.c:591-626`: with `ENABLE_A2DP_EXPLICIT_CONFIG` **not** defined, BTstack
auto-selects the codec itself, and its auto-selection is **hardcoded to SBC**. It calls
`avdtp_get_source_stream_endpoint_for_media_codec_and_type(AVDTP_CODEC_SBC, ...)`,
chooses parameters via `avdtp_choose_sbc_*`, and configures the stream. AAC/ATRAC/
vendor capabilities are forwarded to the app and otherwise ignored.

- **Stage 1 (SBC) needs no negotiation code at all.** Register one SBC endpoint, let
  BTstack's implicit path configure it, react to
  `A2DP_SUBEVENT_SIGNALING_MEDIA_CODEC_SBC_CONFIGURATION`.
- **Stage 4 (LDAC) defines `ENABLE_A2DP_EXPLICIT_CONFIG`**, switching the implicit path
  off entirely; our table-driven selection becomes the thing that runs.
  `a2dp_source_set_config_other()` (`a2dp_source.c:237`) is LDAC's hook;
  `a2dp_source_set_config_mpeg_aac()` (`:229`) is AAC's.

**Named regression risk:** enabling `ENABLE_A2DP_EXPLICIT_CONFIG` changes SBC's path
too — we must then configure SBC ourselves. **Stage 4 must re-prove Stage 1's "SBC
audible" result**, not just "LDAC audible". Put that in the stage-4 bead's acceptance.

The table exists from Stage 1 with one row — that is where the SBC row's capability
bytes, SEID and encoder vtable live regardless.

### 4.2 The structures

```c
// firmware/src/codec_table.h

typedef struct { uint32_t sample_rate_hz; uint8_t channels; uint8_t bits_per_sample; } pl_codec_format_t;

typedef struct {
    uint16_t pcm_frames_per_encoded_frame;  // SBC 48k/8sb/16blk -> 128
    uint16_t encoded_frame_bytes;           // 0 == variable, query per-encode
    uint32_t nominal_bitrate_bps;           // what the panel shows
} pl_codec_frame_info_t;

struct pl_codec {
    /* identity */
    const char *display_name;        // "SBC" / "LDAC" / "AAC". EXACTLY the panel string.
    uint8_t     avdtp_codec_type;    // AVDTP_CODEC_SBC | ..._MPEG_2_4_AAC | ..._NON_A2DP
    uint32_t    vendor_id;           // vendor-specific only (LDAC 0x0000012D), else 0
    uint16_t    vendor_codec_id;     // vendor-specific only (LDAC 0x00AA), else 0

    /* negotiation */
    uint8_t        preference;       // lower tried first; table sorted by this
    const uint8_t *capabilities;     // AVDTP media codec capability bytes we advertise
    uint8_t        capabilities_len;
    uint8_t       *configuration;    // writable; BTstack fills with the negotiated config
    uint8_t        configuration_len;

    /* assigned at init */
    uint8_t     local_seid;          // from a2dp_source_create_stream_endpoint()

    /* encoder vtable */
    // Returns false if this build cannot honour the config -> caller falls through
    // to the next table row.
    bool     (*init)(void *state, const uint8_t *configuration, uint8_t configuration_len,
                     pl_codec_format_t *out_format, pl_codec_frame_info_t *out_frame);
    // Encodes exactly out_frame.pcm_frames_per_encoded_frame interleaved int16 stereo
    // frames into `out`; returns bytes written, 0 on failure.
    // CONTRACT: no allocation, no logging, no blocking, no Rust. IRQ context. Sec 5.
    uint16_t (*encode)(void *state, const int16_t *pcm, uint8_t *out, uint16_t out_cap);
    void     (*deinit)(void *state);
    void      *state;                // statically allocated per codec, never malloc'd
};

// Preference order. SBC is LAST and ALWAYS PRESENT: the A2DP spec mandates it of
// every implementation, so it is the floor the fallback chain terminates on.
extern pl_codec_t *const PL_CODECS[];
extern const size_t PL_CODEC_COUNT;
```

Stage 1's whole table is `{ &pl_codec_sbc }`. Stage 4 prepends `&pl_codec_ldac`. AAC
would prepend `&pl_codec_aac`. **No call site anywhere switches on codec identity** —
the reviewer checks this by grepping for `AVDTP_CODEC_SBC` / `"SBC"` outside
`codec_sbc.c` and the table.

### 4.3 Negotiation flow

Stage 1 (implicit — BTstack drives):

```
Command::Connect(addr)
  -> a2dp_source_establish_stream(addr, &a2dp_cid)
  -> [BTstack: SDP, AVDTP discover, capabilities, auto-select SBC, set config]
  -> A2DP_SUBEVENT_SIGNALING_MEDIA_CODEC_SBC_CONFIGURATION
       find table row by local_seid -> row->init(...) -> record active codec
       push Event::ConnectStepChanged(NegotiatingCodec)
  -> A2DP_SUBEVENT_STREAM_ESTABLISHED   -> prime, then a2dp_source_start_stream()
  -> A2DP_SUBEVENT_STREAM_STARTED       -> start media timer
       push Event::CodecChanged{ name, bitrate } + Event::ConnectSucceeded{ degraded }
  -> A2DP_SUBEVENT_STREAMING_CAN_SEND_MEDIA_PACKET_NOW -> send
```

Stage 4 (explicit — we drive), on `A2DP_SUBEVENT_SIGNALING_CAPABILITIES_DONE`:

```
for row in PL_CODECS (preference order):
    if remote advertised a SEP matching row->avdtp_codec_type
       (and, for NON_A2DP, row->vendor_id/vendor_codec_id):
        if row->init() would accept the intersection:
            a2dp_source_set_config_{sbc,mpeg_aac,other}(cid, row->local_seid, remote_seid, cfg)
            active_codec = row; break
// Cannot fall off the end: SBC is mandatory on every sink. If it somehow does, that
// is ConnectFailureReason::NoA2dpSink -- already representable in the FFI.
```

`degraded` (the UI design's amber hero) = **the selected row is not the first row we
would have accepted**. At Stage 1 the table has one row, so SBC is *not* degraded and
renders in `TEXT_PRIMARY`. It turns amber only once a preferred codec exists and was
not obtained. Backwards, this shows a permanent false alarm on day one.

Remote SEP capabilities must be captured as they arrive (`..._SBC_CAPABILITY`,
`..._MPEG_AAC_CAPABILITY`, `..._OTHER_CAPABILITY`) into a small fixed array indexed by
remote SEID — `CAPABILITIES_DONE` carries none of it.

## 5. Where encoding runs, and why the 1 ms budget is safe

**In the BTstack run-loop timer handler**, i.e. the cyw43 background IRQ at
`PICO_LOWEST_IRQ_PRIORITY` (**0xFF**), the priority `usb_pump.h:20-24` documents.

The safety argument is **structural, not budgetary**:

```
0x80  USBCTRL_IRQ             (TinyUSB DCD)
0xC0  pl_usb_pump_worker_irq  -- tud_task(), ISO-OUT re-arm, ring push, feedback
0xFF  cyw43/BTstack background -- media timer, SBC encode, A2DP send   <-- new work here
```

The 0xC0 worker **preempts** the encoder. A long encode cannot delay the ISO-OUT
re-arm; it can only delay itself and other BT work. Worst case is degraded Bluetooth
throughput — visible as a media-send backlog, recoverable, not the endpoint-killing
failure mode.

Rules the encoder path must obey:

- **No `pl_log`, ever, in the media path.** `pl_log` takes `pl_usb_mutex`; the 0xC0
  worker's `mutex_try_enter` then fails and the worker **skips that tick entirely**
  (`usb_pump.c:97-99`). Enough skipped ticks is an ISO-OUT FIFO overflow, which
  permanently kills the endpoint. This is `pico-link-0d2` and it is the single most
  dangerous line anyone could add here. Counters only; the superloop prints (sec 7).
  `pl_log_locked` is **not** the escape hatch — it is only correct for callers already
  inside the worker's critical section, which the media path is not.
- **No Rust** (`pico-link-5am`). Encoders are pure C with static state.
- **No allocation, no blocking.**
- **Bounded work per pass.** Cap frames encoded per tick at one media packet's worth
  (~5 SBC frames). Never "catch up" unboundedly.

**Measurements required before declaring this safe** — do not guess:

| Measure | How | Bar |
|---|---|---|
| SBC encode us per frame | `time_us_64()` around `row->encode`, keep max, report from superloop | informational; expect low hundreds of us |
| Total encode us per media tick | same, per pass | < 5 ms (half the 10 ms tick) |
| Worker worst interval | **existing** `s_worst_interval_us` in `pl_usb_pump_report` | < 2 ms — the bar M3 passed at ~1.03 ms |

If LDAC later blows the per-tick budget, **core1 is the escape hatch and the SPSC ring
is what makes it cheap** — core1 is currently never launched. Do not build it now. Do
not design around it now. Just know it exists.

## 6. The FFI surface

### 6.1 One deviation from the bead, flagged

The bead specifies `pl_ui_set_codec(name, bitrate_bps)`. **The name-string-not-enum
part is exactly right** and is kept: a new codec surfaces on the panel with zero FFI
churn.

**But a new flat setter is a regression to the shape `pico-link-a67` deliberately
removed.** That bead replaced `pl_ui_set_link_state` / `pl_ui_add_device` /
`pl_ui_clear_devices` with one `PlEvent` union, and `ui-ffi/src/lib.rs:545-554` names
the exact failure mode: *"every new field the approved on-device UI design needs
(codec, bitrate, per-device codec availability + reason, volume, ...) would have been
another setter."* Codec is the literal first example in that list.

| | Quick path | Sustainable path |
|---|---|---|
| What | `pl_ui_set_codec(ui, name, len, bps)` | `PL_EVENT_TAG_CODEC_CHANGED` variant |
| Cost now | ~0 | ~30 lines: payload struct, tag, `From` impl, `BtModel` field |
| Cost later | Two event paths forever. Setters bypass `bt.c`'s MPSC ring, so a codec change arriving in BTstack IRQ context calls straight into Rust — reopening `pico-link-6o2`, fixed 2026-08-28 | Rides the existing ring; ABI-additive, no `PL_EVENT_ABI_VERSION` bump |

The second row is decisive and is not a style argument: the codec configuration event
arrives in `a2dp_source_packet_handler`, in the cyw43 background IRQ. A setter would
have to be hand-deferred anyway. **Decision: the event.**

```rust
// ui-ffi/src/lib.rs -- purely additive, PL_EVENT_ABI_VERSION unchanged
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlCodecChangedPayload {
    /// UTF-8, not necessarily NUL-terminated; null == "no codec / disconnected".
    /// Borrowed for the duration of the pl_ui_push_event call only -- SAME contract
    /// and same lifetime hazard as PlDeviceDiscoveredPayload::name.
    pub name: *const u8,
    pub name_len: usize,
    /// Nominal, not live. 0 == host silent -> panel renders "idle", never "0 kbps".
    /// Live/adaptive bitrate is deferred (E20).
    pub bitrate_bps: u32,
}
// PlEventTag::CodecChanged = 8
```

C side: `bt.c`'s ring entry already carries a `name_buf[240]` for `DeviceDiscovered`.
Generalise the pointer-patching in `pl_bt_ring_push`/`pl_bt_drain_events` to cover both
string-bearing tags — a 4-line change. The borrowed-pointer lifetime bug that comment
block exists to prevent (`bt.c:83-91`) applies identically to codec names.

Core side: `BtModel` gains `pub active_codec: Option<CodecInfo>`. Screens read it.
`App::handle_event` folds it. No new `App` methods per field (`app.rs:304-306`).

### 6.2 Everything else M4 needs already exists

- `PL_EVENT_TAG_CONNECT_STEP_CHANGED` with `SettingUpAudio` / `NegotiatingCodec` —
  maps 1:1 onto `A2DP_SUBEVENT_SIGNALING_*` / `STREAM_ESTABLISHED`.
- `PL_EVENT_TAG_CONNECT_SUCCEEDED { degraded }` — drives the amber hero.
- `PL_FAILURE_REASON_NO_A2DP_SINK` — already representable.
- `PL_COMMAND_TAG_CONNECT` — becomes real (`a2dp_source_establish_stream`) instead of
  `bt.c:407-418`'s log-only stub.

**Deliberately deferred:** a glitch/health event (overrun/underrun to the panel). MVP
surfaces those over CDC only. `PL_COMMAND_TAG_CANCEL_CONNECT` stays unhandled here —
it is its own bead (`pico-link-2pq`).

## 7. Making drops visible (closes `pico-link-z9c`)

Every counter is a plain `volatile uint32_t` bumped on an IRQ path and **printed only
from the superloop** (thread context, where `pl_log` is safe), by extending the
rate-limited `pl_usb_pump_report()`:

```
a2dp: codec=SBC bitrate=345600 fill=4610/4608 ovr_frames=0 und=0
      enc_max_us=210 pkt_sent=7431 pkt_fail=0 misaligned=0
```

Reading discipline, restated because this project has been burned twice:

- **`ovr_frames=0` is a pass only if `pkt_sent` is also large.** Zero-over-zero is
  "the test never ran".
- **`fill` pinned at 0 or at capacity means the control loop is not closed**, whatever
  the drop counts say.
- `misaligned` should be permanently 0. Nonzero means an ISO packet arrived with a
  non-frame-multiple length and something upstream is wrong.

## 8. Staged implementation order

**S0 — PCM ring hardening. No behaviour change.** Move to `pcm_ring.{c,h}`;
frame-granular drops; block copy; power-of-two mask; counters; capacity -> 32 KiB.
*Done when:* M3's streaming result reproduces unchanged — `packets` climbing,
`avail_hwm` well under 784, `worst_interval_us` < 2000.

**S1 — SBC audible. THIS IS THE MVP.** `btstack_config.h` grown for A2DP (sec 9); link
`pico_btstack_sbc_encoder`; SDP records + class of device 0x200408; `codec_table` with
the SBC row; `a2dp.c` with the media timer, ring-fed encode, and send;
`PL_COMMAND_TAG_CONNECT` wired to `a2dp_source_establish_stream`; **and the sec 2
feedback-source change, which is a hard dependency** — commit it separately for
reviewability, but it ships in S1 or S1 overruns within seconds.
*Done when:* sec 10.

**S2 — Panel tells the truth.** `PL_EVENT_TAG_CODEC_CHANGED`; `BtModel::active_codec`;
hero renders `SBC` + nominal bitrate; `"idle"` when the host is silent.

**S3 — Tune the target fill.** Log ring-fill min/max over 10 min of real playback; set
target ~= 2x observed peak-to-peak excursion. Until then 24 ms is a *starting value,
not a measured one*, and should be labelled as such in the code.

**S4 — LDAC.** (Separate bead.) Vendor libldac (Apache-2.0); add the row; define
`ENABLE_A2DP_EXPLICIT_CONFIG`; implement sec 4.3's explicit loop; **re-prove SBC still
plays** (sec 4.1 regression risk); measure encode time against sec 5's bars.

**S5 — AAC.** Not scheduled. Gated on a licence review that happens *before* any code.

## 9. `btstack_config.h` — what certainly changes

The file says so itself (`btstack_config.h:6-8`). Certainties:

- `MAX_NR_L2CAP_CHANNELS 1` and `MAX_NR_L2CAP_SERVICES 1` are **definitely too small** —
  AVDTP signalling + AVDTP media + AVRCP + SDP.
- `MAX_NR_AVDTP_STREAM_ENDPOINTS` >= codec table length; `MAX_NR_AVDTP_CONNECTIONS`,
  `MAX_NR_A2DP_CONNECTIONS`, `MAX_NR_AVRCP_CONNECTIONS` all needed.
- `ENABLE_SDP` / SDP server record capacity — `sdp_init()` and four service records.
- `MAX_NR_CONTROLLER_ACL_BUFFERS 3` / `HCI_HOST_ACL_PACKET_NUM 3` were conservative for
  M2's inquiry-only traffic. Sustained A2DP wants more (start at 8).

**Do not guess the exact set.** BTstack `#error`s loudly and by name for anything
missing, so the build tells you. The ACL buffer count is the one that will *silently*
throttle instead — its observable is media-send backlog / `pkt_fail`.

`ENABLE_A2DP_EXPLICIT_CONFIG` is **not** set until S4.

## 10. What "done" looks like on real hardware

S1 passes when **all** of these hold in one session. Flash via the software path:
`picotool info -f --vid 0x2e8a --pid 0x000c` then `picotool load -x <uf2>`.

1. **Music from macOS is audible in real Bluetooth headphones**, paired from the panel
   with the d-pad, no terminal involved in the pairing.
2. **>= 10 minutes continuous** with no user-perceptible dropout.
3. CDC capture shows: `codec=SBC`, `bitrate` within 10 % of the SBC nominal for the
   negotiated config, `pkt_sent` climbing at ~75/s, `ovr_frames == 0`,
   `misaligned == 0`, `und` zero or a small number with a stated cause.
4. `fill` tracks target within +/-25 % — **the proof the feedback loop is closed.** A
   fill pinned at 0 or at capacity fails this even with zero drops.
5. `worst_interval_us` still < 2000 — the M3 result is not regressed.
6. `enc_max_us` recorded (informational at S1, the gate at S4).
7. **Pause/resume in the host app** recovers without a reboot; **USB unplug/replug**
   recovers without a reboot.
8. **Range walk:** 5 m away and back, link survives or cleanly reconnects; no wedge.
9. Panel shows `SBC` in `TEXT_PRIMARY` (not amber — sec 4.3) and `idle` when paused.

**Not evidence:** "it enumerated"; "tests pass"; any counter reading zero without a
nonzero companion counter proving the path executed.

## 11. Provenance checklist for the code-reviewer

1. **BTstack reference declared.** Every file mirroring
   `${PICO_SDK_PATH}/lib/btstack/example/a2dp_source_demo.c` carries a module doc
   naming it and stating what changed — the convention `bt.c:12-19` already uses. No
   BlueKitchen copyright header stripped from anything close-derived.
2. **The one substantive divergence documented.** `a2dp_demo_audio_timeout_handler`'s
   `samples_ready` accounting (elapsed-time x rate, synthetic source) is **replaced**,
   not copied: our audio comes from the PCM ring and we clamp the backlog (sec 3.5).
3. **USBPods: zero.**
   `git log -p <merge-base>..<branch> | grep -iE 'usbpods|resamp44|audio_slot'` -> 0
   hits. GPL-3 virality would take the whole binary including `core` and `ui-ffi`.
   Reading it was fine; nothing may be copied. Review against the **merge base**.
4. **No libldac in S1.** Zero Sony source, no libldac CMake reference. It arrives with
   S4, Apache-2.0, headers intact.
5. **No FDK-AAC anywhere.** AAC exists here as a table row that does not exist yet.
   **FDK-AAC is not GPL but carries Fraunhofer terms plus patent-grant conditions with
   real commercial patent implications — licence review *before* code, not after.** If
   a supervisor "helpfully" adds AAC groundwork, that is a merge blocker, not a bonus.
6. **New link targets are pico-sdk-provided only.** `pico_btstack_sbc_encoder` ships in
   `${PICO_SDK_PATH}/src/rp2_common/pico_btstack/CMakeLists.txt:242-257`. No new
   external fetch, no new submodule.

## 12. Open questions that need hardware, not argument

1. **SBC encode us per frame on RP2350 @ 150 MHz.** Everything about whether LDAC needs
   core1 hangs off this number.
2. **Is the host actually honouring our feedback?** Free check: M3's `measured=N B/s`
   should read slightly above 192000. Do this before writing S1.
3. **The right target fill.** 24 ms is a starting value. S3 measures it.
4. **`a2dp_max_media_payload_size()`'s real value on this link.** Drives packet cadence
   and the SBC storage buffer; read at runtime, log once, do not hardcode.
5. **ACL buffer counts.** Sec 9 — tune against `pkt_fail`, not intuition.
