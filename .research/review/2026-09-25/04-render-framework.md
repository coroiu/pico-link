# 04 — Render framework (`core/src/render/`, `core/src/input.rs`)

Reviewed tree: `2e37164`. Seam prefix `render`. Scope per charter: the platform-free GUI
framework, its input vocabulary, its golden-image tests, `run.rs`/`app/screens/*` only as
callers.

Baseline measured in this container (2026-09-25):
- `cargo test -p pico-link-core`: **478 unit + 1 + 1 + 5 integration = 485 passed, 0 failed**
  (doc-tests 0). Unit tests inside `render/`: 304 (`grep -c '#[test]'`, per file below).
- `cargo clippy -p pico-link-core --all-targets`: **3 warnings, none in `render/` or
  `input.rs`** (`power.rs:118` derivable impl; `run.rs:1245,1299` type_complexity).
- `rustup target add thumbv8m.main-none-eabihf` + `cargo build -p pico-link-core --target
  thumbv8m.main-none-eabihf`: **builds** (8.2 s). `cargo build -p ui-ffi --target
  thumbv8m.main-none-eabihf`: **builds**. `cargo tree --target thumbv8m...` for core: only
  `embedded-graphics`, `embedded-graphics-framebuf`, `u8g2-fonts`, `serde` (no-std), `log`
  (no-std) — no platform or std-only crate. `grep std::|cfg(target_os` over `render/` +
  `input.rs`: zero hits outside `#[cfg(test)]`.
- Two defects below were **reproduced on host** with a scratch crate against the crate's
  public API (`/tmp/.../scratchpad/probe`, not committed); the numbers quoted are measured.

## 1. Verdict

The framework is in good shape: one `Widget` trait, one navigator, one chrome model, one
theme table, a correct damage pass with a real equivalence property test, and it cross-
compiles for the target with zero warnings in scope. It is genuinely the project's asset.
Two things are wrong today: a **reproducible stale-pixel bug on Home** (a meter-only repaint
erases the tail of any device name wider than ~146 px, and the "OUT" legend visually
collides with it), and the **framework/product boundary has already dissolved** — `render/`
now contains three product screens that import 27 symbols from `crate::app` and two
constants from `crate::run`, so the "render layer had zero coupling to the product layer"
property CLAUDE.md wants preserved is no longer true. The rest is P2/P3: a confirm headline
that clips both ends, three unpinned fixture directories, five copies of `text_width`, and a
handful of predecessor-project leftovers.

## 2. What is well done (leave alone)

- **`framebuffer.rs`** — `Vec<u16>` backing (not `Vec<Rgb565>`, avoids a `repr(Rust)`
  layout assumption for DMA), row-wise `fill_solid`/`fill_contiguous` fast paths that clip
  correctly on every edge and keep the `colors` iterator aligned across off-canvas rows,
  `clear` routed through `fill_solid`. 16 tests cover every clipping case. Byte order for the
  ST7789 is explicit (`write_be_bytes` big-endian; `as_raw_u16` native + hardware bswap).
  `draw_iter` bounds-checks in the upstream crate (`framebuf/src/lib.rs:228`).
- **`chrome.rs` / `rail.rs`** — closed-form regions, `carve_edge` over all four edges,
  orientation carried on `ChromeLayout`, rail labels looked up by logical button through
  `PanelOrientation::slot_order` so a panel flip moves label *with* button; proven by the
  mirror test `mirroring_the_orientation_moves_both_the_edge_and_the_slot_order` with a
  negative half. Every rail slot label the app uses measures 14–28 px in a 34 px slot
  (measured: `select` 25, `forget` 28, `why?` 24).
- **`paint_key.rs` / the damage pass in `screen.rs:583-853`** — `ALWAYS` (`None`, never
  equal) as the migration default is the right call; the diff is exactly the design's §3.3
  eight steps; step 7 repaints *intersecting* widgets, not *changed* ones; `union_rect`
  treats a zero-sized operand as identity (load-bearing for never-rendered slots); chrome is
  keyed (§3.5, "not optional") and `title_paint_key` folds the mute *colour* separately from
  the text. `damage_rendered_frame_matches_a_full_frame_render_of_the_same_state_for_every_screen`
  (`app/invariants.rs:289`) is a real A4 property test — it also asserts pixels *outside*
  the rect are unchanged.
- **Design rule 4 made structural** — `Widget::activation` is the single source for both
  A's liveness and its label; `Screen::activate_focused` and `Screen::resolve_a` read the
  same accessor; `Verb` words are `const`-asserted ≤ 6 chars; `Verb::Exception` is greppable.
  `a_rail_liveness_matches_activation_for_every_screen` pins it centrally.
