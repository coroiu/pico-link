# 07 — Adversarial verification of 03-ffi-seam-and-run-loop.md

Verifier pass, 2026-09-25, tree at `1104a58` (review branch; no source changes vs the reviewed `2e37164` in the files below).
Method: re-read every cited path with 100+ lines of context, re-ran cbindgen, reproduced the CMake staleness in a scratch project. No source edited.

| Finding | Verdict | Severity (report -> mine) | Tier |
|---|---|---|---|
| F-ffi-01 forced-repaint backstop dead | **CONFIRMED**, with one scope correction on (a) | P1 -> **P1** | Sonnet |
| F-ffi-02 seven wire enums hand-pinned | **CONFIRMED** on facts; fix sketch as written **breaks the build** | P1 -> **P2** | Sonnet |
| F-ffi-03 wake policy only in shim | **PARTIALLY CONFIRMED**; **merge into F-app-06** | P1 -> **P2** (merged) | Opus |
| F-ffi-05 blit asserts under NDEBUG | Premise **CONFIRMED**, impact **overstated**; fix targets the wrong check | P2 -> **P3** | Haiku |
| F-ffi-07 Cargo.lock missing from DEPENDS | **CONFIRMED** (reproduced); scope is wider than reported | P2 -> **P2** | Haiku |
| Bonus: "boundary is memory-safe" | **Holds.** No UB reachable from C with a bad in-contract argument | — | — |

---

## F-ffi-01 — CONFIRMED (sub-claim (a) needs one scope correction)

**Evidence (path traced end to end):**
- `firmware/src/main.c:705-707`: `forced_repaint_due = PL_FORCED_REPAINT_MS != 0 && (now - last_blit_us) >= PL_FORCED_REPAINT_MS*1000; needs_paint = pl_ui_dirty(ui) || forced_repaint_due;`. `PL_FORCED_REPAINT_MS` defaults to 1000 (`firmware/CMakeLists.txt:521`).
- `main.c:733-741`: `display_on && needs_paint` -> `pl_ui_render_ex`. `ui-ffi/src/lib.rs:831-882`: nothing about "forced" crosses the FFI. It calls `App::render` (`core/src/app/mod.rs:430-440`), which calls `Navigator::render` -> `Screen::render` with `force_full_damage` false unless `App::mark_dirty` ran.
- `core/src/render/screen.rs:668-718`: with no key/area change, `damage` stays `Rectangle::zero()` and the function returns early at `:716` without writing any pixels. `lib.rs:853` maps that to `rect_count = 0`.
- `main.c:758-762`: `rect_count == 0` -> no blit. `main.c:783-785`: `last_blit_us` advances only when `blit_happened`. So once 1 s passes with no real damage, `forced_repaint_due` stays true on every later iteration.
- Test `pl_ui_render_ex_second_call_with_no_intervening_change_reports_no_damage` (`lib.rs:2704-2723`) pins exactly this zero-damage behaviour.

**(a) "the backstop never repaints": true on every screen whose widgets all have real `PaintKey`s**: Home (`HomeView`, `home.rs:748`), menus, field lists, device page, message, and both chrome regions. **It is not true everywhere:** `DevicesListView` (`app/screens/devices.rs:211`), `PairingWizardView` (`render/wizard.rs:322`) and `ConfirmView` (`render/confirm.rs:112`) do not override `paint_key`, so they inherit `PaintKey::ALWAYS` (`widget.rs:343`), which never compares equal (`paint_key.rs:74`). On those screens a forced render still damages that widget's band and blits it. The accurate claim is: **the backstop never produces a full-frame repaint, and on most screens it produces nothing at all.** That still violates both requirements. ADR 2026-09-02 (`.planning/decisions/2026-09-02-core1-allocation-and-the-repaint-ceiling.md:313-315`) says "1 s forced **full-frame** repaint … kept as the safety net". Design 2026-09-06 §3.4 (`:128-134`) lists it as a full-damage trigger, "the net under an incorrect `paint_key`", and A6 (`:398-399`) repeats it. The report quotes both correctly.

