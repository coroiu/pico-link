# Design: macOS refuses UAC2 alt 1 — root cause and fix

**Bead:** `pico-link-icb` · **Date:** 2026-08-29 · **Author:** Ada (Architect) · **Status:** proposed
**Revision 2 — supersedes revision 1 of the same date.**

## 0. What this revision changes, and why

Revision 1 was written without access to USBPods and mis-ranked its own
candidate list. The orchestrator cloned USBPods locally; reading it, plus a
re-read of the *vendored* TinyUSB example we copied from, killed the top two
candidates and promoted the fourth to near-certainty. The record of what was
refuted, and by what, is kept deliberately — it is the expensive part.

| Rev-1 claim | Status now | Refuted by |
|---|---|---|
| **C1: `bmChannelConfig = 0` / `iChannelNames = 0` is the gate** | **REFUTED** | USBPods `src/tinyusb/usb_descriptors.h:88` (Input Terminal) and `:109` (CS AS Interface) both pass `AUDIO_CHANNEL_CONFIG_NON_PREDEFINED` — byte-identical to ours — in a dongle that demonstrably streams on macOS. |
| **C2: Clock Source `bmControls` omits Clock Validity** | **REFUTED** | USBPods `usb_descriptors.h:83` passes `_ctrl` = `1`, i.e. `bmControls = 0x01` — *weaker* than our `0x03`, and equally without a Clock Validity bit. It streams anyway. |
| **§5: "USBPods is UAC1, so a UAC1 pivot is the fallback"** | **PREMISE WRONG — SECTION RETRACTED** | USBPods' `TUD_AUDIO_DESC_CS_AC` (`usb_descriptors.h:78`) declares `bcdADC 0x0200`. It is UAC2, same as us. It *also* ships a second, UAC1 configuration (`tusb_config.h:100-109`, `USB_UAC1_FIRST=0`), but macOS takes configuration 1 — the UAC2 one. **The entire rev-1 cost analysis of "moving to UAC1" was answering a question that isn't in front of us.** Do not reuse it. A UAC1 pivot remains *possible*, but nothing in the evidence now points there, and rev-1's 48 kHz-ceiling argument was reasoning about a hypothetical. |
| **C4: full-speed feedback endpoint declared 4 bytes (16.16) instead of 3 (10.14)** | **PROMOTED TO ROOT CAUSE** | See §2. Upstream TinyUSB says so in two places, in its own words. |
| **C3: measurement validity / coreaudiod cache** | **STILL LIVE, still cheap** | Phase 0, §6. Unchanged. |
| **C5: composite / bandwidth** | Still no defect found | USBPods carries *more* periodic endpoints than we do (AC interrupt IN + HID + CDC notif) and streams. Bandwidth is not the gate. |

Rev-1's §1 claim that our descriptor is "byte-for-byte identical to upstream
`uac2_speaker_fb`" is also **wrong in the one place that matters** — see §2. It
compared our macro against upstream's macro; the defect lives in the *call
site*, in the `.c` file, which upstream branches on host OS and we did not.

## 1. The evidence, first-hand

Read this session, all from files on this machine:

- the USBPods clone in the session scratchpad (GPL-3 — **read, never copied**;
  only field values and structural facts are recorded here, no expression is
  reproduced): `src/tinyusb/usb_descriptors.h`, `src/tinyusb/uac.c`, `tusb_config.h`.
- `$PICO_SDK_PATH/lib/tinyusb/examples/device/uac2_speaker_fb/src/usb_descriptors.c`
  and `$PICO_SDK_PATH/lib/tinyusb/src/class/audio/audio_device.{c,h}` — the
  *actual* code our firmware links.
- `.worktrees/bd-pico-link-icb/firmware/src/{usb_descriptors.h,usb_descriptors.c,usb_audio.c,tusb_config.h}`.

### The differences between us and USBPods that are real

