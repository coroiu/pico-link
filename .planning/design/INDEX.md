# Design documents index

One design per file, `YYYY-MM-DD-short-title.md`. Newest last within each group.
Research lives in `.research/findings/`; decisions in `.planning/decisions/`;
status in `.planning/progress.md`.

**Do not delete superseded entries — mark them Superseded/Amended and say by what.**

## UI / UX

| Date | Doc | What it settles | Status |
|---|---|---|---|
| 2026-08-28 | `2026-08-28-on-device-ui.md` | The whole on-device UI (v5): navigation model, Home, the nine Home states, the fallback chain, the wizard, settings, the three-tier enabling work list. **The parent design of record.** | Live; sections 6.1/6.3 amended by the meter and volume docs below |
| 2026-08-29 | `2026-08-29-button-rail-edge-region.md` | The button rail and the chrome edge region; `PanelOrientation`'s `button_edge`/`slot_order` | Live |
| 2026-09-01 | `2026-09-01-home-alignment-grid.md` | Home's one grid, two rules; kills the accumulating `cursor_y` rhythm and the Bitwarden shield | Live |
| 2026-09-01 | `2026-09-01-home-fault-strip.md` | Red fault rows bottom-anchored in the hero body (abs y164..218) | Designed, unbuilt (`pico-link-h62`) |
| 2026-09-02 | `2026-09-02-a-button-label-rule.md` | A's label and liveness come from `Widget::activation` alone, never a `ChromeContribution` | Live |
| 2026-09-02 | `2026-09-02-device-page.md` | Per-device settings, codec choice, the field-list mechanic | Live |
| 2026-09-02 | `2026-09-02-field-list-widget-ruling.md` | One row primitive, two containers | Live |
| 2026-09-03 | `2026-09-03-vertical-out-meter.md` | The vertical stereo OUT meter beside the rail; geometry, gap rhythm, absent-never-frozen. **Supersedes the parent doc's horizontal-meter placement.** Section 8 carries the forward constraint that killed a `gauge_edge()` VOL strip. | Live (32 segments as of `pico-link-53c`) |
| 2026-09-07 | `2026-09-07-volume-on-display.md` | Volume is a number in the title bar, not a bar beside the meter; the mute word; the banner remedies; the screensaver wake policy by `source`; the `None` rule. **Amends the parent doc's 6.3 and the volume-sync doc's section 10.** | Design of record, unbuilt (`pico-link-4v2.6`) |

## Render / framework

| Date | Doc | What it settles | Status |
|---|---|---|---|
| 2026-09-02 | `2026-09-02-device-page-seam.md` | The C/Rust seam for codec pinning, per-device settings, live signals | Live |
| 2026-09-02 | `2026-09-02-dirty-gate-across-the-ffi-seam.md` | The dirty gate across the FFI seam | Live |
| 2026-09-06 | `2026-09-06-damage-rect-render-and-partial-blit.md` | `PaintKey`, `damage_hint`, `damage_region_key`, partial blit. Cut a repaint from 100-200ms to ~0.5ms. | Live |

## Audio / Bluetooth / USB

| Date | Doc | What it settles | Status |
|---|---|---|---|
| 2026-08-29 | `2026-08-29-a2dp-source-pipeline.md` | M4 A2DP source pipeline | Live |
| 2026-08-29 | `2026-08-29-usb-audio-alt1.md` | Why macOS refused UAC2 alt 1, and the fix | Live |
| 2026-08-30 | `2026-08-30-a2dp-drain.md` | Decoupling fill from send | Live |
| 2026-08-30 | `2026-08-30-ldac.md` | LDAC design of record | Live |
| 2026-08-30 | `2026-08-30-pcm-pacing.md` | What `pbv` actually measured, and the two defects behind it | Live |
| 2026-09-01 | `2026-09-01-2ap-tolerate-undersupply.md` | Tolerating a host that halves ISO-OUT supply | Live |
| 2026-09-02 | `2026-09-02-media-keys.md` | AVRCP passthrough -> USB HID consumer control | Live |
| 2026-09-02 | `2026-09-02-volume-sync.md` | Host <-> dongle <-> headphones volume: canonical 0..127, the loop-breaking rule, event tag 14 with `source`. Section 6's M1/M2 recommendation is **superseded** (VT4a measured that macOS does act on the UAC2 status interrupt EP); section 10's UX split is answered by the 2026-09-07 volume doc. | Live, partly superseded |

## Platform / power / tooling

| Date | Doc | What it settles | Status |
|---|---|---|---|
| 2026-08-29 | `2026-08-29-picotool-reset-composite.md` | BOOTSEL reset on the audio-composite firmware | Live (see also the CDC `--bootsel` path in CLAUDE.md) |
| 2026-08-30 | `2026-08-30-watchdog.md` | Watchdog for the single-core superloop | Live |
| 2026-09-01 | `2026-09-01-idle-policy-across-the-ffi-seam.md` | `pl_ui_display_power` as a pull-based level; **the firmware does not run `core/src/run.rs`** | Live |
| 2026-09-01 | `2026-09-01-remembered-devices.md` | Remembered-device model, store schema, FFI delta | Live |
