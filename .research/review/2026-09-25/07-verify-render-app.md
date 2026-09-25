# 07 — Adversarial verification of the render (04) and app (05) P1s

Reviewed tree: `1104a58` (review-docs-only commits on top of `2e37164`; `git diff --stat 2e37164..HEAD -- core ui-ffi emulator firmware` is empty, so the source is the same).
Method: a scratch crate (`$SCRATCHPAD/verify`, path-dep on `core/`, not in the repo) driving the public `App` API; a patched copy of `core/` in the scratchpad (`tree/`, `tree2/`) to try the fixes against the full test suite and the pinned PNG fixtures. No source in the repo was edited.

## Summary

| Finding | Verdict | Reviewer sev | My sev | Tier |
|---|---|---|---|---|
| F-render-01 | **CONFIRMED** (numbers reproduce exactly); symptom and fix sketch corrected | P1 | **P1** | Sonnet (the stale-pixel fix); legend placement → Uma, split out as F-render-V1 |
| F-render-02 | **PARTIALLY CONFIRMED** (the coupling is real; the counts are slightly off; the severity is contested) | P1 | **P2** | Sonnet (moves), Opus only if `ScreenId` goes generic |
| F-app-01 | **CONFIRMED** (all three sequences, plus a fourth); separate root cause from F-bt-01/F-app-03 | P1 | **P1** | Sonnet |
| F-app-02 | **PARTIALLY CONFIRMED** (routes as stated; reachability is 4 full + 1 partial of 14, not ~3; the fix sketch cannot be emulator-only) | P1 | **P2** | Sonnet, blocked by F-ffi-03's seam |
| F-app-06 (P2 spot-check) | **CONFIRMED**, divergence table below | P2 | P2 | Opus |
| F-render-04 (P2 spot-check) | **CONFIRMED** (5 / 3 / 2); one correction to the fix sketch | P2 | P2 | Haiku to dedupe, Sonnet to change the algorithm |

New: **F-render-V1** (the OUT legend overlaps long names on every full render, split out of F-render-01, P2) and **F-app-V1** (the emulator never drains `App::poll_command`, P3).

---

## F-render-01 — CONFIRMED (symptom and fix sketch corrected)

**Evidence** (scratch crate: `ConnectSucceeded` + `PairedDeviceUpserted` + `LinkStateChanged(Connected)` + `CodecChanged(LDAC)` + `LevelsChanged` at t=1.00 s, render; second `LevelsChanged` at t=1.05 s, render (frame 2); then `mark_dirty()` + render as the full-frame reference):
```
Sony WH-1000XM5                d2=(158,16 48x224)  name ink x13..156 -> 13..156   diff vs full = 0
Sennheiser Momentum 4 Wireless d2=(158,16 48x224)  name ink x13..190 -> 13..157   diff = 102  bbox x158..190 y30..40
Bang & Olufsen Beoplay HX      d2=(158,16 48x224)  name ink x13..192 -> 13..157   diff = 118  bbox x158..192 y32..43
Bose QC Ultra Headphones       d2=(158,16 48x224)  name ink x13..190 -> 13..157   diff = 123
AirPods Pro / WH-1000XM4       name ink ..104 / ..111                              diff = 0
OUT legend ink (TEXT_SECONDARY, name rows): x174..194, y29..36 — drawn over "Wir..." glyphs (ASCII dump confirmed)
persist: 3 s of 50 ms publishes, then 5 s of silence, rendering whenever dirty -> name ink at t=9 s still x13..157
```
The reviewer's 102 and 118 reproduce exactly.

**Root cause, confirmed by reading the code.** `meter_footprint` (`hero.rs:641-645`) is the strip's x-span × the **full** area height, so it covers name-band columns 158..206. The name is drawn only inside `if ctx.needs(hero_body)` (`hero.rs:853`). `hero_body` is to the left of the strip and below the name band, so its x-range (0..158) is disjoint from the damage rect and the gate is false on a meter-only frame. `Screen` fills the damage rect with background, so the name's tail is erased and never redrawn. The legend draws at `name_y` inside the strip (`hero.rs:1172-1184`).

