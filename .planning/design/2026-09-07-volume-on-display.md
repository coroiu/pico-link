# Volume on the display

**Date:** 2026-09-07
**Author:** Uma (UX)
**Bead:** `pico-link-4v2.6` (VT6), child of epic `pico-link-4v2`
**Status:** Design of record for how volume reads on screen. Blocks the VT6
implementation bead.

**Amends:**
- `.planning/design/2026-08-28-on-device-ui.md` section 6.3 — its premise
  ("the host slider will not move", line 45) is now **false**. Its
  `HEADPHONES`-not-`VOLUME` naming ruling and its `MUTED / Press Up to raise`
  banner text are both superseded here. Section 6.1's *never fully blank while
  a muted banner shows* rule survives and is extended.
- `.planning/design/2026-09-02-volume-sync.md` section 10 — this document is
  the answer that section reserved.

**Depends on / does not disturb:**
- `.planning/design/2026-09-03-vertical-out-meter.md` — the OUT meter. **Nothing
  in this design moves, resizes, recolours or reschedules the meter.** Its
  section 8 forward constraint ("if VOL carves a strip off `gauge_edge()` the
  hero body drops to 124px and `NO LINK` no longer fits") is honoured by not
  carving anything.
- `.planning/design/2026-09-01-home-fault-strip.md` section 7 — reserves the
  hero body's abs y164..218. Honoured.
- `.planning/design/2026-09-06-damage-rect-render-and-partial-blit.md` — the
  damage seam this design deliberately fits inside rather than extends.

---

## 1. GOAL

Volume sync now works in both directions on hardware (VT1-VT5, merged
2026-09-06). A canonical 0..127 level with a `muted` flag and a `source`
(host / sink / device) arrives in `BtModel::volume` as `Option<VolumeState>`
(`core/src/app.rs:364, 785`). Nothing renders it.

Four questions, all answered below: how volume and the OUT meter coexist; whether
a volume change wakes the screensaver; the mute visual; the `None` state.

---

## 2. The ruling that decides everything else

> **Volume is not a bar. It is a number, and it does not live in the content
> band at all — it lives in the title bar, as chrome.**

Three independent arguments land on the same answer.

### 2.1 They are not on the same axis. They are not even in the same signal order.

The OUT meter reads the PCM level **entering the LDAC encoder**
(`firmware/src/a2dp.c`'s `peak_l`/`peak_r`, in dBFS over -48..0). The volume is
the gain the **headphones** apply, downstream, off-device, after the radio.
Volume-sync section 5 settled that we deliberately do **not** attenuate PCM
locally.

Therefore:

- **At volume 0 the OUT meter still swings full scale, and that is correct.**
  Someone will eventually read that as a bug and "fix" it by scaling the meter by
  the volume. That would destroy the meter's only job — telling you whether audio
  is reaching the encoder — precisely in the state where you most need it. Write
  this down in the code, not just here.
- Placing volume on the meter's axis, or on a parallel axis beside it, asserts a
  comparability that does not exist. Two adjacent level-shaped objects on the
  same rule *is* a claim about their relationship. It would be a false claim.

### 2.2 A setting is not a signal, and they should not share a visual genus

| | OUT meter | Volume |
|---|---|---|
| what it is | a **signal** passing through | a **setting** someone chose |
| origin | involuntary, the music | voluntary, a human |
| motion | continuous, 4Hz, moves whenever audio flows | event-driven, static for minutes |
| right form | a bar | **a number** |
| you read it by | watching it move | reading it once |

You do not read a meter, you watch it. You do not watch a setting, you read it.
A number is the correct form for a value you consult and then stop looking at,
and it is exact where a bar is approximate. Nobody needs to know their volume is
"about two thirds"; they want 62, or they want to know it changed.

### 2.3 The coherent surface is the *band*, not the adjacency

Ada's constraint was "one coherent surface, not a competing overlay". Adjacency
is the wrong way to get there. Home already has two bands with two different
jobs, and the coherence comes from using them honestly:

- **Title bar = what the device has been told / what state it is in.** It already
  carries the link glyph and the status dot. Volume — a setting — belongs here.
- **Content band = what is happening right now.** Codec word, bitrate, OUT meter.
  Live signal. Volume does not belong here.

Putting volume in the title bar is what makes the surface coherent. Putting it
beside the meter is what would make it compete. **The bead title says "beside the
VU meter" and I am overriding that premise deliberately** — section 9 has the
fallback if Andreas disagrees.

### 2.4 And it is free

The title bar is already an independently keyed, independently damaged region
(`core/src/render/screen.rs:161` `title_paint_key`, cached as slot 0 with
`area = chrome.title`). A volume change repaints **240x16 = 3840px**, text-only,
with no framework change and no new widget. See section 7 — this matters a lot,
and the content-band alternatives all cost either a full hero repaint or a
change to the `damage_hint` seam.

---

## 3. VISUAL SPEC — the persistent readout

### Placement

The title bar's right cluster is built **right-to-left** (`screen.rs:682-719`):
status dot is rightmost, then the Bluetooth link glyph, then the position
readout. Volume inserts **between the link glyph and the readout**:

```
| status dot | <- rightmost, fixed width
| link glyph | <- fixed width
| VOLUME     | <- NEW, variable width ("7%".."100%"/"MUTE")
| readout    | <- existing "2 / 5"; None on Home
| title text | <- left-anchored, clipped so it can never bleed right
```

Rationale for that slot: the dot and the glyph are fixed-width chrome anchors and
must never shift. A variable-width number placed outboard of them would make them
jitter horizontally every time the volume changed — the single most distracting
thing a status bar can do. Inboard, it floats and they do not move.

Separated by the existing `TITLE_ELEMENT_GAP`. No new spacing constant.

### Typography and colour

| Element | Font | Colour |
|---|---|---|
| Volume value, normal | `font::title()` (helvB10) | `palette::TEXT_PRIMARY` |
| Volume value, muted or 0 | `font::title()` (helvB10) | `palette::STATUS_WARNING` |

`TEXT_PRIMARY`, not `TEXT_SECONDARY`. The title bar's one other text element is
the literal string "Pico Link", which is the least informative pixel on the
panel. The volume number outranks it and should not be dimmer than it.

`font::title()`, not `font::label()`: it is the title bar's own face, it matches
the readout it sits next to, and helvB10 is the smallest face on this panel I
will accept for a number you are meant to read rather than skim.

### Format

**Percent, 0..100, never the raw 0..127.** 0..127 is the AVRCP transport domain;
showing it would be leaking an implementation detail into the one place the user
looks. Both peers (the macOS HUD, every headphone app) speak percent.

```
percent = (level as u32 * 100 + 63) / 127
```

Rounds half-up, and — checked — maps `0 -> 0`, `1 -> 1`, `126 -> 99`,
`127 -> 100`. The two endpoints are exact, which is what matters: **"100%" must
mean maximum and "0%" must mean silent**, with no other level able to render as
either.

Rendered as `62%`. Widest string is `100%`.

### States

| Model state | Renders as | Colour |
|---|---|---|
| `Some(v)`, `!v.muted`, `v.level > 0` | `62%` | `TEXT_PRIMARY` |
| `Some(v)`, `v.muted` | `MUTE` | `STATUS_WARNING` |
| `Some(v)`, `!v.muted`, `v.level == 0` | `0%` | `STATUS_WARNING` |
| `None` | **nothing drawn, zero width consumed** | — |

### The mute visual is a WORD, not an icon — and this is measured, not taste

`MUTE`, in text, amber. Not a speaker-with-a-slash glyph. Two reasons, both hard:

1. The `_tf` u8g2 font subsets this crate uses carry no speaker codepoint, and
   `with_ignore_unknown_chars(true)` would silently render nothing at all — the
   exact failure mode that would ship as "volume just disappears sometimes".
2. A hand-drawn 10x10px speaker-plus-slash at 30-50cm is a smudge. This is the
   same evidence that killed the bare `-` hero glyph (`hero.rs:181-193`, bead
   `pico-link-znb.6`): a 9x5px dash measured as "a tiny coloured smudge rather
   than a legible word". Do not relearn it.

**The source is never displayed.** Ada asked. No: the value is the same value
whoever set it, and a "from host" tag is diagnostic noise in a 16px bar. `source`
earns its place in the event twice over anyway — section 5 (wake policy) and
section 4 (banner remedy wording).

---

## 4. The exceptional states are LOUD, and they reuse the banner

The persistent readout is deliberately small. That is only defensible because the
states that actually hurt the user get a big, central, persistent object — and
that object already exists: the banner slot at abs y116..135, priority
`MUTED > FALLBACK`, wired through `HeroStatusView::with_muted` /
`ActiveBanner::Muted` (`hero.rs:299, 341, 626`).

**Principle: normal is quiet, abnormal is loud.** The number tells you what the
volume is; the banner tells you why there is no sound.

### 4.1 Banner priority (extended)

```
MUTED  >  VOLUME 0  >  FALLBACK
```

`ActiveBanner` gains a `VolumeZero` variant. The hero codec word's own colour is
untouched by any of them, exactly as today.

### 4.2 Banner text — and the current string is a lie that must go

`hero.rs:626` renders **`"MUTED  Press Up to raise"`**. Up is *unbound* on Home's
status face — `core/src/app.rs:3169` asserts it as a test invariant, and
device-side volume is explicitly out of scope (volume-sync section 8, Tier 2).
**The banner currently promises a remedy that does nothing.** Fix it in this
bead; it is a one-line change and a genuine defect.

Remedy text is chosen by `source`, which is its second job:

| Banner | Condition | Text |
|---|---|---|
| `Muted` | `muted && source == Host` | `MUTED  Unmute on the Mac` |
| `Muted` | `muted && source != Host` | `MUTED` |
| `VolumeZero` | `!muted && level == 0` | `HEADPHONE VOLUME AT 0` |

Both `STATUS_WARNING` on `SURFACE_ELEVATED`, matching today's muted banner.

**Width budget: 134px** (`hero_body` 158 minus 2x `BANNER_TEXT_INSET` 12). The
current string measures 128px, so `MUTED  Unmute on the Mac` (same 24 chars) is
at the limit and `HEADPHONE VOLUME AT 0` (21) is comfortable. **Ruby must
measure both with `get_rendered_dimensions_aligned` in `font::label()`, not
assume from character count.** If either exceeds 134, drop the remedy clause
before dropping the state word — `MUTED` alone still does the job.

**When Tier 2 device-side volume ships, all three collapse back to
`... Press Up to raise`**, because then the remedy really is here. Leave a note
at the match arm.

### 4.3 Why `VOLUME 0` is a sink-side story only

Measured constraint: macOS sends the mute flag *and* 0% together. So a host
slider dragged to the bottom arrives as `muted = true`, and the `MUTED` banner
covers it. `!muted && level == 0` is therefore reachable essentially only from
the headphones' own dial, which is why the remedy names the headphones. The
`source != Host` arm above is defensive, not expected.

### 4.4 Consequence, accepted

A device parked at volume 0 or muted shows a persistent banner, and by section
6.1's existing rule *never fully blank while a fallback or muted banner shows —
dim only*, it therefore never fully blanks. That is the correct trade (a silent
device that looks dead is the worst state this product has) and it is already the
ruling for `MUTED`. `VolumeZero` inherits it.

---

## 5. Does a volume change wake the screensaver?

**Ada's instinct is right and I am confirming it, with a sharper reason and one
added tier.**

### 5.1 The rule

| `source` | Blank -> ? | Extends the idle timer? |
|---|---|---|
| `Host` | **no change** | **no** |
| `Sink` (headphones) | **wake to full** | **yes** |
| `Device` (future d-pad) | wake to full | yes (it is a button press; already covered) |
| **any source, into `muted` or `level == 0`** | **blank -> dim, never to full** | no |

### 5.2 Why host must not wake — the reason is stronger than "he's at a screen"

Ada's framing was "the user is at the laptop and does not need the screen".
True, but the load-bearing reason is different:

> **The idle timer measures human presence at the device. A host volume event is
> not evidence of human presence anywhere.**

It can be a mixer app, per-application ducking, a Zoom join, a screen-locked
machine's alarm, or macOS restoring volume on wake. If those wake the panel, the
screensaver becomes unreliable in exactly the way that makes people stop trusting
it — a dongle that lights up at 3am for no visible reason. And the host has
already shown its own volume HUD; waking would duplicate a notification the user
just received on a bigger screen.

**Note this is additive work, not a change of default.** `App::on_volume_changed`
(`app.rs:1701`) currently just stores. Nothing volume-related touches idle today,
so "host does not wake" is already true. The build is: make `Sink`/`Device` wake.

### 5.3 Why sink must wake

A headphone-originated change is the one case where the user is physically at the
device, has just acted, and **has no display in front of them** — the dial has no
readout. The dongle screen is the only surface in the system that can confirm
what they just did. If it stays dark, the feature is invisible exactly where it is
most valuable. This is also the case that makes the whole bidirectional sync
*feel* real.

### 5.4 The third tier: mute promotes blank to dim, never to full

Silence you cannot explain is the failure this whole feature exists to prevent.
If the device blanks and *then* gets muted, section 6.1's dim-not-blank rule has
already lapsed — the screen is already off. So: entering muted or zero from any
source **promotes the screen from blank to dim**, and stops there. Dim is enough
to read the banner from 30-50cm and is not the room-lighting event a full wake is.
Leaving mute out of the wake policy entirely would make `MUTED` a banner you can
only see if you were already looking.

### 5.5 The trap that will make this emulator-only

`core/src/run.rs` is **emulator-only** — the firmware does not run `Runner::step`
(see `.planning/design/2026-09-01-idle-policy-across-the-ffi-seam.md`, and the
project's own standing note "firmware does not run core's run loop"). A wake rule
implemented only against `run.rs`'s idle timer will pass every host test and do
nothing on hardware.

**Requirement:** the activity signal must be written wherever the *firmware's*
idle level (`pl_ui_display_power` and its backing state) reads it, and the same
signal must drive the emulator. If those are two different timestamps today, that
divergence is a bug to file, not to route around. **Tess must prove the wake on
the real target, not only in the emulator.**

---

## 6. The `None` state

`BtModel::volume == None` means: nothing connected, or connected but no volume
reading has arrived yet, or the sink has no AVRCP absolute volume at all.

> **Absent, never faked.** The title-bar volume element is **not drawn** and
> consumes **zero width**. No `--`, no `?`, no greyed `0%`.

This is the same rule the OUT meter's `OUT` legend follows (vertical-out-meter
section 7: *"a label with nothing under it reads as broken, not as silent"*) and
the same rule section 6.3 of the on-device UI design already states for the
volume gauge specifically: **"gauge and binding are both ABSENT, not inert."**

Consequences, all deliberate:

- **No layout shift when it appears.** The title bar's right cluster is built
  right-to-left, so the volume's arrival pushes only the readout (and the title's
  clip width) leftward. The dot and glyph do not move. On Home there is no
  readout, so nothing visibly moves at all.
- **A brief absence right after connect is fine.** The first host feature-unit
  SET normally arrives within a second of enumeration. An element that fades in
  once, at the same moment the codec word appears, reads as the screen filling in
  — not as a glitch. No placeholder, no spinner.
- **"Unsupported" and "not yet known" render identically, and that is correct.**
  `Option<VolumeState>` cannot distinguish them today. It does not need to: both
  mean *we have no volume to show you*, and both produce the same absence. **Do
  not add a capability field to the model for this bead.** Add it only if some
  future screen needs to say *why*, which no screen currently does.

---

## 7. Render budget and damage — the part an implementer must not improvise

**A volume change damages exactly `chrome.title`: 240x16 = 3840px, ~6.7% of the
panel, one short text run, no framework change.**

Mechanism, already built (`screen.rs:514-620`): `title_paint_key` is cache slot 0
with `area = chrome.title`. Fold the volume into it and the existing diff narrows
to the title rect on its own.

```rust
fn title_paint_key(
    title_text: &str,
    readout_text: Option<&str>,
    volume_text: Option<&str>,   // NEW
    volume_muted: bool,          // NEW - colour is not derivable from the text
    status: Option<ChromeStatus>,
    link: Option<LinkState>,
) -> PaintKey
```

`volume_muted` is folded separately because `0%` renders amber and `62%` renders
primary while both are just strings: a key that folded only the text would let a
colour change through unpainted. That is precisely the `paint_key` contract
violation the module docs warn is untestable except by comparing pixels.

**No time is folded, and `redraw_after` is NOT overridden.** Volume is purely
event-driven. Per `paint_key.rs`'s mechanical review rule, folding time without
`redraw_after` (or vice versa) is a bug in one of the two.

### Rate limiting: not needed, and say why

A macOS slider drag produces 30-60 feature-unit SETs/second, but `volume.c`'s
loop rule emits `PL_EVENT_TAG_VOLUME_CHANGED` **only when canonical actually
changes** (volume-sync section 4), and the rendered *percent* changes even less
often than the level does. Worst case is therefore ~60 title-bar repaints/second
at 3840px each = 230kpx/s, against a panel that now repaints its whole 57.6kpx in
~0.5ms. **No coalescing constant. Do not add one.** If profiling ever shows
otherwise, the fix is a leading-edge 250ms limiter on the *model* side (fire
immediately, then coalesce), matching Home's 4Hz motion rule — but ship without
it and measure first.

This is the single biggest reason the title bar beat every content-band
placement. See section 9.2 for what the alternative would have cost.

---

## 8. ASCII SKETCH

240x240, true proportions (1 char ~ 4px horizontally). `ButtonsRight`; a panel
flip mirrors the rail and the meter strip together and does not affect the title
bar's right-to-left cluster.

### Connected, streaming, volume 62%

```
 x0                                                                x240
  +----------------------------------------------------------------+ y0
  | Pico Link                                       62%   BT   *    | y0..16   <- volume: NEW, TEXT_PRIMARY, helvB10
  +-------------------------------------------------+------+-------+ y16
  |  Sony WH-1000XM5                          OUT   |      |   A   | y28..44
  |                                                 | .  . |       |
  |  L D A C                                        | .  . |   B   | y56..88  <- hero, TEXT_PRIMARY
  |                                                 | #  # |       |
  |  909 kbps                                       | #  # |       | y92..108
  |                                                 | #  % |   X   |
  |                    (banner slot, empty)         | #  # |       | y116..135
  |                                                 | #  # |       |
  |  LINK ||||.   USB 48K 24-BIT                    | #  # |   Y   | y144..154
  |                                                 | #  # |       |
  |                 (fault strip, empty)            | #  # |       | y164..218
  |                                                 | #  # |       |
  +-------------------------------------------------+------+-------+ y240
   <------------------ hero_body 158 --------------> <-48-> <- 34 ->
                                                       L  R    rail
```

### Muted (host)

```
  +----------------------------------------------------------------+
  | Pico Link                                      MUTE   BT   *    |  <- amber
  +-------------------------------------------------+------+-------+
  |  Sony WH-1000XM5                          OUT   |      |   A   |
  |                                                 | .  . |       |
  |  L D A C                                        | #  # |   B   |  <- hero still green
  |                                                 | #  # |       |
  |  909 kbps                                       | #  # |       |  <- bitrate still live
  |                                                 | #  % |   X   |
  |  +-------------------------------------------+  | #  # |       |
  |  | MUTED  Unmute on the Mac                  |  | #  # |       |  <- amber on SURFACE_ELEVATED
  |  +-------------------------------------------+  | #  # |       |
  |  LINK ||||.   USB 48K 24-BIT                    | #  # |   Y   |
  |                                                 | #  # |       |
  +-------------------------------------------------+------+-------+
```

**The OUT meter is still swinging.** That is the design, not a bug: the PCM is
still reaching the encoder at full level; the gain that was removed is in the
headphones. The banner is the thing that explains the silence. Section 2.1.

### Nothing connected

```
  +----------------------------------------------------------------+
  | Pico Link                                             BT   *    |  <- no volume element at all
  +-------------------------------------------------+------+-------+
  |  No headphones                                  |      |   A   |  <- no OUT legend
  |                                                 |      |       |
  |  N O   L I N K                                  |      |   B   |  <- STATUS_ERROR
  |                                                 |      |       |
  |                                                 |      |       |  <- no bitrate
  |                                                 |      |   X   |
  |                                                 |      |       |
  |                                                 |      |       |
  |                                                 |      |   Y   |
  |                                                 |      |       |
  +-------------------------------------------------+------+-------+
```

Volume, bitrate, OUT legend and OUT columns are all absent **together**. Nothing
is dashed, nothing is frozen, nothing is zero.

---

## 9. What I ruled out, and what I would do if Andreas disagrees

### 9.1 A second bar beside the meter — rejected

Rejected on section 2.1 (wrong axis, wrong point in the signal chain) and 2.2
(setting vs signal). Also, on the mechanical merits: two vertical segmented
columns at the panel edge plus a third parallel bar is a reading task, not a
glance.

### 9.2 A `VOL` row in the hero body — rejected on geometry AND on damage

I costed it fully before rejecting it, because it is the obvious answer.

**Geometry.** The hero body's vertical budget is already fully allocated:

| band | abs y | owner |
|---|---|---|
| device name | 28..43 | on-device UI / alignment grid |
| hero word | 56..87 | " |
| bitrate | 92..108 | " |
| banner slot | 116..135 | " |
| stat strip | 144..154 | " (already tight: `LINK \|\|\|\|.` + `USB 48K 24-BIT` ~132 in 134) |
| **invariant clearance** | 155..163 | fault strip section 7 — *do not take this* |
| fault strip | 164..218 | `pico-link-h62`, designed, unbuilt |
| free | **219..232** | 13px |

13px fits one `font::label()` line and nothing else. And a horizontal carve off
`gauge_edge()` is already ruled out by vertical-out-meter section 8: a 14px strip
leaves `hero_body` at 144, and the MUTED banner needs 152.

**Damage — the harder objection.** `HeroStatusView` gets exactly **one**
narrowable sub-region (`damage_hint` returns one `Rectangle`, and `Screen` caches
one `region_key` per widget, `screen.rs:601-618`). That region is already spent
on the OUT meter. A volume row in the hero body would therefore either:

- fold into `body_paint_key`, making every volume change a **full hero repaint**
  — undoing, for the most frequently changing value on the screen, exactly what
  the `pico-link-7h5` epic just bought; or
- force the union of the meter strip and the volume row, whose bounding box is
  206x184 = **most of the panel**; or
- require Fern to generalise the seam to N regions per widget.

The title bar costs none of that. **If Andreas wants a `VOL` row anyway, the
N-region generalisation is the right way to pay for it and it is Fern's call —
file it as its own bead rather than bolting a second rect onto `damage_hint`.**

### 9.3 The title bar's existing `readout` field — rejected

`ChromeContribution::readout` is the position readout ("2 / 5") and is genuinely
used on list screens. Volume must be a **separate field** so the two can coexist
on any screen that wants both. See section 10.

### 9.4 If Andreas disagrees with the placement

**Plan B, self-contained:** `VOL 62%` in `font::label()` / `TEXT_PRIMARY`,
left-aligned on the standard x12 rule, ink at abs **y220..230** — bottom rule
y230, **shared with the OUT meter block's bottom**, which is the one honest way
to tie them together without putting them on a shared axis. It replaces the
title-bar element rather than supplementing it (one value, one place). It costs a
full hero repaint per volume change until the damage seam grows N regions, and it
is Home-only. Everything else in this document — the percent formula, the mute
word, the banner rules, the wake policy, the `None` rule — is unchanged by that
swap.

---

## 10. HANDOFF

### Fern needs to make expressible

1. A **`volume` field on `ChromeContribution`**, distinct from `readout`:
   ```rust
   pub struct VolumeChrome { pub percent: u8, pub muted: bool }
   pub volume: Option<VolumeChrome>,
   ```
   Text and colour are the chrome's to derive, not the widget's — same division
   `status`/`link` already use. Home's `HeroStatusView::chrome_contribution`
   populates it; other screens may later.
2. `title_paint_key`'s new parameters (section 7) and the right-to-left cursor
   insertion point in `Screen::render`'s title block (`screen.rs:682-719`).
3. **Nothing else.** No new widget, no layout change, no `ChromeLayout` field, no
   `damage_hint` change.

### Ruby builds

1. `VolumeChrome` + the `ChromeContribution` field + `HeroStatusView`'s
   contribution from `BtModel::volume`.
2. `Screen::render`'s title-bar volume element: right-to-left between the link
   glyph and the readout, `font::title()`, `TEXT_PRIMARY` / `STATUS_WARNING`,
   absent-and-zero-width when `None`.
3. The percent formula `(level * 100 + 63) / 127` with unit tests pinning
   `0 -> 0`, `1 -> 1`, `126 -> 99`, `127 -> 100`.
4. `title_paint_key` folding volume text **and** the muted flag (section 7).
   No `redraw_after`, no time fold.
5. `ActiveBanner::VolumeZero`, the priority `Muted > VolumeZero > Fallback`, and
   the source-selected remedy strings — **and delete
   `"MUTED  Press Up to raise"`** (section 4.2). Measure both new strings against
   the 134px budget.
6. The wake policy: `App::on_volume_changed` marks idle activity when
   `source != Host`; mute/zero entry promotes blank -> dim only. **Write it where
   the firmware's idle level reads it, not only in `run.rs`** (section 5.5).
7. A comment at the OUT meter's sample site stating that the meter is upstream of
   the sink's gain and **must not** be scaled by volume (section 2.1).

### Tess proves

- Headless captures at both `PanelOrientation` values: volume 62%, volume 100%,
  volume 0% (amber, `HEADPHONE VOLUME AT 0` banner), muted (amber `MUTE`, `MUTED`
  banner), `None` (no element, no gap artefact). **Inspect at zoom, not 1x** —
  a title-bar text run is exactly where a sub-row overflow hides.
- A capture with a list screen's `"2 / 5"` readout *and* a volume, proving they
  coexist without overlapping the link glyph.
- A capture with the OUT meter mid-swing *while muted*, committed, so the "meter
  keeps moving while muted" behaviour is pinned by a diffable artefact and a
  future reviewer cannot quietly "fix" it.
- A damage assertion: a frame where only the volume changed must report a damage
  rect equal to `chrome.title`, not the whole frame.
- **On the real target:** turning the headphone dial while the screen is blanked
  wakes it; moving the macOS slider while blanked does not; the value is correct
  the moment it does wake.

### Not designed here

Device-side volume (d-pad Up/Down, Tier 2 — when it lands, the banner remedies
collapse to "Press Up to raise" and the first press on a blanked screen must wake
only, not also step the volume). Per-device volume, startup volume and volume
limit (Settings, on-device UI section 20). The N-region `damage_hint`
generalisation.