**(b) "`pl_ui_render_ex` every frame at idle": confirmed, but only in On/Dim states with no real damage.** It does not happen while blanked: `display_on` gates render at `main.c:733`, and the default is `ScreensaverMode::Off` after `Min1` at Home root (`core/src/power.rs:118-131`). It also does not happen during streaming, because the live meter produces real damage, so blits occur and `last_blit_us` advances. It **does** happen:
- for up to 59 s before each blank;
- indefinitely on any non-Home screen, since the screensaver only fires at the Home root (`run.rs:415`);
- indefinitely in Dim mode, with timeout `Never`, or under the mute/zero Dim floor.

**Does (b) matter in practice?** It is CPU and heap churn, not a lost power state. The firmware has no CPU sleep tier: the loop always paces to 16 ms and only `sleep_us`s the remainder (`main.c:960-969`). `pl_ui_tick` hardcodes `on_external_power = true` (`lib.rs:618`), so deep sleep can never fire on firmware anyway. The cost per call is sync + layout + key fold + 2 `Vec`s + title/volume `String`s at about 60 Hz. The render cost in µs is unverifiable here. The allocation churn adds a little to F-ffi-06's fragmentation concern. (b) is the secondary symptom; (a) is the defect.

**Severity:** P1 stands. It breaks a written ADR requirement, and the damage-rect work is exactly where a wrong `paint_key` would leave stale pixels, which is the case this net exists for.

**Fix-sketch correction.** The report's shape is right: add `pl_ui_mark_dirty` -> `App::mark_dirty` (`mod.rs:407-410`, which calls `Navigator::force_full_damage`, `navigator.rs:78`), and call it in C when `forced_repaint_due` before `pl_ui_render_ex`. Add one explicit warning for the implementer: **do not "simplify" this into a C-only full-frame `st7789_blit_framebuffer(px)` on `forced_repaint_due`.** That heals panel/GRAM corruption, but it re-sends a framebuffer that an incorrect `paint_key` has already left stale. It therefore misses the paint_key half of §3.4, which is the reason the net exists. The full damage must be forced in Rust. With the fix, rect_count is always 1 on a forced frame, so `last_blit_us` advances naturally. Also advancing it when `forced_repaint_due && rect_count == 0` is a harmless belt. Verification as in the report. Also extend `pl_ui_input_while_asleep_wakes_and_swallows_the_batch` to assert full-frame damage.

## F-ffi-02 — CONFIRMED on facts; P2, not P1; the fix sketch will not compile as written

**Evidence:**
- I regenerated the header with `cbindgen --config cbindgen.toml --crate ui-ffi` into the scratchpad. It has 7 enums: `PlDisplayPower, PlCommandTag, PlIntentTag, PlLinkState, PlDiscoveryState, PlFailureReason, PlEventTag`. `ui-ffi/src/lib.rs` defines 14 `pub enum Pl*`. The missing seven are exactly `PlConnectStep`(:1081), `PlVolumeSource`(:1325), `PlStoreStatus`(:1399), `PlFaultKey`(:1543), `PlFaultSeverity`(:1594), `PlFaultGlyph`(:1620) and `PlFaultValueKind`(:1648). (a) confirmed.
- **All seven are defined in `ui-ffi`, not `core`,** so crate scope (`parse_deps = false`) is **not** the cause. They are absent because every payload field carrying them is a raw `u8`/`u32`, so no exported item references the type, and they are not in `[export] include` (`cbindgen.toml:24-48`). Adding them to `include` is the correct lever.
- (b) The values are hand-pinned in `a2dp.h:173-176`, `fault.c:43-66` (C enum + `#define`s), `volume.h:66-71` (C enum), `persist.h:113-118` (C enum `pl_persist_status_t`). Confirmed.
- (c) **All 27 values agree today** (Rust vs C compared variant by variant).
- `PL_RENDER_ABI_VERSION`, `PlRenderOut` and `PlDamageRect` are **already** in the header (`:26`, `:425`, `:450`), so the report's "plus these for explicitness" is a no-op.

**Severity -> P2.** Nothing is wrong today. The convention is documented at each site (`fault.c:41-57` "MUST match … append-only forever", `a2dp.h:166`, `bt.h:79`), and the fault design declares the ordinals append-only. It is a latent-drift trap, not a silently violated ADR. The cbindgen.toml "can never drift" comment is overstated and should be fixed alongside.

