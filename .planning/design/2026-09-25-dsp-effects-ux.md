DESIGN: DSP effects UX -- preset list, editor, device assignment (Uma, 2026-09-25, desk pass, main 7229ecb)

Design of record for ryw.7. The branch-protection hook blocked writing it to .planning/design/ on main. Whoever next works on a branch should commit it verbatim as .planning/design/2026-09-25-dsp-effects-ux.md and index it.

Built on: Ada's DESIGN on pico-link-ryw, Andreas's 2026-09-25 rulings (bundle, Off default, post-DSP meter), design of record 2026-08-28-on-device-ui.md (B always Back, press-edge only, max depth 2, no list over ~12), and the picker idiom of 2026-09-07-ldac-quality-selector.md / 2026-09-25-adaptive-floor-ux.md.
The user-facing word is EFFECT. "Preset" stays an internal/storage word.

== 0. SUMMARY ==
- The Home menu face gets a third row, Effects (Bluetooth / Effects / Settings). The Effects list is depth 1 and the editor is depth 2. Under Settings the editor would be depth 3, which breaks B,B.
- Effects list: one row per effect, then New effect last. A = open, X = delete (via ConfirmView), Y = use (assign to the connected device).
- Editor: ONE screen, six value rows, no scrolling, no sub-screens.
  - Rows: CROSSFEED, then a BAND selector with FREQ / GAIN / Q for the selected band, then NAME.
  - LEFT/RIGHT change the focused value. Up/Down move focus.
- 5 bands in slice 1, with fixed types: low shelf, 3 peaks, high shelf.
- Live preview: while the editor is open, the stream plays the edited effect whatever the device is assigned. X toggles bypass for A/B listening.
- Save exactly once, when the editor leaves the stack by ANY route, not only B.
- The device page gets an EFFECT row, which opens a single-select picker (Off + effects).
- Home shows "FX <name>" in the empty stat-strip slot. It is absent when Off.

== 1. NAVIGATION ==
Home(0) menu face -> Effects(1) -> Effect editor(2)
                              -> Confirm delete(2)
Home(0) Y link    -> Device page(1) -> Effect picker(2)
Everything is B,B to Home, and nothing is depth 3.
- Not under Settings: that makes it depth 3.
- Not on the device page only: effects are global (forgetting a device must not lose the EQ), so their home cannot be a device.
- Amend design-of-record sec 12 ("EQ does not exist").

== 2. INPUT: LEFT/RIGHT ADJUST ON THE EDITOR ==
- Design of record sec 4 says Left=B and Right=A, but that was NEVER BUILT. navigator.rs:322 forwards Left/Right to the focused widget, and every widget ignores them, so today they are no-ops on the device.
- The editor claims them: Left/Right = previous/next value on the focused row. This is the TV-menu idiom: one thumb, and the gesture a user reaches for first.
- If Left stayed Back here, the most natural "decrease" press would become "leave and write flash". That is the one hazard this screen must not have.
- Up/Down move focus. B = back (and save). A/centre are inert here (every row is a value row, so A is dim per the A-label rule).
- Other screens are unchanged: Left/Right stay no-ops.
- Amend the sec 4 table to: "Left/Right: adjust on value rows; otherwise no-op".
- There is no key repeat, so every range is sized so that a typical edit takes under ~10 presses. Do NOT add auto-repeat.

== 3. EFFECTS LIST (depth 1) ==
+------------------------------------------+-----+
| Effects                                  |  A  |
+------------------------------------------+ open|
|  v Relaxed                    2 devices  |     |
|>   Long session               1 device  >+-----+
|    Music                         unused  |  B  |
|    New effect                           >| back|
|                                          +-----+
|                                          |  X  |
|                                          |delete
|                                          +-----+
|                                          |  Y  |
|                                          | use |
+------------------------------------------+-----+
- Rows use FIELD_GUTTERED. The check gutter marks the effect assigned to the CONNECTED device, i.e. what is playing now. No check if nothing is connected or the device is Off.
- The trailing value is in TEXT_SECONDARY: "N devices" / "1 device" / "unused". It counts the paired devices whose preset_id resolves to this id; it is the number you want in front of you before deleting.
- Order is creation order (ascending id). Max 8 (the PL:P slots), so at most 9 rows.
- NEW EFFECT is the last row, with a caret, A = open.
  - It creates the effect in core only and opens the editor. Nothing is written until the editor is left.
  - With 8 effects: the row stays, with value "full" in TEXT_SECONDARY. It is focusable, A is dim and inert (the codec-picker disabled-row precedent).
