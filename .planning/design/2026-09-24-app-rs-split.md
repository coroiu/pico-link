# Split core/src/app.rs into core/src/app/ (bead pico-link-0tr1)

Fern, 2026-09-24. Plan only; Ruby implements. Line numbers are against
`main` at `ed7f2d5`, where `core/src/app.rs` is **6094 lines**: production
1-3632, `mod tests` 3633-6094 (113 `#[test]`s). 1938 of the 3632 production
lines are comments.

## Rules for every step

- **Pure moves.** No renames, no signature changes, no reordering of match
  arms, no comment edits (comment trimming is S7, separate). Only allowed
  edits: `use` lines, `mod` lines, re-exports, and the visibility changes
  listed below.
- **External paths do not change.** `crate::app::X` and
  `pico_link_core::app::X` must keep resolving for every item that resolves
  today (render/home.rs:88, render/wizard.rs:42, render/hero.rs:38,
  render/screen.rs:22, render/navigator.rs:27, run.rs:127, lib.rs:64,
  ui-ffi/src/lib.rs). `app/mod.rs` re-exports with **explicit lists**
  (`pub use events::{Command, Event, ...}`, `pub(crate) use ...` for crate
  items), not globs, so the API surface stays reviewable.
- **Visibility rule.** An item that becomes cross-file inside `app/` gets
  `pub(in crate::app)`. Nothing becomes `pub(crate)` or `pub` that was not
  already. Children see ancestors' private items (App fields, `refresh_stack`,
  `build_identified_screen`), so `App` itself lives in `app/mod.rs`.
- **Gates per step:** `cargo test --workspace` = **577** passed (read the
  count, same invocation as baseline); headless PNGs byte-identical to a
  baseline captured on the step's base commit (`cmp` every file); firmware
  cross-compiles (fresh build dir needs `PICO_SDK_PATH`, toolchain at
  `/Applications/ArmGNUToolchain/15.2.rel1`); clippy as CI runs it.
- **Move-purity check (reviewer):** `git diff -M --color-moved=zebra`
  should show only moved blocks plus `use`/`mod`/visibility lines. Stronger:
  strip `use`/`mod` lines and `pub(...)` tokens from old app.rs and the
  concatenated new files, sort, `diff` -- must be empty apart from the added
  `mod`/`use` scaffolding.
- Each step is one bead, one worktree off local `main`, one merge.

## Target tree (production lines approx, tests beside)