**Fix-sketch corrections (important for a cheap model):**
1. **`volume.h:66-71` declares `typedef enum {…} PlVolumeSource;`, the same type name and the same enumerator names the generated header would emit.** `a2dp.h` and `bt.h` include `pico_link_ui.h`. `main.c`, `a2dp.c`, `debug_remote.c` and `volume.c` (via `bt.h`) include both. Exporting `PlVolumeSource` before `volume.h` is changed therefore produces a typedef/enumerator redefinition error in four TUs. `volume.c:128` (`apply_and_propagate(..., PlVolumeSource source, ...)`) takes `PL_VOLUME_SOURCE_CONSOLE = 3`, which the wire enum does not have. The C-side type must be **renamed** (e.g. `pl_volume_origin_t`, holding HOST/SINK/DEVICE from the generated values plus a local CONSOLE=3). Keeping CONSOLE as a `#define` is not enough.
2. `fault.c:43-51` must be deleted **in the same change** (enumerator redeclaration otherwise). It also supplies `PL_FAULT_KEY_COUNT` (used at `fault.c:155` and to size the glyph table at `:78`), which cbindgen will not emit. Keep a local `#define PL_FAULT_KEY_COUNT 6u` with `_Static_assert(PL_FAULT_KEY_ENC_RESYNC + 1 == PL_FAULT_KEY_COUNT, …)`.
3. The `a2dp.h:173-176` / `fault.c:58-66` `#define`s must go in the same change. A macro defined *before* the header's enum would textually rewrite `PL_CONNECT_STEP_CONNECTING = 0,` into `0u = 0,`.
4. `persist.h`'s `PL_PERSIST_STATUS_*` names do not collide with `PL_STORE_STATUS_*`. The report's `_Static_assert` approach is right there.
- Out of scope but same class, noted only: `ldac_quality` (1-based, `lib.rs:2424`) and `PlDisplaySettingsPayload::mode` are integer-coded wire conventions with no Rust enum to export. The seven are the complete *enum* set.

## F-ffi-03 — PARTIALLY CONFIRMED; dedup: merge into F-app-06, keep F-app-02 separate

**What holds:** The *composition* exists only in `pl_ui_push_event`: which event calls which idle hook, the `already_live` check computed before the fold (`lib.rs:2210-2217`), and the eligibility table `fault_key_wakes_display` (`lib.rs:2266-2278`). `Runner::step` (`run.rs:724-859`) has no event path, and `emulator/src` never calls `handle_event`.

**What is overstated:** "policy exists only in the shim". The decision **predicates are already in core**: `VolumeState::wakes_idle` (`core/src/app/events.rs:182`), `IdlePolicy::on_fault_wake` (`run.rs:564`), `App::volume_requires_dim_floor` (`app/inspect.rs:51`). That matches the letter of 2026-09-01 §4.1 ("a Platform-free unit in core that Runner::step calls and ui-ffi also calls"). What violates it is that `Runner` has no caller for them, and that one table (`fault_key_wakes_display`) sits in the shim *by explicit decision*: its doc comment cites bead scope. So the violation is not silent. The stronger citation is volume design §5.5 (`2026-09-07-volume-on-display.md:313-325`: "the same signal must drive the emulator … that divergence is a bug to file").

**Authoritative divergence table: firmware (`pl_ui_input`/`pl_ui_tick`/`pl_ui_push_event` + `main.c`) vs emulator (`Runner::step`).** This reconciles 03 and 05.

| # | Behaviour | Firmware | Emulator | Source |
|---|---|---|---|---|
| 1 | Volume wake (non-host, not muted/zero) | `idle.on_input()` + extends `last_input` via `volume_wake_since_last_tick` | absent | lib.rs:2140-2143, 616 |
| 2 | Mute/zero promotes a blanked display at once | `idle.on_input()` on the event | only via `IdlePolicy::tick`'s floor on the next step | lib.rs:2144-2156 |
| 3 | Fault wake (Audible ∧ table ∧ !live, storm-limited) | yes | absent | lib.rs:2209-2234 |
| 4 | `on_external_power` | hardcoded `true`, so deep sleep can never fire | platform; emulator default `false` (`emulator/src/platform/power.rs:9,72`) | lib.rs:618, run.rs:779 |
| 5 | Deep-sleep decision | ignored (`_decision`) | `enter_deep_sleep()` | lib.rs:618, run.rs:805 |
| 6 | Dirty on On->Dim / Dim->On | not marked | `mark_dirty()` on every non-Off level change | run.rs:792-801 |
| 7 | Forced 1 s repaint | exists (dead, F-ffi-01) | none | main.c:705 |
| 8 | Frame budget | 16 ms | 33 ms | main.c:514, emulator/src/main.rs:47 |
| 9 | "had input" for a batch that is all malformed tags | true (wakes, extends idle) | n/a (typed intents) | lib.rs:536, 554 |
| 10 | Settings save | via `SetDisplaySettings` command out | `settings.save(storage)` | lib.rs:2591, run.rs:762 |
| 11 | Flush/power error tracking | none | `FlushErrorTracker` | run.rs |
| 12 | Power apply | C pulls a level every frame (idempotent) | edge-applied | main.c:693-694 |