- X "delete": live on effect rows, unlabelled on New effect. Pushes the Confirm (sec 6).
- Y "use": assigns the focused effect to the connected device (the same AssignPreset as the device picker; the check moves on the echo).
  - Unlabelled and inert if nothing is connected, or if the focused effect is already the connected device's.
  - This is the create -> tune -> use path.
- Empty state: only the New effect row, plus a helvR08 TEXT_SECONDARY note: "Shape the sound per headphone. Crossfeed eases hard-left/right stereo on long listens."

== 4. EFFECT EDITOR (depth 2) ==
The title is the effect's name, and it updates live as NAME changes.
+------------------------------------------+-----+
| Relaxed                                  |  A  |
+------------------------------------------+     |
|>CROSSFEED               <  Medium  >     |     |
|------------------------------------------+-----+
| BAND                  <  2 . peak  >     |  B  |
| FREQ                  <  250 Hz    >     | back|
| GAIN                  <  +3 dB     >     +-----+
| Q                     <  1.4       >     |  X  |
|------------------------------------------+ off |
| NAME                  <  Relaxed   >     +-----+
|                                          |  Y  |
| Playing on Sony WH-1000XM5 (in use)      |reset|
+------------------------------------------+-----+
- Layout: FIELD rows at ~28px, so 6 rows is ~168px. Add two 6px section gaps with a DIVIDER hairline, plus a one-line helvR08 footer.
  - This fits 224px WITHOUT scrolling. Ruby: assert it with row_height and text_width in a test, as the adaptive-floor budget did. Do not trust my estimates.
- Section order:
  - CROSSFEED (the reason the feature exists) comes first and takes the initial focus.
  - Then the band block.
  - NAME is last, as the least used.
- Chevrons < > appear on the FOCUSED row only. They are the affordance that says Left/Right work here.
  - At a range end, the chevron on that side is DIVIDER-dim and the press is a no-op.
  - Numbers clamp; only NAME wraps.
- Width: CROSSFEED ~88px + 8 + "Medium" ~58 = ~154 of 182. BAND ~40 + "1 . low shelf" ~92 = ~140. Measure these.

4.2 Ladders (new-effect defaults in brackets):
- CROSSFEED: Off / Light / Medium / Strong, which is u8 0..3 [Medium]. Words, not numbers, because the goal is comfort. The three strengths are tuned by ear in ryw.8; the words stay the same.
- BAND: "1 . low shelf", "2 . peak", "3 . peak", "4 . peak", "5 . high shelf" [1].
- FREQ: 1/3-octave ISO ladder, 20 Hz..20 kHz, 31 steps. Displayed as 63 Hz, 250 Hz, 1 kHz, 1.25 kHz, 12.5 kHz. Band defaults: 100 Hz LS, 250, 1k, 4k, 8 kHz HS. Bands may cross; there is no neighbour clamp.
- GAIN: -12..+12 dB in 1 dB steps [0 dB]. It is stored as half-dB; half-dB UI steps are deferred.
- Q: 0.5, 0.7, 1.0, 1.4, 2.0, 2.8, 4.0, 5.6, which is q_idx 0..7 [0.7 shelves, 1.4 peaks].
- NAME: see 4.5 [Effect N].
- A band at 0 dB is inert. The Rust builder should emit no biquad for it, so a crossfeed-only effect costs crossfeed only.
- Changing BAND does not move focus. The user sits on BAND and watches FREQ/GAIN/Q change; that is how you read the other bands without a graph.

4.3 X = bypass (A/B listening):
- X is labelled "off" while the effect is audible. Pressing it plays Off, and the label becomes "on". It is a toggle on the press edge.
- While bypassed, the footer reads "Bypassed - X to hear the effect" in STATUS_WARNING.
- ANY value change clears bypass.
- Bypass is editor-local and never saved.
- The ryw.1 crossfade is what makes this click-free, and so usable.

4.4 Y = reset:
- On a band row (BAND/FREQ/GAIN/Q), Y restores the selected band to its defaults. It is unlabelled and inert when the band is already at defaults.
- On CROSSFEED and NAME, Y is unlabelled and inert.
- It exists because there is no key repeat (+9 dB to 0 would be 9 presses). It is reversible and heard at once, so there is no confirm.