| | Ours | USBPods | Verdict |
|---|---|---|---|
| ISO data EP sync | `TUSB_ISO_EP_ATT_ASYNCHRONOUS` | `TUSB_ISO_EP_ATT_ADAPTIVE` | consequence of the next row |
| Feedback IN endpoint | present, `wMaxPacketSize` **4** | absent | **the gate — see §2** |
| alt 1 `nEPs` | `0x02` | `0x01` | ditto |
| AC interface `nEPs` | `0x00` | `0x01` + interrupt EP | optional in UAC2; not a gate |
| Function category | `AUDIO_FUNC_DESKTOP_SPEAKER` | `AUDIO_FUNC_HEADSET` | cosmetic (icon/transport hint) |
| Output terminal type | `OUT_DESKTOP_SPEAKER` | `OUT_HEADPHONES` | cosmetic |
| Feature Unit ch1/ch2 | mute+volume RW | `AUDIO_CTRL_NONE` | legal both ways; not a gate |
| Clock/FU entity handlers | `usb_audio.c:52-135` | `uac.c:372-555` | **functionally the same code shape**, same RANGE/CUR answers, same `CLK_VALID → 1`. The control plane is not the difference. |

Note also that USBPods' clock declares sample-frequency **read-only** (`0x01`)
while ours declares it read-write (`0x03`), and USBPods still services a
`SET CUR SAM_FREQ`. Ours does too (`usb_audio.c:75-88`). Not a gate either way,
but it is why §4 adds a counter there rather than a change.

## 2. Root cause — and this time upstream states it explicitly

**Our explicit-feedback endpoint is declared and transmitted in 16.16/4-byte
format on a full-speed device. macOS accepts only 10.14/3-byte. Because the
feedback endpoint is part of alternate setting 1, AppleUSBAudio's rejection of
it takes the whole alternate setting with it — so `SET_INTERFACE(alt 1)` is
never issued, while the AC interface's control plane keeps working normally.**

Three independent confirmations, all in code we already link:

**(a) Upstream ships a macOS-specific descriptor and says why.**
`uac2_speaker_fb/src/usb_descriptors.c` has *two* configuration descriptors.
Line 147 `desc_configuration_default` passes feedback size **4**. Line 163 is
guarded by `#if CFG_QUIRK_OS_GUESSING` and introduced by the comment
`// OS X needs 3 bytes feedback endpoint on FS` (verified first-hand at
`usb_descriptors.c:162`), and line 169 passes **3**.
`tud_descriptor_configuration_cb` (lines 181-192) returns the 3-byte one when
`tud_speed_get() == TUSB_SPEED_FULL && quirk_os_guessing_get() == QUIRK_OS_GUESSING_OSX`.

Our `usb_descriptors.c:101` passes **`4`**. We copied the default (Windows) call
site into a full-speed-only, macOS-first device. **This is the one field where we
diverge from what upstream ships for our host.** Rev-1 missed it because the
value is an argument at the call site, not a constant in the macro.

**(b) TinyUSB's audio driver carries the empirical compatibility matrix.**
`class/audio/audio_device.c:1200-1214`, verbatim from the SDK we build against
(verified first-hand):

```
  // 3 variables: Format | packetSize | sendSize | Working OS:
  //              16.16    4            4          Linux, Windows
  //              16.16    4            3          Linux
  //              16.16    3            4          Linux
  //              16.16    3            3          Linux
  //              10.14    4            4          Linux
  //              10.14    4            3          Linux
  //              10.14    3            4          Linux, OSX
  //              10.14    3            3          Linux, OSX
```

**OSX appears in exactly two rows, and both require format 10.14 and
`packetSize` 3.** Our configuration is row 1 — `16.16 / 4 / 4` — the only row
that names Windows and the row furthest from OSX. `audio_device.c:1214` sends
`apply_correction ? 3 : 4` bytes, and `apply_correction` is
`(speed == FULL) && audio->feedback.format_correction`, which
`audio_device.c:515-518` derives from
`CFG_TUD_AUDIO_ENABLE_FEEDBACK_FORMAT_CORRECTION`. We set that to **0**
(`tusb_config.h:103`).

