# Design: macOS refuses UAC2 alt 1 — root cause and fix

**Bead:** `pico-link-icb` · **Date:** 2026-08-29 · **Author:** Ada (Architect) · **Status:** proposed

> **Provenance note (orchestrator, 2026-08-29):** Ada was dispatched with a
> read-only toolset and **could not read USBPods**. The UAC1-vs-UAC2 comparison
> in section 5 is reasoned from this project's recorded facts and the UAC2 spec,
> **not** from fresh reading of that repository. Section 7 names the two field
> values worth asking USBPods for if someone gains access. Everything about
> *our* descriptors was read directly from our source and is first-hand.

## 1. The single most important new finding

**Our UAC2 audio function descriptor is byte-for-byte identical to TinyUSB's upstream `uac2_speaker_fb` example.**

Diffed macro against vendored upstream, argument by argument:

- Ours: `firmware/src/usb_descriptors.h:93-123`
- Upstream: `~/.pico-sdk/sdk/2.1.1/lib/tinyusb/examples/device/uac2_speaker_fb/src/usb_descriptors.h:50-80`

Every field matches. The only textual differences are named constants
(`UAC2_ENTITY_CLOCK` vs the literal `0x04`) that expand to the same bytes. The
audio section of our `tusb_config.h:85-118` is likewise identical to
`.../uac2_speaker_fb/src/tusb_config.h:131-162` (same `MAX_SAMPLE_RATE 48000`,
`N_CHANNELS_RX 2`, `N_BYTES_PER_SAMPLE_RX 2`, `RESOLUTION_RX 16`, `N_AS_INT 1`,
`CTRL_BUF_SZ 64`, `FEEDBACK_EP 1`, `FEEDBACK_FORMAT_CORRECTION 0`,
`EP_OUT_SW_BUF_SZ = 4 * EP_OUT_SZ_MAX`).

The descriptor arithmetic re-derives to exactly the 227 bytes already verified
off the live device, confirming the macros expand as intended:

| Block | Len |
|---|---|
| Config | 9 |
| IAD 8 + STD_AC 9 + CS_AC 9 + CLK_SRC 8 + IN_TERM 17 + OUT_TERM 12 + FU(2ch) 18 | 81 |
| STD_AS_INT alt0 9 + alt1 9 + CS_AS_INT 16 + TYPE_I 6 | 40 |
| STD_ISO_EP 7 + CS_ISO_EP 8 + STD_FB_EP 7 | 22 |
| CDC (`TUD_CDC_DESC_LEN`) | 66 |
| RPi reset vendor itf | 9 |
| **Total** | **227** ✓ |

**Consequence, and it reframes the whole bug:** this is not "we mis-copied an
example". It is "**the example itself is not macOS-clean**". TinyUSB's UAC2
examples are routinely validated on Windows and Linux; the UAC2 fields macOS
enforces most strictly are exactly the ones the example leaves at permissive
defaults. Two of them are, on a strict reading, non-compliant with UAC2 2.0.

**Do not go looking for a typo.** Every "did we get the endpoint attributes
right" style check passes. The bytes are the upstream bytes; the upstream bytes
are what macOS is refusing.

## 2. What is actually in our alt-1 descriptor chain (measured, field by field)

Decoded from the macros at `usb_descriptors.h:93-123` and
`usb_descriptors.c:99-101`, expanded through pico-sdk's TinyUSB
`src/device/usbd.h:382-450` and the constants at
`src/class/audio/audio.h:487,500-508,547-555,615-617`:

