# Visual identity: replace the Bitwarden-era palette, and a 48-segment meter

Bead: pico-link-5ful (gates pico-link-je0g, the public flip). Author: Uma. Status: proposed,
awaiting Andreas's pick of direction.

Mock: `web-companion-mock-v2.html` in the session scratchpad
(`/private/tmp/claude-501/-Users-andreas-code-esp32-bluetooth-tx/da4b62ee-42a0-4b9e-909b-bc15aa9c11fa/scratchpad/`).
Mock controls (bottom right) switch palette (Iris / Orchid / Ink / Old) and web Dark/Light.
The "Device 240x240" tab renders every palette's Home (live 48-segment meter) and a menu face at
240x240 in RGB565 with 1-bit text, scaled 3x without smoothing. URL params: `?pal=orchid&mode=light&page=dev`.
The scratchpad is ephemeral: copy the mock into the repo (e.g. `web/mock/`) if it should survive.

## 1. Why

`palette::BRAND` = `Rgb565::new(3,23,27)` expands to `#195DDE`, one RGB565 step from Bitwarden's
`#175DDC`, on a navy ground. The navy ground matters as much as the blue: removing the blue
accent but keeping the navy would still read as Bitwarden. All three directions below drop the
navy for a neutral or near-neutral ground.

Incidental finding: the hex values in theme.rs doc comments (`#0B1120`, `#16213A`, ...) do not match
the constants (they expand to `#081021`, `#19203A`, ...). Rewrite them with the new values.

## 2. Token set

Keep all 14 existing tokens and names. Add one:

- **`METER_SAFE`**: the fill of the meter's normal (below -18 dBFS) zone. Today the code draws
  `TEXT_PRIMARY` there while the design doc (2026-09-03 section 5) and the function's own doc comment
  both say green. Decoupling it from text colour fixes the drift and is required for Ink, where
  the peak-hold cap (`BRAND_BRIGHT`, white) would disappear on a white bar. In Iris and Orchid
  `METER_SAFE == TEXT_PRIMARY`, which is what the device shows now.

The semantic tokens (`STATUS_SUCCESS/ERROR/WARNING`) are identical in all three directions. They
are meaning, not identity. They are re-tuned slightly so the RGB565 values sit on clean
steps: `(8,54,17)`, `(31,22,11)`, `(31,44,4)`. `*_DIM` stays as the per-component midpoint toward
`BACKGROUND`, recomputed per palette.

**RGB565 neutrals:** green has 6 bits, so a grey needs `G = 2R` (or `2R+1`). Converting a hex grey
naively tints it: `#161616` quantises to `#191419` (magenta). Every neutral below was picked
directly in 565 space, not converted.

## 3. The three directions (device RGB565)

| Token | Iris | Orchid | Ink |
|---|---|---|---|
| BACKGROUND | (1,2,2) #080810 | (2,3,2) #100C10 | (1,2,1) #080808 |
| SURFACE | (3,6,4) #191821 | (4,6,4) #211821 | (3,6,3) #191819 |
| SURFACE_ELEVATED | (5,10,7) #29283A | (6,9,6) #312431 | (5,10,5) #292829 |
| DIVIDER | (6,12,8) #313142 | (7,11,7) #3A2D3A | (6,12,6) #313131 |
| BRAND | (11,18,26) #5A49D6 | (18,11,19) #942D9C | (8,16,8) #424142 |
| BRAND_BRIGHT | (18,34,31) #948AFF | (27,30,31) #DE79FF | (31,63,31) #FFFFFF |
| TEXT_PRIMARY | (30,61,30) #F7F7F7 | (30,61,30) #F7F7F7 | (29,58,27) #EFEBDE |
| TEXT_SECONDARY | (20,40,23) #A5A2BD | (21,40,21) #ADA2AD | (19,38,18) #9C9A94 |
| METER_SAFE (new) | = TEXT_PRIMARY | = TEXT_PRIMARY | (20,40,19) #A5A29C |
| STATUS_SUCCESS | (8,54,17) #42DB8C | same | same |
| STATUS_ERROR | (31,22,11) #FF595A | same | same |
| STATUS_WARNING | (31,44,4) #FFB221 | same | same |
| STATUS_ERROR_DIM | (16,12,6) #843131 | (16,12,6) | (16,12,6) |
| STATUS_WARNING_DIM | (16,23,3) #845D19 | (16,23,3) | (16,23,2) #845D10 |