**(c) The comment that disabled it conflates two separable things.**
`tusb_config.h:95-102` justifies `CFG_TUD_AUDIO_ENABLE_FEEDBACK_FORMAT_CORRECTION 0`
on the grounds that the OS-guessing quirk "requires a BOS descriptor solely to
host-sniff the OS" and would collide with `reset_interface.c`'s BOS ownership.
That reasoning is correct *about the quirk* and irrelevant *to the flag*.
`CFG_TUD_AUDIO_ENABLE_FEEDBACK_FORMAT_CORRECTION` is a plain compile-time
switch inside `audiod_fb_send`; it touches no BOS descriptor, no device
descriptor, and nothing `reset_interface.c` owns. The comment then asserts
"current macOS does not need it" — which is the claim this bead has spent a
session disproving. **This is the actual defect: a true statement about a
neighbouring feature was used to switch off the required one.**

**Why this produces exactly our counter signature.** AppleUSBAudio parses the
AudioControl interface (clock, terminals, feature unit) and the AudioStreaming
interface independently. The control graph is valid, so the device enumerates,
appears in System Settings, and answers `fu_get`/`fu_set`/`clock_get` — which is
all our counters ever showed. Alternate setting 1 is where the malformed
endpoint lives; an alt setting whose endpoints cannot all be realised is not
made selectable, so the host never issues `SET_INTERFACE(1, 1)`. `set_itf_calls`
sees only the configuration-time `alt 0`. That is what we measured.

**Why USBPods dodges it entirely:** it has no feedback endpoint to get wrong.
Adaptive + one endpoint has no 10.14/16.16 question, no `bRefresh`, no
speed-dependent packet size. It sidesteps the bug rather than solving it.

**Confidence.** High, not certain. The mechanism is inferred from AppleUSBAudio
behaviour, not read from its source. What *is* certain and first-hand is that
(i) upstream ships a different descriptor for full-speed macOS, (ii) upstream's
own matrix excludes our exact configuration from OSX, and (iii) we are running
upstream's non-macOS configuration on macOS. Even if the mechanism differs in
detail, we are demonstrably in the one configuration upstream says does not work
here, and the correction is two lines.

### Residual candidates, ranked, if the fix does not land

1. **C3 — measurement validity / coreaudiod cache.** Unchanged from rev 1; §6
   Phase 0 rules it out at zero hardware cost.
2. **AC interrupt endpoint absent.** UAC2 makes it optional and TinyUSB's own
   speaker example omits it, but USBPods has one and USBPods works. Cheap to add
   (`CFG_TUD_AUDIO_ENABLE_INTERRUPT_EP 1`, `nEPs` 0→1, one more EP descriptor,
   +7 bytes, one more endpoint number). **Not in this cycle** — it changes the
   descriptor length and adds a second variable to a one-shot experiment.
3. **The composite itself.** §7's audio-only firmware, unchanged.

**Explicitly dead — do not revisit:** the dropped-`SET_INTERFACE` theory;
`usbd.c:356-360` as the cause of *this* bug; descriptor corruption; channel
config; clock validity; bandwidth.

## 3. The architectural question: adaptive, or async-with-working-feedback?

This is the heart of the document, and it is not a descriptor detail. The sync
type is baked into the alt-1 endpoint topology (`nEPs` 1 vs 2) — the exact
surface hosts cache and the exact surface that has cost us this bead. Flipping
it after M4 is built on top of it is expensive.

**What our clock actually is.** Pico Link is a USB sink whose audio is consumed
by an A2DP link to remote headphones. The headphones have their own DAC crystal;
we cannot pull it, and A2DP gives us no rate feedback channel from them. Our
only observable is our own transmit-queue depth. So we are, precisely and
permanently, **an asynchronous sink whose true rate is neither the USB host's
SOF nor a local crystal.**

