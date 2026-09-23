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
| 2026-09-01 | `2026-09-01-home-fault-strip.md` | Red fault rows bottom-anchored in the hero body (abs y164..218) | **Superseded** by `2026-09-07-home-fault-strip.md` (the concept survives; the row anatomy, width budget, value slot, freshness windows and the no-X ruling do not) |
| 2026-09-02 | `2026-09-02-a-button-label-rule.md` | A's label and liveness come from `Widget::activation` alone, never a `ChromeContribution` | Live |
| 2026-09-02 | `2026-09-02-device-page.md` | Per-device settings, codec choice, the field-list mechanic | Live |
| 2026-09-02 | `2026-09-02-field-list-widget-ruling.md` | One row primitive, two containers | Live |
| 2026-09-03 | `2026-09-03-vertical-out-meter.md` | The vertical stereo OUT meter beside the rail; geometry, gap rhythm, absent-never-frozen. **Supersedes the parent doc's horizontal-meter placement.** Section 8 carries the forward constraint that killed a `gauge_edge()` VOL strip. | Live (32 segments as of `pico-link-53c`) |
| 2026-09-07 | `2026-09-07-volume-on-display.md` | Volume is a number in the title bar, not a bar beside the meter; the mute word; the banner remedies; the screensaver wake policy by `source`; the `None` rule. **Amends the parent doc's 6.3 and the volume-sync doc's section 10.** | Design of record, unbuilt (`pico-link-4v2.6`) |
| 2026-09-07 | `2026-09-07-ldac-quality-selector.md` | The four-entry LDAC quality picker (990/660/330/Adaptive), live-apply-on-press with no confirm, and what Adaptive shows on Home: the live number plus a dim `ADAPTIVE` tag. **Supersedes the device-page doc's 3.2** (which rendered the word `Adaptive` in place of a number) and renames the row to `QUALITY`. **AMENDMENT 5 (2026-09-08, `pico-link-zl75`)** adds §7.1: the disconnected presence gate `ldac_quality != 0` is withdrawn (it made the row reachable only by having already used it) in favour of a separate persisted `ldac_seen` capability bit. | Design of record, unbuilt (`pico-link-7jol.2`) |
| 2026-09-07 | `2026-09-07-audio-fault-model.md` | Every way the audio path can degrade, walked from source (25 modes; 18 currently invisible). The six-key fault catalogue with its glyph/severity/wake attributes, the ruling that retires `OVERRUN`/`UNDERRUN` as a display pair, the 1 Hz counter-delta evaluator with its raise-on-1/clear-on-3 asymmetry, the quiet tripwires, and event tag 15 across the FFI. **The taxonomy half of `2026-09-07-home-fault-strip.md`**; supplies the catalogue h62 left open and corrects its `ENC OVERRUN` name. | Design of record, unbuilt (`pico-link-9eq2.3`) |
| 2026-09-07 | `2026-09-07-home-fault-strip.md` | The home fault strip v2: the 145px row budget the OUT meter left behind, the three-glyph vocabulary (up=filled buffer, down=starved, square=neither) that makes over-run and under-run distinguishable without reading, 20s Live / 120s retire dwell, wake-on-fault with a 20s hold and four anti-strobe limiters, and the `why?` detail page on X. **Amended 2026-09-08 (§6.2/§6.4 overflow ruling: recency selects the 3 displayed keys, first-seen orders them; row slot and 16-char cap now measured, both hold).** **Supersedes `2026-09-01-home-fault-strip.md`**; amends the parent doc's X binding, the idle-policy doc (`IdlePolicy` gains a third, expiring floor) and the volume doc's wake table. | Design of record, unbuilt (`pico-link-9eq2.2`) |

## Render / framework

| Date | Doc | What it settles | Status |
|---|---|---|---|
| 2026-09-02 | `2026-09-02-device-page-seam.md` | The C/Rust seam for codec pinning, per-device settings, live signals | Live |
| 2026-09-02 | `2026-09-02-dirty-gate-across-the-ffi-seam.md` | The dirty gate across the FFI seam | Live |
| 2026-09-06 | `2026-09-06-damage-rect-render-and-partial-blit.md` | `PaintKey`, `damage_hint`, `damage_region_key`, partial blit. Cut a repaint from 100-200ms to ~0.5ms. | Live |
| 2026-09-07 | `2026-09-07-composite-damage-and-paint-plan.md` | Why Home still repaints all 240 rows (three serially-blocking defects, not one), the `PaintPlan` region-set contract that replaces `paint_key`/`damage_hint`/`damage_region_key`, per-segment meter damage, and the App-level regression test. | Live |
| 2026-09-07 | `2026-09-07-device-page-and-single-select-picker.md` | The device page and the generic single-select picker are **composition** over the existing `FieldList`; the one missing capability is `ScreenId` + a stack-wide `refresh_stack`, retiring the `title_at(1) == DEVICES_TITLE` hack. | Live |
| 2026-09-08 | `2026-09-08-link-state-vs-discovery-axis.md` | Splits `LinkState`'s two conflated axes: the A2DP link lifecycle stays on `LinkState` (which **loses** `Scanning`), discovery moves to a new `BtModel::discovering` bool fed by additive event tag 17. Invariants L1/L2 make a non-link cause of the connected-model wipe unrepresentable rather than merely unlikely; all four honesty guarantees keep their old rule verbatim. Introduces `LinkGlyph` so the chrome takes a resolved glyph, not a domain enum. **Amends** `ChromeContribution::link`'s reuse-`LinkState` doc ruling. `PL_EVENT_ABI_VERSION` stays 5. | Design of record, unbuilt (`pico-link-88xs`) |

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
| 2026-09-07 | `2026-09-07-ldac-abr-control-loop.md` | LDAC adaptive bitrate: the 5-rung ladder, tx-queue-depth EMA as the control input, asymmetric dwell, and why a rung change is purely encoder-side. Records Andreas's ruling that a manual quality pick PINS. | Live |
| 2026-09-23 | `2026-09-23-core1-encoder-default.md` | Root-causes core1's LDAC encode saturating its 2ms fill budget: the accounting is correct, the encode itself is 2-2.8x slower on core1, lead cause libldac running from flash (XIP cache eviction against core0's concurrent work). P1 instrumentation + P2 objcopy-into-SRAM fix; does NOT flip `PL_ENCODER_ON_CORE1`'s default (blocked on pico-link-quzf). | Live, implemented on pico-link-nli.9 |

## Platform / power / tooling

| Date | Doc | What it settles | Status |
|---|---|---|---|
| 2026-08-29 | `2026-08-29-picotool-reset-composite.md` | BOOTSEL reset on the audio-composite firmware | Live (see also the CDC `--bootsel` path in CLAUDE.md) |
| 2026-08-30 | `2026-08-30-watchdog.md` | Watchdog for the single-core superloop | Live |
| 2026-09-01 | `2026-09-01-idle-policy-across-the-ffi-seam.md` | `pl_ui_display_power` as a pull-based level; **the firmware does not run `core/src/run.rs`** | Live |
| 2026-09-01 | `2026-09-01-remembered-devices.md` | Remembered-device model, store schema, FFI delta | Live |
