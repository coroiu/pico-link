# Media keys: AVRCP passthrough -> USB HID consumer control

Bead: `pico-link-5dh` (epic). Date: 2026-09-02. Author: Ada.
Status: design of record. Risk-clearing pass, not a treatise.

## 1. What is being built

The headphones' play/pause and skip buttons send AVRCP **passthrough** commands
to us. We expose a USB **HID consumer-control** interface to the host and
re-emit them as consumer usages, so the host's media player responds.

## 2. THE RISK: interface numbering and the flash path

### 2.1 What the numbers are today

`firmware/src/usb_descriptors.h:52-58`:

| # | Interface | Note |
|---|-----------|------|
| 0 | Audio Control | inside the audio IAD |
| 1 | Audio Streaming | alt 0 / alt 1 |
| 2 | CDC Communication | |
| 3 | CDC Data | the console's bulk endpoint |
| 4 | RPI reset (vendor) | **the BOOTSEL control transfer target** |

`ITF_NUM_TOTAL` = 5. Endpoints: EP1 OUT audio / EP1 IN feedback, EP2 IN CDC
notif, EP3 OUT/IN CDC data (`usb_descriptors.c:34-38`).

### 2.2 What each tool actually depends on

Measured by reading the tools, not the CLAUDE.md summary:

- **`cdc_reader.py` and `cdc_sender.py` both select by CLASS CODE**
  (`find_cdc_data_interface`: `intf.bInterfaceClass == CDC_DATA_CLASS`), with an
  explicit comment at `cdc_reader.py:59-63` saying interface indexes already
  moved once in M3 and that is exactly why. **Both are immune to renumbering.**
  Their only fixed coupling is `DEFAULT_PID = 0x000C`.
- **The iface-4 control transfer is NOT in a checked-in tool.** It is a
  hand-run snippet (memory note `picotool-is-broken-not-the-control-transfer`):
  `claim_interface(d, 4)` then `ctrl_transfer(0x21, 0x01, 0x0000, 4, ...)`.
  The literal `4` appears twice and there is no class-code lookup.
- The **firmware** side is already dynamic: `usb_reset.c:37` captures
  `itf_desc->bInterfaceNumber` at bind time and `:72` compares `wIndex` against
  it. So the firmware follows the descriptor wherever it moves; only the
  hand-run snippet is hardcoded.

### 2.3 The ordering that preserves everything

**Append HID after RESET.** `ITF_NUM_HID` goes last:

| # | Interface |
|---|-----------|
| 0-3 | unchanged |
| 4 | **RPI reset (vendor) -- unchanged** |
| 5 | HID consumer control (new) |

`ITF_NUM_TOTAL` = 6. New endpoint `EPNUM_HID_IN = 0x84` (EP4 IN, interrupt).

With this ordering: **iface 4 still means the reset interface, the hand-run
BOOTSEL snippet is unchanged, and both CDC tools are unaffected.** The flash
path survives with zero tool changes.

There is no host requirement pushing HID earlier. bInterfaceNumber must be
ascending across the config descriptor (Windows is strict about this); appending
satisfies that trivially. HID is a standalone interface and needs no IAD, so it
does not have to sit adjacent to anything.

### 2.4 What happens if someone inserts HID in the middle instead

This is the failure mode to name explicitly, because it is **silent**. If HID
lands at index 4 and RESET slides to 5, the hand-run snippet's
`ctrl_transfer(0x21, 0x01, wIndex=4)` becomes bmRequestType class/interface,
bRequest `0x01` -- which on a HID interface is **HID SET_REPORT**. TinyUSB routes
it to the HID driver, which will accept or stall it. Either way the board does
not reboot, and there is no error that says "wrong interface". You get
"BOOTSEL stopped working" with no diagnosis, on the exact path you need to
recover the board. Hence: **append, and treat `ITF_NUM_RESET`'s position as a
frozen constant.** A comment in `usb_descriptors.h` saying so is part of task 1.

### 2.5 Product ID: keep 0x000C

Adding an interface changes the config descriptor. The tempting move is to bump
PID 0x000C -> 0x000D as a cache-invalidation signal, as M3 did. **Do not.**
`0x000C` is baked into both tools' defaults, the BOOTSEL memory note, and the
`--vid/--pid` habit. Bumping it costs a sweep of the whole tooling surface for a
speculative benefit; macOS keys its audio cache on VID/PID/serial and our serial
is the per-board unique ID, which does not change.

Bump `bcdDevice` `0x0100` -> `0x0101` instead: a free, no-tool-impact version
signal. **Contingency:** if macOS misbehaves after the first flash (stale audio
config, refused alt setting), the fallback is a PID bump *plus* updating both
tool defaults and the memory note in the same commit. Name it, do not pre-pay it.

### 2.6 Verdict