- **Adaptive** means telling the host "we will lock our sample clock to your
  SOF-derived rate." For us that is a false claim: we cannot. The residual
  host-vs-headphone ppm drift then has to be absorbed *by us*, in software,
  forever — drop/insert a sample every N seconds (audible ticks) or build an
  asynchronous sample-rate converter (real DSP work, real CPU, on a core that
  also runs BTstack, LDAC and the display). This is what USBPods does; its tree
  carries `src/resamp44.h` / `resamp44_taps.h` and an `audio_slot_push_samples`
  staging layer with explicit flush-on-EP-close logic and a documented
  "AirPods drift out of sync" bug report in `uac.c:568-574`. That comment is
  the cost of adaptive, written down by someone who paid it.
- **Async + explicit feedback** means telling the host the truth and letting
  **CoreAudio's own resampler** do the rate matching, at zero CPU cost to us and
  with far better quality than anything we would write. We already implement the
  device side: `usb_audio.c:193-198` requests
  `AUDIO_FEEDBACK_METHOD_FIFO_COUNT`, so TinyUSB derives the feedback value from
  OUT-FIFO fill and we never write clock math.

**Recommendation: keep async. Fix the feedback endpoint.** Not a close call.

- It is the correct model for the device we are actually building; adaptive is a
  constraint-driven hack that would become load-bearing in M4 and would be paid
  for in DSP we have not budgeted.
- The fix is **two lines**; the adaptive switch is a descriptor rewrite *plus* an
  M4 rate-matcher debt.
- Copying USBPods' choice here would be copying its *workaround for a bug we can
  simply fix*, and inheriting its drift problem along with it.

**Consequence for M4, stated plainly:** with async feedback working, M4's job at
the USB seam is to drive the feedback value from the *Bluetooth-side* queue
depth rather than the USB FIFO — i.e. replace `AUDIO_FEEDBACK_METHOD_FIFO_COUNT`
in `tud_audio_feedback_params_cb` with a computed value once there is a real
consumer. That is a contained change in one callback, and it is the whole point
of having the endpoint. Under adaptive there is no such lever at all.

**The one cost we knowingly accept: this breaks Windows.** Upstream's matrix is
explicit — `10.14/3/3` is "Linux, OSX", never Windows; Windows' UAC2 driver has
a documented bug requiring 16.16 (`audio_device.h:501-502`). We have no Windows
test rig and macOS is the MVP host, so this is the right trade *today*. The
durable answer later is upstream's `CFG_QUIRK_OS_GUESSING` (two descriptors,
selected in `tud_descriptor_configuration_cb`), which is an isolated swap in one
function — deferring it is cheap-and-right, not debt. **Record it as a known
limitation in the bead; do not build it now.**

**Does Andreas need to decide this?** No. The evidence is upstream's own
comment and matrix, the cheap fix and the architecturally correct fix are the
same change, and the Windows cost is reversible behind a quirk we may add later.
Tell him what was decided and why; do not present a menu.

## 4. The fix — concretely, for Ruby

Three files. No descriptor length changes: `TUD_AUDIO_DESC_STD_AS_ISO_FB_EP_LEN`
is 7 regardless of `wMaxPacketSize`, so the config descriptor stays 227 bytes
and the `_Static_assert` at `usb_descriptors.c:111` keeps guarding it.

**Change 1 — feedback endpoint `wMaxPacketSize` 4 → 3.**
`firmware/src/usb_descriptors.c:101`, last argument of
`TUD_AUDIO_SPEAKER_STEREO_FB_DESCRIPTOR`:

```c
EPNUM_AUDIO_OUT, CFG_TUD_AUDIO_FUNC_1_EP_OUT_SZ_MAX, EPNUM_AUDIO_FB,
(TUD_OPT_HIGH_SPEED ? 4 : 3)
```

Write it speed-conditional even though RP2350 is full-speed-only — it documents
*why* 3, and it is the shape upstream uses.

**Change 2 — enable 16.16 → 10.14 conversion.**
`firmware/src/tusb_config.h:103`:
`#define CFG_TUD_AUDIO_ENABLE_FEEDBACK_FORMAT_CORRECTION 1`.