| Descriptor | Field | Our value | Spec-required / macOS expectation |
|---|---|---|---|
| Clock Source (`h:101`) | `bmAttributes` | `0x03` internal programmable | ok |
| Clock Source (`h:101`) | `bmControls` | **`0x03`** — freq RW, **Clock Validity absent** | **UAC2 §4.7.2.1: Clock Validity Control shall be present (read-only) → `0x07`** |
| Input Terminal (`h:103`) | `bNrChannels` | `2` | ok |
| Input Terminal (`h:103`) | `bmChannelConfig` | **`0x00000000`** (NON_PREDEFINED) | **2 channels, 0 spatial bits** |
| Input Terminal (`h:103`) | `iChannelNames` | **`0`** | **UAC2 §4.7.2.4: unnamed, unplaced channels are undescribed** |
| CS AS Interface (`h:115`) | `bTerminalLink` | `0x01` (= Input Terminal) | ok, correct for a sink |
| CS AS Interface (`h:115`) | `bmFormats` | `0x00000001` PCM | ok |
| CS AS Interface (`h:115`) | `bNrChannels` / `bmChannelConfig` / `iChannelNames` | `2` / **`0x00000000`** / **`0`** | **same defect as the terminal, and must agree with it** |
| Type I Format (`h:117`) | `bSubslotSize`/`bBitResolution` | `2` / `16` | ok |
| ISO data EP (`h:119`) | `bEndpointAddress` | `0x01` OUT | ok |
| ISO data EP (`h:119`) | `bmAttributes` | `0x05` = iso + **async** + data | ok, correct for an async sink |
| ISO data EP (`h:119`) | `wMaxPacketSize` | `196` = `(48+1)*2*2` | ok — one sample of headroom, correct for async |
| ISO data EP (`h:119`) | `bInterval` | `1` | ok |
| Feedback EP (`h:123`) | `bmAttributes` | `0x11` = iso + no-sync + **explicit feedback** | ok |
| Feedback EP (`h:123`) | `wMaxPacketSize` | **`4`** (`usb_descriptors.c:101`) | **USB 2.0 §5.12.4.2: full-speed feedback is 10.14 in *3* bytes** |
| Feedback EP (`h:123`) | `bInterval` | `1` | ok |

Everything the dispatch asked to check that is **fine**: `bTerminalLink`,
`bmFormats`, sync type, `bInterval`, `wMaxPacketSize` vs advertised rate,
terminal types, the presence of an explicit-feedback IN endpoint. The
`bNrChannels`/`bmChannelConfig` consistency check is where it breaks — the two
are *consistent with each other*, and both are consistently under-specified.

## 3. Root cause — ranked, because one measurement is still free

The measurement that discriminates costs **zero hardware cycles** (section 6),
so the one remaining flash can be spent proving the fix rather than finishing
the diagnosis.

### Candidate 1 (most likely) — `bmChannelConfig = 0` with `iChannelNames = 0` on a 2-channel cluster

**Where:** `firmware/src/usb_descriptors.h:103` (Input Terminal) and `:115` (CS AS Interface).

**What it violates:** UAC2 §4.7.2.4 / §4.9.2. A channel cluster is defined by
the triple (`bNrChannels`, `bmChannelConfig`, `iChannelNames`). We declare two
logical channels, assign neither a spatial position, and supply no name string.
Nothing in the descriptor tells a host what channel 1 and channel 2 *are*.

**Why macOS specifically:** CoreAudio's USB audio driver must produce an
`AudioChannelLayout` before it can publish an `AudioStreamBasicDescription`.
Windows and Linux (ALSA) both fall back to "assume interleaved L/R" for a
2-channel non-predefined cluster; CoreAudio is the strict one. If it cannot
build a layout, it publishes **zero formats** for the stream — and a stream with
no formats is one CoreAudio will happily enumerate, expose in the device list,
run the volume/mute controls of, and **never open**. That is *exactly* our
counter signature: full control-plane traffic (`fu_get=30`, `fu_set=12`,
`clock_get=5`), `SET_INTERFACE(1, alt 0)` once at configuration time, and never
an alt 1.

**Evidence for:** it is the only defect that predicts "device present and
controllable, stream never opened" rather than "device fails to enumerate" or
"stream opens and misbehaves".

### Candidate 2 (close second, same shape of failure) — Clock Source `bmControls` omits Clock Validity

**Where:** `firmware/src/usb_descriptors.h:101` —
`bmControls = (AUDIO_CTRL_RW << AUDIO_CLOCK_SOURCE_CTRL_CLK_FRQ_POS)` = `0x03`.
Bits D3..D2 (Clock Validity, `AUDIO_CLOCK_SOURCE_CTRL_CLK_VAL_POS = 2`,
`audio.h:555`) are `00` = "not present".

**What it violates:** UAC2 §4.7.2.1. The Clock Validity Control is required to
be present at least read-only on a Clock Source Entity. A host told validity is
unreadable has no way to learn the clock has locked.

