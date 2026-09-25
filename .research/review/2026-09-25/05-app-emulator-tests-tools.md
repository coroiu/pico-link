# 05 — app / emulator / tests / tools (seam `app`)

Reviewed at `2e37164`. Ran: `cargo test --workspace` (578 passed, 0 failed — matches charter), `cargo clippy --workspace --all-targets` (4 distinct warnings, 5 emitted; the charter's "7" over-counts — see §4 F-app-13), all 8 `firmware/tests/*.c` host tests (8/8 pass once the generated header is produced — §5), and a scratch crate against the public `App` API to prove F-app-01 (three scenarios, output quoted in the finding).

## 1. Verdict

The app layer is a disciplined, single-writer fold with unusually good documentation and a table-driven invariant suite for the render side — the model/discovery axis split from the 2026-09-08 design is implemented exactly as specified and its regression test exists. The real weakness is that **wizard state is global, not scoped to an open wizard**: three C-event paths (`ConnectSucceeded`, `ConnectFailed`, `WizardAutoDismiss`) mutate it and the navigator with no check that the wizard is on the stack or that the event is about the wizard's device, and the firmware fires two of them for every connection, including boot auto-reconnect. That is a shipping-path UX defect (P1), reproduced here without hardware. Second: the headless emulator can inject only `NavIntent`, never an `Event`, so an agent cannot drive any screen past Devices — the "three run modes" claim is one-third true for the wizard, faults, volume and the device page. Third: `WizardPhase::NotResponding` (phase 5) is dead UI — no firmware producer for `ConnectRetrying` exists — and `Command::CancelConnect` has no C consumer, so "B genuinely aborts" (design §9) is not true on the device.

Counts: P0 0 · P1 2 · P2 7 · P3 3 (batched). Firmware-spike: **delete** (§4 F-app-10).

## 2. What is well done (do not touch)

- **Single-writer discipline in `fold.rs`.** `link_state` has one writer (`set_link_state`, fold.rs:250), `discovering` one (`set_discovering`, :278), `paired` exactly two events (`on_paired_device_upserted`/`_forgotten`, :178-196) with the "core never invents a row" rule enforced by construction (`ConnectSucceeded` queues `PersistDevice`, never appends). `record_connect_failure` routes through `set_link_state` rather than assigning the field (fold.rs:448-454). This is the right shape and the doc comments say why.
- **The axis split is real.** `LinkState` lost `Scanning` (model.rs:53-59); `DiscoveryStateChanged` writes one bool and clears nothing (fold.rs:278-284); the regression test `a_scan_while_connected_does_not_wipe_the_connected_model` (tests.rs:845) checks all four cleared fields on both edges, and `only_a_genuine_discovery_state_changed_ends_the_wizard_scan` (:906) closes the §1.2 trap from the design. Design and code agree.
- **`invariants.rs` is genuinely table-driven.** One `freshness_cases()` table (invariants.rs:64-225) feeds three every-screen proofs: dirty-gate freshness (10-minute static/live), damage-rect == full-render equivalence, and A-rail liveness == `focused_activation`. Adding a screen without a row fails loudly (`other => panic!` at :407). This is the mechanism that keeps the FFI dirty gate honest.
- **Identity-carrying refresh.** `ScreenCarry` + `with_selected_identity` (refresh.rs, devices.rs:187) mean an event mid-navigation never resets selection or scroll; `Refresh::Gone` unwinds a device page whose subject vanished from *either* route (mod.rs:283-299, test at tests.rs:712). `HomeView` is the first live-sync widget (`Refresh::Keep`, mod.rs:315) and its damage stays off the title bar (tests.rs:761).
- **Stored-echo rule for pickers.** `build_single_select_screen` has no place to put an optimistic check (picker.rs:52-85); `the_quality_pickers_check_follows_the_stored_echo_not_the_press` (device_page.rs:567) proves it end to end.
- **`ConnectFailureReason::retryable` is wired to the rail.** Non-retryable reasons render X `Inert` (wizard.rs:465-467) and the `ShortcutX` arm is guarded `if reason.retryable()` (wizard.rs:419) — the design §9 table's "B Back only" rows are honoured, and `06c/06d_*_back_only.png` pin it.
- **`decay_peak` / q16 math** (model.rs:289-344) — no_std-safe fixed-point with the overflow argument written down, and `decay_peak_eventually_reaches_zero_and_stays_there` covers `u64::MAX` elapsed.
- **Emulator surface parity** is proven pixel-for-pixel across headless PNG and minifb buffer, including `Off` (surface_parity.rs), and `idle_wake_headless_e2e.rs` drives the real `run()` loop through idle → Dim/Off → wake with the wake-press-is-dropped contract asserted.
- **C host tests** are honest about being model tests (each header says what is copied verbatim and from where) and every one passed first try.

## 3. Architecture assessment

**Reducer?** Mostly. `handle_event` (fold.rs:17-45) is an exhaustive match; every `Event` variant has a fold arm; screens read `&BtModel` or the `ModelHandle` and never write it. But it is not a *pure* fold: `on_connect_succeeded`, `record_connect_failure`, `on_wizard_auto_dismiss` write `wizard_phase`, `home_face`, and the navigator stack (fold.rs:141-146, 214-230, 462) — UI state and navigation are mutated as side effects of Bluetooth events with no scoping. That is the root of F-app-01.

**One source of truth?** For `link_state`, `codec`, `volume`, `fault_log`: yes. Two admitted mirrors exist: `wizard_devices` mirrors `model.discovered` (mod.rs:137-142, kept in lockstep at fold.rs:429/437, but the wizard's own `on_focus` clears only the mirror — wizard.rs:350) and `WizardPhase::{Connecting,NotResponding,Failed}` carry their own `addr`/`reason` alongside `BtModel::last_connect_failure` (ui_state.rs:14-20 argues this is fine; it is, until F-app-01's addr check needs to be written, at which point the phase's `addr` becomes load-bearing).

**Invariants.** `invariants.rs` is `#[cfg(test)]` (mod.rs:468-469) and covers render freshness/damage/rail only. There are **no runtime checks and no model-level invariants** anywhere. The docs imply at least: `connected_addr.is_some() ⇒ link_state == Connected` (model.rs:164-172); `connected_codec.is_some() ⇒ Connected`; `ldac_live_kbps.is_some() ⇒ connected_codec.word == "LDAC"`; `WizardPhase != NothingFound ⇒ wizard is on the stack` (implied by mod.rs:123-135). None is encoded; the first is violated by a legal event sequence (probe D below: `ConnectSucceeded` alone leaves `link_state=Idle, connected_addr=Some`), and the third by the existing test `ldac_live_kbps_is_never_snapped_to_the_nominal_ladder` (tests.rs:812) which folds `LdacBitrateChanged` into a fresh app with no codec. On the device the C ordering (`a2dp.c:3645-3646` pushes `link_state_connected` then `connect_succeeded`) hides the first; nothing hides the third.

**Screens thin?** Yes, with two structural exceptions. (1) `PairingWizardView` owns its phase transitions for input-driven edges and `App` owns the event-driven ones, through one shared `Rc` — deliberate (wizard.rs:12-27) and consistent. (2) `DisplaySettingsState` is a three-flag mailbox mutated from picker closures (settings.rs:79-87, 102-109); `handle_input` drains `refresh_pending` (mod.rs:376-382). Acceptable, but it is the third `Rc<RefCell<_>>` mailbox pattern (`commands`, `wizard_phase`, `wizard_devices`, `home_face`, `why_page_order`, `display_settings` = six on `App`) and the bgnd live-widgets migration (`Refresh::Keep`) is the designed way to retire most of them. No screen owns model state it shouldn't.

**Events vocabulary.** All 19 `PlEventTag` values decode (ui-ffi/src/lib.rs:1805-1823) and all 19 `Event` variants are folded. Producers: **`ConnectRetrying` has none** (`grep CONNECT_RETRYING firmware/src` → nothing), so phase 5 is unreachable on hardware (F-app-03). `NeedsPin` is produced (a2dp.c:2928). Unknown tag: counted in `malformed_tag_count` and dropped (lib.rs:1953-1958) — but **no C code reads `pl_ui_malformed_tag_count`**, so in practice it is a silent drop. Version mismatch: silent early return (lib.rs:1942). Unexpected-in-state: `ConnectStepChanged`/`ConnectRetrying` outside `Connecting|NotResponding` are dropped silently by design (fold.rs:108, 124); `ConnectFailed`/`ConnectSucceeded` are *not* gated at all (F-app-01).

**Commands.** All nine `Command` variants cross the FFI (lib.rs:2505-2595). C handles eight; **`PL_COMMAND_TAG_CANCEL_CONNECT` has no case** in `bt.c:1008-1167` (watchdog_sup.h:179 even annotates it "a tag with no case"). `events.rs:29-33` admits "plumbed-but-unhandled". Design §9 says "B genuinely aborts"; it does not.

**Emulator vs firmware loop.** The emulator runs `core::run::Runner::step` (run.rs:724); the firmware does **not** — `pl_ui_tick` (lib.rs:601-614) re-implements the per-frame policy call. They have already diverged: `pl_ui_tick` OR's `volume_wake_since_last_tick` into `had_input`; `Runner::step` has no volume-wake path. `pl_ui_tick` hardcodes `on_external_power = true`; `Runner` asks the platform. Display power is applied inside `Runner::step` but read back by C via `pl_ui_display_power`. Frame budget: emulator 33 ms (main.rs:47), firmware 16 ms (main.c:514). Input semantics match (both press-edge, no repeat: input.rs:63-73 vs input.c:114); the firmware never emits `JumpBy` (input.c:126) while `debug_remote.c` and the HTTP drive can.

**Design vs code disagreements, and who is right:**
- Design §9 phase 1 (instructions screen) vs code (no phase 1, wizard opens straight into `Scanning`, wizard.rs:63-66, bead 4vb.2). Code is right (the change is recorded); the design doc §9 and `wizard-screenshots/01_instructions.png` are stale.
- `ui_state.rs:35-36` says `NothingFound` is entered on "a C `LinkStateChanged(Idle)`"; fold.rs:68-77 says exactly the opposite (and is right).
- `refresh.rs:35-38`, devices.rs:150-152, device_page.rs:220-221, home.rs (ShortcutY arm), why_page.rs:229 all say "no builder returns `Refresh::Keep` yet"; mod.rs:315 returns it for Home. Code is right.
- `.planning/design/2026-09-24-app-rs-split.md` — the split was executed as planned; file sizes match the table within ~10%. S7 (comment trim) has not run: `events.rs`, `mod.rs` and `fold.rs` are still ~50% comment by line, with bead IDs throughout.

## 4. Findings

### F-app-01: Wizard phase and auto-dismiss are global — any connection (including boot auto-reconnect) mutates the wizard and the navigator with no check that the wizard is open or that the event is about its device
- Severity: P1   Confidence: High   Effort: M   Tier: Sonnet
- Location: core/src/app/fold.rs:141-146 (`on_connect_succeeded`), :214-230 (`on_wizard_auto_dismiss`), :446-464 (`record_connect_failure`); firmware/src/a2dp.c:3645-3660 (dismiss timer armed unconditionally on every `STREAM_ESTABLISHED`)
- Evidence:
  ```rust
  // fold.rs:144  — unconditional, no wizard-open check, no addr check
  *self.wizard_phase.borrow_mut() = WizardPhase::Succeeded { degraded };
  // fold.rs:215-217 — pops to root if the PHASE says Succeeded, not if the WIZARD is on the stack
  let should_pop = matches!(*self.wizard_phase.borrow(), WizardPhase::Succeeded { degraded: false });
  if should_pop { self.navigator.pop_to_root();
  // fold.rs:462 — "Unconditional (not gated on the wizard currently being open/mid-connect)"
  *self.wizard_phase.borrow_mut() = WizardPhase::Failed { addr, reason };
  ```
  Reproduced against the public API (scratch crate, no hardware):
  ```
  A: before success depth=3 title="Idle screen"          (user in Settings > Idle screen picker)
  A: after WizardAutoDismiss depth=1 title="Pico Link"    (yanked to Home, face reset to Status)
  B: depth=3 title="Pair headphones" cmd=Some(StartScan)  (user scanning for a NEW device)
  B: STATUS_ERROR px before=0 after=465                   (auto-reconnect Timeout for the OLD device flips the open scan to "No response")
  C: after manual B depth=2 title="Devices"
  C: after late WizardAutoDismiss depth=1 title="Pico Link" (B pressed within 2s of success still ends on Home)
  ```
  Sequence A is the firmware's own boot path: `StoreLoaded` → `Command::Connect` (fold.rs:162-172) → `a2dp.c:3646` `connect_succeeded` → `a2dp.c:3032/3044` 2 s timer → `WizardAutoDismiss`. Sequence B is the same boot path with a page timeout (BTstack's 5.12 s, a2dp.c:2918) landing after the user opened "Pair new headphones".
- Why it matters: on every power-up with a remembered device, the user has ~2-7 s during which navigating anywhere is undone by the app, and starting a scan for a second pair of headphones can be replaced by a failure screen for the first pair with a live "retry" X that reconnects to the *wrong* device (`wizard.rs:419-422` reissues `Connect` for the phase's `addr`, which is the auto-reconnect target). The fold.rs:455-461 comment argues the stray phase is "harmless" because nothing renders it — that is only true while the wizard is closed.
- Fix sketch: (1) give the wizard a `ScreenId::Wizard` (or compare `navigator.current().title == WIZARD_TITLE`, which `wizard.rs:54-58` already sanctions) and make `on_connect_succeeded`/`record_connect_failure` write `wizard_phase` only when the wizard is on the stack **and** the event's `addr` equals the phase's `addr` (`Connecting`/`NotResponding` carry it); (2) `on_wizard_auto_dismiss` pops only when the wizard is the top screen; (3) a successful non-wizard connect should still update the model (it does) and nothing else; (4) on manual B out of the wizard, reset `wizard_phase` to default so a late dismiss cannot act — wizard.rs's `(Back, _)` fall-through is the place.
- Verification: add to `core/src/app/tests.rs`: `boot_auto_reconnect_success_does_not_pop_a_screen_the_user_navigated_to` (depth stays 3), `a_connect_failure_for_a_different_device_does_not_change_an_open_wizards_phase` (phase stays `Scanning`), `manual_back_from_wizard_success_then_late_auto_dismiss_stays_on_devices` (depth stays 2). The three probe sequences above are the test bodies.
- Related: design 2026-08-28-on-device-ui §9 phase 6; bead pico-link-4vb.2 (bug 3, arms the timer); pico-link-l4d (manual-B policy).

### F-app-02: Headless mode cannot inject `Event`s, so an agent can reach only Home/Devices/Settings — the wizard past Scanning, the fault strip, volume, OUT meter, the device page and the LDAC picker are unreachable over HTTP
- Severity: P1   Confidence: High   Effort: M   Tier: Sonnet
- Location: emulator/src/desktop/http_server.rs:83-88 (three endpoints), :111-123 (`/api/input` accepts `NavIntent` only); emulator/src/main.rs (no `handle_event` anywhere in `emulator/src` — `grep handle_event emulator/src` is empty)
- Evidence: every screen state past Devices in `freshness_cases()` (invariants.rs:78-189) is built by `app.handle_event(...)`; the emulator has no route to that method. The screenshot fixtures that cover those screens are produced by in-process examples (`core/examples/{wizard,home,devices,ldac_quality,device_page}_screenshots.rs`), not by driving the emulator. `README.md:41-44` and `CLAUDE.md` ("three run modes … headless (no window; AI drives it and inspects captured screenshots)") claim more than the drive exposes.
- Why it matters: the project's stated testability story is that Tess can prove UI work headless without hardware; today that is true for ~3 of 14 screen states. Every wizard/fault/volume bead has had to be verified by hand-written examples instead, which is why `wizard-screenshots/` etc. exist as one-off PNG dumps rather than as a drive.
- Fix sketch: derive `Serialize, Deserialize` on `Event` and its payload types (they are already platform-free; `serde` is a `core` dependency for `NavIntent`), add `POST /api/event` that enqueues into a second `Arc<Mutex<VecDeque<Event>>>` the headless loop drains before `input.poll()` (a tiny `EventSource` alongside `InputSource`, or fold it into `HttpInput`). Also `GET /api/state` returning `navigator_depth`, `current_screen_title`, `wizard_phase` would let an agent assert without pixel-peeking.
- Verification: new `emulator/tests/headless_event_drive.rs`: POST `DeviceDiscovered` → `Select` → `ConnectSucceeded` over HTTP, then `GET /api/screenshot` shows `STATUS_SUCCESS` ink; plus a probe that walks every `freshness_cases()` state over HTTP.
- Related: CLAUDE.md "Three run modes"; roadmap "Live risks: No automated input path on the real target" (the emulator half of the same gap).

### F-app-03: Phase 5 (`NotResponding`, "Still trying (N)") is dead UI — no firmware producer for `ConnectRetrying` exists; and `CancelConnect` has no C consumer, so B during Connecting does not abort
- Severity: P2   Confidence: High   Effort: M   Tier: Opus (C side touches a2dp.c's connect state machine)
- Location: core/src/app/events.rs:227-232 (`ConnectRetrying`), :29-33 (`CancelConnect` "plumbed-but-unhandled"); core/src/render/wizard.rs:392-395 (B queues `CancelConnect`), :414-417 (X on NotResponding); firmware/src/bt.c:1008-1167 (no `PL_COMMAND_TAG_CANCEL_CONNECT` case); firmware/src/watchdog_sup.h:179
- Evidence: `grep -rn CONNECT_RETRYING firmware/src` → no matches; `grep -rn CANCEL_CONNECT firmware/src` → only `watchdog_sup.h:179 // a tag with no case (e.g. CANCEL_CONNECT=4)`. The wizard fixtures `05_not_responding_still_trying_2.png` and tests (`wizard.rs:901-904`) exercise a phase the device can never enter. The only C-side retry is `PL_A2DP_RECONNECT_RETRY_DELAY_MS` for the 0x0b "ACL already exists" case (a2dp.c:2990-3010), which does not push `ConnectRetrying`.
- Why it matters: design §9 phase 5 exists to keep a 5.12 s page timeout from being a dead screen; on the device the user sees "Connecting… 1 Connecting" with a ticking elapsed counter for the full timeout then "No response". Worse, B during that window pops the wizard but C keeps going and later delivers `ConnectSucceeded` for the abandoned attempt — which F-app-01 then turns into a pop-to-root. The "can the model get stuck" answer is: the *model* cannot (every phase has an exit), but the *device* can be in `Connecting` with no way to abort for ~5 s and then surprises the user afterwards.
- Fix sketch: either implement both on the C side (a `pl_bt_cancel_connect` that calls `a2dp_source_disconnect`/`gap_disconnect` on the in-flight ACL and pushes `ConnectFailed{Rejected}`-equivalent or a new `ConnectCancelled`; a `ConnectRetrying` push from the page-timeout path when a retry is armed) or delete phase 5 from `WizardPhase` and the design until it has a producer, so the tests stop proving a fiction.
- Verification: on hardware (unverifiable here) — B during Connecting must produce no later `ConnectSucceeded`; on host, after the C change, a `test_*.c` model test of the cancel state transition; in core, `wizard.rs:901` stays as the consumer-side proof.
- Related: .planning/design/2026-08-30-cancel-connect.md; design §9 phases 4-5.

### F-app-04: Model invariants exist only in prose; `BtModel` can hold `connected_addr` with `link_state == Idle`, and `ldac_live_kbps` with no codec
- Severity: P2   Confidence: High   Effort: S   Tier: Sonnet
- Location: core/src/app/model.rs:164-172, 212-225 (the documented lifecycles); core/src/app/fold.rs:141-146 (`on_connect_succeeded` sets `connected_addr` but not `link_state`), :322-325 (`on_ldac_bitrate_changed` unconditional); core/src/app/invariants.rs (render-only, `#[cfg(test)]`)
- Evidence: probe `D: link_state=Idle connected_addr=Some([1,1,1,1,1,1])` after a lone `ConnectSucceeded`; `tests.rs:812-819` folds `LdacBitrateChanged` into a fresh app and asserts `Some(703)` — a state the docs say cannot exist. `build_devices_screen` (devices.rs:81,95) and `device_page_quality_present` (device_page.rs:59-63) key "Connected" off `connected_addr`, not `link_state`, so the two can disagree on screen.
- Why it matters: today the C event order masks it; the next C refactor (or the F-app-01 fix reordering things) will not be caught by any test. `invariants.rs` is named as if it were the place, and it is not.
- Fix sketch: add `BtModel::debug_check(&self)` asserting the four implications above; call it at the end of `handle_event` under `#[cfg(any(test, debug_assertions))]` (never `debug_assert!` alone — mod.rs:69-73 already explains the NDEBUG trap); make `on_connect_succeeded` set `link_state = Connected` (or assert it already is) and `on_ldac_bitrate_changed` ignore a reading when `connected_codec` is not LDAC. Fix the test at :812 to connect first.
- Verification: a table test in `invariants.rs` folding every `Event` variant in every order over a small alphabet (a 3-event permutation sweep is ~7k cases and runs in ms) and calling `debug_check` after each.
- Related: design 2026-09-08 §2.1 ("correct by construction rather than by convention" — this is the missing construction).

### F-app-05: `wizard_devices` is a second copy of `model.discovered` that the wizard clears on its own
- Severity: P2   Confidence: High   Effort: M   Tier: Sonnet
- Location: core/src/app/mod.rs:137-142; fold.rs:426-430, 437; core/src/render/wizard.rs:345-355 (`on_focus` NothingFound clears `self.devices` only)
- Evidence: `self.devices.borrow_mut().clear();` (wizard.rs:350) with the comment "avoids a stale-row flash … between this press and that event arriving" — the model still holds the stale rows until `DevicesCleared` lands, so for that window `model.discovered != wizard_devices`.
- Why it matters: harmless today; it is the pattern that becomes a bug when a second reader of `discovered` appears (the design's own §18/§22 multi-device work). `HomeView` already shows the correct shape: hold the `ModelHandle` and read live (home.rs:447-457).
- Fix sketch: bgnd M4 as planned — `PairingWizardView` takes the `ModelHandle`, `sync_list` diffs against `model.borrow().discovered`, and `wizard_devices` is deleted from `App`.
- Verification: `grep -n wizard_devices core/src` empty; existing wizard tests unchanged.
- Related: bead pico-link-bgnd M4; design 2026-09-24-app-rs-split "bgnd live-widgets fit".

### F-app-06: The firmware does not run `Runner::step`; `pl_ui_tick` re-implements it and the two have already diverged
- Severity: P2   Confidence: High   Effort: M   Tier: Opus (FFI ownership)
- Location: core/src/run.rs:724-790 (`Runner::step`); ui-ffi/src/lib.rs:601-614 (`pl_ui_tick`); emulator/src/main.rs:47 (33 ms) vs firmware/src/main.c:514 (16 ms)
- Evidence:
  ```rust
  // lib.rs:608-612
  let had_input = core::mem::take(&mut ui.input_since_last_tick) || core::mem::take(&mut ui.volume_wake_since_last_tick);
  let _decision = ui.idle.tick(now, had_input, ui.app.is_at_home_root(), true, mute_or_zero);
  // run.rs:729-755: had_input = !intents.is_empty(); on_external_power from platform; no volume-wake path
  ```
  The `lib.rs:587` doc even says "firmware never runs `pico_link_core::run::Runner`".
- Why it matters: the emulator is sold as the fidelity model for the device (CLAUDE.md, roadmap "crown jewel"). Every idle/wake/fault-wake rule is now tested twice (`run.rs` tests and `lib.rs:2798+` tests) and a rule added on one side (volume wake, fault wake) can be missed on the other. Nothing structural prevents the next divergence.
- Fix sketch: extract the per-frame policy into one platform-free function `fn frame_policy(app, idle, now, had_input, volume_wake, on_external_power) -> FrameDecision` in `run.rs`; `Runner::step` and `pl_ui_tick` both call it; `Runner` gains the volume-wake input so the emulator matches. Frame budget: make the emulator's 16 ms to match, or document why 33.
- Verification: a shared test module over `frame_policy` run from both crates; `grep -c "idle.tick(" core/src/run.rs ui-ffi/src/lib.rs` → one call site each.
- Related: .planning/design/2026-09-01-idle-policy-across-the-ffi-seam.md (D9); 2026-09-02-dirty-gate-across-the-ffi-seam.md.

### F-app-07: None of the last three hardware bugs has a host test, and two of them could
- Severity: P2   Confidence: High   Effort: M   Tier: Sonnet
- Location: firmware/src/usb_audio.c:456-490 (`pl_usb_audio_feedback_task`, PI); firmware/src/a2dp.c:245 (`PL_A2DP_MAX_ENCODE_DWELL_US`); .planning/design/2026-09-23-on-arm-ring-collapse.md (Fix B: `samples_owed -= min(owed, dropped)`)
- Evidence: `progress.md:1-25` names dwell (6000 → 10000 µs), the P-only feedback controller ("cannot null a constant offset … fill_ema 5664 → 1764"), and the ring collapse. `firmware/tests/` has none of: a feedback-loop test, a dwell-budget test, a resync/owed test. `pl_usb_audio_feedback_task` is pure over `pl_pcm_fill_bytes()`/`pl_pcm_target_fill_bytes()` → `tud_audio_fb_set()` (lines above), i.e. exactly the "copy verbatim, stub the two calls" shape every existing `test_*.c` uses.
- Why it matters: the PI fix is "unproven by ear"; a host test would have shown the P-controller's non-zero steady-state error before the board did, and would now pin the Ki/anti-windup values against regression. The ring-collapse Fix B is an arithmetic identity ("dropped audio is never also owed") with no test.
- Fix sketch — three concrete tests:
  1. `firmware/tests/test_usb_feedback_pi.c`: simulate a ring with a constant +220 ppm producer/consumer mismatch, run the copied task at 1 kHz for 30 s, assert `|fill_ema − target| < 64 bytes` at the end and `fb_rail_ticks == 0`; then repeat with `PL_FB_KI_DIV` set huge (P-only) and assert the error does *not* converge — proving the test discriminates.
  2. `firmware/tests/test_a2dp_dwell_budget.c`: assert `PL_A2DP_MAX_ENCODE_DWELL_US >= frames_per_tick(990k) * PL_LDAC_FRAME_COST_US_MEASURED(1159) + margin` using the same `frames_per_packet` derivation `test_ldac_frames_per_packet.c` already copies.
  3. `firmware/tests/test_a2dp_resync_owed.c`: copy `pl_a2dp_resync_apply`'s arithmetic; assert `owed_after == owed_before − min(owed_before, dropped)` and never underflows.
- Verification: the three binaries pass; test 1's P-only variant fails.
- Related: beads pico-link-nxf, pico-link-rzqd, pico-link-zmg.

### F-app-08: No runner for the C host tests; one of them depends on a build artifact that is gitignored
- Severity: P2   Confidence: High   Effort: S   Tier: Haiku
- Location: firmware/tests/*.c (8 files, 8 different `cc` lines); firmware/tests/test_paired_device_upserted_ldac_quality_echo.c:46 (`-I firmware/include`); firmware/.gitignore:4 (`include/pico_link_ui.h` is generated by cbindgen in the CMake build)
- Evidence: 7/8 built and passed on the first attempt in < 1 s total once each header was read; the 8th failed with `fatal error: pico_link_ui.h: No such file` until I ran `cbindgen --config cbindgen.toml --crate ui-ffi --output /tmp/hostinc2/pico_link_ui.h` from `ui-ffi/`, after which it passed. `test_pcm_ring_cross_core.c` needs a hand-made `__dmb()` stub file first. Total time to figure out and run all 8: ~4 minutes, all of it reading headers. There is no `tools/run-host-tests.sh`, no CMake host target, no CI.
- Why it matters: 8 tests nobody runs are 8 tests that will rot; the paired-device echo test already cannot run from a fresh checkout without knowing about cbindgen.
- Fix sketch: `tools/run-host-tests.sh` that (a) generates the header into a temp dir, (b) writes the `__dmb` stub, (c) compiles and runs each test, (d) prints a pass/fail table and exits non-zero on any failure. Mention it in each test header instead of the per-file `cc` line.
- Verification: `tools/run-host-tests.sh` → `8 passed, 0 failed` from a fresh clone with only `cc` and `cbindgen`.

### F-app-09: `usb-console` docs and tools cover a third of `debug_remote.c`'s protocol, and carry pre-TinyUSB text
- Severity: P2   Confidence: High   Effort: S   Tier: Haiku
- Location: tools/usb-console/cdc_sender.py:17-33 (docstring protocol list), :96-106 (`_SHORTHAND`); tools/usb-console/README.md:130-170; firmware/src/debug_remote.c:190-330; tools/usb-console/README.md ("assumes the pico-sdk default `0x2e8a:0x000a`" vs `cdc_reader.py:70` `DEFAULT_PID = 0x000C`); tools/usb-console/tty_fallback.py:26-28 ("embassy-usb style firmware's `wait_connection()`")
- Evidence: `debug_remote.c` accepts `SKIPTICKS n`, `MEDIA PLAYPAUSE|NEXT|PREV`, `VOL GET|WATCH|SET n|FUSET n|INT|HOSTUP n|HOSTDOWN n`, `TRIM POLICY GET|LOW|STABLE|<hold> <band>`; none appear in the sender's docstring, `_SHORTHAND`, or the README — reachable only via `--raw`, which a new agent would not know to use. Parsing on the C side is robust (32-byte line cap with resync at :319-324, CRLF tolerance, per-poll byte budget, argument range clamps); the tools are the weak half. Error handling in `cdc_sender.py` is adequate (claim/release in `try/finally`, tolerant of the post-`BOOTSEL` disappearance). `safe_capture.sh`/`detach.py` are documented and measured. No dead tool: `tty_fallback.py` is a deliberate, labelled fallback.
- Why it matters: the debug channel is the only automation path to the real target; undocumented verbs get rediscovered by reading C.
- Fix sketch: one protocol table in the README generated from (or at least cross-checked against) `debug_remote.c`; add `--skipticks`, `--vol`, `--trim` (or a generic `--cmd "VOL SET 64"` alias for `--raw`) to the sender; fix the stale PID sentence and the embassy reference.
- Verification: every `strcmp/strncmp` literal in `debug_remote.c:190-330` appears in README.md (`grep -o '"[A-Z][A-Z ]*"' firmware/src/debug_remote.c | sort -u` vs the README).

### F-app-10: `firmware-spike/` — delete
- Severity: P2   Confidence: High   Effort: S   Tier: Haiku
- Location: firmware-spike/ (14 tracked files, ~1.1k lines: embassy-rp/embassy-usb Rust-owns-`main()` spike + a BTstack linking probe)
- Evidence: own `[workspace]` root (Cargo.toml:1) so it is outside the main workspace; nothing in the live build references it (`grep -rn firmware-spike` outside the dir → only `firmware/src/st7789.{c,h}` provenance comments, two ADRs, and `progress.md`); last touched 2026-09-06 by a docs-only commit (`004bf75`); it cannot be built here (`cargo check --offline` → `no matching package named cortex-m-rt`; the embassy stack it pins is not in the registry cache); the roadmap's "Settled decisions" retire both things it proved (Rust-owns-main, embassy-usb console). Nothing in it is not elsewhere: the ST7789 init sequence was ported to `st7789.c` "verbatim in behaviour", and the BOOTSEL-over-vendor-control-transfer trick it contains is exactly the one CLAUDE.md says STALLs and must not be used.
- Why it matters: 1.1k lines of a superseded architecture with a different toolchain pin, a different USB stack and a different HAL, sitting beside the live firmware, is a trap for a cheap model doing a grep-driven change (e.g. "find the ST7789 init" returns two answers).
- Fix sketch: `git rm -r firmware-spike`; change the two provenance comments in `st7789.{c,h}` and the two ADR line-number references to `git show 004bf75:firmware-spike/src/main.rs:285-410`; add one line to `progress.md`.
- Verification: `git grep -n firmware-spike` returns only historical references that name a commit hash; `cargo test --workspace` still 578.
- Related: ADRs 2026-08-27-c-first-pico-sdk-owns-main.md, 2026-08-27-usb-device-stack-returns-to-tinyusb.md.

### F-app-11: `Cargo.lock` / workspace metadata — one leftover, otherwise fine
- Severity: P3   Confidence: High   Effort: S   Tier: Haiku
- Location: Cargo.lock (committed, 116 packages); core/src/render/theme.rs:245; core/src/input.rs:16-20
- Evidence: the lock is committed and consistent with `rust-toolchain.toml` (`stable`). Of 116 packages, ~40 are minifb's platform tail (sdl2, wayland-*, x11-dl, orbclient, wasm-bindgen/web-sys, winapi) — expected and host-only; `zmij` (a serde_json float formatter, pulled by serde_json 1.0.14x) is the only unfamiliar name and is legitimate. Predecessor names: `theme.rs:245` keeps a "Bitwarden-hardware-key leftover with no consumer"; `input.rs:16-20` still says the module is "a placeholder module seam only: nothing in the core wires this up to a real product screen yet" — false since the wizard. No `bhk`/`credential`/`passkey` in code, fixtures or Cargo metadata.
- Fix sketch: delete the theme.rs constant; rewrite the input.rs paragraph.
- Verification: `git grep -in "bitwarden\|placeholder module seam" core emulator ui-ffi` → empty.

### F-app-12: Stale comments that a cheap model will act on (batch)
- Severity: P3   Confidence: High   Effort: S   Tier: Haiku
- Location: core/src/app/refresh.rs:35-38 (`Keep` "No builder returns this yet" + `#[allow(dead_code)]`), core/src/app/screens/devices.rs:150-152, core/src/app/screens/device_page.rs:220-221, core/src/render/home.rs (ShortcutY arm, "no builder does yet"), core/src/app/screens/why_page.rs:229; core/src/app/ui_state.rs:35-36 (`NothingFound` "a C `LinkStateChanged(Idle)` event"); emulator/src/desktop/http_server.rs:112-113 (`"Next"`, `"Prev"`, `"Activate"`, `{"NextN":5}` — the real names are `Up`/`Down`/`Select`/`JumpBy`); emulator/src/platform/input.rs:4,29,139 and headless_surface.rs:205 (`SyncServer`, `desktop::input::DesktopInput` — neither exists); README.md:14-16 ("Status: early … Firmware integration is in progress" — LDAC streams); AGENTS.md:15-38 ("Work is NOT complete until `git push` succeeds … NEVER stop before pushing") contradicts CLAUDE.md's Conservative profile ("Do not run git commits, git pushes … unless explicitly asked") and CLAUDE.md's "Ask … a push"; .planning/design/2026-08-28-on-device-ui.md §9 phase 1 and `wizard-screenshots/01_instructions.png` (phase removed by 4vb.2).
- Fix sketch: one comment-only commit; delete `01_instructions.png`; pick one push policy in AGENTS.md (CLAUDE.md's).
- Verification: `grep -rn "No builder returns\|no builder does yet\|NextN\|SyncServer\|DesktopInput" core emulator` → empty.

### F-app-13: Clippy (4 distinct warnings, not 7) and two HTTP-server robustness nits
- Severity: P3   Confidence: High   Effort: S   Tier: Haiku
- Location: see list
- Evidence / judgement:
  1. `core/src/power.rs:118` `derivable_impls` on `impl Default for ScreensaverMode` — **fix** (`#[derive(Default)]` + `#[default] Off`).
  2. `ui-ffi/src/lib.rs:262` `pl_ui_panic_hook` never used — **allow-with-reason**: it is the declared C-side panic sink; either wire it into the panic handler (the intent per its doc) or `#[allow(dead_code)]` with the reason. Wiring is the better fix and is Sonnet-tier.
  3. `core/src/run.rs:1245` `type_complexity` (test helper `Rc<RefCell<Vec<(String, Vec<u8>)>>>`) — **fix** with a `type StorageLog = …` alias.
  4. `core/src/run.rs:1299` same — same alias.
  The charter's "7" counts the per-target summary lines and the "1 duplicate"; distinct sites are 4.
  HTTP: `http_server.rs:118` `serde_json::from_reader(...)?` on a malformed `/api/input` body returns from `handle_request` **before** responding, so the client hangs until its own timeout (the loop just prints the error). Respond 400 instead. `main.rs:73` binds a hard-coded `127.0.0.1:8080` (tests correctly use `:0`); a second emulator instance panics at startup — add `--port`.
- Verification: `cargo clippy --workspace --all-targets` → 0 warnings; a test posting `{` to `/api/input` gets a 400 within 1 s.

## 5. Test coverage

**Counts (measured):** Rust 578 passed / 0 failed (core 478, ui-ffi 58, emulator 27+2+3+3, core integration 1+1+5). C host tests 8/8 pass (`test_a2dp_priming_cushion_frames_per_packet`, `test_a2dp_tx_ring_count`, `test_codec_id_stability` (+`stub_codec_rows.c`, links real `codec_table.c`), `test_fault_evaluator`, `test_ldac_abr_controller`, `test_ldac_frames_per_packet`, `test_paired_device_upserted_ldac_quality_echo` (needs the cbindgen header), `test_pcm_ring_cross_core` (links real `pcm_ring.c`, needs a `__dmb` stub)). Assertion counts per file range 1-25. Time to work out how to run all eight: ~4 minutes, entirely spent reading headers — see F-app-08.

**Covered well:** fold semantics for every event; the axis split; selection/scroll carry across refresh; `Refresh::Gone` unwinding; picker stored-echo; wizard phase transitions from both input and events (wizard.rs:600-1145); every-screen freshness/damage/rail (invariants.rs); volume percent endpoints and `wakes_idle`; fault log assign-not-accumulate; `decay_peak`; headless/windowed pixel parity incl. Off; idle→Dim/Off→wake e2e through the real loop; HTTP drive for `NavIntent`; FFI decode of every tag and every command (ui-ffi tests); home PNG fixtures byte-compared against a fresh render (`home_screenshot_fixtures.rs`).

**Fixtures — what is pinned and is it useful:** `home-screenshots/` (10 PNG) are *load-bearing*: `core/tests/home_screenshot_fixtures.rs` fails if a fresh render differs, with a regeneration recipe in the message. `wizard-screenshots/` (19 PNG) and `fixtures/{devices,fault-strip}` (8+11 PNG) are *evidence only* — nothing compares against them (`grep -rn "wizard-screenshots\|fixtures/" core/tests` → only the home test). They were regenerated 2026-08-31 / 09-02 / 09-08 and `01_instructions.png` shows a phase that no longer exists. Either promote them to compared fixtures (the home test's `home_fixtures.rs` support module is the template) or move them under `.research/` so they stop looking like tests.

**Not covered:**
- **C firmware modules with no host test at all:** `bt.c` (only one copied helper), `main.c`, `persist.c` (CRC/slot/eviction logic is pure and untested), `volume.c` (the loop-breaking rule, pure over a few statics — testable), `usb_audio.c` (PI feedback — F-app-07), `usb_pump.c`, `usb_descriptors.c`, `usb_reset.c`, `media_keys.c`, `debug_remote.c` (`parse_line`/`parse_connect_addr`/`parse_skip_ticks` are pure and trivially testable), `input.c` (debounce is pure over a sampled bool), `st7789.c`, `flash_lockout.c`, `panic_recorder.c`, `pl_log_ring.c`, `pl_loop_prof.c`, `pl_prio.c`, `watchdog_sup.c`, `codec_sbc.c`, `codec_ldac.c` (beyond frames-per-packet), `ldac_bench.c`. Cheapest wins for a Sonnet: `debug_remote.c` parsers, `volume.c` loop rule, `persist.c` CRC16/slot selection, `input.c` debounce.
- **The FFI boundary from the C side:** every ui-ffi test is Rust calling its own `extern "C"` functions. No C compilation unit is ever compiled against `pico_link_ui.h` on host (the one test that includes it copies a helper). A host C test that links `libui_ffi` (host build of the staticlib) and drives `pl_ui_create/push_event/tick/render/poll_command` would catch struct-layout and lifetime mistakes the Rust-side tests structurally cannot.
- **Emulator/device parity:** none beyond the shared `IdlePolicy`; F-app-06.
- **The last three hardware bugs:** F-app-07.
- **Wizard phase scoping:** F-app-01 (three named tests).
- **Model invariants:** F-app-04.

**What a cheap model could add today (Haiku/Sonnet, no design needed):** the three F-app-01 regression tests; the F-app-04 `debug_check` + permutation sweep; `firmware/tests/test_debug_remote_parsers.c`; `tools/run-host-tests.sh`; deleting `01_instructions.png`; the clippy fixes.

## 6. Open questions for Andreas

1. **Phase 5 ("Not responding / Still trying")** — implement the C producer (`ConnectRetrying` on page-timeout retry) or cut the phase from the design and the code? Recommendation: cut it until the C retry policy exists; today it is fixtures and tests for a screen the device cannot show. (F-app-03)
2. **`CancelConnect`** — is a real ACL/AVDTP abort on B during Connecting in scope for the MVP? Recommendation: yes, because without it B leaves a connect running that later pops the user to Home (F-app-01 × F-app-03 compound).
3. **Boot auto-reconnect UX** — when the remembered device connects at boot, should Home *ever* be forced (the current behaviour, by accident), or only the hero updated? Recommendation: never navigate on a non-wizard connect. (F-app-01)
4. **`firmware-spike/`** — delete now, or keep until the ST7789 provenance comments are rewritten? Recommendation: delete in the same commit as the comment rewrite. (F-app-10)
5. **AGENTS.md vs CLAUDE.md push policy** — which one wins? (F-app-12)

## 7. Unverifiable here

- That B during phase 4 really leaves the connect running on the board and that a later `ConnectSucceeded` then pops to Home (F-app-01/03 compound) — inferred from `bt.c` having no `CANCEL_CONNECT` case and from the probe; needs a board and `cdc_sender.py`.
- Whether the boot auto-reconnect window is long enough for a user to reach Settings (probe A) — the 2 s dismiss timer is certain (a2dp.c:220); the connect duration is BTstack-dependent.
- Whether `PL_A2DP_MAX_ENCODE_DWELL_US = 10000` is still the right value under `PL_ENCODER_ON_CORE1=ON` — the dwell budget test in F-app-07 pins the arithmetic, not the measured frame cost.
- Firmware cross-compile after any of the fixes above (no ARM toolchain here).