**Corrections to the report:**
- *Symptom.* It is not "a 1 Hz flicker". Today the tail stays erased: the first meter-only frame erases it, and it stays gone, even 5 s after the meter disappears, until something in the body changes. The 1 s backstop cannot repair it because it is dead (F-ffi-01: a render with no change reports zero damage, so nothing is blitted). If F-ffi-01 is fixed with a full-frame `mark_dirty`, the symptom *becomes* a 1 Hz flicker.
- *"Most non-Sony names measure > 146 px"* overstates it. Common short names (AirPods Pro 93 px, WH-1000XM4 100 px) are unaffected. Long marketing names are affected.
- *Fix sketch (a) is not "no budget changes".* The meter block's top rule sits flush with the hero word at content y40 (`hero.rs` comment "40 + 174 = 214"). Putting the legend "top of the meter block, inside the strip" means shrinking the 174 px block or moving the legend into the 10 px bottom inset. That is a geometry change and a design call. It re-renders the two pinned fixtures that show a live meter (`home-screenshots/04_connected_with_out_level.png`, `08_muted_with_out_level_still_swinging.png`).

**Better fix (tried and verified).** Decouple the two problems. The stale-pixel bug needs no design call: draw the device name under `ctx.needs(name_band)` instead of only under `ctx.needs(hero_body)`. A meter-only frame then re-rasterises one text line, and the hero word, bitrate, banner and stats are still skipped. The full-render output is unchanged by construction.
```
patched copy of core/: cargo test -p pico-link-core -> 478 + 1 (home_screenshot_fixtures, byte-compare) + 1 + 5 passed, 0 failed
repro against patched copy: diff vs full = 0 for all six names; persist case name ink x13..190
```
**Would it break a golden?** No: all 8 pinned home fixtures stay byte-identical. The reviewer's option (a) *does* change fixtures 04 and 08. Option (b) changes nothing pinned (the fixtures use "Sony WH-1000XM5", 144 px), but truncates that name on screen.

**Verification line validated.** Changing the `home_connected_with_out_level` case in `freshness_cases()` to "Sennheiser Momentum 4 Wireless" makes `damage_rendered_frame_matches_a_full_frame_render_of_the_same_state_for_every_screen` **fail** on unpatched code (invariants.rs:322) and **pass** on the patched copy. Use exactly that as the regression test.

Severity **P1** (a visible defect on the MVP screen), Tier **Sonnet**, Effort **S**.

### F-render-V1: The OUT legend is painted over any device name wider than ~161 px, on every full render
- Severity: P2   Confidence: High   Effort: S (after the design call)   Tier: Uma decides, then Sonnet
- Location: `core/src/render/hero.rs:853-874` (name budget 182 px, `name_band` full width), `hero.rs:1172-1184` (legend at `name_y`, x-centred in the strip)
- Evidence: legend ink x174..194 / y29..36. The name ink for "Sennheiser Momentum 4 Wireless" reaches x190, and the legend overwrites its last ~5 glyphs even in a full frame (so this is not a damage bug). Design `2026-09-03-vertical-out-meter.md` §3 keeps the full name budget *and* puts the legend in the name row. The two rules collide.
- Why it matters: long names read as garbage while audio plays. This was split out of F-render-01 so the P1 stale-pixel fix does not wait on a design decision.
- Fix sketch: Uma picks one: legend in the strip's 10 px bottom inset; legend inside a shortened block; or clamp the name to 134 px while the meter is live. Regenerate fixtures 04/08 if the chosen option moves pixels in them.
- Verification: a `hero.rs` test that renders a 30-char name with a live meter and asserts no `TEXT_SECONDARY` pixel within the name's ink bbox.

---

## F-render-02 — PARTIALLY CONFIRMED; severity P1 → P2

