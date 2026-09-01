# The dirty gate across the FFI seam (bead `pico-link-vxc`)

**Author:** Ada (Architect) · **Date:** 2026-09-02 · **Status:** proposed, not implemented
**Beads:** `pico-link-vxc` (this) · adjacent: `pico-link-ary`, `pico-link-p1r`,
`pico-link-3uq` (unmerged), `pico-link-i3e` (merged), `pico-link-6wz` (merged),
`pico-link-14l`
**Depends on:** `.planning/decisions/2026-08-31-render-ctx-frame-scoped-clock.md`,
`.planning/design/2026-09-01-idle-policy-across-the-ffi-seam.md`

---

## 1. The problem, stated once

`core`'s `Runner::step` (`core/src/run.rs:528`) gates render+flush on
`app.dirty()`. `firmware/src/main.c:511-549` does not: it calls `pl_ui_render`
and `st7789_blit_framebuffer` on every iteration the display is lit.
`pl_ui_render`'s own doc comment (`ui-ffi/src/lib.rs:~636`) says *"skipping a
redundant blit when nothing changed is C's own call to make"* — but there is no
`pl_ui_dirty()` in the FFI, so **C cannot make the call the doc tells it to
make.** `main.c:693` even carries a comment acknowledging the gap.

Cost: the full-frame blit is 38.6ms (`pico-link-14l`), paid ~30 times a second
on a screen that is static almost always. That is the single largest known waste
in the superloop and a sufficient explanation for both `pico-link-ary` (frequent
small stutter absent on USBPods) and `pico-link-p1r` (loop collapsing to ~6Hz
under streaming load) without any subtler theory.

**Adding the function is trivial. The audit below is the deliverable**, because
the failure mode of getting it wrong is a screen that silently stops updating —
which on this device is indistinguishable from a crash and is a far worse bug
than a slow loop.

---

## 2. Audit: every repaint source in the tree

### 2.1 The mechanical result, first

There are **eight production `impl Widget` blocks** in the tree:

| Widget | File | Reads the clock? | `redraw_after` |
|---|---|---|---|
| `HomeView` | `render/home.rs:208` | no | default (`None`) |
| `HeroStatusView` | `render/hero.rs:291` | no | default |
| `MenuList` | `render/menu.rs:333` | no | default |
| `VerticalList` | `render/list.rs:765` | no | default |
| `MessageView` | `render/message.rs:117` | no | default |
| `ConfirmView` | `render/confirm.rs:112` | no | default |
| `DevicesListView` | `app.rs:829` | no | default |
| `PairingWizardView` | `render/wizard.rs:272` | **yes** (`wizard.rs:461`) | **`Some(250ms)` while `Connecting`** (`wizard.rs:423-428`) |

**How I know, and it is checkable rather than argued:** a widget's *only* access
to time is `RenderCtx` (the frame-scoped clock ADR made this the single seam —
no widget holds a `Clock`, and `core` has no other time source). Grepping
`ctx.now()` / `ctx.elapsed_since(` / `Instant` across `core/src/render/*.rs` and
`core/src/app.rs`, excluding `#[cfg(test)]` modules and `ctx.rs` itself, yields
**exactly one production hit: `wizard.rs:461`**, the Connecting phase's
elapsed-seconds readout. Every other widget's render output is a pure function
of state it was *built* from.

That is the whole safety argument in one line: **a widget that never reads `ctx`
cannot render differently without a state change, and every state change already
goes through a path that sets `dirty`.**

### 2.2 Screen-by-screen, against the brief's named suspects