4.5 Naming without a keyboard:
- On-screen text entry is designed out (design of record sec 2). NAME cycles a canned list and wraps: Effect N, Relaxed, Long session, Meetings, Music, Movies, Podcasts, Speech, Warm, Bright, Bass, Late night.
- "Effect N": N is the lowest display number not used by another "Effect N". It is NOT the storage id: ids are never reused, so after a few deletes it would read "Effect 7".
- Names used by another effect are SKIPPED, so names are always unique with no rule for the user to learn. There are 8 slots and 12 names, so the list never runs out.
- The longest name is "Long session" (~95px), which fits the title and rows. It is stored in blob v1 name[16], so a later host-side rename needs no format change.

4.6 Footer (the truth about what you hear and what happens on exit):
- Streaming, and assigned to this device: "Playing on <device> (in use)".
- Streaming, not assigned: "Preview on <device> - not in use".
- Connected, not streaming: "Nothing playing - start audio to hear it".
- No device: "No headphones connected".
- Bypassed: "Bypassed - X to hear the effect" (amber).
- Combined boost above the preamp clamp: "Too much boost - may distort" (amber).
  - Ada's auto preamp clamps at -12 dB, so a combined peak above +12 dB can clip.
  - Rust knows this statically from the same response it builds the preamp from, and the editor is where the user can fix it.
- The device name is ellipsised to fit.

== 5. PREVIEW AND SAVE ==
5.1 Program resolution in core (extends Ada 3.1):
(1) Editor open and not bypassed: the edited params.
(2) Editor open and bypassed: Off.
(3) Otherwise: the connected device's preset_id. 0 or unknown resolves to Off.
- Every Left/Right press rebuilds the program. pl_ui_take_dsp_program coalesces, and core1 crossfades.

5.2 Leaving the editor:
- SAVE EXACTLY ONCE WHEN THE EDITOR LEAVES THE STACK BY ANY ROUTE: B, replace_root, or pop_to_root from a connection event. Hook it on removal, not on the B handler, or an event-driven stack reset loses the edit.
- Save only if the params differ from the stored ones, EXCEPT for a new effect, which is always saved. An accidental create is one X-delete away.
- On exit, the program reverts to rule (3). An unassigned effect therefore stops playing on exit.
  - The footer said "not in use" while editing.
  - Y "use" in the list is the one-press fix.
- There is no "saving" UI. The write is user-initiated and may skip audio briefly (accepted, per the memory note on user interaction).
- Rows follow the PresetLoaded echo and are never optimistic. On a save error status, show a MessageView toast: "Couldn't save effect".

5.3 While editing:
- The screensaver does not end the edit: the preview continues, and nothing is saved until the editor leaves the stack.
- If a device connects mid-edit, rule (1) applies at once.

== 6. DELETE (ConfirmView, depth 2) ==
+------------------------------------------+-----+
| Delete effect                            |  A  |
+------------------------------------------+selec|
|  Delete "Relaxed"?                       |     |
|                                          +-----+
|  2 devices use it. They will play        |  B  |
|  with no effect.                         | back|
|                                          +-----+
| >Cancel                                  |     |
|  Delete                                  |     |
+------------------------------------------+-----+
- Existing ConfirmView, with focus on Cancel. "Delete" is in STATUS_ERROR.
- The body follows the usage count:
  - "No device uses it."
  - "1 device uses it. It will play with no effect."
  - "N devices use it. They will play with no effect."
- If the connected device uses it, the stream crossfades to Off on the delete echo.
- The delete is one PL:P erase. Dangling ids read Off (Ada 2.4). The UI never shows a dangling name: every consumer goes through core's resolution.

== 7. DEVICE PAGE: EFFECT ROW + PICKER ==
+------------------------------------------+-----+
| Sony WH-1000XM5                          |  A  |
+------------------------------------------+ open|
| CODEC                              LDAC  |     |
| QUALITY                 Adaptive . 660   +-----+
|>EFFECT                         Relaxed  >|  B  |
| SAMPLE RATE                     48 kHz   | back|
| ...                                      |     |
| Forget this device                       |     |
+------------------------------------------+-----+
- EFFECT is an action row after the codec block (QUALITY, and DOWN TO when shown), before SAMPLE RATE.
- ALWAYS present, connected or not: it is a stored setting and never dashes. Its value is the resolved name, or Off (which covers 0 and dangling ids).
- A opens the "Effect" picker:
+------------------------------------------+-----+
| Effect                                   |  A  |
+------------------------------------------+selec|
| v Off                                    |     |
|   Relaxed                                +-----+
|   Long session                           |  B  |
|   Music                                  | back|
+------------------------------------------+-----+
- Picker mechanics are those of every non-CODEC picker:
  - A applies live (AssignPreset), heard at once if streaming.
  - There is no confirm, and the picker stays open. B is the only exit.
  - The check follows the PairedDeviceUpserted echo.