**The flash path survives, unchanged, with no tool edits required** -- provided
HID is appended after RESET. Nothing else in this design touches it.

## 3. AVRCP side: which handler, and press/release

### 3.1 We are the TARGET for passthrough

`firmware/src/a2dp.c:2174-2179` registers all three handlers. Passthrough from
the headphones arrives at **`pl_a2dp_avrcp_target_packet_handler`** (currently a
stub at `a2dp.c:698`). Verified in BTstack 2.1.1:
`src/classic/avrcp_target.c:1092-1109` handles `AVRCP_CMD_OPCODE_PASS_THROUGH`
and emits `AVRCP_SUBEVENT_OPERATION` from `avrcp_target_emit_operation`
(`avrcp_target.c:85-96`). The controller module only emits
`AVRCP_SUBEVENT_OPERATION_START/COMPLETE`, which are for commands *we* send.

We are both target and controller on the wire (both SDP records are registered,
`a2dp.c:2190-2201`); **for this feature we act only as target.** The controller
registration stays for volume/absolute-volume later.

### 3.2 Press and release are both already delivered

`AVRCP_SUBEVENT_OPERATION` carries a **`button_pressed`** field
(`btstack_event.h:11566`), derived at `avrcp_target.c:1102` from
`(packet[6] & 0x80) == 0`. Press and release arrive as two separate events with
the same `operation_id`.

**BTstack already auto-responds** -- `avrcp_target_operation_accepted()` is
called at `avrcp_target.c:1104` before the event is emitted. We must NOT call
`avrcp_target_operation_accepted/rejected` ourselves; doing so would double-send
the AVCTP response.

### 3.3 Key-state model

One-key-at-a-time is sufficient (headphone media buttons are not chorded).

- press event -> set current usage, send report
- release event -> clear usage, send empty report
- **safety timeout, 600 ms**: if no release arrives, force the empty report.

The timeout is not optional. A lost release, a link drop mid-press, or a sink
that only sends press leaves the host holding a media key down. It is cheap
(one `time_us_64()` comparison in the superloop drain) and it is the difference
between "works" and "occasionally wedges the host's media stack".

### 3.4 Usage mapping

| AVRCP operation | HID Consumer usage |
|---|---|
| `AVRCP_OPERATION_ID_PLAY` (0x44) | `0x00CD` Play/Pause |
| `AVRCP_OPERATION_ID_PAUSE` (0x46) | `0x00CD` Play/Pause |
| `AVRCP_OPERATION_ID_FORWARD` (0x4B) | `0x00B5` Scan Next Track |
| `AVRCP_OPERATION_ID_BACKWARD` (0x4C) | `0x00B6` Scan Previous Track |
| `AVRCP_OPERATION_ID_STOP` (0x45) | `0x00B7` Stop |
| anything else | ignored (no report) |

PLAY and PAUSE both map to the 0x00CD toggle rather than the discrete
`0x00B0`/`0x00B1`. Rationale: the XM3 decides which of PLAY/PAUSE to send from
*its own* view of playback state, which we never update (we do not implement
`avrcp_target_set_playback_status`). Mapping both to a toggle makes the feature
correct regardless of whether the headphone's state view matches the host's.
Discrete play/pause is the better long-term answer *once* we report playback
status upstream -- that is a later bead, not this one.

## 4. Send path: the TinyUSB context constraint

**`tud_task()` does not run in the superloop.** Since `pico-link-tfj` it runs in
a 1 ms user IRQ worker at priority 0xC0 (`usb_pump.h:19-31`, `main.c:390-395`).
So `tud_hid_report()` cannot be called from wherever we feel like it.

Two separate hazards, both already solved by existing patterns -- reuse them,
do not invent:

1. **BTstack context is not superloop context.** The AVRCP handler runs on the
   cyw43/BTstack background IRQ (priority 0xFF). This is the same constraint as
   `pico-link-6o2`. The handler must **only push into a ring** and return.
2. **TinyUSB is owned by the 0xC0 worker.** The superloop drains the ring and
   calls `tud_hid_report()` guarded by `pl_usb_lock_try()` /
   `pl_usb_unlock()` (`usb_pump.h:94-103`). Contract: never spin, skip the tick
   on failure.

Consequence for correctness: **do not pop the ring entry until the report is
actually accepted.** `tud_hid_report()` also returns false when the endpoint is
still busy. A dropped media keypress is a user-visible bug and a dropped
*release* is the stuck-key bug from 3.3. Peek, send, pop on success.

Use a **dedicated small ring in a new `firmware/src/media_keys.c`**, not the
`bt.c` UI event ring. The UI ring's contract is "events for Rust"; media keys
never enter Rust on the MVP path. Overloading it would blur the FFI seam for no
gain. This is a ~40-line SPSC ring of `{usage, pressed}` -- cheap and contained.

