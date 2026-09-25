# 03 — FFI seam and run loop (`ffi`)

Reviewed at `2e37164`. Scope: `ui-ffi/`, `firmware/src/{main,input,st7789}.c/.h`, `core/src/{platform,run,lib}.rs`, `core/src/app/{mod,events}.rs` (boundary only), `firmware/CMakeLists.txt`, `panic_recorder.c` (Rust intersection), and the ADRs/design docs the charter lists.

What was actually run here (no board, no pico-sdk):
- `cargo test -p ui-ffi`: **58 passed, 0 failed**.
- `cargo build -p ui-ffi --release --target thumbv8m.main-none-eabi` (the triple CMake uses): **builds**; `nm` on the `.a` shows all 11 `pl_ui_*` entry points defined, `_critical_section_1_0_{acquire,release}` defined, the panic handler (`rust_begin_unwind`) defined, `HEAP_MEM` = `0x30000` (192 KB) in `.bss`. Soft-float helpers are referenced only from `micromath`/`num_traits`/`compiler_builtins` objects, not from `core`/`ui-ffi` code.
- `cbindgen --config cbindgen.toml --crate ui-ffi` (installed locally): generates a 1263-line header that passes `gcc -fsyntax-only`. Cross-checking every `PL_*`/`Pl*` identifier the C sources use against it found **seven wire enums absent from the header and re-declared by hand in C** (F-ffi-02).
- `cargo build --workspace` on host: OK. `cargo build -p ui-ffi` / `cargo check -p ui-ffi` / `cargo clippy -p ui-ffi` on host: **fail** (`#[panic_handler]` required; unwinding not supported without std). See F-ffi-08.

---

## 1. Verdict

The seam is in good shape where it matters most: memory safety across the boundary is handled with unusual care (raw-integer discriminants + checked conversion everywhere, null checks on every entry, `Copy`-only union members, a real panic → watchdog path), and the IRQ-context bug (`pico-link-6o2`) was fixed structurally with an interrupt-masked MPSC ring, not a patch. The Rust surface is genuinely single-core and single-context, and I verified core1's entry loop never reaches Rust or the ring. Three things are wrong, none of them memory-unsafe: the 1 s forced-repaint backstop that two design docs require has been dead since the damage-rect phase landed; seven wire enums are hand-copied on the C side despite the header's "can never drift" promise; and the volume/fault wake policy exists only in the FFI shim, so the emulator can never run it. Nothing here is P0.

## 2. What is well done

- **Discriminant hardening (`ui-ffi/src/lib.rs:434-507`, `955-1069`, `1331-1431`, `1552-1669`).** Every value C supplies is a plain `u32`/`u8`, converted through `TryFrom`, counted on failure. Every union read is guarded by the outer tag first. This is the correct shape and it is applied uniformly, including one level down (`PlLinkStateChangedPayload::state`) where tag-only hardening would have left UB. Do not touch.
- **Null polarity is consistent and self-healing** (`pl_ui_display_power` → `On`, `pl_ui_backlight_permille` → `1000`, `pl_ui_dirty` → `true`, `pl_ui_render_ex` zeroes `*out`). Every degenerate case fails toward painting.
- **Heap init latch** (`lib.rs:169-184`): `compare_exchange` makes a second `pl_ui_create` sound. `pl_ui_create` is called exactly once (`main.c:391`).
- **Panic path is complete and one-directional** (`lib.rs:213-263` → `main.c:79-81` → `panic_recorder.c:178-180`): fixed 256-byte stack buffer, no allocation, watchdog armed first, reboot; C never calls back into `pl_ui_*` from the hook. OOM routes through the same path (`__rust_alloc_error_handler` → panic) so it is a recorded reboot, not a silent null.
- **The 6o2 fix is structural** (`bt.c:53-135`, `162-215`): a 32-slot MPSC ring whose only consumer is `pl_bt_drain_events` in thread context; the two producer contexts (BTstack IRQ, thread-context `pl_bt_poll_commands`) are serialised under `save_and_disable_interrupts`. DeviceDiscovered names are copied into the ring entry and re-pointed at drain time, so the "borrowed for the call" contract is honoured across the deferral. `a2dp.c:1110`, `a2dp.c:4148` and `fault.c:206` call `pl_ui_push_event` directly, but all three are invoked from `main.c`'s superloop (lines 668, 674, 908). `input.c` is SPSC with one IRQ producer.
- **Single-core invariant holds with core1 live.** `pl_a2dp_core1_entry` (`a2dp.c:2366+`) calls `pl_a2dp_core1_arm_running/fill/resync_apply/debug_skip_media_ticks/pl_flash_lockout_core1_init`; none of those bodies contain a `pl_bt_push_*`, `pl_ui_*` or `pl_persist_save_*` call (grep-verified). So the PRIMASK-only `critical_section` impl (`lib.rs:90-129`) is sound today.
- **Idle policy really is one implementation, two drivers** (`run.rs:298-610` shared by `Runner::step` and `pl_ui_{input,tick,push_event}`), exactly as `2026-09-01-idle-policy-across-the-ffi-seam.md` §3–4 asked. The pull-level shape (`pl_ui_display_power`/`pl_ui_backlight_permille`/`pl_ui_dirty`) is idempotent and re-applied every frame.
- **Damage-rect boundary math is right.** `screen.rs:710` clamps to `whole_frame`; `st7789_blit_rect` uses inclusive `y1 = y+h-1` (`st7789.c:239-241`); C quantises to a full-width band and never reads `rect->x/w` (`main.c:773-779`); the window is set on every blit; the blit blocks so Rust can never write during DMA.
- **Header is generated, not checked in**: `firmware/.gitignore:4` ignores it, CMake regenerates it (`CMakeLists.txt:202-210`).
- **Tests drive the real `extern "C"` functions** with nulls, garbage tags at every nesting level, mixed batches, and idle/wake sequences (`lib.rs:2617-3994`).

