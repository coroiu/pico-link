# On-device UI design (v5, design of record)

Bead: `pico-link-aii.2` (epic `pico-link-aii`). Author: Uma (ux-designer),
reconciled against Ada's capability inventory (`pico-link-aii.1`) and Andreas's
sketch. Supersedes v1-v4 in the bead comments; written to be implemented from,
not read as history.

## 1. Goals, ranked by frequency

| Frequency | Goal | Budget |
|---|---|---|
| Every use | "My audio is in my headphones" | **zero presses** |
| Every few days | "Is it actually LDAC, or did it quietly fall back?" | **zero presses - already on screen** |
| Weekly | Adjust volume | 1 press |
| Weekly | Switch headphones | 2 presses |
| Rare | Pair new headphones | many presses, allowed |
| Almost never | Settings | buried is fine |

**Home is a status display, not a menu.** Menus are reached *from* Home; they
never stand in front of it.

## 2. Platform constraints this design is built on

- 240x240, read at 30-50cm. 5-way d-pad + A/B/X/Y along one edge.
- **Press edges only. No key repeat, no long-press** (`firmware/src/input.c:120`).
- **~18fps, full-frame redraw only.** No partial update.
- **The core has no clock** (`ui-ffi/src/lib.rs:351` discards `now_us`). Every
  time-varying element must be driven by an event from C.
- Scan 10.24s fixed. Connect-to-audio 2-8s. ACL page timeout up to 5.12s per
  attempt. Boot 1.5s + 1-2s radio.
- Impossible, designed nowhere: now-playing metadata, battery, dual-sink,
  on-screen text entry.
- Volume is device-side via AVRCP. The host slider will not move.
- Pairing is Just Works. Device names may be **permanently** absent.

Two rules follow directly:

> **No list may exceed ~12 items.** No key repeat means one press per row.
> Nothing here exceeds 7. A longer list is a design failure, not a scrolling
> failure - do not add auto-repeat.

> **Maximum navigation depth is 2.** With no long-press, `B, B` is the only
> reliable escape, and it only works if depth is capped and B is inviolable.

## 3. Layout and chrome

```
 +--+--------------------------------+-----+  y=0
 |  | (bt) PICO LINK           (usb) |     |  title bar 16px, full width
 +--+--------------------------------+  A  |  y=16
 |[]|                                | devs|
 |[]|                                +-----+
 |[]|                                |  B  |
 |[]|          content               |     |
 |[]|         188 x 224              +-----+
 |  |                                |  X  |
 |  |                                | link|
 |V |                                +-----+
 |O | O                              |  Y  |
 |L | U                              | set |
 +--+--------------------------------+-----+  y=240
  gauge 18px                          rail 34px
  (d-pad edge, Home only)             (button edge, every screen)
```

The **button rail** labels A/B/X/Y in physical order, `helvR08`,
`TEXT_SECONDARY` live / `DIVIDER`-dim inert. It earns its 34px because four
unlabelled buttons on a device you touch once a month are four buttons you
never press. The **gauge column** sits on the edge *opposite* the rail - the
d-pad edge - so the volume control and its display are under the same thumb.

> **BLOCKING, PARAMETERISED:** the physical A/B/X/Y arrangement is unknown and
> the panel rotation (`pico-link-zzq`) is unresolved. The rail's edge and slot
> order must derive from the same constant that maps GPIO to `NavIntent`, so a
> rotation flips labels *with* the buttons instead of silently lying. The gauge
> binds to the opposite edge off that same constant, so they can never collide.

## 4. Global input contract

| Input | Meaning on every screen |
|---|---|
| Up / Down | Move focus. Never activates. |
| Right | Identical to A. |
| Left | Identical to B. |
| Centre | Identical to A. |
| **A** | Activate the focused thing / confirm. |
| **B** | Back, cancel, dismiss. Aborts in-flight operations. |
| **X** | Primary contextual action - **always labelled, or inert.** |
| **Y** | Secondary contextual action - **always labelled, or inert.** |

