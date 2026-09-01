# Idle policy across the FFI seam — and the two-superloop divergence

**Bead:** pico-link-i3e (P1, screensaver does nothing on hardware)
**Author:** Ada (architect), 2026-09-01
**Status:** proposed — design of record for the fix
**Related:** pico-link-4vb.3 (the screensaver, merged 4bedb30), pico-link-3uq
(unmerged blit split), pico-link-p1r (loop profiling), pico-link-4vb.7 (bt.c,
in flight), ADR 2026-08-27 (C-first, pico-sdk owns `main()`)

---

## 1. What is actually true today

Established by the orchestrator's ROOT CAUSE comment on the bead and re-verified
here by reading the code. Not re-derived.

- `core/src/run.rs` contains `run()` (host driver loop) and `Runner::step()`
  (one iteration: input poll → idle/deep-sleep tiers → dirty-gated render+flush).
  `Runner::step`'s own doc comment already names the intended second caller:
  *"a caller with its own timing source -- e.g. C owning the clock over FFI"*.
  **That caller was never written.**
- `ui-ffi/src/lib.rs` never mentions `Runner`, `run`, or `DisplayPower`.
  `PlUi` is `{ app: App, malformed_tag_count: u32 }` — no idle state at all.
- `firmware/src/main.c`'s superloop calls `pl_ui_input` → `pl_ui_tick` →
  `pl_ui_render` → `st7789_blit_framebuffer` unconditionally, every iteration,
  forever. It has no idle concept and no post-init backlight control.
- `firmware/src/st7789.c` drives `ST7789_PIN_BL` (GP13) exactly twice, both at
  init: `gpio_put(BL, 0)` in `st7789_init` (line 65) and `gpio_put(BL, 1)` at the
  end of `st7789_init_and_fill` (line 184). There is no `st7789_set_backlight`.

So the screensaver is real, tested, and unreachable from the shipping binary.

---

## 2. The narrow question: how does `core` tell the firmware to blank?

**Answer: none of the three offered options. A pull-based *level* on the FFI
surface — `pl_ui_display_power(ui)` — read by `main.c` every iteration and
applied idempotently to GP13.**

### 2.1 Why not a `DisplaySurface` trait method