- **Iris.** Blue-violet on violet-graphite. The smallest step away from today, and every screen keeps its
  feel. Risk: violet on dark gray is common app territory (Discord `#5865F2`, Twitch `#9146FF`);
  the least ownable of the three.
- **Orchid.** Violet-magenta on plum-black. The most ownable: no audio brand or password tool
  sits here, and magenta is the one saturated hue clear of red/amber/green across the common
  colour-vision deficiencies. Hi-fi/night feel without the gold "Hi-Res Audio" cliché.
- **Ink.** Monochrome: paper white on black, grey surfaces, no brand hue. The semantic colours
  are the only colour on screen, so faults and meter zones read fastest. It gives up a brand
  colour on the web, and it needs `METER_SAFE`.

Brand-proximity checks: none of the three uses 170-230 degree hues (Bitwarden, and the teal
family). Orchid avoids T-Mobile's `#E20074` (hotter, pinker) and Raspberry Pi's raspberry.

## 4. Web tokens

Dark = the device values above, expanded (`round(v*255/31)`, `round(v*255/63)`). Light is
derived. Web-only extras: `--text-3` (recent-tier metadata, was hardcoded `#6d7690`),
`--meter-safe/-warn/-err` (meter fills can be brighter than text colours on light),
`--on-brand`, `--on-warn`, `--scrim`, `--wash` (EQ band fill), `--zero` (0 dB gridline).
The full CSS is in the mock's `<style>` (`[data-pal=..][data-mode=..]` blocks). Light sets:

| | Iris light | Orchid light | Ink light |
|---|---|---|---|
| bg / surface / surface-2 | #F6F6F9 / #FFF / #ECEBF4 | #FAF6F9 / #FFF / #F3EAF1 | #F3F0E8 / #FBFAF5 / #E8E4DA |
| divider | #D9D7E6 | #E4D6E1 | #D6D0C3 |
| brand / brand-bright | #5A49D6 / #4A39C2 | #8E2C99 / #7A2FA0 | #141414 / #141414 |
| text / text-2 | #15141C / #5C5A6E | #1C121B / #6A5867 | #141414 / #5E5A52 |
| meter-safe | #2B2A36 | #2E2230 | #6E6A62 |

Light semantics (all palettes): ok `#137A4B`, err `#C0145E`, warn `#8C5800`, meter-warn fill
`#E8A200`, meter-err fill `#C8175A`. The error is deliberately crimson on light: with the obvious
`#C8262E` red, err and warn text collapse under deuteranopia (dE 8). Crimson brings that to dE 38.
Orchid light shifts its accent toward violet (`#7A2FA0`) so it stays clear of crimson (dE 55).

Ink web dark uses a paper-white `--brand` with dark `--on-brand` for primary buttons. That is the one
place web `--brand` differs from device `BRAND`, whose only device use is the letter chip.

## 5. Contrast and colour-vision checks (measured, script-computed)