| File | Contents (old line ranges) | Prod | Tests moved in |
|---|---|---|---|
| `app/mod.rs` | module doc 1-11; `mod`/re-exports; `MIN_REDRAW_DELAY` 120-135; `ModelHandle` 2555; `App` struct 2560-2657; `new`, display-settings accessors, `refresh_stack`, `build_identified_screen` 2658-2848; `poll_command`, `tick` 3367-3395; `handle_input`, `dirty`, `mark_dirty`, `render` 3522-3619; `RenderOutput` 3620-3632 | ~560 | none (see `tests.rs`) |
| `app/fold.rs` | `impl App`: `handle_event` + every `on_*`/`stamp_*` 2849-3096; `set_link_state`..`record_connect_failure` 3097-3346 | ~500 | 4641-4665 minus 4641-4653; 5095-5119, 5167-5214; 5268-5302; 5863-5884; 5885-5991 |
| `app/inspect.rs` | `impl App`: `model`, `volume_requires_dim_floor` 3347-3366; `now_us`..`home_face_for_test` 3396-3521 (read-only accessors + `#[cfg(test)]` hooks) | ~150 | 3716-3736 |
| `app/events.rs` | FFI vocabulary: `Command` 216-316, `ConnectFailureReason`+impl 317-347, 429-458, `StoreStatus` 348-372, `VolumeSource`/`VolumeState`+impl 373-428, `Event` 459-647, `ConnectStep`+impl 870-916 | ~480 | 3660-3715; 4641-4653 |
| `app/fault.rs` | `FaultKey`..`FaultLog`+impls 648-869 | ~220 | 3737-3798 |
| `app/model.rs` | `MAX_PAIRED_DEVICES`, `MAX_DEVICE_NAME_BYTES`, `truncate_device_name` 52-119; `LinkState`, `DeviceEntry`, `is_audio_sink`, `MAX_SCAN_LIST_ITEMS` 136-215; `OUT_LEVEL_HOLD_DURATION` 982-989; `BtModel` 1036-1173; `OutLevelSample`, q16 math, `decay_peak` 1174-1268; `DeviceAddr`, `PairedDevice`, `ConnectedCodec` 1269-1336 | ~430 | 4249-4273; 5120-5166; 5215-5267 |
| `app/ui_state.rs` | `WizardPhase`+impl, `PENDING_TIMESTAMP` 917-1014 (minus the OUT_LEVEL const); `HomeFace` 1015-1035; `DisplaySettingsState`+impl 2419-2444 | ~150 | none |
| `app/screen_id.rs` | `ScreenId`, `PickerKind`, `SettingsPickerKind` 1338-1399 | ~65 | none |
| `app/refresh.rs` | `ScreenCarry`, `Refresh` 1400-1438 -- **its own file so bgnd M5 deletes the file** | ~40 | none |
| `app/screens/mod.rs` | `mod` lines only | ~10 | -- |
| `app/screens/devices.rs` | `PAIR_NEW_ROW_KEY` 41-51; `DEVICES_TITLE` 1337; `paired_device_label`, `build_devices_screen`, `DevicesListView`, forget picker + forget confirm 1439-1744 | ~320 | 4383-4640 minus 4340-4382 (the a67 group goes to tests.rs) |
| `app/screens/device_page.rs` | `DASH`, `DEVICE_PAGE_QUALITY_ROW_INDEX` 1745-1750; `device_page_quality_present` .. `DevicePageView` 1821-2115 | ~310 | 5366-5492; 5660-5755 |
| `app/screens/ldac_quality.rs` | `LDAC_QUALITY_ADAPTIVE`, picker keys, `ldac_quality_*` helpers 1751-1820; `build_ldac_quality_picker_screen` 2359-2417 | ~130 | 5639-5659; 5756-5862 |
| `app/screens/picker.rs` | `PickerOption`, `build_single_select_screen` 2116-2193 | ~80 | 5514-5638 |
| `app/screens/why_page.rs` | `WHY_PAGE_TITLE` .. `build_why_page_screen` 2194-2358 | ~165 | 5992-6094 |
| `app/screens/settings.rs` | `SETTINGS_TITLE`, `build_settings_screen`, `build_settings_picker_screen` 2418, 2445-2554 | ~115 | none in app.rs today |
| `app/invariants.rs` (`#[cfg(test)]`) | every-screen invariants: freshness, damage==full, rail liveness 3799-4248 | -- | ~450 |
| `app/tests.rs` (`#[cfg(test)]`) | App-level behaviour: 4274-4382, 4666-4793, 4794-4911 (Home faces), 4912-5094 (dgx), 5303-5365, 5493-5513 | -- | ~600 |
| `app/test_support.rs` (`#[cfg(test)]`) | helpers used by more than one test file: `open_devices`, `open_wizard` 3643-3659, `upsert`, `upsert_with_quality` 4383-4393, `connect_link`, `assert_link_still_connected`, `assert_no_commands_queued` 4926-4965, `assert_home_hero_renders_connected` 5080-5094, `no_carry` 5526-5529 | -- | ~90 |

Ranges are approximate at boundaries (doc comments above an item move with
the item). Test-group placement rule when a range is ambiguous: a test goes
beside the builder/type whose behaviour it names; a test that only drives
`App` end-to-end goes to `tests.rs`; a helper used from 2+ files goes to
`test_support.rs` as `pub(in crate::app)`, a helper used by one file stays
local. Update the prose path at `render/screen.rs:496`
(`app::tests::a_rail_liveness...` -> `app::invariants::...`) in the step that
moves it -- a comment path, not behaviour.

Largest production file after the split: `mod.rs` ~560. No file over 800.

## bgnd live-widgets fit

- M1 Home: builder is `render/home.rs` (unchanged by this split); App side
  touches `mod.rs::build_identified_screen` only.
- M2 Devices: `screens/devices.rs` only.
- M3 Pickers + device page: `screens/picker.rs`, `ldac_quality.rs`,
  `device_page.rs`, `settings.rs` (settings picker) -- M3 is multi-screen by
  its own definition; consider splitting M3 into M3a picker/LDAC/settings
  and M3b device page, which this tree makes one file each.