1. **A is never irreversible.** Destructive actions are labelled rows behind a
   `ConfirmView` focused on Cancel.
2. **An unlabelled X or Y does nothing.** A mispress is always free.
3. **B never navigates forward, on any screen, ever.** It is the only escape
   gesture and its reliability is the point.

> **The Home exception, stated once:** Home has no focusable list, so "move
> focus" and "activate the focused thing" are vacuous there. On Home *only*,
> **Up/Down = volume, centre = toggle the menu face.** A/B/X/Y keep their global
> meanings. Exactly one screen wide; never repeated.

X's label may change with context - that *is* the contract, not a violation.

| Screen | X | Y |
|---|---|---|
| Home (status) | Link detail - **label becomes `why?` under fallback** | Settings |
| Home (menu) | Manage connected device | - |
| Devices | Manage selected device | - |
| Device detail | Disconnect *(when connected)* | - |
| Pair: scanning | Rescan | - |
| Pair: not responding | Keep trying | - |
| Pair: failed | Try again *(where meaningful)* | - |
| Pair: degraded success | Codec settings | - |

## 5. Screen inventory

Home (status + menu faces), Devices, Pair wizard (one screen, phased), Device
detail, Codec picker, Settings, Confirm (`confirm.rs`, exists).

Every path verifies at depth <= 2:

```
Home(0) [status <-> menu]  -> Devices(1)       -> Pair wizard(2)
                           -> Devices(1)       -> Confirm forget(2)
                           -> Device detail(1) -> Codec picker(2)
                           -> Settings(1)      -> Confirm reset(2)
```

## 6. Home - status face

```
 |[]| Sony WH-1000XM5                |  A  | devs
 |[]|                                |  B  |
 |[]|      L D A C                   |  X  | link
 |[]|      909 kbps                  |  Y  | set
 |VO| OUT  LINK ||||.  USB 48k 24-bit|
```

| Element | Font | Colour |
|---|---|---|
| Codec word (hero) | **`font::hero()` - NEW, ~20-26px** | see below |
| Device name | `font::name` (helvB12) | `TEXT_PRIMARY` |
| Bitrate | `font::value` (helvR12) | `TEXT_PRIMARY` |
| Stat labels | `font::label` (helvB08) uppercase | `TEXT_SECONDARY` |

The hero is the design's central bet: *"is it actually LDAC"* is answered by
glancing across a desk, and a `helvB12` value in a label-value list cannot do
that. `helvB12` is the current ceiling in `theme.rs`.

**Codec word colour is the reassurance mechanic:** `TEXT_PRIMARY` = you got what
you asked for; **`STATUS_WARNING` amber = you got less than you asked for** (SBC
instead of LDAC is not an error and must not be red - it is a downgrade you must
*notice*); `STATUS_ERROR` = no link.

**Device name truncates with an ellipsis. Never a marquee** - a full-frame
redraw forever at 18fps, and unreadable in the 1.5s a glance lasts.

**Numeric rules.** Bitrate at **1Hz, smoothed, snapped to the nominal LDAC
ladder** (330/660/909/990) within tolerance: raw adaptive bitrate jitters, and
`894 -> 912 -> 901` reads as an unstable link when the link is perfect. Steady
digits mean "fine"; moving digits should mean "something changed." Every number
is right-aligned into a fixed slot cleared to `BACKGROUND` first.

> **Motion rule.** No *smooth* animation on Home. Coarse, quantised,
> event-driven motion only, **capped at 4Hz, frozen entirely when the value is
> static.** Home therefore moves only while audio is actually flowing - exactly
> when "it's working" is the message - and is perfectly still otherwise.
> Budget: 1Hz already for bitrate; 4Hz costs 3 extra frames/sec ~= 11.6% duty.