WCAG ratios on the panel values (dark): TEXT_PRIMARY/BG 16.8-18.6; TEXT_SECONDARY/BG 7.1-8.1;
TEXT_SECONDARY/SURFACE_ELEVATED 5.2-6.2; TEXT_PRIMARY on BRAND 5.8 (Iris), 6.1 (Orchid), 8.5 (Ink);
BRAND_BRIGHT/BG 7.0 / 7.7 / 20; ERROR 6.3-6.5, WARNING ~11, SUCCESS ~11. The `*_DIM` pair stays
at 2.3/3.4 on purpose (the fault strip's Recent tier is meant to recede, as it does today).
Web light: text 16-17, text-2 >= 5.4 on every surface, brand-bright as text >= 7.2, ok/err/warn
text 4.7-5.9 (all AA for body text).

CIE76 dE after Machado-2009 simulation, key pairs (normal / protan / deutan / tritan):

- warn vs err (device): 64 / 59 / 37 / 45. Also separated by luminance (11 vs 6.4 contrast).
- success vs error: 122 / 33 / 20 / 130. This is the classic red-green weak pair. It is acceptable
  because the two never share a slot. Success is always a word ("On", "Streaming") and the meter uses
  white/amber/red, never green.
- METER_SAFE vs amber zone: >= 77 in normal/protan/deutan, 39-46 tritan.
- Hold cap (BRAND_BRIGHT) vs amber zone. Iris: 58 tritan, Orchid: **25 tritan** (the one weak
  spot; tritanopia is ~0.01%, and the cap is still a single row above a lit bar), Ink: 47.
- ERROR_DIM vs WARNING_DIM: 37 / 35 / 22 / 24, the same as today. The fault strip also
  separates them by glyph shape and name.

## 6. Meter: 48 segments

**Verdict: 48 fits on the panel, as 2px segment + 1px gap = 143px**, centred in the unchanged
174px footprint (top pad 15; today 4+1 x32 = 159px, pad 7). 48 x (3+1) - 1 = 191px does not fit.

Why 48 works well beyond "3x":

- **48 segments over -48..0 dBFS = exactly 1 dB per segment.** The colour zones become the
  conventional marks. Red = top 6 (indices 42-47, -6..0 dBFS), amber = the next 12 (30-41,
  -18..-6 dBFS, where -18 is the EBU alignment level), safe = 0-29. These are the same 5/8, 2/8, 1/8
  proportions as today.
- The gap is still 1px, as it is today. Only the segment shrinks. Pixel pitch is ~0.097mm
  (1.3in, 240px). A 1px gap is ~1.1 arcmin at 30cm and ~0.84 arcmin at 40cm, so segments are
  countable up close and fuse into a finely ruled bar at arm's length. That is the right behaviour
  for 48 (nobody counts them), and it reads as "hi-res".
- A 2px peak-hold cap in BRAND_BRIGHT is still a clearly visible 12x2 bright line (checked in the
  3x render).

**Known limit, flagged for Ada/Ruby:** the level arrives as a linear `u8`. The 48-entry table
`round(255*10^((-48+i+1)/20))` is
`1,1,1,2,2,2,2,3,3,3,4,4,5,5,6,6,7,8,9,10,11,13,14,16,18,20,23,26,29,32,36,40,45,51,57,64,72,81,90,102,114,128,143,161,181,203,227,255`
and has only 38 distinct values. The bottom 18 segments (-48..-30 dBFS) have just 8 distinct
thresholds and will move in clumps of 2-3. That is harmless for music, which lives above -30 dBFS.
True 1 dB steps at the bottom need a wider level (u16) or a dB-domain u8 from C. Do not block on it.

Fallbacks if the panel check disagrees: 43 segments at 3+1 = 171px (fills the footprint), or
40 at 3+1 = 159px (today's exact drawn height). Both lose the 1 dB/segment mapping.

The web meter uses the same 48, 1px gaps, square corners (mock updated).

## 7. Recommendation

**Orchid.** It is the only direction that is both unmistakably not-Bitwarden and ownable as a
public project's colour (README, logo, companion). It passes contrast everywhere, its one CVD
weak spot is rare and non-critical, and it keeps the device's white codec hero. Runner-up: Ink,
which is the most legible on the panel but gives the project no colour of its own.

Andreas's call. The mock shows all three on both surfaces.

## 8. Handoff

**Ruby** (after Andreas picks):
1. `core/src/render/theme.rs`: replace the palette constants with the chosen column. Add `METER_SAFE`
   and use it in `vertical_level_segment_color` (and `level_segment_color`) instead of
   `TEXT_PRIMARY`. Rewrite the stale hex doc comments. Recompute both `*_DIM` from the new
   BACKGROUND with the documented midpoint rule.
2. Meter: `VERTICAL_METER_SEGMENT_COUNT` 48, `_SEGMENT_HEIGHT` 2, `_GAP` 1. Put the 48-entry table
   above in `VERTICAL_METER_DBFS_THRESHOLDS`. Zones `>= COUNT-6` red, `>= COUNT-18` amber. Update the
   paint-key `expect("segment count is in 0..=16")` strings in hero.rs:1333-1336 and the
   32-specific tests in theme.rs (1081-1160).
3. Regenerate the committed screenshot fixtures (home-screenshots/, fixtures/, wizard-screenshots/).
   They are the regression net for the colour change.
4. Web tokens: copy the chosen `[data-pal]` dark+light blocks from the mock.

**Tess:** zoomed fixture diff plus a webcam capture of Home on the panel. Two questions, each seconds
to answer: does the 2+1 meter read as a bar (not mush), and is the Recent-tier dim fault still
distinct from TEXT_SECONDARY on the real IPS.

**Fern:** nothing structural. Palette stays a flat `const` module. A second compile-time palette
is not needed.