- M4 why? + wizard: `screens/why_page.rs` + `render/wizard.rs`.
- M5 delete: remove `refresh.rs`, and `refresh_stack`/`build_identified_screen`
  in `mod.rs`.

## Forced visibility changes (the complete expected list)

All become `pub(in crate::app)` unless noted:

- `screens/devices.rs`: `paired_device_label`, `build_forget_confirm_screen`
  (used by device_page.rs:1960, 2002).
- `screens/ldac_quality.rs`: `build_ldac_quality_picker_screen` (device page
  1987, `mod.rs` 2836); `ldac_quality_fixed_kbps` (device page 1873).
- `ui_state.rs`: `PENDING_TIMESTAMP` (fold.rs 2893).
- `model.rs`: `OUT_LEVEL_HOLD_DURATION` (fold.rs 3213); `OutLevelSample`
  fields `hold_l_at`, `hold_r_at` (fold.rs 3209-3253).
- `ui_state.rs`: `DisplaySettingsState` fields `current`, `apply_pending`,
  `save_pending`, `refresh_pending` (mod.rs 2703-2744, 3535-3543;
  settings.rs; invariants 4231).
- `fold.rs`: any `App` method defined there and called from `mod.rs` or
  `inspect.rs` (e.g. if `tick` calls `on_wizard_auto_dismiss`) -- let the
  compiler list them; each gets `pub(in crate::app)`.
- `screens/*` items already `pub(crate)` stay `pub(crate)`.

If the compiler demands anything beyond this list, stop and note it on the
bead rather than widening further.

## Steps (each independently mergeable, one Sonnet context each)

- **S0** `git mv core/src/app.rs core/src/app/mod.rs`. Zero content change;
  keeps rename detection clean. Merge alone.
- **S1** Create `test_support.rs`; move the shared helpers; `mod tests` gains
  `use super::test_support::*`. Establishes the helper module every later
  step imports.
- **S2** Leaf types: `events.rs`, `fault.rs` + their tests.
- **S3** State types: `model.rs`, `ui_state.rs`, `screen_id.rs`,
  `refresh.rs` + model tests.
- **S4** Leaf screens: `screens/{mod,picker,settings,why_page}.rs` + tests.
- **S5** Coupled screens: `screens/{ldac_quality,device_page,devices}.rs` +
  tests.
- **S6** App split: `fold.rs`, `inspect.rs`, `invariants.rs`, `tests.rs`;
  `mod.rs` ends as struct + core impl + re-exports.
- **S7 (optional, separate)** comment trim, below.

S2-S5 each read ~600-900 lines of app.rs in slices plus the destination
files; S6 is the largest (~1500 lines touched) -- if it trends past ~100k
tokens, split it into S6a (`fold.rs`, `inspect.rs`) and S6b (`invariants.rs`,
`tests.rs`).

## S7: comment trimming (optional, after S6, never mixed with a move)

1938/3632 production lines are comments. Target roughly half that. One
commit per file so a reviewer can read each against the code.

- **Cut** bead IDs, "design section N", "this bead's scope item", and
  "replaces the old X, so the old name was stale" narration from doc
  comments. Example: the `PAIR_NEW_ROW_KEY` doc (old 41-50) keeps only "why
  0xFE and why it cannot collide with a device key".
- **Cut** test section headers that are bead IDs (`// --- pico-link-a67: ...`)
  -- rename to the behaviour they group.
- **Keep** invariants, units, overflow/cast safety arguments (e.g. the
  `decay_peak` truncation note, old 1255-1259), and every "why not the
  obvious alternative" that a future change would otherwise re-litigate.
- **Replace** provenance with one line per file head:
  `// Design: .planning/design/<doc>.md`.
- **Fix stale text:** the module header (old 1-11) still says "no domain
  model, no output seam" -- false since a67. Dead references to
  `app::placeholder_screen` and `App::rebuild_root` exist in `render/`;
  rewrite them to the current names.
- Gates same as the moves (test count, PNGs, firmware) plus
  `cargo doc --no-deps` with no new broken intra-doc links.