**Evidence** (`grep -n 'crate::app\|crate::run' core/src/render/*.rs`, non-comment):
```
production lines: hero.rs 9 (L38 use {FaultGlyphClass,FaultKey,FaultLog,FaultSeverity}; L614,616,660,717 crate::run::FAULT_*; L1212-1213,1314-1315 crate::app::decay_peak)
                  home.rs 1 (L94: 20 symbols incl. 4 build_*_screen, BtModel, ModelHandle, Refresh, ScreenCarry, Command)
                  wizard.rs 1 (L42: 8 symbols — the reviewer said 7)
                  screen.rs 1, navigator.rs 1 (ScreenId only; stored and returned, never inspected by render)
test-only:        hero.rs 2196+, home.rs 781-908, wizard.rs 574
consumers of render::{home,wizard}: only app/mod.rs:22, app/screens/devices.rs:16 (+ app tests); render::hero also used by core/examples/{hero_widget_probe,measure_vs_render_bench}.rs
```
So there are 13 non-test hits in 5 files (the reviewer said 12 in 4) and 20+8 imported names in home+wizard (the reviewer said 27). The substance is right: `screen.rs:273-278`'s claim ("the one place render still depends on app") is **false**.

**Severity argument, P2 not P1:**
- The property that makes three run modes and the firmware build possible is *platform* freedom. It is intact and compiler-enforced by `core/Cargo.toml`'s dependency list (reviewer 04 cross-built `core` and `ui-ffi` for thumbv8m). Render→app coupling does not affect either.
- The ADR on the framework (`2026-08-11-ui-framework-reuse-vs-rewrite.md`) rejects generality explicitly ("the product's screens are a closed, small set of shapes"). No roadmap item re-uses `render/` for another product. The ADR the reviewer cites for the boundary (`2026-08-11-portability-boundary-and-workspace-split.md`) does not exist (already noted in 06).
- What is actually violated is CLAUDE.md prose plus one false code comment. That is real drift that grows with every screen: P2 by the charter ("costs future work, not currently wrong"). It is not "will bite within MVP + next milestone".
- One cost is real today: `render` depends on `run` (the loop that calls it) for the fault tier windows. That is the part worth fixing first.

**Minimal boundary and honest effort:**
1. Move `render/home.rs`, `render/wizard.rs` **and `render/hero.rs`** to `app/screens/` (hero is a product composite: codec word, fault strip, OUT meter). Fix the `super::` paths to `crate::render::…` (`carve_edge` and friends are already `pub`), update the `render/mod.rs` re-exports (`HOME_TITLE`, `build_wizard_screen`, `WIZARD_TITLE`, `hero::*`) and the two `core/examples` imports. No trait is needed. Tests move with the files, so the count is unchanged. **S–M, Sonnet** (mechanical, ~1–2 h, compiler-guided).
2. Move `FAULT_LIVE_WINDOW`/`FAULT_RETIRE` from `run.rs:241-247` to `app/fault.rs`. `fault.rs:177,190` already use them, so they are model policy. `run.rs`/`ui-ffi` import them from there. **S, Haiku.**
3. After steps 1 and 2, the only remaining edge is `ScreenId` in `screen.rs`/`navigator.rs`, which render stores but never inspects. Either keep it as the one documented exception (which makes `screen.rs:273-278` true again at zero cost) or make it a type parameter (`Screen<Id>`/`Navigator<Id>`). The type parameter ripples through every builder signature: **M, Sonnet**, and not worth it now.
4. Add a guard test (`grep` in a `#[test]`, or `include_str!` scan) asserting no `crate::app`/`crate::run` in non-test `render/` code, except the documented `ScreenId`.

A crate split is **L** and is not recommended. Nothing needs it.

---

## F-app-01 — CONFIRMED; separate root cause from F-bt-01/F-app-03

