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

**Physical arrangement (confirmed by Andreas, 2026-08-28):** the four buttons
are a **column on the right**, ordered **A, B, X, Y top to bottom**, with the
**d-pad on the left**. So the rail sits on the right edge with slot order A/B/X/Y
downward, and the gauge column sits on the left, on the d-pad edge. That is the
layout drawn above.

> **STILL PARAMETERISE IT.** The panel rotation (`pico-link-zzq`) is unresolved,
> and a 180-degree flip puts the d-pad on the right and the buttons on the left.
> The rail's edge and slot order must derive from the same constant that maps
> GPIO to `NavIntent`, so a rotation flips the labels *with* the buttons instead
> of silently lying, and the gauge binds to the opposite edge off that same
> constant. Hard-coding "right" would make the rotation fix a UI bug.

**Note the interaction:** `pico-link-zzq` wants the UI upright with the *cable*
exiting right, and the panel is currently upright with the cable exiting left.
Resolving that rotates the board 180 degrees, which moves the d-pad to the right
and the buttons to the left. The design is unaffected because both edges are
parameterised - but whoever fixes the rotation must re-derive the button-to-
intent table and the rail edge together, never separately.

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

> **SUPERSEDED IN PART BY S17.** This table was written under a brief that said
> to cut anything lacking data in today's firmware. That brief was wrong: it
> turns schedule facts into product decisions. S17 re-classifies every entry as
> IMPOSSIBLE (stays cut) or NOT BUILT YET (designed properly, with enabling
> work in S19). Read S17 first; the rulings below hold only for the IMPOSSIBLE
> rows and for build ORDER - ship a field absent, never frozen or faked.

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

## 15. Build-order rule (survives from S13)

Where a field is designed but its data is not built yet, **ship it ABSENT, never
frozen and never faked.** A still VU meter reads as silence when it means no
data; a permanently-dashed signal meter is a recurring admission of ignorance on
the screen the product's credibility depends on; a volume bar that does not move
on press is a bug report. Absent is honest; frozen is a lie.

## 16. Still blocking

Nothing blocks the design. The button arrangement is answered (S3). Rail edge,
rail slot order and gauge edge stay parameterised off one constant so the
unresolved panel rotation cannot turn into a UI bug.


---

# v6 — designed for the product we intend

The design above was written under a brief to cut anything without data in
today's firmware. Andreas corrected it: *"you are allowed to think ahead. Just
because there is only one device and everything is lost on reboot doesn't mean
we won't fix that. UX first! figure out what we want, and then plan for building
it."* Sections 17-19 apply that.

## 17. Impossible vs not built yet

**IMPOSSIBLE — stays cut.** *Now-playing metadata* (we are the A2DP source; the
host sends raw PCM and there is no metadata in our data path, so nothing
downstream can invent it). *Simultaneous streaming to two sinks* (bandwidth plus
two LDAC encodes on a 150MHz M33) - but see E23, because the useful half of
multipoint is not impossible. *Headphone battery* - with one counter-datum
logged rather than sat on: AVRCP 1.3+ defines `EVENT_BATT_STATUS_CHANGED` (0x06)
on the target, and we are the controller. Support is patchy and values are
coarse (Normal/Warning/Critical/External/FullCharge, not a percentage), so even
at best this is a **three-state icon, never a gauge**. Not designed. Ada to close
the question properly; "vendor-proprietary" may be slightly too strong.

**NOT BUILT YET — back on the table, designed properly.** `SIGNAL` row and Home
LINK bar (E18). `OUT` level meter and its peak cap (E17). `VOL` gauge and the
d-pad binding (E16). `USB IN` rate (E19). Live/adaptive bitrate (E20).
Class-of-Device scan filter (E9). The multi-device paired list (S18). The core
clock, **upgraded from not-MVP-blocking to high priority** (S20). And the scan
elapsed timer, **reinstated as a determinate bar** - the scan is exactly 10.24s,
so with a clock this is measured progress, not an estimate, which is better than
the streaming-rows-only design v5 settled for.

**RETIRED AS A DESIGN RULE:** *"a setting with no persistence behind it does not
appear."* Correct under the old brief, wrong under this one. See S19.

**DECLINED ON MERIT — unchanged, and on the record as merit, not scarcity:**
on-screen text entry for renaming, long-press gestures, B navigating forward,
verb-first device menus, marquee names, toasts for the fallback, RSSI-sorted
scan lists, and a volume control that pretends the host slider will follow.

## 18. The multi-device story