Rows 6 and 10-12 are shape differences with no behavioural consequence: GRAM retains content, and the damage cache tracks what was painted. Rows 1-5 and 8-9 are real divergences.

**Dedup ruling:** F-ffi-03 is **two halves of two existing app findings**:
- The policy-location half (rows 1-3 plus the `fault_key_wakes_display` table) and F-app-06 (rows 4-5, 8, and `pl_ui_tick` duplicating `Runner::step`) have one root cause: the firmware driver composes idle policy in `ui-ffi` instead of calling a shared core unit. **Merge them as one finding under F-app-06.** The fix sketch should cover both per-frame *and* per-event policy. Shape: core `fn apply_event_wake(idle: &mut IdlePolicy, app: &App, event: &Event, now) -> bool` (moving `fault_key_wakes_display` next to `FaultKey`), called by `pl_ui_push_event` and by a new `Runner::handle_event`. Tier Opus, **P2**: the firmware is correct today and the emulator side cannot be observed until F-app-02 lands.
- The emulator-event-source half **is F-app-02** (P1). Drop it from F-ffi-03. It is the prerequisite: without it, moving the policy into core buys unit tests but no headless verification.

## F-ffi-05 (P2 spot-check) — premise CONFIRMED, impact overstated -> P3

- **NDEBUG is set in the default firmware build.** The report's cited evidence (`tools/apply-sdk-patches.sh:22`) does not show that. The repo's own measured correction does: `firmware/src/panic_recorder.c:269-277` ("verified 2026-08-29 by rebuilding bare and reading flags.make … pico_pre_load_toolchain.cmake:5-11 forces CMAKE_BUILD_TYPE=Release … assert() IS ELIDED PROJECT-WIDE"). `firmware/CMakeLists.txt` sets no build type. `persist.c:242-244` and `a2dp.c:2527-2529` rely on the same fact.
- **Both asserts are trivially satisfied at every call site.** `st7789_blit_rect(px, PANEL_WIDTH, 0, rect->y, PANEL_WIDTH, rect->h)` (`main.c:779`) always passes `x=0, w=stride`. `st7789_blit_framebuffer` is only called with `W*H` (`main.c:241, 325`) or after `px_len_ok` has already proven `px_len == W*H` (`main.c:747-756`). `px_len` could only mismatch if `pl_ui_create` were called with other dimensions, and it is called once with the same `PANEL_*` constants (`main.c:391`). So eliding them loses nothing today.
- **Fix-sketch correction:** the value C actually receives from Rust and trusts unchecked is `rect->y`/`rect->h`. `st7789_blit_rect` (`st7789.c:237-244`) computes `fb + y*stride` and DMAs `h*stride` pixels with no bound. Today Rust clamps (`screen.rs:710` intersection with `whole_frame`) and saturates (`lib.rs:855-858`), so it is safe. The one useful check to add in C is `rect->h > 0 && rect->y + rect->h <= PANEL_HEIGHT`, falling back to a full-frame blit and one rate-limited `pl_log` otherwise. That check, plus a one-shot log on `!px_len_ok`, is the whole fix. Replacing the two tautological asserts is optional. Tier Haiku.

## F-ffi-07 (P2 spot-check) — CONFIRMED, and wider than reported

