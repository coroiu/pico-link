# LDAC Adaptive floor: name, place, and what the panel shows

Bead `pico-link-d42g`. Uma (UX), 2026-09-25. Desk design against `54156dd`.

**RULED 2026-09-25 (Ada, `2026-09-25-adaptive-floor.md` sec 1): GLOBAL**, not
per-device -- same reasoning as the BUFFER/cushion-policy setting (the air
varies, not the headset). Section 7 below ("Preference: per-device") is
therefore **not** the shape shipped; it is kept for the record per this
project's "don't delete superseded entries" rule. **Section 6 ("If Ada rules
global: Settings > `LDAC MIN`") is the design that applies** -- Settings row 2,
directly under `BUFFER`, label `LDAC MIN`, always visible, nothing added to the
device page.

**Settled by Andreas, not re-opened:** a setting that caps how far Adaptive
(ABR) may step down, options **330 kbps (default) / 246 / 198**. Not extra
pinned picks. Sub-330 is probably untested by sinks (Android ships the same MQ
limit), so the UI must not oversell it. Ada decides global vs per-device and the
persisted record in parallel; this design works for both (section 6).

Builds on: `2026-09-07-ldac-quality-selector.md` (QUALITY row and picker,
check = intent / number = reality, absent-not-dim), `2026-09-24-congestion-cushion.md`
(Settings > BUFFER precedent).

## 1. Width budget (measured, not guessed)

Clamp source: `core/src/render/menu.rs` `draw_row` + `RowStyle::FIELD`
(left 12, value right margin 12, caret in its own gutter) and
`LABEL_VALUE_GAP = 8`. The label is **clipped, not ellipsised**, at
`value_left - 8`. Budget: label x 12 to value right edge 194 = **182px**;
pickers use `FIELD_GUTTERED` (+12 check gutter) = **170px**. Labels and values
are both `font::value` (helvR12). Widths below are summed glyph advances read
from `u8g2_font_helvR12_tf` in the u8g2-fonts 0.5.2 font data.

| Text | px |
|---|---|
| `DOWN TO` | 80 |
| `LDAC MIN` | 78 |
| `330 kbps` / `246 kbps` / `198 kbps` | 66 each |
| `default` | 49 |
| `untested` | 63 |
| rejected: `ADAPTIVE MIN` 115, `ADAPTIVE FLOOR` 143, `LDAC FLOOR` 106 | overflow, or 2px spare |

- Device page row: `DOWN TO` 80 + 8 + 66 = **154 / 182**.
- Settings row (global variant): `LDAC MIN` 78 + 8 + 66 = **152 / 182**.
- Picker row: gutter 12 + `330 kbps` 66 + 8 + `untested` 63 = **149 / 170**.

Ruby: assert these in a test with `text_width`, as
`DEVICE_PAGE_ADAPTIVE_VALUE_BUDGET_PX` already does.

## 2. Preferred placement: device page, directly under QUALITY (per-device)

```
+----------------------------------------+-----+
| Sony WH-1000XM5                        |     |
+----------------------------------------+  A  | open
| CODEC                            LDAC  |     |
| QUALITY                Adaptive · 660  |     |
|>DOWN TO                     330 kbps >+-----+
| SAMPLE RATE                   48 kHz   |  B  | back
| USB IN                          ...    |     |
| ...                                    +-----+
+----------------------------------------+-----+
```

- **Label `DOWN TO`, value `330 kbps`.** Read with the row above it, the pair is
  a sentence: *Quality: Adaptive, down to 330 kbps.* No "floor" or "ABR"
  concept needed; the unit is the word the user just read on QUALITY. "Floor"
  stays an internal/doc word.
- **Present only when the QUALITY setting (stored, or the firmware default if
  never chosen) is Adaptive.** Absent, not dim, when QUALITY is pinned to
  990/660/330, and absent whenever QUALITY itself is absent (not LDAC). This is
  the quality selector's absent-not-dim rule: a readonly row is still
  focusable, so a dim row would be a dead d-pad stop that means nothing while a
  pin is active. The stored floor is **kept** while pinned, so returning to
  Adaptive restores it.
- Consequence to accept: picking Adaptive in the (stay-open) Quality picker and
  pressing B makes `DOWN TO` appear under QUALITY. That is the discovery path:
  it shows up exactly when it starts to mean something.