**Multiple means REMEMBERED, never simultaneously connected.** Exactly one active
connection, always. What scales is the paired-device store.

**Capacity: 8 paired devices.** Covers every plausible user with margin (desk
over-ears, portable earbuds, a speaker, a spare) and keeps the list at 8 + "Pair
new" = 9 rows, inside the 12-item rule. Eight link keys is a trivial flash cost.
`NVM_NUM_LINK_KEYS` goes 1 -> 8.

**Ordering: the connected device is PINNED to row 1; everything else is
most-recently-used below it.** Pinning matters - under pure MRU the connected
device drifts down as you use others, and "where did my headphones go" is
disorienting on a screen you glance at. Pinned-then-MRU always puts the two most
likely targets, what you are on and what you were on last, in rows 1 and 2.

**Presence: never claim availability we have not verified.** We cannot know
whether a paired device is in range without paging it at up to 5.12s each;
probing all eight would cost 40 seconds of radio time and thrash the link. So a
paired row shows `Connected` or `Paired` **and nothing else** - no availability
dots, no "in range" claim. When you pick one and it is not there, the wizard's
6-second "Not responding" screen handles it, which is why that screen exists and
now earns its keep twice over. The one honest enrichment, once we have a clock
and persistence: **"Last connected 2 days ago"** for devices not used recently -
a fact we own, not a guess about the radio.

**Switching.** `A` on a paired row switches; no confirm, it is reversible and it
is the recurring 2-press job. Today 2-8s through the wizard phases; with a warm
second link (E23) it becomes sub-second. Handle both: **suppress the transition
UI entirely if the operation completes in under ~500ms** - a progress screen that
flashes for 200ms is worse than none. Needs the clock.

**Pairing when full. Never silently evict the least-recently-used.** Silent
forgetting is the fastest way to make someone distrust a device: they blame the
pairing, not the capacity. Instead: "Paired device list is full. You can pair 8
devices. Choose one to forget and make room." -> Devices in a pick-one-to-forget
mode -> Confirm -> **pairing resumes automatically from where it left off.** More
presses, never a surprise.

**Forgetting.** X -> manage -> `Forget this device` -> Confirm focused on Cancel,
naming the consequence: "Forget Sony WH-1000XM5? You'll need to pair it again."
plus "This will also disconnect it." when connected. Forgetting removes the link
key **and that device's per-device settings** (codec choice, volume); this is
conveyed by re-pairing starting fresh, not by an extra sentence.

**Default device.** New per-device action on the manage page: **`Make default`**,
feeding Settings -> "Connect to". Some people always want the desk headphones on
plug-in regardless of what they used last, and MRU alone cannot express that.

**Nameless devices.** `(unknown device)` + address tail as designed, plus two
additions now affordable: **retry the remote-name request on connect** (E21 -
most nameless devices are a transient GAP failure during inquiry, not a permanent
absence, so this fixes the majority for free), and **a tag picker** for the
genuinely nameless (E25 - 8 icons x 8 colours, four presses, a distinguishable
tag without d-pad text entry). The tag picker keeps the merit-based decline of
text entry intact while actually solving the underlying problem, which the
decline alone did not.

## 19. Settings, with persistence

**Flat, single list, no nesting** - nesting would make
`Home(0) -> Settings(1) -> Audio(2) -> picker(3)` and break the depth-2 rule that
`B, B` depends on. Eleven rows: Default codec, Volume limit, Startup volume,
Per-device volume, Auto-connect, Connect to, Brightness, Dim after, Blank after,
About, Reset all.

- **Default codec** - reinstated but **scoped to devices we have not met yet.**
  The global preference was killed on merit because the available set is a
  per-device intersection, and that still holds for connected devices; but "which
  codec do I try first on something new" is a real unconstrained preference.
  Labelled so it never competes with the per-device picker.
- **Volume limit** - hearing safety; stops a device that can blast you.
- **Startup volume** - `Last used` or a fixed safe level. A fixed safe default is
  genuinely right for a device whose volume the host cannot see.
- **Per-device volume** - earbuds and over-ears need different volumes.
- **Connect to** - `Most recent` or a pinned default device.
- **About** - firmware, BT address, uptime, and an `Advanced` sub-view for
  last-error and link stats. Depth 2, terminal.

**Declined: screen orientation as a user setting.** Rotation should be *correct*
in firmware; exposing it would be shipping a bug as a preference.

## 20. The clock, and two re-decisions