- Order: Off first, then the effects in list order.
- There is NO New effect row here: creation lives in one place, which keeps it clear that effects are global.
- No effects yet: Off, plus a non-activatable secondary note row "Create one in Home > Effects".
- A new or never-assigned device shows Off (ruling 2).

== 8. HOME INDICATOR ==
+------------------------------------------+-----+
| Pico Link                           (bt) |  A  |
+------------------------------------------+ devs|
| Sony WH-1000XM5                    OUT   |     |
| LDAC                              |# #|  +-----+
| 990 kbps                          |# #|  |  B  |
|                                   |# #|  |     |
| FX Relaxed                        |# #|  +-----+
|                                   |# #|  |  X  |
+------------------------------------------+-----+
- One line in the stat-strip slot (STAT_TOP, area y 128, hero.rs:115), which is empty today.
  - The text is "FX " + name, in TEXT_SECONDARY.
  - Same small face as the bitrate tag, left rule 12, clipped before the meter column.
- ABSENT when Off, which is the default for every new device. A permanent "FX Off" is noise on the glance face.
- Absent when nothing is connected.
- Never in the banner (y 100) or the fault strip (y 158+): it is state, not an alert.
- Fold only the drawn string into the hero paint key.
- The meter is post-DSP (ruling 3), so it needs no UI change. It visibly moves on an X-bypass in the editor, which is a free confirmation.

== 9. VISUAL DELTA ==
- Rows use font::value (helvR12). The footer and notes use helvR08.
- Colour: values TEXT_PRIMARY; counts, footer and FX line TEXT_SECONDARY; the bypass and too-much-boost footers STATUS_WARNING; Delete STATUS_ERROR; end-of-range chevrons DIVIDER.
- The ONLY new render concept is a VALUE ROW: a FieldRow flavour or sibling widget that consumes Left/Right and draws the chevrons, with clamp vs wrap. Fern decides its shape.

== 10. DEFERRED (not slice 1) ==
- RESPONSE CURVE above the rows. This is the first candidate for slice 2: it is what makes a parametric EQ legible, and Rust already samples the response for the preamp. It costs ~60px, which forces scrolling or fewer rows.
- Band-type editing, more than 5 bands (the format allows 10), per-channel EQ.
- Half-dB steps, and a fine/coarse toggle.
- A runtime clip cue (a fault-strip key from dsp_clip, or a red meter peak). Slice 1 relies on the static footer.
- A global default effect for new devices.
- Duplicate-effect, and templates beyond flat + Medium crossfeed.
- Names beyond the canned list.
- HRTF (a separate epic).

== 11. HANDOFF ==
- Fern:
  - the value-row concept;
  - the save-on-removal hook, which must fire on replace_root/pop_to_root as well as pop;
  - the third row on the Home menu face.
- Ruby (ryw.7):
  - the list, editor, delete confirm, and device-page row + picker;
  - the Home FX line;
  - program-resolution rules 1-3;
  - name cycling with skip-used;
  - width/height budget tests.
- Tess:
  - zoomed screenshots of every footer state, the full list (8 effects, New effect "full"), and the empty state;
  - a replace_root-during-edit test proving the save fires.
- Doc amendments: design of record sec 4 (Left/Right) and sec 12 (EQ exists, but not in Settings).

## Implementation notes (Ruby, pico-link-ryw.7, 2026-09-25)

The following deviate from the sketch above, decided during implementation:

- **Save timing supersedes sec 5.2.** Andreas's 2026-09-25 ruling (bead
  comment): save IMMEDIATELY on every value change, not on editor exit.
  Live preview still applies instantly and independently. A brand-new
  effect is saved once immediately on creation (`SavePreset{preset_id: 0,
  ..}`), before the editor screen is even built.
- **Q ladder uses the shipped `Q_TABLE`** (`0.4, 0.6, 0.71, 1.0, 1.4, 2.0,
  3.2, 8.0`, `core/src/dsp/preset.rs`), not this doc's approximate worked
  values (`0.5, 0.7, 1.0, 1.4, 2.0, 2.8, 4.0, 5.6`) -- `q_idx` indexes the
  one table `coeffs.rs` actually builds biquads from, so the editor always
  displays exactly what's shipped rather than a second, incompatible
  ladder.
- **`ConfirmView::with_subline` is one clipped, non-wrapping line** -- the
  delete confirm's body text is shorter than sec 6's two-line mockup
  ("1 device will lose this effect." rather than "...uses it. It will
  play with no effect."), measured to fit at `font::username()` on a
  240px panel; a width-budget test pins the worst case (8 devices).