**Why macOS specifically:** CoreAudio will not start an isochronous stream
against a clock it cannot confirm is valid. Note the asymmetry with our code:
`usb_audio.c:68-71` *does* answer `AUDIO_CS_CTRL_CLK_VALID` with `bCur = 1` — we
implement the control but declare it absent. If macOS trusts the descriptor and
never asks, it concludes "clock never validates" and holds the stream at alt 0
forever. The `clock_get=5` counter is consistent with either reading (5 calls
could be all `SAM_FREQ` RANGE/CUR retries), which is why per-selector
instrumentation is in the fix.

### Candidate 3 — measurement validity: nothing was actually playing, or coreaudiod cached stale state

**Why it has to be ranked:** this project's own recorded history is that three
"firmware bugs" in one night were broken measurements, and "a counter reading
zero is not a pass". macOS sets alt 1 **only when an application has an active
stream on that device**. Selecting the device in System Settings and moving the
volume slider produces exactly `fu_set`/`fu_get`/`clock_get` traffic with
`alt=0` — which is *also* our observed signature. Separately, coreaudiod caches
per-VID:PID device state; the PID was changed to `0x000C` for M3
(`usb_descriptors.c:52`), which reduces but does not eliminate this.

**Cost to rule out:** zero. Section 6 phase 0 rules it out before anything is built.

### Candidate 4 — full-speed feedback endpoint declared as 4 bytes (16.16) instead of 3 (10.14)

**Where:** `usb_descriptors.c:101` passes `4` as `_epfbsize`; `tusb_config.h:103`
sets `CFG_TUD_AUDIO_ENABLE_FEEDBACK_FORMAT_CORRECTION 0`.

**What it violates:** USB 2.0 §5.12.4.2 — full-speed explicit feedback is 10.14
in 3 bytes; 16.16/4 bytes is high-speed. TinyUSB documents this at
`class/audio/audio_device.h:497-502` and notes it is *Windows* that requires the
non-compliant 16.16 on full speed. We have chosen the Windows-compatible,
spec-non-compliant option, and the comment at `tusb_config.h:95-102` explicitly
justifies not wiring the OS-guessing quirk.

**Why ranked 4th:** this is a streaming-*quality* defect (drift, dropouts) far
more than an alt-selection one; there is no evidence CoreAudio validates
feedback packet size before opening the pipe. But it is nearly free to fix and
it will bite us in M4 regardless, so it rides along — **in its own commit**.

### Candidate 5 — composite/bandwidth/pipe-open failure

Audio ISO OUT 196 + ISO IN 4 + CDC interrupt 8 is roughly 208 bytes of the
~1157-byte full-speed periodic budget. Endpoint numbering (`0x01`/`0x81` audio,
`0x82` CDC notif, `0x03`/`0x83` CDC data, `usb_descriptors.c:34-38`) is the same
shape TinyUSB's own RP2040 example uses. No defect found. Listed only so the
next agent does not re-derive it.

### Explicitly dead — do not revisit

The dropped-`SET_INTERFACE` theory; `usbd.c:356-360` as the cause of *this* bug;
descriptor corruption. Banked from probe 2.

## 4. The fix — concretely, for Ruby

All descriptor changes are in **one file**, `firmware/src/usb_descriptors.h`.
Nothing structural changes; no interface, endpoint, or length arithmetic moves;
the config descriptor stays 227 bytes and the `_Static_assert` at
`usb_descriptors.c:111` keeps guarding that.

**Change 1 — Clock Source `bmControls` (line 101).** Replace the `_ctrl`
argument with the frequency control OR'd with a read-only validity control:

```c
(AUDIO_CTRL_RW << AUDIO_CLOCK_SOURCE_CTRL_CLK_FRQ_POS) | \
(AUDIO_CTRL_R  << AUDIO_CLOCK_SOURCE_CTRL_CLK_VAL_POS)
```

Byte value goes `0x03` → `0x07`. Both constants already exist
(`audio.h:501-502,554-555`). No code change needed: `usb_audio.c:68-71` already
answers `CLK_VALID` correctly.

**Change 2 — channel cluster, Input Terminal (line 103).** Replace `_channelcfg`
`AUDIO_CHANNEL_CONFIG_NON_PREDEFINED` with

```c
(AUDIO_CHANNEL_CONFIG_FRONT_LEFT | AUDIO_CHANNEL_CONFIG_FRONT_RIGHT)
```