- **Three row containers, one primitive** — `list::draw_row` (two-line, chip) and
  `menu::draw_row` (single-line, `RowStyle`) with `FieldList` reusing `menu::draw_row` and
  `list::reconcile_top_index` verbatim, as the field-list ruling specified. `FieldKind::
  Readonly` makes "A on an inert row" un-typeable. `const _: () = assert!(FIELD.caret_right_
  margin < FIELD.value_right_margin)` enforces "the caret never moves the value" at compile
  time.
- **Identity-keyed selection carry** (`ListItemKey`, `with_selected_identity`) and the
  "only scroll at viewport edges" `reconcile_top_index`, both with regression tests that name
  the original bug.
- **`RenderCtx`** — implemented exactly as ADR 2026-08-31 specifies, including `ctx` on
  `measure`, `redraw_after` with the `MIN_REDRAW_DELAY` floor, and no `tick`-always-dirty.
- **Theme is a data table** — `grep Rgb565::new|WebColors|fonts::u8g2` over non-test
  `render/` code hits only `theme.rs`. Every colour and font goes through `palette::` /
  `font::`. Widths are always *measured* (`get_rendered_dimensions_aligned`) with the same
  renderer that draws, so measure and render cannot disagree; ellipsis is ASCII `...` because
  the `_tf` subsets may lack U+2026 and `ignore_unknown_chars` would drop it silently.
- **Input** — `NavIntent` is complete and minimal for the hardware (9 variants); no screen
  reads raw buttons; press-edge-only is enforced at both producers (`firmware/src/input.c`
  edge detection; emulator `edge_triggered_intents`), so it is a *platform* property the
  framework correctly does not assume either way.

## 3. Architecture assessment

| Doc | Code matches? | Divergence |
|---|---|---|
| ADR 2026-08-11 (fixed chrome + linear stacks) | Yes | Vertical-only stacking is now the reason `damage_hint` exists (the OUT strip cannot be a sibling); the damage design §8 already names this a "real gap". See F-render-06. |
| ADR 2026-08-31 (`RenderCtx`) | Yes, fully | Its "re-export `Instant` from `crate::render`" follow-up is done (`mod.rs:98`). |
| ADR 2026-09-02 (repaint ceiling → damage-rect first) | Yes | `App::render` returns the rect; host surfaces ignore it (advisory, per design §9). |
| Design 2026-09-06 (damage pass) | Yes, §3–4 | One-rect union (§5) as shipped. |
| Design 2026-09-07 composite damage (B1/B2/B3) | B1 fixed (`home.rs:695-730`); B2 fixed for Home only (`ScreenId::Home => Refresh::Keep`, `app/mod.rs:315`); **B3 (`PaintPlan`) not built** — the `paint_key`/`damage_hint`/`damage_region_key` trio with its prose-only invariant is still the shape. | `replace_at` still forces full damage on every other live-rebuilt screen (device page under Adaptive, Devices while scanning). |
| Design 2026-09-01 (Home grid, L=12/R=194) | Yes for Home | The left rule exists under five names: `TITLE_SIDE_MARGIN` (screen.rs:39), `LEFT_MARGIN` (hero.rs:59), `RowStyle::FIELD.left_margin` (menu.rs:101), `FAULT_GLYPH_X` (hero.rs:198), `BANNER_TEXT_INSET` (hero.rs:104). Not a grid type; a convention. `MenuList` uses 8 px (`RowStyle::MENU`, deliberately legacy per the field-list ruling §7.2). |
| Design 2026-09-03 (vertical OUT meter) | Yes, geometry verbatim | §3 says the name keeps its full 206 px budget *and* the legend renders in the name row inside the strip — those two facts collide (F-render-01). |
| Design 2026-09-07 fault strip §4 | Yes | Row anatomy and y-table match `FAULT_ROW_TOP`. |
| Design 2026-09-07 device page §3.3 / §5 | Yes | `Spacer` as designed; damage rule 5 ("known cost") still true. |
| Design 2026-08-28 §4 global input contract | **No** for `Left`/`Right` | Rule "Right: identical to A. Left: identical to B." is not implemented anywhere: `Navigator::dispatch` forwards them to the focused widget only (`navigator.rs:359-362`) and every widget's `on_intent` treats them as no-ops. See F-render-08. |
| CLAUDE.md "render had zero coupling to the product layer — keep it that way" | **No** | See F-render-02. |