**The clock is one FFI parameter discarded at `ui-ffi/src/lib.rs:351` and it
unlocks eight things:** liveness during multi-second waits (a 5.12s page timeout
with zero moving pixels is indistinguishable from a hang, which this project has
already paid for once); a determinate 10.24s scan bar; wizard auto-dismiss
without a C-side timer; "Last connected N days ago"; suppressing sub-500ms
transition UI; dim/blank timing owned by core; the bitrate smoothing window; and
peak-hold decay. Near-zero cost, eight payoffs - land it early.

**Key repeat: the 12-item rule STAYS, and it was merit.** Re-checked
independently of cost: a product where no list exceeds one screen-and-a-bit means
the information architecture is right, and at 18fps a held-scroll you cannot
visually track is how people overshoot. Longest list is Settings at 11.
Auto-repeat is accepted as a **low-priority comfort item** (E24); it changes no
screen. **Long-press stays declined** even though a clock plus release edges would
enable it - undiscoverable on a device with a labelled button rail, and `B, B` at
depth 2 already solves the problem it would solve.

**Dirty-rect / partial update: the design wants it, and it is not MVP.** On
merit: the level meter at 4Hz repaints all 57,600 pixels to move a few bars, and
ST7789 supports column/row address windows natively. It buys a smoother meter
(10-15Hz instead of 4Hz), much lower idle cost, and less SPI/CPU contention with
audio. The design works without it at 4Hz with peak-hold, so it is the biggest
quality-of-feel item **after** the MVP ships (E22).

## 21. Enabling work list

**Tier 1 - MVP REQUIRED. The MVP is fully realised at E13.**

| | Item | Owner | Why MVP |
|---|---|---|---|
| E1 | **Cancel-scan command** | Ada | Without it scan phase 2 is a 10.24s dead end where B does nothing. Permanent trust cost. |
| E2 | **Button rail** - `ChromeContribution` gains a/b/x/y; rail on a parameterised edge with parameterised slot order | Fern | Four unlabelled buttons are four buttons nobody presses. |
| E3 | **`font::hero()`** ~20-26px | Fern | The reassurance goal is a glance at 30-50cm; `helvB12` cannot do it. |
| E4 | **Hero widget + persistent banner slot** | Fern | Fallback chain links 1-2. |
| E5 | **Phase/wizard screen**, content replaced by events, no stack push | Fern | The whole pairing flow; keeps depth at 2. |
| E6 | **Per-device codec availability + reason**, and **disabled-but-focusable `MenuItem`** | Ada + Fern | Fallback chain link 5. The reason text is the payload. |
| E7 | **Home two-face toggle** (status <-> menu, no push) | Fern | Discoverability without the rail; keeps depth at 2. |
| E8 | **Core clock** - plumb the discarded `now_us` | Ada | Eight payoffs for one parameter. |
| E9 | **Class-of-Device on inquiry results** | Ada | Keeps the scan list inside the 12-item rule in a crowded room. |
| E10 | **Icon probes**: headphones, USB, warning triangle, check, plus | Ruby | Nothing renders without them. |
| E11 | **Key/value field list** (check `MenuList` + `Trailing` first) | Fern | Device detail. Reuse over new. |
| E12 | **Live list with stable identity** - append/update in place without resetting selection by position | Fern | The stable-scan-ordering rule is unimplementable otherwise. |
| E13 | **Rename `HidLinkState` -> `LinkState`** (`widget.rs:141`) | Fern | The chrome currently lies about what the glyph means. |

**Tier 2 - post-MVP, the design already assumes it.**

| | Item | Unlocks |
|---|---|---|
| E14 | **`NVM_NUM_LINK_KEYS` 1 -> 8** + the persisted bond store specced in S22 | The entire multi-device story. **Highest-value Tier 2 item** - until it lands, pairing a second set of headphones forgets the first, which makes the whole Devices screen a fiction. |
| E15 | **Settings persistence (M5)** | S19. Without it Settings is a demo. |
| E16 | **AVRCP absolute volume + capability flag** | VOL gauge, d-pad Up/Down, MUTED banner, volume limit, per-device volume. |
| E17 | **Per-channel peak/RMS at ~4Hz**, off the real-time path | The OUT meter and its peak cap. Ship absent, never frozen. |
| E18 | **Connected-link HCI RSSI** | SIGNAL row and the Home LINK bar. |
| E19 | **USB IN rate (M3)** | USB IN field; also gates the meter, since without USB audio there is no PCM. |
| E20 | **Live/adaptive bitrate readout** | The "(adaptive)" qualifier. Nominal-only until then. |
| E21 | **Remote-name retry on connect** | Fixes most `(unknown device)` rows for near-zero work. |