**Evidence** (scratch crate; "boot" = `PairedDeviceUpserted(A)` + `StoreLoaded(Loaded)`, which queues `Connect{A}` exactly as `fold.rs:162-172` says):
```
a : Settings > "Idle screen" picker depth=3; ConnectSucceeded(A) -> depth=3; WizardAutoDismiss -> depth=1 "Pico Link"
a': same with degraded=true -> depth stays 3 (guard holds; stale Succeeded{degraded} left in wizard_phase)
b : wizard opened to pair B (cmds [StartScan]), B discovered; ConnectFailed(A,Timeout) -> phase Failed{A};
    X pressed -> cmds [Connect { addr: [10;6] (=A), name: "" }]           <- retries the OLD device; screen says only "No response / On and in range?" (wizard.rs:117), no name
b2: wizard scanning for B; ConnectSucceeded(A) -> wizard shows success; WizardAutoDismiss -> depth=1   (new sub-case: a scan is hijacked by A's success)
c : wizard success for B, manual Back -> depth=2 "Devices"; late WizardAutoDismiss -> depth=1
control: a Devices paired-row connect pushes the wizard in Connecting{A} (depth 3) and success+dismiss -> Home: intended, and must keep working after the fix
```
**Code read.** `fold.rs:141-146` sets `wizard_phase = Succeeded` unconditionally. `fold.rs:214-230` pops to root on phase alone. `fold.rs:446-464` sets `Failed{addr}` unconditionally; its "harmless" comment is only true while the wizard is closed. In firmware, `a2dp.c:3642-3656` pushes `link_state_connected` + `connect_succeeded` and arms the dismiss timer for *every* first STREAM_ESTABLISHED, boot auto-reconnect included. `bt.c:1008-1020` accepts `START_SCAN` with no check for a connect in flight, so scenario b is reachable.

**Dedup ruling: a separate defect with a shared seam.**
- F-bt-01/F-app-03's root cause is that the C side never implements `CancelConnect`. F-app-01's root cause is that core applies connect events to the wizard without scoping them to the wizard's own attempt. Scenarios a, b and b2 involve **no** Back press and **no** cancel: the boot auto-reconnect is a legitimate, live attempt. Implementing CancelConnect fixes none of them.
- Design C7 (`2026-08-30-cancel-connect.md:220`, which F-bt-01 lists as "a one-line change") guards only `on_connect_succeeded` on the phase `Connecting|NotResponding`. That fixes a and b2. It does **not** fix b (`record_connect_failure` is unguarded), c (late dismiss after manual B), or a stray success for A while the wizard is `Connecting{B}` (no address check).
- Fixing only one leaves user-visible bugs. With only F-app-01, B during Connecting still lets C finish, so audio flows to and `PersistDevice` records a headset the user abandoned. With only F-bt-01 plus C7, scenarios b and c remain, and the retry reconnects to the wrong device.
- **Ruling:** keep both findings. **F-app-01 owns all core-side scoping and absorbs C7**: remove "Core's C7 guard" from F-bt-01's fix sketch so two beads don't edit `on_connect_succeeded` differently. F-bt-01 keeps C1–C5 (C side). **Order: F-app-01 first** (host-testable, Sonnet, fixes something that happens on every power-up), then F-bt-01 (Opus, needs hardware). F-app-01's regression tests stay valid after F-bt-01 lands.

**Fix sketch amendments:**
- Gate on "the phase carries this `addr`" (`Connecting{addr}`/`NotResponding{addr}` for success and failure). That alone implies the wizard is mid-attempt, and it keeps the Devices-row connect working (that path enters `Connecting{A}`). "Wizard on the stack" is then only needed for the dismiss.
- The model updates (`connected_addr`, `last_connect_failure`, `link_state`, `PersistDevice`) must stay unconditional.
- Add test b2 and the Devices-row control case to the three named tests.

Severity **P1**, Tier **Sonnet**, Effort **S–M**.

---

## F-app-02 — PARTIALLY CONFIRMED; severity P1 → P2; fix sketch corrected