Component model census (question 1): there is **one** `Widget` trait (`widget.rs:297`) and
no second focus model. Implementors: `VerticalList`, `MenuList`, `FieldList`, `MessageView`,
`ConfirmView` (wraps `MenuList`), `Spacer`, `HeroStatusView`, `HomeView` (switch over
hero/menu), `PairingWizardView` (switch over list/message/steps), `DevicesListView` (wraps
`VerticalList`), `DevicePageView` (wraps `FieldList`), plus picker/why-page/ldac views in
`app/screens`. `hero.rs` and `wizard.rs` do **not** reinvent layout/focus: they are
composite leaf widgets that own their internal vertical rhythm, which is the sanctioned
`ConfirmView` pattern. The one real duplication is wrapper forwarding: every wrapper must
hand-forward `activation`/`redraw_after`/`scroll_top`/`selected_*`/`sync`, and the trait
documents this as a hazard four times rather than providing a forwarding default or a
`Wrapper<W>` helper.

## 4. Findings

### F-render-01: A meter-only repaint erases the tail of any device name wider than ~146 px, and the OUT legend overlaps it
- Severity: P1   Confidence: High   Effort: M   Tier: Opus (design call on name budget vs legend placement; the code change itself is Sonnet)
- Location: `core/src/render/hero.rs:641-645` (`meter_footprint` = strip x-span × **full area height**), `hero.rs:808-811,853-874` (name drawn across `name_band`'s full 206 px, gated on `ctx.needs(hero_body)` whose x-range is 0..158), `hero.rs:1172-1184` (legend drawn at `name_y` inside the strip), `screen.rs:684-698` (narrowing to `damage_hint`)
- Evidence: reproduced through the real `App` (PairedDeviceUpserted + Connected + CodecChanged + two `LevelsChanged` 50 ms apart):
  ```
  Sony WH-1000XM5                 helvB12 width=144px  frame2 damage=(158,16 48x224)
    frame1 name ink x 13..156; OUT legend ink x 174..; frame2 differing pixels vs full render: 0
  Sennheiser Momentum 4 Wireless  helvB12 width=266px  frame2 damage=(158,16 48x224)
    frame1 name ink x 13..190; frame2 name ink max_x 157; differing pixels vs full render: 102
  Bang & Olufsen Beoplay HX       helvB12 width=218px  ... frame1 name ink x 13..192 -> frame2 157; 118 differing
  ```
  The name budget is `206 - 12 - 12 = 182` px (`hero.rs:860`), so a truncated name legitimately reaches x=194; the meter strip starts at x=158. Frame 2's damage rect is the strip, `Screen` fills it `BACKGROUND`, `ctx.needs(hero_body)` is false (x 0..158 ∩ 158..206 is empty), the name is not redrawn, and columns 158..194 of it stay erased until the next body change or the 1 s forced backstop.
- Why it matters: on hardware this is a 1 Hz flicker of the name's last 4–6 characters whenever audio is playing on a device with a long name (most non-Sony names measure > 146 px in helvB12). Independently of damage, the "OUT" legend (x 174..193 in the name row) is painted **over** the name's ink for any name wider than ~160 px — a design collision the vertical-meter design §3 did not account for when it kept the name at full width.
- Fix sketch: pick one of two, both small: (a) shrink `meter_footprint` to start at `NAME_BAND_HEIGHT` and move the "OUT" legend below the name band (top of the meter block, inside the strip) — the name and the meter then never share a pixel and no budget changes; or (b) keep the legend and clamp the name budget to `hero_body.width - 24 = 134` px only while `out_level` is live — this truncates "Sony WH-1000XM5" to "Sony WH-1000X...", which design §3 explicitly rejected. Recommend (a). Either way the fix must keep `damage_hint` and `ctx.needs` reading the *same* helper (`meter_footprint`), which is what makes the current code internally consistent.
- Verification: add a case to `freshness_cases()` (`app/invariants.rs`) with a 30-char paired-device name (every case today uses "Sony WH-1000XM5" or "Cans", which is why A4 passes); the existing `damage_rendered_frame_matches_...for_every_screen` then fails on `2e37164` and passes after. Plus a `hero.rs` unit test: full render, then a narrowed render via `damage_hint`, assert no `TEXT_PRIMARY` column of the name row was lost.
- Related: design `2026-09-03-vertical-out-meter.md` §3 ("the name keeps the full 206px"), `2026-09-07-composite-damage-and-paint-plan.md` B3, beads `pico-link-7h5.9`, `pico-link-ky8`.

### F-render-02: `render/` is no longer decoupled from the product — three product screens, 27 `crate::app` imports and a `crate::run` dependency live inside the framework
- Severity: P1   Confidence: High   Effort: L   Tier: Opus
- Location: `core/src/render/home.rs:94-98` (20 `crate::app` symbols incl. four `build_*_screen` functions, `BtModel`, `ModelHandle`, `Command`), `render/wizard.rs:42-44` (7 symbols incl. `Command`, `WizardPhase`, `is_audio_sink`), `render/hero.rs:38,614-617,660,717,1212-1213,1314-1315` (`FaultLog`/`FaultKey`/`FaultSeverity`/`FaultGlyphClass`, `crate::app::decay_peak`, `crate::run::FAULT_LIVE_WINDOW`/`FAULT_RETIRE`), `render/screen.rs:22,273-279` and `render/navigator.rs:27` (`crate::app::ScreenId`)
- Evidence: `screen.rs:273-278` states "`Screen` importing `crate::app::ScreenId` is the one place `render` still depends on `app`" — `grep -n "crate::app\|crate::run" core/src/render/*.rs` (non-doc lines) returns 12 hits across four files. `app/screens/` already exists and holds the other six product screens (`devices.rs`, `device_page.rs`, `picker.rs`, `why_page.rs`, `settings.rs`, `ldac_quality.rs`); `home.rs`, `wizard.rs` and `hero.rs` are the same kind of thing filed under `render/`. `hero.rs` deliberately built hero-local vocabularies (`OutLevelDisplay`, `HeroVolume`, `CodecStatus`) to stay decoupled from `BtModel` — and then `with_fault_log(FaultLog)` (`hero.rs:451`) took the model type straight through, dragging the run loop's tier constants with it.
- Why it matters: CLAUDE.md's stated reason the core survived the pivot is that "the render layer had zero coupling to the product layer above it. Keep it that way." That property is gone, silently. Concretely: `render` cannot be compiled or tested without `app`; `render` depends on `run` (the loop that *calls* it) for fault-tier timing; the next screen will be filed wherever the last one was; and a future crate split (the obvious way to make the boundary compiler-enforced, as `core`/`emulator` already is for platform-freedom) is now a large move instead of a `mv`.
- Fix sketch: (1) `git mv core/src/render/{home,wizard}.rs core/src/app/screens/` — they are product screens; nothing in `render` needs them (only `mod.rs` re-exports `HOME_TITLE`/`build_wizard_screen`/`WIZARD_TITLE`, all consumed by `app`). (2) Give `hero.rs` a hero-local `FaultRowDisplay { shape, live, name, count }` projected by `HomeView::project_hero` — exactly the `OutLevelDisplay` pattern — and move `FAULT_LIVE_WINDOW`/`FAULT_RETIRE` out of `run.rs` into the app's fault module (they are model policy, not loop policy). (3) Make `ScreenId` a render-owned opaque `Copy` id (or generic `Screen<Id>`), so `app` defines the enum and `render` only compares it. (4) Fix the stale claim at `screen.rs:273-278`. Consider a `render/` crate split afterwards; not required for the fix.
- Verification: `grep -rn "crate::app\|crate::run" core/src/render/ | grep -v "^\S*:\s*[0-9]*:\s*//"` returns nothing; `cargo test -p pico-link-core` count unchanged; add a one-line test or CI grep to keep it that way.
- Related: CLAUDE.md "Repo layout"; ADR `2026-08-11-portability-boundary-and-workspace-split.md`; design `2026-09-24-live-widgets-retire-refresh-stack.md`.

### F-render-03: `ConfirmView` / `MessageView` have no width clamp — "Forget <device>?" clips at both ends for ordinary device names
- Severity: P2   Confidence: High   Effort: S   Tier: Sonnet
- Location: `core/src/render/confirm.rs:146-153` (headline centred, no truncation), `core/src/render/message.rs:194-212` (same), `core/src/app/screens/devices.rs` `build_forget_confirm_screen` (`format!("Forget {label}?")`), `core/src/render/wizard.rs:107-114` (copy hand-fitted to the width, with the constraint acknowledged in a comment)
- Evidence: reproduced through `Navigator`:
  ```
  Forget Cans?                          width=104px (content 206px)  ink at x=0: false  ink at x=205: false
  Forget Sennheiser Momentum 4 Wi?      width=284px                  ink at x=0: true   ink at x=205: true
  ```
  `MAX_DEVICE_NAME_BYTES = 32` (`app/model.rs:20`) permits names far wider than 206 px in `font::name()`. Centred text that overflows is clipped at *both* ends by `target.clipped(&area)`, so "Forget" and "?" are both cut and the row reads as gibberish.
- Why it matters: this is the destructive-action confirm; the design requires the headline to name what is being forgotten. Reachable today from Devices → X on any paired device with a long name.
- Fix sketch: reuse `list::truncate_label_to_width` (already `pub(crate)`) on `ConfirmView`'s headline and `MessageView`'s headline/subline with budget `area.width - 2*12`; or put the device name on the subline and keep the headline constant ("Forget this device?"). Either is a few lines.
- Verification: a `confirm.rs` test rendering `"Forget Sennheiser Momentum 4 Wi?"` into 206×206 and asserting no `TEXT_PRIMARY` pixel in columns 0 and 205 of rows 26..44 and that the rendered text ends with `...`.
- Related: `pico-link-ok1` (the same fix already done for list rows).

### F-render-04: Text measurement/truncation helpers are copied five times, with two identical O(n²) ellipsis loops
- Severity: P2   Confidence: High   Effort: S   Tier: Haiku
- Location: `text_width` at `screen.rs:56`, `hero.rs:740`, `list.rs:490`, `menu.rs:238` (returns `i32`), `app/screens/device_page.rs:77`; `line_height("Agjpqy")` at `menu.rs:228`, `confirm.rs:61`, `hero.rs:731`; truncation at `list.rs:527` (`truncate_label_to_width`) and `hero.rs:759` (`truncate_to_width`) — same algorithm, different signature (`i32` vs `u32` budget)
- Evidence: each copy carries a doc comment justifying "no shared home for a helper this small, used by only one module each" (`list.rs:485-489`, `menu.rs:234-237`, `confirm.rs:59-60`, `hero.rs:727-730`). At five copies that rationale is false. Both truncation loops re-measure the whole candidate string per dropped character (`text_width(font, &candidate)` inside `while end > 0`), i.e. O(n²) glyph-advance lookups per overflowing label per repaint.
- Why it matters: maintainability (F-render-03's fix wants a sixth caller); and on the M33 an overflowing 30-char scan-list row costs ~900 glyph lookups per repaint per row for no reason — small today, but this is the hot path the repaint-ceiling ADR is about.
- Fix sketch: `render/theme/text.rs` (or `render/text.rs`) with `width(font, &str) -> u32`, `line_height(font) -> i32`, `truncate(font, &str, max: u32) -> String` that measures cumulative advance once (walk chars, accumulate `width(prefix)` via a single `get_rendered_dimensions` per char or binary-search on prefix length). Delete the five copies.
- Verification: existing `truncate_label_to_width` tests (`list.rs:1504-1575`) and `hero.rs:1639-1660` move to the new module unchanged and pass; `grep -rn "fn text_width\|fn line_height" core/src` returns one hit each.

### F-render-05: 38 of 48 committed golden PNGs are pinned by no test, and three of the ten "pinned" ones are orphans
- Severity: P2   Confidence: High   Effort: S   Tier: Sonnet
- Location: `core/tests/home_screenshot_fixtures.rs` (the only fixture test; covers `FIXTURE_NAMES` = 8 files), `core/examples/{wizard,devices,device_page,ldac_quality}_screenshots.rs` (regenerate `wizard-screenshots/` 19 files, `fixtures/devices/` 8, `fixtures/fault-strip/` 11 — no test reads any of them)
- Evidence: `home-screenshots/` contains `01_status_face.png` and `02_menu_face.png`, which are not in `FIXTURE_NAMES` and are regenerated by nothing (stale from a pre-`pico-link-1v5` example). `wizard-screenshots/01_instructions.png` depicts the instructions phase that `pico-link-4vb.2` deleted (`wizard.rs:65-66`). `home_screenshot_fixtures.rs`'s own header explains this exact failure mode ("a stale-but-unchecked fixture trains the next reviewer to read 'the fixtures differ' as normal noise") — and then only fixed it for Home.
- Why it matters: the project's stated verification discipline is "inspect at zoom"; fixtures that nothing checks are documentation that can lie. The wizard fixtures cover states (five failure reasons, degraded success, liveness t0/t1) that no pixel test pins.
- Fix sketch: generalise `home_screenshot_fixtures.rs` into one table-driven test over `(support module, committed dir)` for all four sets, using the same `#[path]`-shared `generate()` pattern; delete the three orphan PNGs; make every `*_screenshots.rs` example write through a `tests/support/*_fixtures.rs` module the test can also call. Determinism is already guaranteed (fonts are embedded, time is `RenderCtx::at` pinned), so the brittleness cost is "one intentional render change → regenerate N files", which is acceptable given the message the existing test prints. A cheaper structural assertion (per-row text/colour snapshot) is possible but not needed while the PNG tests are byte-stable.
- Verification: `cargo test -p pico-link-core --test screenshot_fixtures` fails when any committed PNG differs from a fresh render; `git ls-files '*.png' | wc -l` equals the sum of the tables.

### F-render-06: The composite-widget seam is three hand-synchronised methods plus a per-composite forwarding checklist, and `Screen` special-cases its own chrome slots
- Severity: P2   Confidence: High   Effort: L   Tier: Opus
- Location: `widget.rs:343-406` (`paint_key`/`damage_hint`/`damage_region_key`), `widget.rs:453-455,510-522,573-575,594-597` (four "a wrapper must forward this" warnings), `screen.rs:684-698` (`index >= 2` chrome-slot offset), `navigator.rs:293-334` (Up/Down drive both the widget and top-level focus, "no way for a widget to say I consumed that"), `hero.rs:487-588` (`body_paint_key` shared by hand between `paint_key` and `damage_region_key`)
- Evidence: the composite-damage design (`2026-09-07-composite-damage-and-paint-plan.md` §3) already diagnosed this: "the invariant linking the two methods is prose only", "'not implemented' and 'always dirty' are the same value", and proposed `PaintPlan`. B1/B2 were fixed; B3 was not built. Wrapper forwarding is enforced only by review — `ConfirmView` (`confirm.rs:112-172`) forwards `activation`/`on_focus`/`on_intent` but not `paint_key`/`scroll_top`/`sync`/`redraw_after` (correct *today* because it is never rebuilt and `MenuList` is time-free, but the trait cannot tell). `Navigator::dispatch`'s Up/Down double-drive means the framework supports exactly one focusable widget per screen; every shipped screen has exactly one, and the device page's second widget is a non-focusable `Spacer`.
- Why it matters: F-render-01 is precisely a body/hint invariant that the type system cannot see; the next composite (the Tier-2 VOL gauge column beside the hero, `on-device-ui.md` §3) will need either a horizontal container or another hand-rolled `damage_hint`, and a second focusable widget on any screen will misbehave. This is debt that lands inside the roadmap's next milestone.
- Fix sketch: build the design's `PaintPlan` (one method returning keyed regions; `Screen`'s slot loop becomes a plan; `hero` returns `[name_band, hero_body, legend, meter]` regions) and delete the trio; add a `Widget::children()`-style default or a `Forwarding<W>` newtype so wrappers forward everything by default; return a `Consumed` flag from `on_intent` so `Navigator` only cycles focus when the widget declined. Do the first before the VOL gauge, not after.
- Verification: `hero.rs` compiles with no `body_paint_key`; the A4 property test plus F-render-01's long-name case pass; a new `navigator.rs` test with two focusable stub widgets where Down moves inside the first without also moving top-level focus.
- Related: design §3/§5 of the composite-damage doc; `pico-link-4ube`, `pico-link-vxc` D2/D3.

### F-render-07: `replace_at`/`replace_root` still force a full repaint on every live rebuild of any non-Home screen
- Severity: P2   Confidence: High   Effort: M   Tier: Sonnet
- Location: `navigator.rs:240-267` (`force_full_damage = true` unconditionally), `app/mod.rs:282-294` (`refresh_stack` → `replace_at` for every `Refresh::Rebuild`; only `ScreenId::Home` returns `Keep`, `app/mod.rs:315`)
- Evidence: the composite-damage design's B2 fix ("carry `paint_cache` iff `old.id() == new.id()`") was superseded for Home by the live-widgets M1 work, but every other identified screen (Devices during a scan, DevicePage under Adaptive bitrate stepping, Picker, WhyPage) is still torn down and repainted in full on each model event. The device-page design §5 rule 5 records this as a "known cost, deliberately not fixed here" and asks for "a render-pipeline bead of its own".
- Why it matters: on the device page the live Adaptive kbps row updates exactly while the encoder is starving core 0 (the design's own words); each update is a full 240×240 render (~25 ms measured) instead of one `FieldList` row band.
- Fix sketch: either finish the live-widgets migration (M2+: `sync` on `DevicePageView`/`DevicesListView`, `Refresh::Keep`) or implement the cheaper B2 carry in `Navigator::replace_at` (move `paint_cache` when `ScreenId`s match; keep every downstream guard). The carry is ~15 lines and independently testable.
- Verification: `navigator.rs` test: replace a `with_id` screen with a same-id screen whose one widget's key is unchanged → `render` returns `Rectangle::zero()`; different id → full frame.
- Related: `2026-09-07-composite-damage-and-paint-plan.md` stage 2; `2026-09-24-live-widgets-retire-refresh-stack.md`; `pico-link-bgnd`.

### F-render-08: Design §4's "Right ≡ A, Left ≡ B" is not implemented; `Left`/`Right` are no-ops on every screen
- Severity: P2   Confidence: High   Effort: S   Tier: Sonnet
- Location: `navigator.rs:359-362` (`Left | Right | ShortcutX | ShortcutY` forwarded to the focused widget only), `list.rs:949`, `menu.rs:514`, `fields.rs:368`, `home.rs:602`, `wizard.rs:441` (every `on_intent` treats `Left`/`Right` as `{}`), `input.rs:29-36` (doc: "No widget in this crate consumes this yet")
- Evidence: `.planning/design/2026-08-28-on-device-ui.md` §4 table: "Right — Identical to A. Left — Identical to B." Firmware maps GP16/GP20 to `PL_INTENT_TAG_LEFT/RIGHT` (`input.c:33-34`), the emulator maps arrow keys, the FFI round-trips them, and then nothing happens.
- Why it matters: two of the five joystick directions are dead on hardware, contradicting the design of record. Either the code or the doc is wrong; the doc is the design of record and the rule is cheap.
- Fix sketch: in `Navigator::dispatch`, `Right => self.dispatch(Select)`, `Left => self.dispatch(Back)` — unless Andreas strikes the rule (see §6). Keep `Left`/`Right` in `NavIntent` either way (the wizard's future horizontal paging is the reason they exist).
- Verification: `navigator.rs` tests: `dispatch(Right)` on a list with an `on_activate` pushing a screen → depth 2; `dispatch(Left)` → depth 1.

### F-render-09: Batched P3 nits (one bead)
- Severity: P3   Confidence: High   Effort: S   Tier: Haiku (except the last two, Sonnet)
- Location / evidence / fix, one line each:
  - `paint_key.rs:176-187` — `impl Eq for PaintKey` while `PartialEq` is deliberately non-reflexive (`ALWAYS != ALWAYS`). `Eq` promises reflexivity; nothing needs `Eq`. Delete the impl.
  - `list.rs:897-902` vs `:966-968` — `paint_key` folds `top_index` *before* `render` reconciles it, so every scroll costs one extra full-list repaint on the following frame (cache holds the pre-reconcile key). Reconcile in `paint_key` too (it is idempotent) or fold `reconcile_top_index(...)` instead of the raw cell.
  - `hero.rs:52` `HERO_PAINT_KEY_SEED = 14` == `spacer.rs:32` `SPACER_PAINT_KEY_SEED = 14`; `home.rs:119` `HOME_PAINT_KEY_SEED = 15` == `device_page.rs` `DEVICE_PAGE_PAINT_KEY_SEED = 15`. Harmless (keys are compared per slot) but every comment says "only needs to differ from other widgets' seeds", which is untrue as written. Fix the comments or the numbers.
  - `framebuffer.rs:109` `pixel(p)` is unchecked: `x >= width` silently returns the next row's pixel (`framebuf` `point_to_index = width*y + x`), negative `x` panics. Test-only caller today; add a `debug_assert!` or clamp.
  - `list.rs:583` `to_ascii_uppercase` — a non-ASCII initial ("é", "ñ") stays lowercase in the chip; `theme.rs:100-107` `ignore_unknown_chars` means a name outside Latin-1 (`_tf` glyph set) renders as missing glyphs with no fallback. Width is measured with the same renderer so truncation stays correct. Product call — see §6.
  - Predecessor-project leftovers with no non-test caller (question 11): `font::secret()` (profont, the password field — `theme.rs:139-146`); `Action::Emit` / `Navigator::take_output` / `pending_output` / `platform::OutputRequest` (the BLE-HID keyboard seam, ADR 2026-08-18 — no producer in this product; `widget.rs:75-81`, `navigator.rs:33-38,277,289-291`); `ChromeContribution::status` + `ChromeStatus` (the Bitwarden sync dot — no producer outside `screen.rs` tests); `ChromeContribution::readout` (no producer outside tests); `theme::draw_level_meter` + `METER_GLYPH_WIDTH` (superseded horizontal meter, "kept for now" at `theme.rs:745-748`); `icon::{USB, HEADPHONES, PLUS}` (probed, never drawn); `ListItem::with_icon`/`RowChip::Icon`/`draw_icon_chip` (no app caller); `MessageView::with_icon` (no caller); `Navigator::{title_at, root_selected_index, root_selected_key}` (superseded by `*_at`); `MenuItem::{with_trailing_label, with_no_trailing}` (tests only); `menu.rs:626` test strings "Reveal"/"Type password". `input.rs:15-19` module doc still says "placeholder module seam only". Delete or `#[allow(dead_code)]` with a reason; the `Emit`/`OutputRequest` removal touches `platform.rs` (Sonnet).
  - Per-frame allocation (question 5), for the ADR's accounting rather than as a defect: `Screen::render` allocates two `Vec`s and 2–3 `String`s per *rendered* frame (`screen.rs:504,540,605,620,647`); `HomeView::sync` rebuilds `HeroStatusView` every rendered frame and every input, cloning the device name and codec word (`home.rs:273,317,454`); `hero.rs` `render` allocates `format!`/`to_uppercase`/`String::from` per body repaint. All bounded, none in the idle path (the dirty gate skips `render` entirely). Acceptable; `Screen::render`'s two `Vec`s could be `paint_cache`-sized scratch fields if the M33 numbers ever say so.
  - Doc rot: `chrome.rs:11-12` still cites a "128x32 HUZZAH32 OLED"; `message.rs:225-229`, `confirm.rs:229-232` reference `HINT_BAR_HEIGHT` (deleted); `list.rs:256-260` computes the row budget against the hint bar.
- Verification: `cargo clippy -p pico-link-core --all-targets` stays at ≤ 3 warnings; test count unchanged after deletions (delete the tests with the dead code).

## 5. Test coverage

Covered well: framebuffer clipping (16 tests); chrome/rail geometry and mirroring; list
scrolling rule and identity carry (40); menu/field row styles and paint keys (37); hero
banner priority, fixed grid, fault strip tiers, paint-key time trap (52); wizard phases (31);
navigator stack ops (15); the three cross-screen invariants in `app/invariants.rs` (dirty
gate freshness, damage equivalence, A-liveness). All of it is deterministic: fonts are
embedded, time is pinned via `RenderCtx::at`, no wall clock anywhere in `render/`.

Gaps a cheap model can close:
1. Long-name cases in `freshness_cases()` — would have caught F-render-01 (Sonnet).
2. `ConfirmView`/`MessageView` overflow tests — F-render-03 (Sonnet).
3. Fixture pinning for the three unpinned directories — F-render-05 (Sonnet).
4. `Navigator::dispatch(Left/Right)` — F-render-08 (Haiku once the behaviour is decided).
5. A two-focusable-widget navigator test — F-render-06 (Sonnet).
6. `FrameBuffer565::pixel` bounds — F-render-09 (Haiku).
7. A "render is free of `crate::app`" grep test or crate split — F-render-02.

Note for the synthesis: `render_png_dump.rs`'s `text_never_bleeds_past_a_rows_bottom_padding`
is the test that encodes the sub-row overflow lesson from CLAUDE.md; it is real and passes.

## 6. Open questions for Andreas

1. **F-render-01**: when the OUT meter is live, should the device name keep its full 182 px
   budget (move the "OUT" legend down into the strip) or shrink to 134 px (legend stays,
   "Sony WH-1000XM5" becomes "Sony WH-1000X...")? Recommendation: move the legend.
2. **F-render-08**: implement "Right ≡ A, Left ≡ B" as the design says, or strike it?
   Recommendation: implement — it is two lines and makes the joystick honest.
3. Non-Latin-1 device names (Japanese/Chinese/Korean headphones report UTF-8 names): today
   the glyphs vanish silently. Accept, or render a `?` per missing glyph? Recommendation:
   accept for MVP, file for Tier 2.
4. Delete the BLE-HID `Action::Emit`/`OutputRequest` seam and `font::secret()` now
   (F-render-09)? They are the last visible Bitwarden bones. Recommendation: delete.

## 7. Unverifiable here

- The on-hardware appearance of F-render-01 (a 1 Hz flicker of the name tail under the
  forced-repaint backstop) — reproduced only as pixels on the host.
- Whether the row-band blit quantisation in `main.c` masks or exposes F-render-01 on the
  panel (it blits full-width rows for the strip's y-range, which *includes* the name row —
  so the erased tail does reach the panel).
- Rail label legibility at 34 px / helvR08 on the physical panel (measured widths are fine;
  legibility is a look).
- The M33 cost of the O(n²) truncation loop and the per-frame allocations (F-render-04/-09)
  — host-only reasoning.