`DisplaySurface::set_power` already exists and is already the emulator's seam;
it stays. It is unreachable from firmware for a structural reason: **the firmware
is not a `Platform` implementor and never will be under C-first.** Making
`set_power` the firmware's mechanism means implementing `DisplaySurface`,
`InputSource`, `Clock`, `Storage` and `PowerControl` in Rust as thin shims that
call back into C — five traits, ~a dozen `extern "C"` callbacks — so that `core`
can push one bit. That inverts the call direction the whole seam is built on
(`ui-ffi/src/lib.rs` module doc: *"C calls Rust; Rust calls nothing back except
`pl_ui_panic_hook`"*) to deliver something C can simply read.

### 2.2 Why not a `PlCommand`

`PlCommandTag` is an **edge** queue, drained by `pl_bt_poll_commands(ui)` in
`bt.c` — semantically the Bluetooth command channel, and about to be touched by
pico-link-4vb.7. Display power is a **level**. An edge that is dropped,
reordered, or consumed by a handler that doesn't recognise the tag leaves the
backlight permanently wrong with no self-healing path; a level re-applied every
frame converges from any state, including after a watchdog reboot. Putting a
display concern in the BT command queue also blurs a seam that is currently
clean.

### 2.3 Why not a Rust→C callback

`pl_ui_panic_hook` is the only Rust→C call and exists because a panic genuinely
cannot be polled. A blank can. A callback would fire from inside `pl_ui_tick`
while `core` holds `&mut App`, adding a re-entrancy surface (C must not call any
`pl_ui_*` from the hook) for zero latency benefit — `main.c` cannot act on it any
earlier than the next line of the same iteration anyway.

### 2.4 The chosen shape

```c
/* GENERATED — cbindgen, from ui-ffi/src/lib.rs */
typedef enum PlDisplayPower { PL_DISPLAY_POWER_ON = 0, PL_DISPLAY_POWER_OFF = 1 } PlDisplayPower;

/* Current requested panel power. A LEVEL, not an edge: read it once per
 * superloop iteration after pl_ui_tick() and apply it idempotently. Safe to
 * call at any time; PL_DISPLAY_POWER_ON if `ui` is null. */
PlDisplayPower pl_ui_display_power(struct PlUi *ui);
```

Properties that make this the sustainable choice, not just the cheap one:

- Preserves the FFI direction rule exactly. No new callback, no new re-entrancy.
- Idempotent and self-healing. C may apply it every frame or only on change.
- Extends without reshaping: a future brightness level, or the "was this frame
  new" dirty signal (§5.1), is another pull-getter or a field on a returned
  frame-state struct. Nothing about this commits us against that.
- `firmware/include/pico_link_ui.h` must gain both `PlDisplayPower` and
  `pl_ui_display_power` via cbindgen (add `PlDisplayPower` to the `export.include`
  list in `ui-ffi/cbindgen.toml`). **If the generated header does not change, the
  change did not land.**

**No signature changes to `pl_ui_create` / `pl_ui_input` / `pl_ui_tick` /
`pl_ui_render` are required.** See §3.2 for why.

---

## 3. The minimal fix (ship this; do not wait on §4)

### 3.1 `core`: extract the idle decision from `Runner` into a Platform-free unit

New in `core/src/run.rs` (or `core/src/idle.rs`, either is fine — keep it in
`core`, keep it `Platform`-free):

```rust
pub struct IdlePolicy { /* power_state, last_input: Option<Instant>,
                           deep_sleep_triggered, idle_timeout, deep_sleep_timeout */ }

pub struct IdleDecision {
    /// Some(_) only on the frame the level CHANGES; callers that re-apply a
    /// level every frame can ignore this and read `display_power()` instead.
    pub power_transition: Option<DisplayPower>,
    pub enter_deep_sleep: bool,
}

impl IdlePolicy {
    pub fn new(idle_timeout: Option<Duration>, deep_sleep_timeout: Option<Duration>) -> Self;

    /// The wake half. Call when input has arrived, BEFORE forwarding it to
    /// `App`. Returns true if the caller must SWALLOW the intents (the
    /// wake-triggering input is never forwarded — see run.rs's module doc)
    /// and mark the app dirty.
    pub fn on_input(&mut self) -> bool;

    /// The arm half. Call once per frame with the frame's clock reading,
    /// whether input arrived this frame, and `App::is_at_home_root()`.
    pub fn tick(&mut self, now: Instant, had_input: bool, at_home_root: bool,
                on_external_power: bool) -> IdleDecision;

    pub fn display_power(&self) -> DisplayPower;
}
```

`last_input` is `Option<Instant>` and lazily initialised on the first `tick`.
This matters: `pl_ui_create` takes no clock, and seeding `last_input = 0` would
mean a UI created 61 s after boot blanks on its first frame.

**`Runner::step` is then refactored to call `IdlePolicy`** rather than keeping
its own copy of the tiers. That is the whole point — see §4. `Runner` keeps the
`DisplaySurface::set_power` calls and the `FlushErrorTracker`s; only the
*decision* moves. The existing `run.rs` tests should keep passing essentially
unchanged, which is the evidence the extraction was behaviour-preserving. Add
direct `IdlePolicy` unit tests too (they need no `Platform`, which is the point).

### 3.2 `ui-ffi`: `PlUi` owns an `IdlePolicy`

- `PlUi` gains `idle: IdlePolicy`, constructed in `pl_ui_create` with
  `Some(DEFAULT_IDLE_TIMEOUT)` and `None` for deep sleep (see §5.2).
- `pl_ui_input`: before forwarding, `if ui.idle.on_input() { ui.app.mark_dirty();
  /* swallow — do NOT call app.handle_input */ }` else forward as today. Also set
  an internal `input_since_last_tick` flag either way.
- `pl_ui_tick(now_us)`: `let d = ui.idle.tick(Instant::from_micros(now_us),
  take(input_since_last_tick), ui.app.is_at_home_root(), true /* see §5.2 */);`
  then `ui.app.tick(now_us)` as today. `d.power_transition` may be ignored by the
  FFI (C reads the level); `d.enter_deep_sleep` is ignored for now (§5.2).
- `pl_ui_display_power(ui)` returns `ui.idle.display_power()` mapped to
  `PlDisplayPower`.

This reproduces `Runner::step`'s exact semantics under the existing call
ordering (`pl_ui_input` … then `pl_ui_tick`), including the rule that the
wake-triggering input is swallowed and the frame after it resumes navigation.

*Known cosmetic wrinkle:* `main.c` calls `pl_ui_input` twice per frame (GPIO,
then `pl_debug_remote_poll` under `PL_DEBUG_REMOTE`). If both deliver intents in
the same frame while asleep, the first wakes and is swallowed and the second is
forwarded. Harmless, matches "wake, then act", and only reachable in debug
builds. Do not add machinery for it.

### 3.3 `firmware`: a backlight setter and one branch in the superloop

**`st7789.c/.h`** — new:

```c
/* Backlight only (GP13). Deliberately NOT DISPOFF/SLPIN: the backlight is a
 * plain GPIO, out of band from SPI1, so it cannot race an in-flight blit DMA
 * (pico-link-3uq) and needs no panel re-init on wake. Idempotent. */
void st7789_set_backlight(bool on);
```

**`main.c`** — after the existing `pl_ui_tick(ui, frame_start_us)` call:

```c
bool display_on = (pl_ui_display_power(ui) == PL_DISPLAY_POWER_ON);
st7789_set_backlight(display_on);
```

and gate the render+blit on `display_on`.

**Placement rule (this is the part that must be got right against
pico-link-3uq):** the `blit_wait` for the *previous* frame's DMA is
unconditional and must stay above the gate. Order within the iteration:

```
pl_ui_tick(...)
if (blit_pending) { st7789_blit_wait(spi1); blit_pending = false; }   /* 3uq — NEVER skipped */
display_on = (pl_ui_display_power(ui) == PL_DISPLAY_POWER_ON);
st7789_set_backlight(display_on);
if (display_on) {
    pl_ui_render(ui, &px, &px_len);
    if (px valid) { st7789_blit_start(...); blit_pending = true; }    /* or the blocking blit pre-3uq */
}
```

Skipping `blit_wait` while asleep would leave a DMA in flight over a
framebuffer Rust may later mutate — the exact no-tearing invariant 3uq moved
across an iteration boundary. Skipping *render+blit_start* is safe and is what
makes the blank actually take effect (and, as a bonus, removes the ~38 ms blit
from the idle path entirely).

If 3uq lands first, use the shape above verbatim. If it has not landed, the same
gate sits around the single blocking `st7789_blit_framebuffer` call and there is
no wait to preserve. **Do not couple the two beads; write the gate so it survives
either merge order.**

### 3.4 Acceptance — on hardware, not in a test

- `firmware/include/pico_link_ui.h` contains `pl_ui_display_power` and
  `PlDisplayPower` after a build. (Cheap pre-check; not sufficient.)
- Flash. Sit at Home root, depth 1, **state the depth in the report**. Photograph
  the panel at t=0 and t=75 s. Report the chroma delta. The panel must be dark.
- Press any button: the panel relights and the focused row does **not** activate.
- Navigate into the pairing wizard, wait 75 s: the panel stays lit.

---

## 4. The real question: two superloops

### 4.1 Recommendation

**Neither "firmware adopts `run.rs`" nor "`run.rs` is emulator-only, documented".
The third shape: demote `run.rs` from *the loop* to *a driver over
Platform-free policy units that both drivers share*.**

`run::run` stays the emulator's host driver. `main.c` stays the firmware's
driver, per ADR 2026-08-27. But every *decision* `run.rs` currently makes inline
— the idle tiers now, whatever comes next later — moves into a `Platform`-free
unit in `core` that `Runner::step` calls and `ui-ffi` also calls. One
implementation of each decision, two drivers.

### 4.2 Why not adopt `run.rs` in the firmware

This is the option that sounds most principled and is actually the expensive
wrong one.

- It contradicts ADR 2026-08-27 head-on. `run()` is an infinite loop; adopting it
  means Rust owns the schedule and C becomes a callback library.
- `main.c`'s loop body carries work `core` knows nothing about and should never
  know about: the X+Y BOOTSEL hold, `pl_bt_drain_events`, `pl_bt_poll_commands`,
  `pl_persist_service`, `pl_usb_pump_report`, `pl_a2dp_report`,
  `pl_a2dp_publish_counters`, `pl_loop_prof_*`, `pl_log_ring_drain`,
  `pl_wdt_mark`/`pl_wdt_service`. That is ~10 new Rust→C callbacks, each an FFI
  safety surface, bought to gain 3 decisions.
- `pl_wdt_service` has an explicit contract: fed exactly once, from thread
  context, in the superloop (`watchdog_sup.h`). Feeding it from inside a Rust
  loop that C cannot see is a real regression in a safety mechanism.
- The 3uq blit split is a two-phase, cross-iteration DMA contract.
  `DisplaySurface::flush(&FrameBuffer565) -> Result<(), E>` cannot express it.
  Adopting `run.rs` would force `flush` to grow a start/wait shape for one
  platform's benefit — a hardware constraint leaking into the platform-free
  core. That is precisely the kind of load-bearing quick fix `core/` exists to
  prevent.

### 4.3 Why not "`run.rs` is emulator-only, documented"

Honest, one-line cheap, and inadequate. `run.rs`'s module doc **already** claims
it is the mode-generic loop *"regardless of which run mode … a future real-board
target"*, and `Runner::step`'s doc **already** names the FFI as its intended
second caller. The documentation was not merely absent; it was present and
wrong, and nobody noticed for a whole feature. A comment is a static defence
against a dynamic problem: it does not stop the next `run.rs` feature from being
emulator-only, it only makes the next occurrence documented.

### 4.4 What the third shape buys, honestly

It does not give compiler enforcement — someone can still write new policy
inline in `Runner::step` and skip `IdlePolicy`. What it does give:

- **One implementation, so the FFI path is a call site, not a copy.** A future
  change to the tiers lands in both drivers or neither.
- **Policy becomes testable without a `Platform`,** which removes the incentive
  that produced the emulator-only implementation in the first place (`run.rs`
  tests were easy; an FFI-side test looked like work).
- **A cheap review rule** to add to `run.rs`'s module doc: *"`run()` is the host
  driver. Any behaviour here that is a decision rather than a host mechanism
  belongs in a `Platform`-free unit `ui-ffi` can also call — otherwise the
  firmware silently does not get it. See pico-link-i3e."*

Cost: one mechanical refactor of `Runner::step`, covered by existing tests. This
is cheap-and-right, not gold plating.

---

## 5. What else does `run.rs` implement that the firmware silently does not get

Checked exhaustively against `Runner::step`. Five items; one of them is
arguably more valuable than the screensaver.

### 5.1 The dirty gate — **the big one, file a bead**

`Runner::step` gates render+flush on `app.dirty()`. `main.c` blits every
iteration by explicit decision, and `pl_ui_render`'s doc comment says so
("skipping a redundant blit when nothing changed is C's own call to make") —
except there is no `pl_ui_dirty()` in the FFI, so C cannot make that call. The
blit is measured at **38.6 ms** (pico-link-14l) against a 16 ms frame budget.
On a completely static screen the firmware currently pays it 100% of the time.

That is very likely the largest single recoverable chunk of superloop time, and
it is directly adjacent to the pico-link-3uq / pico-link-p1r stall work. The fix
is small and fits this design exactly: add `bool pl_ui_dirty(struct PlUi *ui)`
(or fold it into a `PlFrameState` struct alongside `PlDisplayPower`), and gate
render+blit on it. `pl_ui_render`'s unconditional/idempotent contract does not
change — C just stops calling it when nothing changed.

Caveat for whoever takes it: `App::dirty` must be genuinely trustworthy for every
time-driven repaint source (the level meter, the fault strip, the wizard's
liveness indicators) before C is allowed to skip on it, or screens will freeze.
`App::tick` explicitly does *not* mark dirty. **Do not bundle this with the
screensaver fix** — it needs its own verification pass on hardware.

### 5.2 Deep sleep — inert today, but the firmware has no path to it at all

`Runner::step`'s tier 2 calls `PowerControl::enter_deep_sleep`, vetoed by
`on_external_power()`. The firmware has no `PowerControl` and no
external-power sense. `core::power::DEEP_SLEEP_ARMED` is `false`, so nothing is
broken *today* — but the const's doc comment says flipping it to `true` is "the
one-line change that arms it once that decision lands; nothing else … needs to
change", and **that is now false for the firmware.** The minimal fix passes
`None` for `deep_sleep_timeout` in `ui-ffi` and `true` for `on_external_power`
(the board is USB-powered by definition today), which keeps the tier
unreachable and honest. File a bead to correct the const's doc comment and to
plumb a real power source when the tier is armed.

### 5.3 `IdlePowerSetting` (the persisted enable/disable toggle) is unread on device

`core/src/power.rs`'s `IdlePowerSetting::load` is called only from
`emulator/src/main.rs`. `ui-ffi` never reads storage. So even once the
screensaver works on hardware, the persisted toggle will not. Acceptable for the
minimal fix — hardcode `DEFAULT_IDLE_TIMEOUT` — but it must be a bead, not an
assumption, because a Settings screen that appears to toggle a dead setting is
worse than no toggle.

### 5.4 Flush-error visibility

`FlushErrorTracker` gives rate-limited warnings on repeated `flush` /
`set_power` failures. `st7789_blit_framebuffer` returns `void`; there is no error
channel at all. Vacuous today, but it means an SPI fault on hardware is
completely silent — the "inexplicable frozen screen" the tracker was written to
prevent. Low priority; note it, do not act now.

### 5.5 Wake-input swallowing

Part of the screensaver (§3.2). Called out separately only because it is the
half most likely to be dropped in implementation, and its absence is exactly the
regression Tess's evidence 04/05 already tests for.

Not in scope / correctly divergent: the frame-budget sleep (the firmware
deliberately has its own deadline-based pacing, pico-link-tfj), and
`should_continue` (a host-only concept).

---

## 6. Task breakdown

1. **`core`** — extract `IdlePolicy`; refactor `Runner::step` to call it; add
   direct unit tests. Existing `run.rs` tests must pass unchanged. → Ruby
2. **`ui-ffi`** — `PlUi.idle`, wake/swallow in `pl_ui_input`, arm in
   `pl_ui_tick`, new `pl_ui_display_power` + `PlDisplayPower`; add
   `PlDisplayPower` to `cbindgen.toml`'s export list. → Ruby (same bead as 1)
3. **`firmware`** — `st7789_set_backlight`; superloop gate per §3.3's ordering.
   → Ruby (same bead)
4. **Verify on hardware** per §3.4, with the photograph and the chroma delta and
   the stated navigator depth. → Tess
5. **New beads, not this one:** the dirty gate (§5.1, P1-adjacent, own
   verification), deep-sleep plumbing + the wrong doc comment (§5.2), persisted
   `IdlePowerSetting` on device (§5.3), flush-error channel (§5.4).

**Dependencies:** none blocking. Independent of pico-link-3uq and
pico-link-4vb.7 by construction (§3.3 survives either merge order; nothing here
touches `bt.c`).

**Risks:**
- Merge-order coupling with 3uq if the gate is written against today's exact
  lines. Mitigated by §3.3's ordering rule.
- The `Runner::step` refactor is behaviour-preserving only if the existing tests
  really cover the tiers. If a test has to be *changed* to pass, that is a signal
  the extraction changed semantics — stop and report, do not adjust the test.
- Hardware acceptance can be silently satisfied from the wrong screen. The report
  must state the navigator depth, per the bead's own hypothesis (b).