`VOL` is the AVRCP setting; `OUT` is the live output level, 6-8 segments per
channel with peak-hold and a `STATUS_ERROR` cap. **The OUT meter is a
commissioned feature, not one drawn in the sketch** - proposed with its redraw
cost and M3 dependency stated, and accepted on that basis.

### 6.1 Home states - all nine

1. **Boot** - brand mark + "Starting radio..." flowing **straight** into (2)
   with no transition, so the user perceives one continuous ~4s wake-up.
2. **Auto-reconnecting** - MRU device only.
3. **First run, nothing paired** - "No headphones paired", "**A** to pair".
4. **Connected, host silent** - bitrate reads **"idle"**, never `0 kbps`; zero
   looks broken. OUT frozen at floor.
5. **Connected, streaming** - nominal.
6. **Connected, fell back** - see 6.2.
7. **Muted** - see 6.3.
8. **Lost connection** - "retrying (2)"; the attempt count makes a retry loop
   legible instead of eerie.
9. **Gave up** - "Couldn't reconnect. **A** Devices." Always exits into an action.

**Auto-reconnect: MRU device only, backoff. Never silently hop to a different
paired device** - waking up connected to headphones you did not choose is worse
than not connecting, because it is inexplicable.

**Sleep:** dim ~30% after 60s, blank after 5 min. **Never fully blank while a
fallback or muted banner shows - dim only.** A warning you must press a button
to read has failed.

### 6.2 The fallback chain - the most important part of this document

> **The "fell back to SBC" state appears nowhere in Andreas's sketch.** He drew
> `Codec: LDAC` and `Codec: -`. The third case - *connected, but worse than you
> asked for* - is the single reason this product has a screen, and it was not
> drawn. This section is the resolution and must survive review intact.

Five links, glance to remedy, each required:

1. Home hero codec word turns **amber** - visible across a room.
2. A **persistent banner** states what happened and why. **Not a toast** - the
   value is entirely in its persistence; you glance an hour later and the answer
   is still there.
3. The **X rail label changes from `link` to `why?`**.
4. The wizard's degraded-success phase **does not auto-dismiss** (S9 phase 6).
5. The codec picker shows LDAC **dimmed with its reason inline** (S11).

**Banner priority:** at most one. `MUTED` outranks `FALLBACK`; the codec word
stays amber underneath regardless.

### 6.3 Muted

Device-side volume at zero with the host slider unaware. A user who mutes here
and forgets will blame the host, reinstall drivers, and file an issue. Amber
persistent banner: "MUTED / Press Up to raise".

- **~8 steps across AVRCP 0-127** (~16 per press). No key repeat, so one press
  is one step; 127 presses is not a design.
- **Label it `HEADPHONES`, never `VOLUME`** - two gain stages in series, and the
  host slider will not move. Name the control after what it controls.
- **If the sink lacks AVRCP absolute volume, gauge and binding are both ABSENT,
  not inert.** A bar that does not move on press is a bug report.

## 7. Home - menu face (centre toggles)

Rows: Bluetooth, Settings. A **face**, not a pushed screen - as a push,
`Home -> Menu -> Devices -> Pair` is depth 3 and breaks `B, B`. B returns to the
status face. It is the discoverable path for a user who has not learned the
rail; the rail is the fast path once they have.

**No `Home` row** - B already does it, and a row meaning "go back" is wasted on
a 240px screen. **X = manage connected device**, preserving the intent of
Andreas's "B-button on main menu gets you to managing the currently connected
device" without letting B navigate forward.

## 8. Devices (A from Home)

Rows show name + status sublabel; **most-recently-connected first**, which is
the mitigation for nameless devices (one you just paired is at the top, where
you recognise it by position).

- **A** on another paired device -> connect. No confirm; reversible, and it is
  the 2-press job. Switching takes 2-8s and **reuses the wizard's phase
  display**, so it never looks instant and then isn't.