- `A` on the row opens the picker (default `open` verb, like other action rows).

## 3. The picker

Title **`Adapts down to`**.

```
+----------------------------------------+-----+
| Adapts down to                         |     |
+----------------------------------------+  A  | select
| v 330 kbps                     default |     |
|   246 kbps                    untested |     |
|   198 kbps                    untested +-----+
|                                        |  B  | back
+----------------------------------------+-----+
```

- Highest first, matching the Quality picker; the default sits under initial focus.
- Notes in `TEXT_SECONDARY`, **not amber**. `STATUS_WARNING` means "you did not
  get what you asked for"; here the user gets exactly what they pick. The
  honesty lives in the word, not the colour.
- **`untested`, not `more reliable` / `fewer dropouts`.** We do not know a given
  sink handles 246 or 198 well; claiming reliability is the oversell to avoid.
- **If the by-ear round fails a rung on both test pairs, drop it from the
  picker rather than label it.** Shipping an option known to be bad, with a
  hedge, is worse than a two-option list.
- Same mechanics as every device/settings picker except CODEC: `A` applies
  live, no confirm, picker stays open (`Action::None`), B is the only way out.
  Check follows the stored echo, never optimistic. Reversible in one press, so
  no ConfirmView.
- First run: storage `0` = default; the check shows on `330 kbps` (quality
  selector §7: `0` is a storage state, never a display state).

## 4. Home and the quality readouts when Adaptive sits at the floor

**No change, no at-floor indicator.** Home keeps `N kbps  ADAPTIVE`
(`198 kbps  ADAPTIVE` at a 198 floor). Device page QUALITY keeps
`Adaptive · 198`; the Quality picker's Adaptive note keeps `198 now`.

- The digits already say where ABR is. "At floor" only means something relative
  to a setting the user chose, and that setting is one press away (`X` ->
  device page, `DOWN TO` is the row under the live number).
- Sitting at the floor is ABR doing its job (quality selector §6.1: meetings; no
  attention-grabbing events for success).
- Sub-330 is where a sink may crackle. A user who hears it and glances sees
  `198 kbps`, which is the diagnosis; the fix is two presses away.
- Forward note for the future link-quality fault ("at the floor and still
  congested", quality selector §6.1): its threshold must be the **configured**
  floor, not a hardcoded 330.

## 5. Save note

Reuse what exists. If Ada's persistence path stages the write while streaming,
the existing `Saves when playback stops` note row covers `DOWN TO` exactly as it
covers CODEC/QUALITY. If the write is immediate (D11 / BUFFER precedent), no
note. No new copy either way.

## 6. If Ada rules global: Settings > `LDAC MIN`

- Row 2 of Settings, directly under `BUFFER` (audio rows together; the two
  display rows stay adjacent below): `LDAC MIN   330 kbps`.
- Out of device context the label must name the codec, so `DOWN TO` does not
  work; `LDAC MIN` is the clearest label that fits (section 1).
- **Always visible**, never dim: Settings has no device whose QUALITY could gate
  it, and it applies whenever any device is on Adaptive.
- Same picker and notes; title `LDAC minimum`.
- Nothing added to the device page in this variant (a device-page row editing a
  global setting would lie about its scope).

## 7. Preference: per-device

1. **The risk is per-headphone.** Sub-330 is untested *by sinks*; one pair may
   be fine at 198 while another crackles. A global floor lets an opt-in that
   suits one headset degrade the other. BUFFER went global because the air
   varies, not the headset; this is the opposite case.
2. **It sits next to the knob it qualifies.** QUALITY is already per-device on
   the device page.
3. **Conditional visibility only works per-device.** The global variant has to
   be always visible, including to users whose devices are all pinned.

Cost: one more byte in the paired-device record, and a row that appears and
disappears with QUALITY's value. Both have precedent (`ldac_quality`).

## Handoff

- **Ada:** global vs per-device and the record; UX prefers per-device (§7).
- **Fern:** nothing new; FieldList row + existing single-select picker.
- **Ruby:** row, picker, width asserts (§1), headless screenshots of the device
  page with QUALITY Adaptive vs pinned (row present/absent), the picker, and
  Home at a 198 floor. Copy verbatim from this doc.