**Evidence:**
```
emulator/src/desktop/http_server.rs:98-104: POST /api/input, GET /api/screenshot, POST /api/shutdown — nothing else
grep -rn "handle_event\|Event::" emulator/src -> empty
emulator/src/main.rs:165,173: headless = core::run::run(platform, app, ...) — run() owns &mut App for the whole loop
core/src/platform.rs:91-92: InputSource::poll(&mut self) -> Vec<NavIntent>   (no Event channel)
```
**Reachability, counted against `freshness_cases()` (14 states)**, from a fresh `App` with NavIntent only (the emulator seeds no model state):
- Fully reachable (4): home status, home menu, devices (empty), settings. The two Settings pickers are also reachable but are not in the table.
- Partially reachable (1): wizard Scanning with zero rows. It then sits there forever, because no `DiscoveryStateChanged` ever arrives.
- Unreachable (9): nothing found, connecting, not responding, succeeded, failed, forget picker, forget confirm, device detail, home with a live OUT meter.

So 4 full + 1 partial of 14. The reviewer's "~3 / only Home/Devices/Settings" undercounts slightly. The conclusion stands.

**Is "derive serde on Event" feasible? Yes, with two amendments.**
1. Three payloads carry `String` (`DeviceEntry.name`, `PairedDevice.name`, `ConnectedCodec.word`). Core's serde is `default-features = false, features = ["derive"]` (`core/Cargo.toml`), and that comment already says "add `alloc` if that ever changes". Add `alloc`: it is no_std-safe and is not a platform crate, so the portability rule is not violated. Put the derives behind `#[cfg_attr(feature = "serde-events", derive(Serialize, Deserialize))]` (on `Event`, `Command`, `DeviceEntry`, `PairedDevice`, `ConnectedCodec`, `LinkState`, `ConnectFailureReason`, `ConnectStep`, `StoreStatus`, `VolumeSource`, `FaultKey`, `FaultValue`). The emulator enables the feature and the firmware binary is unchanged.
2. The route **cannot be added in the emulator alone** (the reviewer's "or fold it into `HttpInput`" does not work). `InputSource` returns only `NavIntent`, and `run()` holds the `App`. It needs a core seam: an event source on `Platform`, or a default `fn poll_events(&mut self) -> Vec<Event> { Vec::new() }` on `InputSource`, which `Runner::step` folds through `app.handle_event`. That is exactly F-ffi-03's "emulator has no event source" half.
3. Also expose commands (`GET /api/commands`, drained from `poll_command`). Without it an agent cannot see what a press asked C to do. See F-app-V1.

**Severity argument, P2.** No shipping behaviour is wrong. All 14 states are already driven in-process through the same `App` code (`invariants.rs`, the `*_screenshots` examples), and `Runner::step` + surfaces are covered by `surface_parity.rs`/`idle_wake_headless_e2e.rs`. What an HTTP event drive adds is agent convenience plus F-ffi-03's wake-path coverage. That is a test-infrastructure gap. It costs future work but is not a defect.

**Dedup ruling with F-ffi-03.** One seam, two consumers. F-ffi-03 owns moving the wake policy into core **and** the core event-source seam in `Runner::step` (it needs that seam so the wake rules run headless). F-app-02 becomes a dependent child: serde feature + `POST /api/event` + `GET /api/commands` + `GET /api/state` on top. File it `--deps` on F-ffi-03. F-app-06 (the `pl_ui_tick` fork) belongs to the same epic ("one implementation, two drivers").

---

## Spot-check F-app-06 — CONFIRMED

Divergence table, from reading `core/src/run.rs:724-830` against `ui-ffi/src/lib.rs:530-560` (`pl_ui_input`), `:601-614` (`pl_ui_tick`), `:2130-2156, 2222-2234` (`pl_ui_push_event`):

| Aspect | `Runner::step` (emulator) | FFI (firmware) |
|---|---|---|
| `had_input` | `!intents.is_empty()` after polling | `input_since_last_tick` is set when `count>0`, **even if every tag is malformed**, OR'd with `volume_wake_since_last_tick` |
| Volume wake | none | `VolumeChanged` → `idle.on_input()` (sink/device, or muted/zero) |
| Fault wake | none | `FaultRaised` → `idle.on_fault_wake(now)` |
| Wake from Off by a non-input source | n/a | `on_input()`/`on_fault_wake` return value ignored, no `mark_dirty` (only `pl_ui_input` marks dirty) |
| `on_external_power` | `platform.power()` | hard-coded `true` |
| Deep sleep | `enter_deep_sleep()` on decision | decision ignored (`deep_sleep_timeout` None) |
| Dim transition | `mark_dirty()` on On↔Dim | none (C only changes the backlight) |
| Settings save | inside `step` | inside `pl_ui_poll_command` |
| Frame budget | 33 ms (`emulator/src/main.rs:47`) | 16 ms (`main.c:514`) |

Seven real behavioural differences, three of them wake rules. P2 and Opus stand. The "wake from Off without `mark_dirty`" row is worth a line in the F-app-06/F-ffi-03 bead. On hardware ST7789 DISPOFF keeps GRAM, so it is probably benign. Unverifiable here.

## Spot-check F-render-04 — CONFIRMED (fix sketch corrected)

`fn text_width` ×5 (`screen.rs:56`, `hero.rs:740`, `list.rs:490`, `menu.rs:238` returning i32, `app/screens/device_page.rs:77`). `fn line_height` ×3 (`hero.rs:731`, `menu.rs:228`, `confirm.rs:61`). Truncation ×2 (`list.rs:527`, `hero.rs:759`), with the same drop-one-char-and-remeasure loop, O(k·n) measurements for k dropped chars. Counts match the report exactly.

**Correction.** The sketch's "walk chars, accumulate width via a single `get_rendered_dimensions` per char" would break the "measure and render cannot disagree" property that reviewer 04 praises: `text_width` returns the whole string's **bounding-box** width, and a sum of per-glyph bboxes is not that. Keep the whole-string measure and binary-search the prefix end over char boundaries (candidate width is monotone in prefix length), giving O(log n) measurements. The dedupe alone is Haiku. The algorithm change is Sonnet, because the existing `list.rs:1504-1575`/`hero.rs:1639-1660` tests must stay byte-exact.

---

## New findings

### F-render-V1
See above, under F-render-01.

### F-app-V1: The emulator never drains `App::poll_command`, so commands pile up and cannot be observed
- Severity: P3   Confidence: High   Effort: S   Tier: Sonnet
- Location: `core/src/run.rs:724-830` (`Runner::step`, no `poll_command`); `emulator/src/**` (no caller); `core/src/app/mod.rs:336`
- Evidence: `grep -rn "poll_command()" core/src/run.rs emulator/src` → empty. Every `StartScan`/`Connect`/`ForgetDevice`/`CancelScan` a windowed or headless user triggers is pushed onto `App::commands` (`VecDeque`) and never popped.
- Why it matters: host-only unbounded growth (tiny: one entry per action). More importantly, neither the windowed human nor an HTTP-driving agent can see what the UI asked the radio to do, and that is half of verifying any Bluetooth screen. It is a precondition for F-app-02's `GET /api/commands`.
- Fix sketch: `Runner::step` drains `poll_command` into an optional platform sink (default: `log::info!`). The emulator keeps a bounded ring served at `GET /api/commands`.
- Verification: an emulator test that POSTs `Select`×3 (Home → Devices → Pair) and then GETs `/api/commands` → `["StartScan"]`; the queue is empty afterwards.

## Scratch artefacts
Everything is under `/tmp/claude-0/-home-user-pico-link/7ce6c821-ae69-5064-9e8d-fc34eb8422ff/scratchpad/{verify,tree,tree2}`. None are in the repo.