| Screen / phase | What changes without input | Dirtied by | Verdict |
|---|---|---|---|
| Home, status face — codec word, device name, bitrate line | `Event::CodecChanged` → `set_connected_codec` → `rebuild_root` (`app.rs:1364`) | event | **correct**. Nothing on the hero is time-derived; the bitrate is `nominal_bitrate_bps` carried on the event, not a running average. Emitted once per A2DP configuration (`a2dp.c:1142`), not per media frame. |
| Home, status face — `NO LINK` on disconnect | `Event::LinkStateChanged` → `set_link_state` → `rebuild_root` (`app.rs:1354`) | event | **correct** |
| Home, menu face | selection only | `handle_input` (`app.rs:1562`) | **correct** |
| Devices list — paired rows appearing/renaming/reordering | `PairedDeviceUpserted`/`Forgotten` → `rebuild_root` | event | **correct** |
| Wizard phase 2 (Scanning) — list filling, RSSI updating | `Event::DeviceDiscovered` → `add_device` → `rebuild_root` (`app.rs:1381`) | event | **correct**. The scanning phase has no spinner or animation — it renders a `VerticalList`, so a scan that discovers nothing genuinely has nothing to repaint. |
| Wizard phase 2 → `NothingFound` | `on_scan_ended_if_applicable` sets `dirty` (`app.rs:1191`) | event | **correct** |
| Wizard phase 4 (Connecting) — **step progression** | `Event::ConnectStepChanged` → `on_connect_step_changed` sets `dirty` (`app.rs:1219`) | event | **correct** |
| Wizard phase 4 — **elapsed-seconds readout** | nothing; pure clock | `redraw_after` → `next_redraw_at` → `App::tick` (`app.rs:1442`) | **correct, and the only time-driven source in the product.** Covered end to end by `wizard.rs:630` (`connecting_phase_liveness_end_to_end_tick_alone_marks_dirty_and_the_elapsed_readout_changes`). |
| Wizard phase 5 (`NotResponding`) retry counter | `Event::ConnectRetrying` → `on_connect_retrying` sets `dirty` (`app.rs:1235`) | event | **correct** |
| Wizard phase 6 success / failure | `on_connect_succeeded` (`app.rs:1254`), `record_connect_failure` → `rebuild_root` | event | **correct** |
| Wizard auto-dismiss (pop to Home) | `on_wizard_auto_dismiss` sets `dirty` (`app.rs:1308`) | event | **correct** |
| Forget picker / forget confirm / device detail / settings | static content, selection only | `handle_input` | **correct** |
| Boot, first frame | `App::new` sets `dirty: true` (`app.rs:1053`) | construction | **correct** |
| Wake from the idle blank | `pl_ui_input`'s wake branch calls `app.mark_dirty()` (`ui-ffi/src/lib.rs:~528`); `Runner::step` does the same (`run.rs:490`) | explicit | **correct**, and this is exactly why the gate composes with `pico-link-i3e` (§4). |

### 2.3 Defects found (both latent today, both cheap and right to fix now)

**D1 — `on_store_loaded` mutates model state without dirtying.**
`app.rs:on_store_loaded` sets `self.model.store_status` and queues a `Connect`
command, but never sets `dirty` and never calls `rebuild_root`. Harmless
*today*: grep confirms `store_status` is read by no widget, only by tests. It
becomes a silent-freeze bug the first time any screen shows store state (a
"first boot" or "storage failed" chip is a natural future addition). Fix: call
`rebuild_root()` (not a bare `self.dirty = true`) so it matches every sibling
`on_*` handler's shape. Two lines.

**D2 — composite widgets swallow their children's `redraw_after`.**
`HomeView` (`home.rs:208`) delegates `measure`/`render`/`on_focus`/`on_intent`
to its `hero` and `menu`; `DevicesListView` (`app.rs:829`) delegates everything
to its `list`; `PairingWizardView` wraps a `VerticalList`. **None of the three
forwards `redraw_after`.** No live bug — no child requests one — but it is a
loaded gun aimed at the exact failure this design is trying to prevent: the
moment `HeroStatusView` grows the commissioned stereo level meter or a live
bitrate readout, it will return `Some(..)`, `HomeView` will drop it on the
floor, and Home will freeze with the gate on. Fix: each wrapper returns the
`min` of its children's answers. ~10 lines total, and it makes the composition
rule uniform with `Screen::redraw_after` (`screen.rs:203`), which already does
exactly this `min` over its widget list.