**Tier 3 - later, improves the product, changes no screen.**

| | Item | Value |
|---|---|---|
| E22 | **Dirty-rect / partial update** (ST7789 address windows) | Meter at 10-15Hz, much lower idle cost, less contention with audio. Biggest feel improvement after MVP. |
| ~~E23~~ | ~~Warm second ACL link~~ | **WITHDRAWN.** Andreas set "exactly one active connection at a time, always", and a warm idle ACL link is a second connection - keeping it under the justification that it is not *streaming* would smuggle back the thing he declined, under a different name. It optimised a problem nobody has complained about (2-8s switching) at the cost of a constraint he explicitly set. Switching stays 2-8s, presented honestly through the wizard phases. |
| E24 | **Auto-repeat on the d-pad** | Comfort on the 11-row Settings list. |
| E25 | **Device tag picker** - 8 icons x 8 colours | Solves permanently-nameless devices without text entry. |
| E26 | ~~"Last connected N days ago"~~ -> **monotonic MRU sequence number** | **AMENDED - the original was factually wrong. There is no RTC on this board**, so wall-clock age is not derivable across reboots; a clock gives time since boot, not calendar time. The `N days ago` sublabel is **cut on merit**, not deferred, because we cannot produce the number honestly. Replaced by a 2-byte monotonic use-sequence number, which is what MRU ordering actually needs and is cheaper. Folded into E14's record format. If an RTC ever arrives - host time over USB is a plausible source - the sublabel becomes possible again. |


## 22. The paired-device store

**Capacity 8, justified from the UI outward rather than from the flash inward.**
Eight covers every plausible user with margin, so "forget one to make room" fires
rarely enough to be a non-event; 8 + "Pair new" = 9 rows, inside the 12-item
rule, so Devices never needs the scrolling behaviour the design removed. Sixteen
would fit the flash but breaks the rule and nobody owns sixteen pairs of
headphones. The record is fixed-size, so raising the cap later is a constant
change, not a migration.

**Bond record, 64 bytes:** BD_ADDR 6, link key 16, key type 1, device name 32,
per-device codec 1, LDAC quality 1, per-device volume 1, flags 1 (is-default,
tag-assigned), tag 1 (reserved for E25), MRU sequence 2, CRC 2.

**On the name field:** the spec allows remote names up to 248 bytes, and storing
that for 8 devices would burn ~2KB - half the budget on strings. **32 bytes,
truncated on a UTF-8 CHARACTER boundary**, is driven by the display: at
`helvB12` on 240px we truncate around 20 characters anyway and the detail page
wraps at roughly 40. `Sony WH-1000XM5` is 15 bytes; `Bose QuietComfort 45
Headphones` is 30. The cap is invisible in practice. It must truncate on a
character boundary, not a byte boundary, or a multi-byte name renders as a
broken glyph.

**Budget:** bonds 8 x 64 + 8-byte header = 520 B; settings ~128 B; total ~648 B,
about 16% of the ~4KB TLV space. Room to double the cap if the UI rule ever
changes.

**One blob for bonds, a separate blob for settings.** Recommended, with the
trade stated so Ada can overrule on facts the design does not own. For one blob:
wear is sector-granular anyway (the QSPI flash erases in 4KB sectors and
pico-sdk's TLV appends and compacts within one, so writing 64 bytes and writing
512 costs the same erase - which is the argument that usually decides this);
MRU reordering and forget-one-to-make-room are list operations, naturally atomic
against a blob and racy across N independent entries; and one version byte, one
migration path, instead of N. The one real argument for N entries is failure
isolation, and **the per-record CRC restores it** - the blob can drop a single
bad record and keep the rest. Settings stay separate deliberately: different
change frequency (settings churn while you fiddle, bonds rarely change), and a
settings-format migration must never be able to take your bonds with it.

**What Ada owns:** BTstack's `btstack_link_key_db` interface is what actually has
to be satisfied, and pico-sdk ships a TLV-backed implementation. A blob means
*replacing* that implementation rather than configuring it - read the blob into
a RAM mirror at boot and serve BTstack's per-device get/put/delete callbacks from
the mirror, writing back on change. That is a real integration cost and it is
Ada's call. **The UI is indifferent** to blob-versus-TLV as long as the store
holds 8 bonds, survives reboot, and preserves MRU order.