## 3. Architecture assessment

| Doc | Code matches? | Notes |
|---|---|---|
| ADR 2026-08-27 C-first | Yes | `main.c` owns `main()`, boot, superloop; Rust is a staticlib; direction rule (one hook) holds. `CLAUDE.md` "Current State" claim (one `PlEvent` in / one `PlCommand` out, versioned) still true, plus versioned `PlRenderOut` and three pull levels. |
| ADR 2026-08-26 (superseded) | Index yes, file no | `INDEX.md:11` marks it Superseded; the file itself still says `**Status:** Accepted` (line 4). P3. |
| ADR 2026-08-31 render-ctx | Yes | `pl_ui_tick` → `App::tick` → `next_redraw_at` → dirty (`app/mod.rs:348-356`). |
| ADR 2026-09-02 core1/repaint ceiling | Yes, with one violation | Rust stays single-core ✓; damage-rect landed ✓; **"keep the 1 s forced repaint as the safety net" — violated by omission** (F-ffi-01). `pico-link-3uq` is demoted, unmerged, and no branch exists in this clone; `main.c:684-687`, `729-730` still hedge for it. |
| 2026-09-01 idle-policy | Yes | Divergences that are *not* contract breaks: emulator 33 ms budget (`emulator/src/main.rs:47`) vs firmware 16 ms (`main.c:514`); `Runner::step` marks dirty on On↔Dim (`run.rs:796-801`), FFI does not (unneeded — GRAM retains); `pl_ui_tick` hardcodes `on_external_power = true` (`lib.rs:618`). |
| 2026-09-02 dirty-gate | §3.1/§3.3 yes; §5 dead | Gate ordering in `main.c:693-731` is exactly §3.3. The §5 backstop exists in C but cannot fire (F-ffi-01). |
| 2026-09-06 damage-rect | §6/§7 yes; §3.4 no | FFI shape matches §6; §3.4 lists the forced repaint as a *full-damage trigger*; there is no FFI to express that, so it was never wired. Doc is right, code is wrong. |
| 2026-09-07 composite damage | n/a to this seam | Row-band quantisation (`main.c:764-779`) is the B3 it names. |
| 2026-09-24 app-rs split | n/a | No boundary impact. |

**Is `lib.rs` (3994 lines) all boundary code?** ~2600 production lines, most of it doc comment; ~1400 test. Two blocks are *application policy*, not adaptation: the volume-wake decision (`lib.rs:2140-2156`) and the fault-wake decision plus its eligibility table (`lib.rs:2209-2234`, `2266-2278`). See F-ffi-03. Everything else (twelve `TryFrom`s, the deferred-flag plumbing, `pl_command_from`) is legitimately boundary.