- **A** on the connected device -> its detail page.
- **A** on "Pair new headphones" -> the wizard. On first run this is the *only*
  row, so it is maximally discoverable exactly when discovery matters.
- **X** -> manage the selected device without connecting.

**Noun-then-verb, deliberately.** A verb-first menu (`Change device` / `Forget` /
`Manage existing`) routes to the same device list three ways and forces the user
to decide what they intend before seeing which device they mean. Recognition
comes first.

## 9. The pairing wizard

**One screen at depth 2 whose content advances through phases. Phases replace,
never push.** B from any phase aborts the whole flow back to Devices - which
also removes any way to reverse into a stale scan list.

**Phase 1 - instructions, before any scanning.** "Put your headphones in pairing
mode now. Usually: hold power ~5s until the light flashes." Inquiry only finds
devices *already* in pairing mode; scanning first and showing an empty list is
the most common way this flow fails, and it fails looking like broken hardware.
One press converts that entire failure class into "you haven't done step 1."
The human wait is unbounded, so this screen is instructional - **no timer, no
spinner, nothing moving.** The user is the one doing work.

**Phase 2 - scanning (10.24s).** Four rules, each preventing a specific failure:

1. **Stable sort: first-seen order, append at bottom only. Never sort by RSSI.**
   RSSI moves constantly; a re-sorting list moves the row out from under the
   cursor between "I see it" and "I press A", and the user connects to the wrong
   device without knowing why.
2. **Row identity is the BD address.** A late name replaces the label in place;
   the row never re-enters at the bottom.
3. **Filter by Class-of-Device to audio sinks.** Every phone and laptop in the
   room is noise the user cannot disambiguate. This keeps the list under 12.
4. **Signal as a 4-bar glyph here; dBm only in detail.**

**No elapsed timer** - no clock, and rows appearing are a better progress
indicator anyway. **B cancels the scan.** Without a cancel command this screen is
a 10.24s dead end where B does nothing, the user concludes the device is frozen,
and that trust cost is permanent. In the zero-results case there is literally
nothing on screen for 10.24s - precisely when someone reaches for a button.

**Phase 3 - nothing found.** Repeat the instruction; "None found" alone gives the
user nothing to act on.

**Phase 4 - connecting.** Named sub-steps advanced by C events: **1 Connecting**
(ACL), **2 Pairing** (SSP/link key), **3 Setting up audio** (AVDTP discover +
configure), **4 Negotiating codec**. Each step fails differently; naming the
current one tells the user *and us* where it stalled. B genuinely aborts.

**Phase 5 - "Not responding", surfaced at ~6s while retrying behind it.** A page
timeout is up to 5.12s per attempt; silently retrying three times means 15.4
seconds of dead screen, and the common real case - headphones asleep in a bag -
burns the full timeout every time. Surfacing at 6s is honest (we *are* still
trying), hands the user the choice to bail, and the incrementing attempt counter
is free liveness on an otherwise motionless screen.

**Phase 6 - outcomes.** Success auto-dismisses after ~2s via a **C-side timer
event**, safe here specifically because the destination shows the same
information permanently. **Degraded success does NOT auto-dismiss** - the one
screen in the product that requires acknowledgement, because everything
downstream of the value proposition depends on the user noticing it.

Named failure causes, because the remedies differ:

| Cause | Message | Offered |
|---|---|---|
| Timeout / no response | "No response. Are they switched on and in range?" | X Keep trying / B Back |
| Rejected / auth failed | "Pairing was refused. Try removing this dongle from the headphones' paired list." | X Try again / B Back |
| No A2DP sink | "These don't accept audio." | **B Back only** |
| Needs a PIN | "These headphones need a PIN, which this dongle can't enter." | **B Back only** |
| Radio error | "Bluetooth error (0x%02X)." | X Try again / B Back |

**No retry offered for structurally impossible cases.** Retrying something that
cannot work is cruel.

## 10. Device detail (X from Home, or A/X from Devices)