This makes `audiod_fb_send` (`audio_device.c:1187-1214`) both convert the value
and transmit 3 bytes instead of 4. Changes 1 and 2 must land **together** —
either alone leaves us in a row of the matrix that names only Linux.

**Change 3 — rewrite the comment at `tusb_config.h:95-102`.** It is the reason
this bug exists and it will re-cause it if left. It must say: format correction
is a compile-time flag inside `audiod_fb_send`, it involves no BOS descriptor
and does not collide with `reset_interface.c`; full-speed macOS requires 10.14
in 3 bytes (USB 2.0 §5.12.4.2, and TinyUSB's own matrix at
`audio_device.c:1203-1211`); this deliberately trades Windows compatibility,
whose eventual answer is `CFG_QUIRK_OS_GUESSING` — and *that* is the thing the
old comment's BOS reasoning correctly applies to.

**Change 4 — instrumentation, so one cycle is conclusive.** All plain
`volatile uint32_t` increments. **No string formatting anywhere in these
paths** — they run inside the 0xC0 worker IRQ via `tud_task()`
(`usb_audio.c:35-41`; probe 1's hang was formatting in exactly this context).
Formatting happens only in `pl_usb_pump_report`, in thread context.

- `s_set_itf_alt1_calls` — in `tud_audio_set_itf_cb` (`usb_audio.c:162-176`),
  incremented when `itf == ITF_NUM_AUDIO_STREAMING && alt == 1`. **Primary pass
  criterion.**
- `s_clock_set_calls` — in `clock_set_request` (`usb_audio.c:75`), currently
  uncounted. Tells us whether macOS ever sets the rate.
- Split `s_clock_get_calls` into `s_clk_get_freq_cur`, `s_clk_get_freq_range`,
  `s_clk_get_valid` inside `clock_get_request` (`usb_audio.c:52-73`). Cheap, and
  it retires the last ambiguity in the old `clock_get=5` reading.
- `s_fb_sends` — a counter in `tud_audio_feedback_interval_isr` (currently not
  implemented by us; TinyUSB's weak default at `audio_device.c:520` does
  nothing). Override it with a single `s_fb_sends++`. This proves the feedback
  endpoint is actually being serviced once alt 1 opens, which is the difference
  between "macOS opened the pipe" and "macOS opened it and we are feeding it".
- Expose each via a `pl_usb_audio_*` getter next to the existing ones
  (`usb_audio.c:259-281`) and add them to `pl_usb_pump_report`'s output line.

**Deliberately NOT changed in this cycle** (each is defensible, each would add a
confounding variable to a single-shot experiment):

- Clock Source `bmControls` `0x03` → `0x07`. Rev-1's change 1. USBPods proves
  `0x01` streams, so this cannot be the gate and cannot help. It is still a UAC2
  §4.7.2.1 conformance improvement — **file it as a follow-up bead**, do not
  land it here.
- `bmChannelConfig` → `FRONT_LEFT|FRONT_RIGHT`. Rev-1's changes 2 and 3.
  Refuted. Same treatment: follow-up, cosmetically correct, not now.
- AC interrupt endpoint. §2 residual candidate 2. Changes descriptor length.
- Function category / terminal type → `HEADSET` / `OUT_HEADPHONES`. Arguably
  *more* honest for this product and it changes the macOS icon. Cosmetic;
  bundle it into the follow-up bead.

**Constraints that hold, unchanged:** no string formatting in the 0xC0 worker
IRQ; `core/` is not touched and nothing here crosses the FFI seam;
`PICO_STDIO_USB_CONNECTION_WITHOUT_DTR=1` stays exactly as it is in
`firmware/CMakeLists.txt`; capture only via `tools/usb-console/cdc_reader.py`,
never the tty path.

**Branch base:** the audio code lives on the unmerged `bd-pico-link-icb`
worktree (`.worktrees/bd-pico-link-icb`). Ruby must branch from whatever ref
actually carries `firmware/src/usb_audio.c` — **not** a bare `main`. Verify the
base commit before starting.

**Commit shape:** changes 1+2+3 in one commit (they are one logical fix and must
not be bisected apart), change 4 in a second.

## 5. *(retracted — see §0)*

The rev-1 section 5, "if the answer turns out to be move to UAC1", is withdrawn.
Its premise — that USBPods is UAC1 and that this is where the evidence points —
is false. USBPods is UAC2 (`bcdADC 0x0200`). Nothing in the current evidence
argues for a UAC1 pivot, and rev-1's 48 kHz-ceiling cost analysis should not be
cited as a finding. The one line worth carrying forward: **if a UAC1 pivot is
ever proposed, it is an ADR with the sample-rate ceiling stated out loud and a
decision for Andreas, not a quiet descriptor swap.**

## 6. Verification plan

### Phase 0 — free host-side checks. Zero hardware cycles. Unchanged in content, changed in status.

**Status change:** in rev 1 Phase 0 was a *gate* ("stop and re-plan" on one
branch). It is now **informative, not blocking.** The §4 fix is two lines,
endorsed by upstream's own macOS quirk, and should ship in the one cycle
regardless of what Phase 0 says. Run Phase 0 in parallel; use it to interpret
the result, not to decide whether to build.

Run against the **current, unmodified** firmware:

1. Select "Pico Link Audio Dongle" as the macOS output device, then open
   **Audio MIDI Setup** and look at the device's **Format** popup.
   - **Empty / greyed / no output stream** → CoreAudio published no usable
     stream format. Consistent with alternate setting 1 having been discarded,
     i.e. consistent with §2.
   - **Shows "2 ch 16-bit Integer 48.0 kHz"** → AppleUSBAudio built an engine
     and the failure is at pipe-open rather than parse time. Still consistent
     with §2 (the feedback pipe can fail at open), but it makes C3 and the
     composite theory more live. **Note it; do not stop.**
2. `system_profiler SPUSBDataType | grep -A 20 "Pico Link"` — confirm the
   device, PID `0x000c`, current draw. Keep output trimmed; this runs in a
   subagent, not the main thread.
3. `log show --last 5m --predicate 'senderImagePath CONTAINS "AppleUSBAudio" OR process == "coreaudiod"' --info`
   immediately after a replug. **This is the highest-value check.** If it names
   an endpoint, a `wMaxPacketSize`, or a rejected alternate setting, the ranking
   in §2 collapses to a fact.
4. Rule out C3 properly: with the device selected, **actually play audio**
   (`afplay` a long file on a loop) and confirm the host believes it is
   playing — macOS sets alt 1 only when an application has an active stream.
   Clear stale state once: unplug, `sudo killall coreaudiod`, replug.

Phase 0 needs the board running application firmware. **As of 2026-08-29 it is
sitting in the RP2350 BOOTSEL bootloader (PID 0x000F)**, so Phase 0 cannot run
until something is flashed — see the orchestrator's note in the bead about
folding Phase 0 into the Phase 1 flash rather than spending a flash on it.

### Phase 1 — the one budgeted hardware cycle

**Build:** the §4 changes on a worktree branch off the branch carrying
`usb_audio.c`. `PICO_SDK_PATH=/Users/andreas/.pico-sdk/sdk/2.1.1`, toolchain at
`/Applications/ArmGNUToolchain/15.2.rel1/arm-none-eabi/bin`. The
`_Static_assert` at `usb_descriptors.c:111` must still pass (it will — no length
change). Confirm a `.uf2` is produced.

**Flash:** BOOTSEL + copy. Remote `picotool reboot -f -u` needs a healthy
control path, which a wedged board does not have.

**Capture:** `tools/usb-console/cdc_reader.py --pid 0x000c`. Direct libusb only,
one reader, one long capture, no open/close loops.

**Drive it:** select the device as output; play a continuous file for ≥30 s.

**Pass criteria — updated for the new fix. All of these, or it is not a pass:**

- `set_itf_alt1_calls >= 1`, **and** `set_itf_calls >= 2`, **and**
  `last_itf=1, last_alt=1`. **Primary criterion — this is the bead.**
- `streaming=1` while audio is playing.
- `fb_sends` increasing — proves the feedback endpoint is live and serviced,
  i.e. that the thing we fixed is the thing now working.
- `packets` increasing across **successive** report lines at roughly 1000/s.
  One non-zero reading is not proof; two readings with a plausible delta are.
- `pcm_bytes_total` increasing at roughly **192000 B/s** (48000 × 2 × 2). A rate
  materially off that means packets arrive but the format is misread — a
  different bug, worth knowing.
- No panic line from the panic recorder; board still answers `picotool` after.

**Capture `clk_get_valid`, `clk_get_freq_range`, `clock_set_calls` and
`fb_sends` even on success** — they are the record of what the host actually
did, and worth more than the pass itself.

**A zero is not a pass.** Two failure shapes must be read differently:

- `alt1_calls = 0` → the fix did not move the host. Go to §7.
- `alt1_calls >= 1`, `packets > 0`, then **frozen** → **that is progress, and it
  is the already-diagnosed `audio_device.c:759-762` re-arm defect from
  `pico-link-tfj`** (one FIFO overflow returns before re-arming ISO-OUT and
  kills the endpoint permanently). It becomes testable for the first time. Do
  not re-diagnose it as this bead.

## 7. Risks — what is still wrong if this does not work

- **The fix lands and macOS still holds alt 0.** Then AppleUSBAudio is rejecting
  the alt setting for a different reason. Next moves, in order and each cheap:
  (a) re-run Phase 0 step 3's `log show` against the *fixed* firmware — a parser
  that got further usually fails later and more verbosely; (b) add the AC
  interrupt endpoint (§2 residual candidate 2, matching USBPods); (c) build a
  **temporary audio-only** firmware — drop CDC and the reset interface, audio at
  interfaces 0/1 only — reducing us to exactly upstream's `uac2_speaker_fb`
  topology. If audio-only streams, the defect is in the composite. **(c)
  sacrifices the CDC console**, so instrument via the display or an LED, and
  budget it as a separate hardware cycle Andreas has to approve.
- **Only then does adaptive become live.** If async cannot be made to work on
  macOS at all, dropping the feedback endpoint and going adaptive (USBPods'
  shape) is the fallback — with the M4 software-rate-matching debt from §3
  written into an ADR and accepted explicitly, not slipped in.
- **Windows regresses.** Known, accepted, recorded in the bead. Answer later is
  `CFG_QUIRK_OS_GUESSING`.
- **C3 was the truth all along.** Phase 0 catches it. Do not skip it because it
  feels like a formality — this project's record says the formality is where the
  answer usually is.

## 8. Sustainability note

The proximate cause was not "we copied an example". It was that we copied an
example's **macro** and its **Windows call site**, while upstream selects a
*different call site* for full-speed macOS — and then wrote a comment
(`tusb_config.h:95-102`) that reasoned correctly about the OS-sniffing
machinery and used that reasoning to switch off a plain, unrelated,
one-line format flag. The comment was thoughtful, well-written, and led us
wrong for a session. That is the failure mode worth naming: **a well-argued
comment about the adjacent thing is how a wrong default survives review.**

Two durable corrections, both cheap:

1. When Ruby lands §4, annotate the feedback endpoint fields with the USB 2.0
   §5.12.4.2 requirement, the `audio_device.c:1203-1211` matrix, and the
   Windows trade — so the next person changing them sees the constraint, not a
   plausible-sounding reason to revert. A comment, not architecture.
2. **Record in the bead that our descriptor is host-specific.** It is a real
   product property (macOS-first, Windows deferred), not a bug, and it should be
   visible rather than buried in a `tusb_config.h` define.

Do **not** build a descriptor-generation abstraction, a lint harness, or the
OS-guessing quirk now. One audio function, one host, one hardware cycle —
gold-plating a PoC is the other failure mode.