## 4. Findings

### F-ffi-01: The 1 s forced-repaint backstop has been dead since damage-rect landed, and it now costs a render every frame at idle
- Severity: P1   Confidence: High   Effort: S   Tier: Sonnet
- Location: `firmware/src/main.c:704-707`, `758-762`, `783-785`; `ui-ffi/src/lib.rs:861-871`; `core/src/app/mod.rs:430-440`
- Evidence: `needs_paint = pl_ui_dirty(ui) || forced_repaint_due` (main.c:707) leads to `pl_ui_render_ex`, but `App::render` with no state change returns `Rectangle::zero()` damage (test `pl_ui_render_ex_second_call_with_no_intervening_change_reports_no_damage`, lib.rs:2704), so C hits `rect_count == 0` → "do not blit at all" (main.c:758-762). `last_blit_us` is only advanced when `blit_happened` (main.c:783-785), so once 1 s has elapsed `forced_repaint_due` is true on **every** subsequent iteration and `pl_ui_render_ex` (navigator sync + damage pass) runs every frame for nothing.
- Why it matters: ADR 2026-09-02 ("keep it as the safety net — do not remove it") and design 2026-09-06 §3.4/A6 both require this net under an incorrect `paint_key` or panel-side corruption. It silently vanished in `pico-link-7h5.8`, and A6 was signed off without a test that could see it. Secondary: the dirty gate's render saving (2026-09-02 §3.2 reason 1) is partially undone at idle.
- Fix sketch: add `pl_ui_mark_dirty(ui)` (calls `App::mark_dirty`, which already sets `force_full_damage`); in `main.c`, when `forced_repaint_due`, call it before `pl_ui_render_ex` so the next render reports the whole frame; advance `last_blit_us` (or a separate `last_forced_us`) whenever the forced path ran, regardless of `rect_count`.
- Verification: new ui-ffi test: `render_ex` → `pl_ui_mark_dirty` → `render_ex` asserts `rect_count == 1` and rect == `(0,0,stride,16)`; on hardware, `PL_LOOP_PHASE_BLIT` sample count ≈ 1/s at idle (the ADR's fact A measured 0.99 Hz pre-7h5.8) and `PL_LOOP_PHASE_UI_RENDER` count ≈ 1/s, not ≈ 60/s.
- Related: `pico-link-vxc`, `pico-link-7h5.8`, ADR 2026-09-02, design 2026-09-06 §3.4/A6, 2026-09-02 §5.

### F-ffi-02: Seven wire enums are hand-copied in C because cbindgen's export list is stale — the header's "can never drift" promise is false for them
- Severity: P1   Confidence: High   Effort: M   Tier: Sonnet
- Location: `ui-ffi/cbindgen.toml:30-55`; `firmware/src/a2dp.h:173-176`; `firmware/src/fault.c:43-66`; `firmware/src/volume.h:66-71`; `firmware/src/persist.h:114+`
- Evidence: running cbindgen here, the header contains enums `PlDisplayPower, PlCommandTag, PlIntentTag, PlLinkState, PlDiscoveryState, PlFailureReason, PlEventTag` only. `PlConnectStep, PlStoreStatus, PlVolumeSource, PlFaultKey, PlFaultSeverity, PlFaultGlyph, PlFaultValueKind` are absent (only reachable via `u32`/`u8` fields, and not in `[export] include`). C therefore pins them by hand: `#define PL_CONNECT_STEP_NEGOTIATING_CODEC 3u` (a2dp.h:176), `PL_FAULT_KEY_BUF_STARVED = 0` (fault.c:44), `PL_FAULT_SEVERITY_AUDIBLE 1u` (fault.c:59), `PL_VOLUME_SOURCE_SINK = 1` (volume.h:68), `PL_PERSIST_STATUS_LOADED = 1` (persist.h:115). fault.c:53-57 even documents this as "the same convention as a2dp.h".
- Why it matters: this is exactly the silent-ABI-drift class the generated header exists to kill. The `pico-link-88xs` change (removing `Scanning = 1`) shows renumbering happens; a Rust-side renumber of any of these seven compiles cleanly on both sides and misroutes events at runtime, caught only by `malformed_tag_count` — which nothing reads (F-ffi-04).
- Fix sketch: add the seven enums (plus `PlRenderOut`, `PlDamageRect`, `PL_RENDER_ABI_VERSION` for explicitness) to `[export] include`; delete the C copies and use the generated names. `volume.h`'s `PL_VOLUME_SOURCE_CONSOLE = 3` is not a wire value — keep it as a separate `#define` outside the generated enum. `persist.h`'s status enum predates the seam; keep it but add `_Static_assert(PL_PERSIST_STATUS_LOADED == PL_STORE_STATUS_LOADED, …)` for each value.
- Verification: `cd ui-ffi && cbindgen --config cbindgen.toml --crate ui-ffi | grep -c "PL_CONNECT_STEP_NEGOTIATING_CODEC\|PL_FAULT_KEY_ENC_RESYNC\|PL_STORE_STATUS_VERSION_MISMATCH\|PL_VOLUME_SOURCE_DEVICE"` → 4; `grep -rn "define PL_CONNECT_STEP_\|define PL_FAULT_SEVERITY_\|define PL_FAULT_GLYPH_\|define PL_FAULT_VALUE_KIND_" firmware/src` → 0; firmware cross-compiles.
- Related: `pico-link-ptu`, `pico-link-88xs`, `pico-link-9eq2.3.2`, M1b header rationale (`CMakeLists.txt:160-163`).

### F-ffi-03: Volume-wake and fault-wake policy live only in the FFI shim; the emulator has no counterpart and no event source
- Severity: P1   Confidence: High   Effort: M   Tier: Opus
- Location: `ui-ffi/src/lib.rs:2140-2156` (volume), `2209-2234` + `2266-2278` (fault); `core/src/run.rs:724-859` (`Runner::step`, no equivalent); `emulator/src` (no `handle_event`/`push_event` call site at all)
- Evidence: `pl_ui_push_event`'s `VolumeChanged` arm decides `if volume.wakes_idle() { on_input(); … } else if muted || level == 0 { on_input() }`; the `AudioFault` arm computes `already_live`, `wants_wake` and calls `IdlePolicy::on_fault_wake`. `fault_key_wakes_display` is a policy table defined in the shim. `Runner::step` never calls `on_fault_wake` or `wakes_idle` (grep: the only non-test callers are in `ui-ffi`), and the emulator never constructs an `Event`.
- Why it matters: design 2026-09-01 §4.1 set the rule "one implementation of each decision, two drivers"; this is the first decision added since that is *not* shared. Design 2026-09-07-volume-on-display §5.5 already flagged that a rule proven only against `run.rs` never runs on firmware — this is the mirror image: a rule that only runs on firmware can never be exercised headless/windowed, i.e. the three-mode guarantee the project pays for is broken for the two most recently commissioned wake behaviours. It is also application logic in a file whose stated job is adaptation.
- Fix sketch: move the decision into `core` — e.g. `IdlePolicy::on_event(&Event, fault_already_live: bool, now) -> WakeOutcome` or have `App::handle_event` return an `EventEffect { wake: … }` that both `Runner::step` and `pl_ui_push_event` apply; move `fault_key_wakes_display` next to `FaultKey` (or onto `IdlePolicy`). Give the emulator an event-injection source (a headless script or the existing HTTP mode) so `Runner::step` exercises the path.
- Verification: `grep -n "wakes_idle\|on_fault_wake\|fault_key_wakes_display" ui-ffi/src/lib.rs` outside `mod tests` → 0; the eight wake tests at `lib.rs:3156-3266`, `3925-3993` gain core-side twins in `run.rs`/`app/tests.rs` driven through `Runner::step`; ui-ffi keeps them as pass-through regression tests.
- Related: 2026-09-01 §4, 2026-09-07-volume-on-display §5, 2026-09-07-audio-fault-model §7.2/§7.5, `pico-link-9eq2.3.1`, `pico-link-4v2.5`.

### F-ffi-04: `pl_ui_malformed_tag_count` is write-only on hardware, and ABI-version rejections are uncounted
- Severity: P2   Confidence: High   Effort: S   Tier: Haiku
- Location: `ui-ffi/src/lib.rs:303-313`, `391-399`, `1941-1944`; `firmware/src/*.c` (no caller — grep `malformed` hits only debug_remote comments)
- Evidence: the counter's doc says a bad tag is "graceful *and* observable"; no C file calls `pl_ui_malformed_tag_count`. `pl_ui_push_event` returns silently on `event.version != PL_EVENT_ABI_VERSION` with no counter (lib.rs:1942).
- Why it matters: this counter is the only runtime signal for F-ffi-02's failure mode and for ring corruption; today it is invisible. A zero-initialised `PlEvent` (`version` unset) vanishes with no trace.
- Fix sketch: read it in `main.c`'s 1 Hz shared-report block and `pl_log` when it changed (same pattern as `bt.c:210-214`); add `version_mismatch_count` alongside it, exposed by the same getter or a second one.
- Verification: `grep -n pl_ui_malformed_tag_count firmware/src/main.c` → 1 hit in the 1 Hz block; a `PL_DEBUG_REMOTE` injection of a bad tag produces one log line on hardware.
- Related: `pico-link-ptu`, `pico-link-6o2`.

### F-ffi-05: Blit-path contract checks are `assert()`s (no-ops in the default Release build) and the `px_len` mismatch path is silent
- Severity: P2   Confidence: Medium   Effort: S   Tier: Sonnet
- Location: `firmware/src/st7789.c:237`, `:324`; `firmware/src/main.c:747-749`, `750-782`
- Evidence: `assert(x == 0 && w == stride)` and `assert(pixel_count == W*H)` guard the phase-1 band restriction and the full-frame length; pico-sdk defaults `CMAKE_BUILD_TYPE` to Release (`-DNDEBUG`), which the repo's own note at `tools/apply-sdk-patches.sh:22` ("NDEBUG does NOT elide panic()") implies is the case. `main.c:747-749` computes `px_len_ok` and, when false, does nothing — no log, no counter.
- Why it matters: a stride/size mismatch (a future framebuffer change, or a `PlRenderOut` produced by a mismatched `.a`) yields a permanently frozen panel with no evidence, which on this device reads as a crash. With `NDEBUG` the `panic_recorder.c:262+` `__assert_func` override is unreachable from these sites.
- Fix sketch: replace both asserts with explicit checks that `pl_log` once (rate-limited) and return without DMA; log `px_len`/`stride` mismatch once. Optionally route to `pl_panic_c_hook` since either is a programming error.
- Verification: `grep -n "assert(" firmware/src/st7789.c` → 0; `cmake -LA build | grep CMAKE_BUILD_TYPE` and `grep -o '\-DNDEBUG' build/compile_commands.json | head -1` to confirm the premise on the real build dir (unverifiable here).
- Related: `pico-link-7h5.7`, `pico-link-itf`.

### F-ffi-06: No heap telemetry across the seam; OOM is a reboot with no warning, on a 192 KB LLFF arena fed by per-event and per-render allocations
- Severity: P2   Confidence: Medium   Effort: S   Tier: Sonnet
- Location: `ui-ffi/src/lib.rs:152-159` (arena), `530-566` (`pl_ui_input` allocates `Vec::with_capacity(count)` from a C-supplied `count`); `core/src/render/hero.rs:762-1000` (`String::from`/`format!` inside render); `core/src/app/mod.rs:359-388`
- Evidence: `HEAP_MEM` is `0x30000` in `.bss` (nm-verified); the framebuffer alone is 115,200 B, leaving ~77 KB for everything else. ~390 allocation sites in `core/src/{render,app}` (grep count, rough), including `format!("{kbps} kbps")` per hero render and `rebuild_root`-style screen rebuilds per event. `embedded_alloc::LlffHeap` exposes `used()`/`free()`; nothing calls them. On exhaustion `__rust_alloc_error_handler` → panic → watchdog reboot.
- Why it matters: LLFF fragmentation in a long-running loop is invisible until it reboots the device mid-session; there is currently no way to see headroom trending down, and the arena size (192 KB) was picked once on the M1b linker map (lib.rs:133-146), before BTstack/TinyUSB/LDAC/PSRAM-less layout existed.
- Fix sketch: `pl_ui_heap_stats(ui, *used, *free)` reading `HEAP.used()/free()` plus a high-water field; report in the 1 Hz block; clamp `count` in `pl_ui_input` (e.g. 64) before `with_capacity`.
- Verification: hardware log line `ui-heap: used=… free=… hw=…` at 1 Hz; host test that `pl_ui_input` with 65 valid intents forwards all 65 without ever allocating a `Vec` above the clamp (process in chunks, or map into a fixed-capacity buffer). Note the C contract still requires `count` valid elements — a clamp bounds the allocation, not the read.
- Related: `pico-link-cz0.2` (the 64 KB → 192 KB correction), ADR 2026-09-02 A4.

### F-ffi-07: Build hygiene — a dependency bump can link a stale `.a`, and the ui-ffi host build only works by accident of feature unification
- Severity: P2   Confidence: High   Effort: S   Tier: Haiku
- Location: `firmware/CMakeLists.txt:182-210`; `ui-ffi/src/lib.rs:199-212`
- Evidence: the `.a` custom command `DEPENDS` on `*.rs` and the three `Cargo.toml`s but not `Cargo.lock` (tracked in git); a lockfile-only change (dependency bump) leaves `${UI_FFI_LIB}` newer than all DEPENDS, so CMake never invokes cargo and links the old archive. The header command `DEPENDS` only `lib.rs` — correct today, a trap the day `ui-ffi/src` is split. On the host, `cargo build/check/clippy -p ui-ffi` fails (`#[panic_handler]` required) while `--workspace` succeeds because the emulator's features drag `std` into the unified graph; the lib.rs comment describes only the latter.
- Why it matters: a silent stale-`.a` link is the one FFI failure the generated header cannot catch (header fresh, code old). The host-build asymmetry means `cargo clippy -p ui-ffi` is unusable and the charter's clean workspace clippy is conditional on the emulator's feature set.
- Fix sketch: add `${PICO_LINK_WORKSPACE_ROOT}/Cargo.lock` to the `.a` DEPENDS (or make `ui_ffi_build` always run cargo, which is a no-op when fresh); reuse the `UI_FFI_RUST_SOURCES` glob for the header command; document `cargo clippy -p ui-ffi --target thumbv8m.main-none-eabi` as the per-crate lint invocation (it works: the release cross-build here was clean).
- Verification: `touch Cargo.lock && cmake --build build` re-runs cargo; `cargo clippy -p ui-ffi --target thumbv8m.main-none-eabi` exits 0.

### F-ffi-08: P3 batch — stale comments and small contract drifts
- Severity: P3   Confidence: High   Effort: S   Tier: Haiku
- Location / Evidence:
  1. `.planning/decisions/2026-08-26-rust-owns-the-binary-no-usbpods-fork.md:4` still `**Status:** Accepted`; only `INDEX.md:11` says Superseded.
  2. Target triple: `CLAUDE.md` "Shared core" says `thumbv8m.main-none-eabihf`, `ui-ffi/src/lib.rs:194` and `Cargo.toml:31` say `eabihf`; CMake builds `thumbv8m.main-none-eabi` (`CMakeLists.txt:175`) for the softfp-link reason it documents. Keep the CMake reasoning; fix the three docs. (Soft-float is harmless today: `core` uses no `f32`/`f64` outside the off-by-default `frame-timing` feature.)
  3. `core/src/lib.rs:47-49`: "global allocator, likely backed by the RP2350's PSRAM" — it is a 192 KB static in SRAM `.bss`, and the zero-copy DMA blit reads from it, so moving it to PSRAM is not a free change (ADR 2026-09-02 A4).
  4. `ui-ffi/src/lib.rs:22-25`: "Strings Rust receives from C (none in the M1 surface below)" — `PlDeviceDiscoveredPayload::name` is exactly that.
  5. Retired `pl_ui_render` still referenced as live: `main.c:736-737` ("stays live until pico-link-7h5.10 retires it"), `st7789.h:64`, `:96`.
  6. `main.c:684-687`, `729-730` and 2026-09-01 §3.3 still plan around `pico-link-3uq`; ADR 2026-09-02 demoted it and no branch exists in this clone. Either close the bead or drop the hedges.
  7. `pl_ui_poll_command`'s contract (`lib.rs:2569-2574`: "drain it in a loop") vs `bt.c:989-1000`, which pops exactly one per iteration and lets the `SetDisplaySettings` latch pre-empt it; not lossy, ≤16 ms extra latency per queued command. Either loop (bounded) in C or fix the Rust doc.
  8. `lib.rs:73-76` justifies the PRIMASK-only critical section with "everything here runs on one core" — true, and now load-bearing with core1 live by default since 2026-09-23 (`CMakeLists.txt:352`). Add the sentence "core1 must never enter Rust or `pl_bt_ring_push`" next to `pl_a2dp_core1_entry` so the invariant is stated where it can be broken.
  9. cbindgen prints `Missing [defines] entry for test` — add `[defines] "test" = "PL_UI_TEST"` (or the equivalent) to silence it.
- Verification: grep each cited line after the edit.

## 5. Test coverage

Covered (58 tests, all through the real `extern "C"` surface): null `ui`/`out`; out-of-range and garbage tags at the outer level and nested (`state`, `reason`, `step`, `status`, `source`, `key`, `severity`, `discovery`); mixed valid/invalid batches; first-render full damage vs second-render zero damage; idle blank/wake/swallow, lazy baseline, host-vs-sink volume wake, mute floor → Dim, fault wake incl. Concealed/repeat-Live; every `Command` → wire mapping; paired-device fold and MRU auto-reconnect.

Not covered — cheap for a Sonnet/Haiku to add:
- `PL_EVENT_ABI_VERSION` mismatch (nothing sends `version != PL_EVENT_ABI_VERSION`; the `version: 0` literals at `lib.rs:2638-2707` are `PlRenderOut` inits). Assert the event is dropped and, after F-ffi-04, counted.
- `pl_ui_input(ui, null, n)` and `count == 0`; `pl_ui_tick`/`pl_ui_render_ex` before any input; `pl_ui_push_event` before the first `pl_ui_tick` (`now_us == 0` path for fault wake).
- Wake → next `pl_ui_render_ex` reports the **whole** frame (`mark_dirty` → `force_full_damage`); today `pl_ui_input_while_asleep_wakes_and_swallows_the_batch` asserts `dirty` only. Same test, extended, catches F-ffi-01 once `pl_ui_mark_dirty` exists.
- Layout pins: `const _: () = assert!(core::mem::size_of::<PlEvent>() == …)` per `target_pointer_width` (the `DeviceDiscovered` member holds a pointer), same for `PlCommand`/`PlRenderOut`. This catches a non-additive change that forgot the version bump — the one thing header regeneration cannot.
- `pl_ui_backlight_permille` (Dim = `DIM_BACKLIGHT_PERMILLE`), `DisplaySettingsLoaded` in, `SetDisplaySettings` out ahead of a queued BT command.
- A damage rect never exceeds the framebuffer (`y + h <= height`) after a partial repaint — property-style over a few events.
- `firmware/tests/`: nothing exercises `st7789_blit_rect`'s coordinate math or `pl_link_input_poll`'s ring; both are host-testable with a stubbed `spi_get_hw`/`gpio_get`.

## 6. Open questions for Andreas

1. **Keep the 1 s forced full-frame repaint?** With damage-rect it is now the *only* full-frame blit source at idle (~13 ms of SPI per second). ADR 2026-09-02 and design 2026-09-06 say keep it. My recommendation: keep it, implement it properly (F-ffi-01), and measure `=0` vs `=1000` as 2026-09-02 §5 always intended.
2. **Should the emulator get an event-injection source** (scripted `Event`s in headless mode)? F-ffi-03 is only half-fixable without it; it is test infrastructure, but it decides whether the wake behaviours are ever verifiable off-hardware.

## 7. Unverifiable here

- Whether the real build dir compiles with `-DNDEBUG` (F-ffi-05 premise) — check `build/compile_commands.json`.
- The per-frame cost of the no-op `pl_ui_render_ex` under F-ffi-01 (`PL_LOOP_PHASE_UI_RENDER` at idle) and the blit count.
- Heap used/high-water and fragmentation over a long session (F-ffi-06).
- Core0 stack headroom for the Rust render path under `PICO_STACK_SIZE=0xC000` shared with cyw43 init.
- That the header cbindgen produces on Andreas's machine matches the one generated here (cbindgen version not pinned anywhere; the CMake command runs whatever is on `PATH`).
- The 33 ms / 16 ms budget difference has no observable effect — it shouldn't, but it is a number nobody has measured against.