Full untruncated name in the title. Rows: CODEC (activatable), LDAC QUALITY
(activatable, only when LDAC is active), SAMPLE RATE, SIGNAL, USB IN, A2DP,
ADDRESS, and **`Forget this device` as a labelled row at the bottom** - a
destructive action is safer as a row you must scroll to than a button you can
fat-finger - via `ConfirmView` focused on Cancel. A caret marks activatable rows
only. For a paired-but-not-connected device, the same screen with live fields
dashed.

**Degraded-field rule: if a value is unavailable, show the label with an
em-dash. Never a fake zero, never a hidden row** - a missing row is
indistinguishable from a layout bug. One exception: `SIGNAL`, see S13.

## 11. Codec picker (A on the CODEC row)

A **constrained** picker: we choose the SEP as the source, but LDAC cannot be
forced onto a sink that does not advertise it, and only SBC is mandatory. The
set is the intersection of what we support and what *this* headphone offers,
computed per device after AVDTP discovery - which is why there is no global
codec preference anywhere in this design. The payoff: diagnosis and remedy are
one gesture - glance at amber `SBC` -> X -> CODEC row -> A -> here is exactly why.

1. **Unavailable entries are shown, dimmed, with the reason inline.** Hiding
   them means the user never learns why they cannot have LDAC, and blames the
   dongle.
2. **Focus MUST land on disabled rows** - the reason text is the payload.
   Focusable, not activatable; A does nothing. **New `MenuList` capability.**
3. **SBC is labelled "always available"** - mandatory in the spec, and saying so
   turns a mystery into a fact.
4. **LDAC quality (990/660/330/ABR)** is a separate, unconstrained picker.
5. Applying costs a 2-8s reconnect. **Ask: "Reconnect to apply? A Yes / B
   Later."** Never silently do nothing; never silently drop the link.

## 12. Settings (Y from Home)

Four items: auto-connect on plug-in; screen (brightness, dim/blank timeout);
device info; forget all / factory reset (via `ConfirmView`).