Byte value `0x00000000` → `0x00000003` (`audio.h:616-617`). Leave
`_nchannelslogical` at `0x02` and `_idxchannelnames` at `0x00` — with two
spatial bits set for two channels, no name strings are required.

**Change 3 — channel cluster, CS AS Interface (line 115).** Identical
substitution for `_channelcfg`. **These two must stay equal**; add a comment
saying so, because a future format change that touches one and not the other
reintroduces the bug silently.

**Change 4 — full-speed feedback format.** In `firmware/src/usb_descriptors.c:101`,
change the last argument of `TUD_AUDIO_SPEAKER_STEREO_FB_DESCRIPTOR` from `4` to
`TUD_OPT_HIGH_SPEED ? 4 : 3`, and in `firmware/src/tusb_config.h:103` set
`CFG_TUD_AUDIO_ENABLE_FEEDBACK_FORMAT_CORRECTION 1`. That makes TinyUSB take our
16.16 value and emit compliant 10.14 on full speed.
`TUD_AUDIO_DESC_STD_AS_ISO_FB_EP_LEN` is 7 regardless of `wMaxPacketSize`, so
the 227-byte total does not change.

*Judgement call:* changes 1-3 are pure spec compliance and cannot make anything
worse. Change 4 trades Windows compatibility for spec compliance. **Land it as
its own commit** so a regression can be bisected. Recommendation: include it —
we have no Windows test rig and M4's clock story needs the compliant path anyway.