**D3 (noted, not a defect) — chrome is outside the `redraw_after` fold.**
`Screen::redraw_after` folds over `self.widgets` only. The chrome/button rail is
rendered from widgets' `chrome_contribution`, so it has no independent clock
today and is safe. If the rail ever gains time-driven content (a blinking or
timing-out label), it will need its own hook. Document in `Widget::redraw_after`'s
doc comment; do not build it now.

### 2.4 What the audit does *not* cover, and why that is fine

Nothing in C writes the panel outside `st7789_blit_framebuffer`, and every
pre-loop diagnostic path (`main.c:218`, `main.c:302`) halts or runs before
`pl_ui_create`. The ST7789 retains its GRAM, so a skipped blit leaves the last
frame intact rather than blanking — the gate is visually a no-op by
construction. Panel-side corruption (an SPI fault, a truncated DMA, a brownout)
is the one class the dirty flag cannot see; §5's backstop exists for exactly
that and for nothing else.

### 2.5 Verdict

**The tree is safe to gate all at once.** The audit did not turn up a screen
whose repaint depends on an untrustworthy source; it turned up one correct
time-driven widget, two latent traps, and a mechanically checkable invariant.

I explicitly **reject the per-screen staged gate** the brief offered as a
fallback. Gating "only screens I have proven" means a table keyed by screen
identity inside `main.c` or `ui-ffi` — a special case that must be maintained
forever, that no test can prove complete, and that every new screen must
remember to opt into. That is a load-bearing hack, and it would be *less* safe
than the whole-tree gate because the check it replaces (§6's invariant test) is
the thing that actually catches regressions. The staging I do recommend is
temporal, not spatial: land the invariant test and the backstop **first, in the
same change**, and tune the backstop period afterwards on evidence.

---

## 3. Design

### 3.1 The FFI addition

```c
/* pico_link_ui.h */
bool pl_ui_dirty(const struct PlUi *ui);
```

```rust
// ui-ffi/src/lib.rs
#[no_mangle]
pub unsafe extern "C" fn pl_ui_dirty(ui: *const PlUi) -> bool
```

Contract, to be written into the doc comment:

- **A level, not an edge** — deliberately the same shape as
  `pl_ui_display_power` (see the idle-policy design's §2.4 for why a pull-based
  level beat a command or a callback at this seam). Read once per superloop
  iteration, after `pl_ui_tick`.
- **Cleared only by `pl_ui_render`** (`App::render` clears it, `app.rs:1605`).
  If C skips the render, the flag persists to the next iteration.
- **Returns `true` if `ui` is null** — never let a null `ui` look clean. Same
  self-healing polarity as `pl_ui_display_power` returning `On` for null: every
  degenerate case fails toward *painting*, never toward a frozen screen.
- Takes `*const`, not `*mut`: this is a pure query. It does not consume,
  latch, or reset anything.

### 3.2 Why a separate query rather than folding it into `pl_ui_render`

The alternative — have `pl_ui_render` set `*out_px = NULL` when clean and let C
skip only the blit — is fewer FFI calls and makes it impossible for C to
render-then-wrongly-skip. I reject it for two reasons:

1. It throws away the render saving. `PL_LOOP_PHASE_UI_RENDER` is a real,
   already-instrumented cost (a full 240x240 widget tree draw); a separate query
   lets C skip **render and blit**, not just the blit.
2. It contradicts the seam's own settled direction of travel. `pl_ui_render`'s
   doc already says the skip is C's call; `pl_ui_display_power` established the
   pull-a-level pattern one bead ago. Two levels read at the same point in the
   loop, applied by C, is a consistent seam. A magic NULL return is a second,
   different idiom for the same kind of decision.

### 3.3 Where the gate sits — the ordering rule

This is the part that must be right against both adjacent beads.

**Today (post-`i3e`, pre-`3uq`), `main.c`'s loop tail becomes:**

```
  ... input poll, debug remote, bt drain ...
  pl_ui_tick(ui, frame_start_us);
  display_on = (pl_ui_display_power(ui) == PL_DISPLAY_POWER_ON);
  st7789_set_backlight(display_on);
  needs_paint = pl_ui_dirty(ui) || forced_repaint_due(now);   /* §5 */
  if (display_on && needs_paint) {
      pl_ui_render(ui, &px, &px_len);
      st7789_blit_framebuffer(spi1, px, px_len);
      last_blit_us = time_us_64();
  }
```

**Ordering rules, in priority order:**

1. **The dirty gate sits *inside* `i3e`'s display-power gate, not beside it.**
   `display_on && needs_paint`, in that order. While blanked we must skip
   render regardless of dirty — and critically we must **not** call
   `pl_ui_render`, because that would clear the dirty flag for a frame nobody
   saw. This is the identical contract `Runner::step` already encodes at
   `run.rs:528` (`app.dirty() && display_power() == On`), so `core` and the
   firmware stay structurally the same loop. That parity is the point.
2. **`pico-link-3uq`'s `blit_wait` for the *previous* frame's DMA stays
   unconditional and stays ABOVE both gates.** Exactly as `i3e`'s design
   already ruled: `blit_wait -> tick -> read power -> read dirty -> if(on &&
   dirty){ render; blit_start }`. Skipping the wait on a clean frame would let
   the loop run on while a DMA is still reading a framebuffer Rust may mutate
   on the next `pl_ui_render`. Written this way the gate survives either merge
   order for `3uq`, and `3uq` needs no change to accommodate it.
3. `pl_ui_dirty` is read **after** `pl_ui_tick`, because `tick` is what turns a
   due `redraw_after` into a dirty flag (`app.rs:1442`). Reading it before the
   tick would delay every time-driven repaint by one frame.
4. The `PL_LOOP_PHASE_UI_RENDER` / `PL_LOOP_PHASE_BLIT` recordings stay inside
   the gate, on the same "only record the frame that actually did it"
   discipline `i3e` established — otherwise skipped frames inject ~0us samples
   and the *mean* becomes a lie exactly when we are trying to measure this
   change. `PL_LOOP_PHASE_TOTAL` is unaffected (wall-clock gap between frame
   starts) and remains the headline acceptance number.

### 3.4 What does *not* change

- `core` is untouched by the gate itself — `Runner::step` already has it. The
  only `core` changes are D1, D2 and the §6 test.
- No new `core`/platform dependency; `pl_ui_dirty` is a pure read of existing
  state. The `core`/platform boundary and the C-owns-`main()` ADR are both
  unaffected.
- The frame-report print (`main.c:618`) already tolerates a skipped
  render/blit (it defaults its timestamps to `frame_start_us`, from `i3e`), so
  a clean frame reports `render=0us blit=0us`. Leave it; it is a useful signal
  that the gate is firing.

---

## 4. Interaction with `pico-link-i3e` (merged) and `pico-link-3uq` (unmerged)

- **`i3e`** already skips render+blit while blanked, at the same point in the
  loop, and already establishes that `App::dirty()` survives a blanked frame so
  the wake repaints. This design nests inside it (§3.3 rule 1) and reuses its
  wake path verbatim — `pl_ui_input`'s wake branch calling `mark_dirty()` is
  what makes wake-after-blank still paint under the new gate. **No change to
  `i3e` is required.**
- **`3uq`** splits the blit into start/wait. The two are independent and
  compose: `3uq` reduces the cost of the blits we still do; this bead reduces
  how many we do. If both land, expect them to be roughly multiplicative on
  idle throughput. **No change to `3uq` is required** provided rule 2 holds.
  Merge order does not matter; whichever lands second should re-read §3.3 and
  confirm the four-step ordering by eye.
- Note for measurement: `3uq`'s idle number (30.0 → 32.1 Hz, +7%) was taken
  with the blit unconditional. If this bead lands first, `3uq`'s idle win will
  measure *smaller* (there are far fewer blits left to overlap) while its
  **load** win should be unchanged. Do not read that as a regression in `3uq`.

---

## 5. The backstop: a bounded forced repaint

The dirty flag cannot see panel-side corruption (SPI fault, truncated DMA,
brownout, an ST7789 that lost its address window). Before this change, the
unconditional blit repaired all of those within 33ms and nobody ever noticed.
Removing it removes that accidental self-healing. **Do not remove it silently.**

**Recommendation: a period-bounded forced repaint in C.**

```c
/* firmware/CMakeLists.txt */
option(PL_FORCED_REPAINT_MS "Repaint the panel at least this often, 0 = never" 1000)
```

`needs_paint = pl_ui_dirty(ui) || (PL_FORCED_REPAINT_MS && now - last_blit_us >= PL_FORCED_REPAINT_MS * 1000)`

- **Why C, not `core`.** This defends the *ST7789's GRAM* — a platform artifact
  that `core` cannot know about and the emulator does not have. Putting a
  global floor on `redraw_after` in `core` would defeat `Runner::step`'s gate in
  the emulator too, for a hazard that exists only on the panel. Keeping it in C
  is the seam-respecting placement, not a compromise.
- **Why 1000ms.** At ~30Hz idle the gate removes ~97% of blits at 1s, and 38.6ms
  once per second is 3.9% duty versus ~100% today. A residual 1Hz hitch on a
  static screen is invisible (nothing is animating) and is a rounding error
  against the p1r collapse. The remaining 3% is not worth trading for the loss
  of a self-healing property we have relied on unknowingly for weeks.
- **Make it a build option so it can be measured both ways.** Tess should take
  the load numbers at `PL_FORCED_REPAINT_MS=1000` and `=0`; if the delta is
  inside the noise (it should be), keep 1000 permanently and stop thinking
  about it. Raising to 5000 or disabling is a one-line decision to make *later*,
  on evidence — that is the cheap, contained, easily-reversed kind of choice,
  and it is fine.

**Rejected alternative:** repaint-on-suspicion (e.g. only after an SPI error).
`st7789_blit_framebuffer` returns `void` and reports nothing — see `i3e`'s own
finding that `FlushErrorTracker` has no firmware counterpart (`pico-link-e7n`
territory). We cannot condition on an error we do not detect. A blind periodic
repaint is the honest version.

---

## 6. Positive evidence that no screen goes stale

The acceptance bar here is **not a test count**. Two independent kinds of
evidence, both required.

### 6.1 A mechanical invariant test in `core` (host, no hardware)

Add one property test asserting the exact contract the gate depends on:

> **Freshness invariant:** for every screen the product can build, if
> `Screen::redraw_after(ctx)` is `None`, then rendering at `t` and rendering at
> `t + 10 minutes` with no intervening input or event must produce a
> **byte-identical** framebuffer.

Table-driven over every builder: `build_home_screen` (both `HomeFace`s),
`build_devices_screen` (empty, populated, and full-store), `build_wizard_screen`
in **all six** `WizardPhase` variants, `build_forget_picker_screen`,
`build_forget_confirm_screen`, `build_device_detail_screen`,
`build_settings_screen`.

- The `None` branch is the safety-critical direction and is what catches a
  future widget that reads `ctx` and forgets `redraw_after` — including the D2
  swallowing case, which this test would have caught.
- The `Some(d)` branch gets the weaker complementary assertion: rendering at `t`
  and `t + d` must **differ**, i.e. the request is not spurious. Today only the
  wizard's `Connecting` phase hits this, and `wizard.rs:630` already proves it
  end to end; fold that existing test into the table rather than duplicating it.
- Keep the table in one place with a doc comment saying **"every new screen
  builder must be added here"**, and reference this bead. That is the
  maintenance cost, and it is the right one to pay: it is a single obvious list,
  not a per-screen gate scattered through the loop.

### 6.2 Hardware evidence, on the device, by photograph

Four captures, all with `PL_DEBUG_REMOTE=ON`, all with the navigator depth
stated in the report (per `pico-link-4vb.3`'s lesson):

1. **The one time-driven screen.** Drive the wizard to phase 4 (Connecting) and
   photograph at `t` and `t+3s` **with no input in between**. The elapsed-seconds
   line must have advanced — a non-zero pixel delta in the elapsed-line region.
   This is the single test that would catch a broken gate, so it is mandatory.
2. **Event-driven repaint under the gate.** From Home, connect to the WH-1000XM3
   (94:DB:56:54:7C:F2) via the debug console and photograph before/after: the
   hero must go `NO LINK` → `LDAC` **with no button press**.
3. **Scan-list fill.** Enter the wizard, photograph during scanning as devices
   appear. Rows must appear without input.
4. **`i3e` regression.** Idle to blank (~60s at Home root, depth 1), then a
   single `NAV DOWN`: the screen must relight showing the *correct* current
   frame, not a stale or blank one. This proves the wake path still forces a
   paint through the new gate.

### 6.3 The performance numbers

`PL_LOOP_PHASE_TOTAL` `n`-delta over fixed windows (the `3uq` methodology —
14s round-robin windows, ≥5 samples, fresh flash verified via `--list` before
and after each build), four cells:

| | before (`main`) | after |
|---|---|---|
| **idle**, Home root, lit | (re-baseline; `3uq` measured 30.0 Hz) | expect ≫ |
| **under LDAC streaming load** | (unknown — `p1r` reports ~6 Hz) | the number this bead exists to move |

Plus `PL_LOOP_PHASE_BLIT`'s sample **count** before and after at idle — a direct
count of blits actually performed is the cleanest single proof the gate fires,
and unlike a mean it cannot be diluted by skipped frames.

**The load cell is queued behind Andreas powering on the WH-1000XM3.** `3uq`'s
attempt already died on HCI Page Timeout with the headphones unreachable. Idle
numbers alone do not close this bead: idle is the easy case, and `p1r`'s
collapse is the case that matters. Take the idle numbers when the board is free;
hold the bead open for the load pair.

---

## 7. Task breakdown

| # | Task | Owner | Depends on |
|---|---|---|---|
| 1 | D1: `on_store_loaded` calls `rebuild_root()` | Ruby | — |
| 2 | D2: `HomeView`, `DevicesListView`, `PairingWizardView` forward `redraw_after` as `min` over children; note D3 in `Widget::redraw_after`'s doc | Ruby | — |
| 3 | §6.1 freshness-invariant table test in `core` | Ruby | 2 |
| 4 | `pl_ui_dirty` in `ui-ffi` + `pico_link_ui.h`, per §3.1 | Ruby | — |
| 5 | `main.c` gate per §3.3 + `PL_FORCED_REPAINT_MS` per §5; delete the stale `main.c:693` "no dirty-gate here" comment | Ruby | 4 |
| 6 | §6.2 hardware captures 1, 3, 4 (no headphones needed) + idle numbers | Tess | 5 |
| 7 | §6.2 capture 2 + the load cell of §6.3 | Tess | 6, **headphones on** |

Tasks 1–5 are one bead's worth of work for one supervisor and should land as one
change: shipping the gate (4, 5) without the invariant test (3) or the trap
fixes (1, 2) is precisely the quick-fix this document exists to prevent.

---

## 8. Risks

- **A missed repaint source freezes a screen.** Mitigated three ways: the §2
  audit (one clock reader, mechanically identified), the §6.1 invariant test
  (catches the next one), and the §5 backstop (bounds any miss to 1s). The
  residual risk is a source that is neither clock-driven nor event-driven —
  and none exists, because those are the only two ways `App` state can change.
- **`3uq` merge-order confusion.** §3.3 rule 2 is the whole mitigation. Whoever
  merges second re-reads it.
- **The load measurement stays blocked.** Real. Land the change on idle evidence
  plus §6.1/§6.2, but **do not close the bead** or claim `p1r`/`ary` are fixed
  until the load pair exists. An idle-only number here would be the same
  category of error as `3uq`'s current state.
- **Minor, non-blocking:** the wizard's `ELAPSED_REDRAW_INTERVAL` is 250ms for a
  readout that only shows whole seconds — 3 of every 4 forced repaints during
  Connecting are redundant (~15% blit duty, but only while a connect is in
  flight). 250ms is nonetheless the right value: raising it to 1s without phase
  alignment would make the counter visibly lurch. Leave it; do not "optimise"
  it into a worse readout.