Absent deliberately: **codec** (per-device, on the device page), **volume** (on
Home's d-pad), **EQ** (does not exist), **sample rate** (host-owned). Exposing
any of these would be lying about what we control.

> **A setting with no persistence behind it does not appear.** A toggle that
> forgets is worse than no toggle.

## 13. Data dependencies - degraded or cut

| Field | Status | Ruling if unavailable |
|---|---|---|
| **Paired list / link keys** | **LOAD-BEARING** | Devices *and* the zero-press goal both collapse. Not degradable. |
| Device name | May be **permanently** absent | `(unknown device)` + address tail as a **first-class label, not a placeholder** - no ellipsis, nothing implying pending. Scan-list signal bars and MRU ordering are the discriminators. No text entry means the user can never fix it. |
| Codec, availability + reason, LDAC quality, nominal bitrate | Confirmed | - |
| Live/adaptive bitrate | Unconfirmed | **Drop the "(adaptive)" qualifier**; never fake a live number. |
| Sample rate / bit depth | Expected | Em-dash, row stays. |
| `USB IN` rate | **Gated on M3** | Em-dash until M3 lands. |
| **`SIGNAL` row + Home LINK bar** | **Unconfirmed** (inquiry RSSI != connected RSSI) | **CUT the row and the bar entirely - do not dash them.** The one exception to the em-dash rule: a permanently-dashed signal meter is a recurring visible admission of ignorance on the screen the product's credibility depends on. Does **not** affect the scan list, which uses inquiry RSSI we have. |
| **`OUT` level meter** | **Unconfirmed, gated on M3** | **CUT entirely; keep VOL. A frozen VU meter reads as silence when it means no data** - the most misleading object you can put on an audio device. A cut, not a dash. |
| Peak/clip red cap | Unconfirmed | **Drop the cap, keep the bar.** Never infer a clip indicator from an average. |
| `VOL` gauge + Up/Down binding | Unconfirmed | **Absent together, not inert.** |
| Class-of-Device | Expected | Without it, cap the scan list at 12 with a "showing 12 of 31 - X to rescan" readout. Do not reach for auto-repeat. |
| Scan elapsed timer | **CUT** | No clock; streaming rows are a better indicator. |
| Now-playing / battery / dual-sink | **CUT** | Impossible. |

**On metadata being impossible: this is *why* the codec hero works.** Track title
and artist would compete for the hero slot and win - people look at song names -
and the reassurance goal would be pushed into a sub-screen, taking the product's
differentiator with it. Dual-sink is impossible (`MAX_NR_HCI_CONNECTIONS` is 1);
fast switching (S8) is the answer and is more valuable anyway.

## 14. Handoff

**Fern - components.** F1 edge button rail (`compute_chrome` is vertical-stack
only; `ChromeContribution` carries one free-text `hint`; needs `a/b/x/y` labels
and a **parameterised edge and slot order**) - largest ask, most user value.
F2 `font::hero()` ~20-26px. F3 hero/status widget owning its internal layout
(the `ConfirmView` adapter pattern). F4 persistent banner slot (MUTED > FALLBACK,
at most one). F5 vertical gauge column bound to the edge opposite F1 off the same
constant. F6 **disabled `MenuItem`: dimmed, focusable, non-activatable, inline
reason**. F7 key/value field list - **check `MenuList` + `Trailing` first**.
F8 phase/wizard screen replaced by C events without pushing (this is what keeps
depth at 2). F9 Home two-face toggle. F10 4-bar signal glyph (conditional for
Home/detail on the RSSI ruling; unconditional for the scan list). F11 live list
with **stable identity** - append and update in place without resetting selection
by position. F12 rename `HidLinkState` -> `LinkState` (`widget.rs:141`), a
Bitwarden HID leftover; the chrome currently lies about what the glyph means.

**Retired, do not build:** dirty-rect / partial-update path (none exists); timed
auto-dismiss as a core-clock feature (now a C event).

**Ruby - icons to probe:** headphones, USB, warning triangle, check, plus. Only
`SHIELD`/`LOCK_*`/`EYE`/`CARET_RIGHT`/`BLUETOOTH`/`COG` exist in `theme::icon`.
Same throwaway-grid-probe method as `icon_probe_home.rs`.

**Ada - commands and data.** C1 **cancel scan (MVP-blocking)**. C2 per-device
codec set + availability + reason. C3 LDAC quality set. C4 AVRCP absolute volume
set **+ capability flag** (needed to hide vs disable). C5 **paired-device store:
link keys + MRU order (load-bearing)**. C6 Class-of-Device on inquiry. C7
**connected-link RSSI, or a definitive no** (decides cut-vs-keep). C8 per-channel
peak/RMS at ~4Hz without disturbing the real-time path. C9 auto-dismiss timer
event. C10 core clock - **not MVP-blocking**, wanted only for liveness during
multi-second waits, since 5.12s with zero moving pixels is indistinguishable
from a hang, which this project has been burned by before.

**Tess - fixture states, all captured ZOOMED** per the project's
rendering-verification rule: boot -> first run; scan with a late-arriving name
*and* a permanently nameless device; all four connecting phases; "Still trying
(2)" at the 6s surfacing point; each named failure including the two with no
retry; **degraded success -> Home with banner -> X (`why?`) -> codec picker with
the dimmed row and its reason**; MUTED; level meter at silence/mid/clipping;
every dashed field and every **cut** field in its cut form; Devices with a
nameless row and MRU ordering. The degraded chain and the cut fields are the ones
that will otherwise never be looked at until a user hits them.

## 15. Still blocking

The physical A/B/X/Y arrangement on the Waveshare Pico-LCD-1.3. Rail edge, rail
slot order and gauge edge all remain parameterised off one constant until it
lands.