## 5. Display feedback: deferred, not designed away

Not in the MVP path. If wanted later it is a new `PlEvent` at **tag 14**
(`pico-link-du0` took tag 13 for `LevelsChanged`), additive, `PL_EVENT_ABI_VERSION`
stays 4, pushed from the superloop drain (not the BT handler). Filed as an
optional child so it does not gate the feature.

## 6. Config changes required

`firmware/src/tusb_config.h`: `CFG_TUD_HID 0` -> `1`, add
`CFG_TUD_HID_EP_BUFSIZE 16`.

`firmware/src/usb_descriptors.c`: `EPNUM_HID_IN 0x84`; add `TUD_HID_DESC_LEN` to
`USBD_DESC_LEN`; add `TUD_HID_DESCRIPTOR(ITF_NUM_HID, STRID_HID,
HID_ITF_PROTOCOL_NONE, sizeof(desc_hid_report), EPNUM_HID_IN, 8, 10)` as the
LAST entry; `STRID_HID` added to the string enum after `STRID_RESET`.
Report descriptor: `TUD_HID_REPORT_DESC_CONSUMER()`, no report ID, single
16-bit usage field. Callbacks `tud_hid_descriptor_report_cb`,
`tud_hid_get_report_cb` (return 0), `tud_hid_set_report_cb` (no-op).

The existing `_Static_assert(sizeof(usbd_desc_cfg) == USBD_DESC_LEN)` at
`usb_descriptors.c:129` already catches a descriptor-length arithmetic mistake
at compile time. Good -- no new safety net needed.

USB DPRAM: adding one 8-byte interrupt IN endpoint to a budget currently holding
~192 B audio ISO + 3 B feedback + 3x64 B CDC is not a concern on RP2350's 4 KB.
The TinyUSB RP2040 DCD asserts if it is, so the build proves it.

## 7. Task breakdown (epic children)

**T1 -- HID interface + descriptors only, no AVRCP, no key logic.**
Appends `ITF_NUM_HID = 5` after `ITF_NUM_RESET`, freezes RESET's index with a
comment, bumps `bcdDevice`, wires the three HID callbacks. Sends nothing ever.
*Proves:* the host enumerates a consumer-control device; **audio still plays**;
`cdc_reader.py` still captures; and the **iface-4 BOOTSEL snippet still
reboots the board**, unchanged. This is the risk-clearing child and must land
and be flashed **alone**, before anything else. If this child is red, the whole
design is wrong and nothing downstream is worth writing.
-> Ruby, then Tess on hardware.

**T2 -- `media_keys.c`: ring + superloop drain + release timeout.**
SPSC ring, `pl_usb_lock_try`-guarded `tud_hid_report()`, peek/send/pop-on-success,
600 ms safety release. Exercised via a new `PL_DEBUG_REMOTE` console command
(`MEDIA PLAYPAUSE` / `NEXT` / `PREV`) -- **no Bluetooth involved**.
*Proves:* the USB half works end to end (a console command pauses Spotify on the
host) independently of AVRCP, so a T3 failure is unambiguously an AVRCP failure.
Depends on T1.
-> Ruby.

**T3 -- AVRCP target passthrough -> ring.**
Fills in `pl_a2dp_avrcp_target_packet_handler`: filter `HCI_EVENT_AVRCP_META` /
`AVRCP_SUBEVENT_OPERATION`, read `operation_id` + `button_pressed`, map per 3.4,
push. No TinyUSB calls, no Rust calls, no `avrcp_target_operation_accepted`.
*Proves:* pressing the button on the XM3 pauses and skips on the Mac, and
**holding it does not stick** (the release path).
Depends on T2.
-> Ruby, then Tess on hardware with the XM3.

**T4 (optional, defer) -- on-screen media-key feedback.**
`PlEvent` tag 14, additive, ABI version unchanged. Only if Uma/Andreas want it.
Depends on T3.

## 8. Risks

- **Silent BOOTSEL breakage if HID is not appended last.** Mitigated by T1's
  standalone hardware verification and the frozen-index comment. This is the
  one that costs physical button holds for the rest of the project.
- **macOS caching a stale config descriptor.** Contingency in 2.5.
- **A sink that never sends release.** Mitigated by the 600 ms timeout (3.3).
- **Dropping a report on a busy endpoint.** Mitigated by pop-on-success (4).
- Not a risk: DPRAM, descriptor length arithmetic, the CDC tools.

## 9. What this design deliberately does not do

No absolute volume, no metadata / now-playing, no playback-status reporting, no
discrete play vs pause. Each is a defensible later bead; none is needed for
"skip a song from my headphones", and 3.4's toggle mapping is chosen precisely
so that omitting playback-status reporting is *correct* rather than a latent bug.