- `firmware/CMakeLists.txt:186-194`: the `OUTPUT ${UI_FFI_LIB}` `DEPENDS` list has the globbed `ui-ffi/src` + `core/src` `.rs` files and the three `Cargo.toml`s. It does **not** have `Cargo.lock`, **`rust-toolchain.toml`** (which exists at the repo root) or any `build.rs`.
- **Reproduced** in a scratch CMake/Ninja project with the same `add_custom_command` shape: build (command ran) -> change only `Cargo.lock` -> rebuild -> **command did not run, output still held v1 content.**
- Exact stale-link sequence:
  1. `cmake --build build`.
  2. `git pull` or `git checkout` of a commit that changes only `Cargo.lock` (a `cargo update` / semver-compatible `embedded-graphics` or `embedded-alloc` bump), or only `rust-toolchain.toml`.
  3. `cmake --build build`: the `.a` mtime is newer than every DEPENDS entry, so cargo is never invoked and the old archive is linked, with a header that may be fresh.
- The header command's `DEPENDS lib.rs` is correct today: `ui-ffi/src` holds only `lib.rs`, and `parse_deps=false` means core never contributes. The report's "trap when split" note is fair.
- Fix: add `Cargo.lock` and `rust-toolchain.toml` to DEPENDS, or make the target always invoke cargo (cargo is a fast no-op when fresh). The latter is more robust because it also covers `.cargo/config.toml` if one is ever added. P2, Haiku.

## Bonus — "the boundary is memory-safe": holds

I checked each unsafe path and a bad argument from C reaches at worst a panic, which leads to a recorded watchdog reboot, not UB:
- `pl_ui_create`: dims 0 or overflow -> null (`lib.rs:361-366`). Huge dims -> alloc failure -> panic -> reboot. Heap init latch is atomic.
- `pl_ui_input`: null/0 guarded. Tags `TryFrom`-checked. `jump_by: i16` is widened to `i32` everywhere it is used (`list.rs:948`, `menu.rs:513`, `fields.rs:367`). Huge `count` is a C contract breach (UB is inherent to `from_raw_parts`). The allocation side is already raised as F-ffi-06.
- `pl_ui_push_event`: version and tag are checked before any union read. Every union member is `Copy` with integer/array fields; nested discriminants are checked. Inline name arrays are clamped with `min(name.len())` (`lib.rs:2041, 2087`). The one pointer-carrying payload, `DeviceDiscovered`, is clamped at the only producer: `bt.c:163-167` clamps `name_len` to `PL_BT_RING_NAME_CAP` *and writes the clamp back into the event*, and the drain re-points `name` at the entry's own buffer (`bt.c:204-206`). A name longer than the cap from a remote device cannot cause an out-of-bounds read.
- `pl_ui_render_ex`: the damage rect is clamped to the framebuffer in core and saturating in the shim. `px`/`rects` are borrowed until the next mutating call, and C's blit blocks.
- Reentrancy / IRQ: every `pl_ui_*` call site (`a2dp.c:1110, 4148`, `bt.c:207, 995`, `fault.c:206`, `main.c:391-741`, `main.c:456`) runs on the core0 thread. Debug-remote input goes through `pl_debug_remote_poll` in the superloop (`main.c:589-593`), not a TinyUSB callback in the 0xC0 IRQ. The only Rust->C call is the panic hook, which does not re-enter.
- `RefCell` double-borrow panics and `malformed_tag_count` wrapping (release, no overflow checks) are both non-UB.
- Residual non-boundary risk, already in 03 §7: Rust stack overflow on core0 without a verified MSPLIM guard. Unverifiable here.

## New findings

None at P0/P1. One P3 for the batch:

### F-ffi-V01: A `pl_ui_input` batch of only malformed tags still counts as user input
- Severity: P3   Confidence: High   Effort: S   Tier: Haiku
- Location: `ui-ffi/src/lib.rs:536`, `:554`
- Evidence: `ui.input_since_last_tick = true;` is set, and `ui.idle.on_input()` is called, before or regardless of whether `mapped` is empty. `Runner::step` keys `had_input` on non-empty typed intents.
- Why it matters: a corrupted input-ring slot wakes the display and extends the idle timer. This is divergence row 9. Cosmetic.
- Fix sketch: after mapping, `if mapped.is_empty() { return; }` before setting the flag and calling `on_input`.
- Verification: extend `pl_ui_input_rejects_out_of_range_tag_without_panicking` to tick past the idle timeout and assert `PlDisplayPower::Off`.