**Change 5 — instrumentation (needed to make the one hardware cycle conclusive).**
In `firmware/src/usb_audio.c`, all as plain `volatile uint32_t` increments,
**no string formatting anywhere in these paths** — they run inside the 0xC0
worker IRQ (`usb_audio.c:35-41`, and probe 1's hang was formatting in that context):

- `s_clock_set_calls` in `clock_set_request` (line 75) — currently uncounted, so
  we cannot tell whether macOS ever sets the sample rate.
- Split `s_clock_get_calls` into `s_clk_get_freq_cur`, `s_clk_get_freq_range`,
  `s_clk_get_valid` inside `clock_get_request` (lines 54-71). This directly
  discriminates candidate 2: if `s_clk_get_valid` is 0 before the fix and
  non-zero after, the validity control was the gate.
- `s_set_itf_alt1_calls`, incremented in `tud_audio_set_itf_cb` (line 162) when
  `itf == ITF_NUM_AUDIO_STREAMING && alt == 1`.
- Expose each via a `pl_usb_audio_*` getter alongside the existing ones
  (lines 259-281) and add them to `pl_usb_pump_report`'s output line. Formatting
  happens there, in thread context — not in the IRQ.

**No changes to `core/`.** Nothing here crosses the FFI seam, nothing calls Rust
from interrupt context, and `PICO_STDIO_USB_CONNECTION_WITHOUT_DTR=1`
(`firmware/CMakeLists.txt:117`) stays exactly as it is.

**Branch base:** the audio code lives on the unmerged `bd-pico-link-icb`
worktree. Ruby must start from whatever branch actually carries
`firmware/src/usb_audio.c`, **not** from a bare `main` that does not have it.
Confirm before dispatch.

## 5. If the answer turns out to be "move to UAC1" — the cost, stated before anyone starts

USBPods was not read in this session (see the provenance note at the top). Treat
this as the *decision frame*, not a finding. The project record says USBPods is
UAC1 (possibly UAC1+UAC2 dual) and works on this exact silicon.

**Why UAC1 would help at all:** UAC1 has no clock entity, no `bmControls`, no
channel-cluster triple, no feedback-format subtlety, and a much smaller surface
for a host to reject. macOS has supported UAC1 driverlessly since forever and
its parser is correspondingly forgiving. Most of the descriptor risk above
simply does not exist in UAC1.

**What we would lose, concretely:**

- **Sample rates:** UAC1 caps practically at 48 kHz on full speed. We lose the
  headroom for 88.2/96 kHz. For LDAC this matters — LDAC's high-quality modes
  are defined at 96 kHz, and a UAC1 front end permanently caps the product at
  48 kHz input. **This is the one real product cost**, and it lands squarely on
  Pico Link's "hi-res codec" positioning.
- **Channels:** >2 channels becomes awkward. Irrelevant for a stereo headphone dongle.
- **Clock control:** no clock entity means no way to tell the host our clock is
  or is not valid. For a device whose sink clock is ultimately the *Bluetooth*
  link, that is a genuine loss — UAC2's clock-validity signalling is how you
  tell the host "the headphones just dropped, stop sending". Under UAC1 we would
  have to fake it, which is exactly the kind of constraint-driven hack that
  becomes load-bearing.
- **Explicit feedback:** UAC1 feedback exists (`bRefresh`/3-byte 10.10) but is
  more thinly supported and more host-quirky.

**What we would keep:** everything above the USB seam — the PCM ring in
`usb_pump.c`, the drain in `pl_usb_audio_task` (`usb_audio.c:222`), the whole
Bluetooth/LDAC path, the entire display product. The blast radius is
`usb_descriptors.{c,h}`, `tusb_config.h`, and the entity-request callbacks in
`usb_audio.c`. Roughly one to two days of Ruby's time plus hardware cycles,
which we are short of.

**Is UAC1+UAC2 dual worth it?** No, not now. Dual means two complete function
descriptor sets, two sets of entity callbacks, host-dependent branching, and
roughly double the surface to test — on a project with **one hardware
verification cycle left in the budget**. It is the right *eventual* answer for a
shipping product that must please Windows, macOS and Linux; it is the wrong
answer for a PoC that has not yet gotten one host to stream once.

**Recommendation: do not move to UAC1 yet.** Four constants in one file, all
unambiguous UAC2 spec compliance improvements, are the correct next move. UAC1
is a real architectural pivot with a real cost to the "hi-res" product claim,
and should be taken only after the cheap, correct, reversible fix has been shown
to fail. If it does fail, revisit with the section 7 measurement in hand — and
revisit it **as a decision for Andreas, with the 48 kHz cap named out loud**,
not as an implementation detail.

## 6. Verification plan

### Phase 0 — free host-side checks, BEFORE building anything. Zero hardware cycles.

Run these against the **current, unmodified** firmware. They rule out candidate
3 and, more importantly, discriminate candidates 1/2 from everything else.

1. Plug the board in, select "Pico Link Audio Dongle" as the macOS output
   device, then open **Audio MIDI Setup** and look at the device's **Format** popup.
   - **Empty / greyed / no format listed** → CoreAudio parsed the control graph
     but produced **zero stream formats**. Confirms the candidate-1/2 family and
     means the section 4 fix is aimed correctly.
   - **Shows "2 ch 16-bit Integer 48.0 kHz"** → CoreAudio *does* have a usable
     format and the problem is downstream (pipe open, bandwidth, or candidate 3).
     **Stop and re-plan** — the section 4 fix is not the answer and spending the
     hardware cycle on it wastes it.
2. `system_profiler SPUSBDataType | grep -A 20 "Pico Link"` — confirm the device,
   PID `0x000c`, and current draw. Keep the output trimmed; this goes in a
   subagent, not the main thread.
3. `log show --last 5m --predicate 'senderImagePath CONTAINS "AppleUSBAudio" OR process == "coreaudiod"' --info`
   immediately after a replug. AppleUSBAudio logs descriptor-parse rejections
   with reasonably specific text. If it names a field, that field *is* the root
   cause and this whole ranking collapses to a fact.
4. Rule out candidate 3 properly: with the device selected, **actually play
   audio** (`afplay /System/Library/Sounds/Glass.aiff` on a loop, or a long file)
   and confirm the host believes it is playing. Also clear stale state once: quit
   and let `coreaudiod` restart (`sudo killall coreaudiod`) with the device
   unplugged, then replug.

Phase 0 is a subagent dispatch. It requires no flash, no `.uf2`, no branch.

### Phase 1 — the one budgeted hardware cycle

Only if phase 0 points at candidates 1/2.

**Build:** the section 4 changes on a worktree branch off the branch carrying
`usb_audio.c`. Cross-compile with `PICO_SDK_PATH=/Users/andreas/.pico-sdk/sdk/2.1.1`
and the ARM toolchain at `/Applications/ArmGNUToolchain/15.2.rel1/arm-none-eabi/bin`.
Confirm the `_Static_assert` at `usb_descriptors.c:111` still passes (it will —
no length changes) and that the `.uf2` is produced.

**Flash:** BOOTSEL + copy, since the board may be wedged and remote
`picotool reboot -f -u` needs a healthy control path.

**Capture:** `tools/usb-console/cdc_reader.py` with `--pid 0x000c`. **Direct
libusb only — never the tty path.** One long capture, one reader, no
open/close loops.

**Drive it:** select the device as output, then play a continuous file for at
least 30 seconds.

**Proof — all of these, or it is not a pass:**

- `set_itf_calls >= 2` **and** `last_itf=1, last_alt=1` **and** the new
  `set_itf_alt1_calls >= 1`. This is the primary pass criterion.
- `streaming=1` while audio is playing.
- `packets` **increasing over successive report lines**, at roughly 1000/s. A
  single non-zero reading is not proof; two readings with a plausible delta are.
- `pcm_bytes_total` increasing at roughly **192000 bytes/s** (48000 × 2 ch × 2 B).
  A rate materially off that means packets are arriving but the format is
  misinterpreted — a different bug, worth knowing.
- No panic line from the panic recorder; board still answers `picotool` afterwards.

**A zero is not a pass.** `packets=0` with `streaming=1` means the alt was
entered and the endpoint is not delivering — that is progress, not success, and
it points at the `audio_device.c:759-762` re-arm defect already diagnosed under
`pico-link-tfj`, which would then finally be testable.

**Capture the new diagnostic counters even on success** — `clk_get_valid` and
`clock_set_calls` are what tell us *which* of changes 1-3 mattered, and that
knowledge is worth more than the pass itself.

## 7. Risks — what is still wrong if this does not work

- **The fix lands and macOS still holds alt 0.** Then the channel cluster and
  clock validity are not the gate. **Next discriminating measurement:** the
  phase-0 `log show` against AppleUSBAudio with the *fixed* firmware — a parser
  that got further will usually fail later and more verbosely. If that is still
  silent, the next candidate is the composite itself: build a **temporary
  audio-only** firmware (drop CDC and the reset interface, audio at interfaces
  0/1 only, endpoints 0x01/0x81 only). That reduces us to literally the upstream
  `uac2_speaker_fb` topology. If audio-only streams, the defect is in the
  composite; if it still does not, the defect is UAC2-on-macOS itself and the
  UAC1 decision in section 5 becomes live. **Cost: it sacrifices the CDC
  console**, so instrument via the display or an LED instead, and budget it as a
  separate hardware cycle Andreas has to approve.
- **Change 4 regresses Windows.** Accepted knowingly; that is why it is its own commit.
- **Candidate 3 was the truth all along.** Phase 0 is specifically designed to
  catch this before any hardware is spent. Do not skip it because it feels like a
  formality — this project's recorded history says the formality is where the
  answer usually is.
- **The board is wedged and phase 0 cannot run.** Phase 0 needs a physically
  replugged, healthy board. That is an Andreas action, and should be requested at
  the same time as the dispatch, not discovered mid-task.
- **The USBPods comparison is unread.** If someone gains GitHub access, the
  highest-value single question to ask that repo is: *does its audio descriptor
  set `bmChannelConfig` to a non-zero spatial mask, and does its clock source (if
  UAC2) set `bmControls` to `0x07`?* Two field values, no code copied, and it
  either confirms or kills the top two candidates outright.

## 8. Sustainability note

The proximate cause here is that we vendored an upstream example's descriptor
verbatim and inherited its host-compatibility profile along with its bytes. The
provenance comments at `usb_descriptors.h:1-40` are excellent and did their
licensing job — but they framed the descriptor as *copied because it has a high
floor for getting subtly wrong*, and the copy turned out to be wrong in exactly
the subtle way it was meant to protect against.

The durable correction is small and worth taking now: **our descriptor should be
a deliberately spec-audited artifact, not an example copy.** When Ruby makes
these changes, have her annotate each of the four fields with the UAC2/USB2.0
section it satisfies and why the upstream default was wrong. That is a comment,
not architecture — cheap and right. Do *not* build a descriptor-generation
abstraction or a lint harness for this; at one audio function, that is
gold-plating a PoC.

The one thing that would be genuinely load-bearing to get wrong is section 5: if
the UAC1 pivot happens, it must be a recorded ADR with the 48 kHz ceiling
stated, not a quiet descriptor swap. That constraint would propagate all the way
to the LDAC codec selection screen the product is built around.
