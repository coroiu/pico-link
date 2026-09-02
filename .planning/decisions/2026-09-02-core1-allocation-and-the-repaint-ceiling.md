# Core 1 allocation, and how to raise the repaint ceiling

- **Date:** 2026-09-02
- **Bead:** `pico-link-yz6` (supersedes that bead's original premise)
- **Status:** Accepted — **do not move the display and UI to core1.** Reserve
  core1 for the LDAC encoder. Raise the repaint ceiling with damage-rect
  render + partial blit instead.
- **Author:** Ada (architect)

## Context

Andreas asked a specific question on 2026-09-02: *"if we add a volume meter,
that's going to have to refresh the screen, reverting to 6 Hz? OR moving
rendering to another core?"* This ADR answers that question. It is not
documentation of an old plan — the old plan is one of the things it overturns.

`pico-link-yz6` was filed on 2026-08-28 with the premise **core1 owns display
and UI, core0 owns USB and Bluetooth**, to be built "at M4". We are at M4: LDAC
streams to a real headset. The trigger arrived, so the premise has to be
re-examined against what has been measured since, and three of the four facts
it rested on have changed.

### Four things now interact

1. **Core1 owning the display** — this bead.
2. **Damage-rect render + partial blit** — does not exist. `st7789_blit_framebuffer`
   (`firmware/src/st7789.c:187`) always sends all 240x240 pixels, and
   `core`'s `Framebuffer` (`core/src/render/framebuffer.rs`) has no damage or
   clip API at all: `pixels()`, `as_raw_u16()` and `write_be_bytes()` are all
   whole-buffer.
3. **`pico-link-3uq`** — split `st7789_blit_framebuffer` into
   `blit_start`/`blit_wait` so the superloop can run during the DMA. In
   progress, being rebased, value undecided.
4. **The VU meter (`pico-link-du0`)** — commissioned in the design of record.
   Repaints every frame by definition. It is the feature that stresses all of
   the above.

### Measured facts (use these, not the August estimates)

**A. The dirty gate closed the collapse.** `pico-link-vxc` landed. Under real
LDAC streaming to a WH-1000XM4, main sustains **63.0-63.2 Hz** with a static
screen. `pico-link-p1r`'s 6 Hz collapse is closed. Idle blits fell 96.4%
(1742 -> 62 samples); the residual is exactly the 1 s forced-repaint backstop
(measured 0.99 Hz).

**B. "The 38.6 ms blit" is a misattribution, and it has propagated.** The
number is real but it is the **whole frame**, not the blit. Reconstructing it
from `pico-link-14l`'s own measurements:

| | at 1 MHz SPI (measured) | at 75 MHz SPI |
|---|---|---|
| render (CPU, Rust) | 25 147 us | ~25 100 us — *does not scale with SPI clock* |
| blit (DMA, SPI-bound) | 1 008 024 us | ~13 400 us |
| **frame total** | **1 033 178 us** | **~38 500 us** |

`pico-link-14l` reported "full frame time down from ~1.03 s to ~38.6 ms" — the
whole row, not the blit row. The independent per-phase profiler agrees:
idle baseline `rend mx=32070us`, `blit mx=13795us`.

> **Render is roughly 65% of a painted frame and blit roughly 35%. The dominant
> cost of putting a pixel on this screen is Rust rasterisation on the CPU, not
> the SPI transfer.** Every plan that treats "the 38.6 ms blit" as one DMA
> transfer is sized against a number that is 2.9x too large for the thing it
> names.

**C. My own August preemption hypothesis is refuted.** I argued on `pico-link-p1r`
that LDAC encode at ~43% of core0 in IRQ context would starve thread context to
about a fifth of the core, that phases would inflate *across the board*, and
that this pointed at `pico-link-yz6` (core1) rather than at `pico-link-3uq`. The
profiler was built specifically to tell those apart. It came back with **one
phase dominating**, and gating that one phase bought 63 Hz *under the same
streaming load*. The load is not starving the loop. The core1 rationale that
rested on this is gone.

**D. The audio-critical work already preempts the display, by hardware.** LDAC
encode and BTstack run on the cyw43 background IRQ at `PICO_LOWEST_IRQ_PRIORITY`
(0xFF); the USB pump runs at 0xC0 (`firmware/src/usb_pump.c:33`). Render and
blit run in **thread context** in the superloop. The NVIC already guarantees
that no repaint, however slow, can delay either.

**E. Andreas already ruled on core1, on 2026-08-30**, recorded on this very
bead: *"prioritise LDAC over screen refreshing — it works as-is, and even 1 fps
is acceptable if it means Bluetooth works."* The LDAC encoder's claim is
measured and near-term: L0 bench, 600 encodes, **HQ mean 1159 us against an
1100 us budget**, every EQMID over. The display's claim is a comfort.

## Decision

1. **Do not move the display and UI to core1.** `pico-link-yz6`'s original
   premise is rejected. The bead is not closed as done; it is re-scoped by
   this ADR into the two decisions below.

2. **Core1, if and when it is started, runs the LDAC encoder — C, no Rust, no
   heap.** Andreas's ruling stands, and it turns out to be the architecturally
   clean allocation too (see Rationale, "the seam argument"). Core1 remains
   **unstarted** until `pico-link-cz0.5.6` demonstrates that encoder
   optimisation on core0 cannot close the 1159/1100 us gap.

3. **The ceiling-raiser is damage-rect render + partial blit.** This is the
   prerequisite for the VU meter, and it is the piece that does not exist.
   Build it before `pico-link-du0`.

4. **`pico-link-3uq` (blit split) is demoted to optional, and must not be
   merged on the strength of the 38.6 ms figure.** It recovers the ~13.4 ms
   DMA spin — the minority term — and once damage-rect blit lands, the spin
   it recovers shrinks proportionally with the rectangle. Finish the rebase,
   but require a measured consumer before merging.

### Build order

```
1. Damage-rect render + partial blit   <- raises the ceiling; unblocks the meter
2. pico-link-du0 (VU meter)            <- built on top of it, rate-budgeted
3. pico-link-15n (flash lockout)       <- independent; unaffected while single-core
4. pico-link-3uq                       <- only if a measured consumer appears
5. Core1 for LDAC                      <- only if cz0.5.6 fails on core0
```

### What becomes unnecessary if another lands

- Damage-rect blit **substantially subsumes** `pico-link-3uq`. A 40-row band
  blits in ~2.2 ms; splitting a 2.2 ms spin buys nothing worth the tearing
  hazard the split introduces.
- Damage-rect blit **removes the VU meter's need for core1 entirely** (see
  Rationale).
- Core1-for-display would **not** have subsumed damage-rect blit. That is the
  crux: they were never alternatives, and only one of them is a ceiling-raiser.

## Rationale

### Core1 does not raise the ceiling, and here it does not even remove a risk

The orchestrator's reading was that core1 "does not raise the frame-rate
ceiling, but removes the risk that a slow repaint starves LDAC encoding or
BTstack." The first half is right. **The second half is not**, per fact D:
LDAC and BTstack are IRQ-context at 0xFF, the USB pump at 0xC0, and the repaint
is thread-context. The interrupt controller already provides that isolation
unconditionally and for free. Core1 would be buying a guarantee we already own.

What a slow repaint *does* starve is the rest of the **thread-context**
superloop — console drain, BT command polling, input sampling. That is a real
problem, it is what `pico-link-p1r` actually measured, and it was fixed by the
dirty gate for the static case and is `pico-link-3uq`'s remit for the dynamic
case. Neither needs a second core.

### Core1 does not solve the VU meter either

Take a meter that repaints a 240x40 band at 20 Hz.

| | full-frame repaint | damage-rect repaint (40/240 = 1/6 area) |
|---|---|---|
| render | ~25.1 ms | ~4.2 ms |
| blit | ~13.4 ms | ~2.2 ms |
| per frame | **~38.5 ms** | **~6.4 ms** |
| duty at 20 Hz | **77%** | **13%** |

A full-frame VU meter at 20 Hz costs 77% of a core. Moving it to core1 does not
make it cost less — **it costs 77% of core1**, which is the core the LDAC
encoder is being reserved for. Core1 relocates the problem onto the one place
we have already decided it must not go.

There is also an absolute ceiling core1 cannot touch: a full frame is 38.5 ms of
serial render-then-blit, so **~26 fps is the hard full-frame limit on any core**.
Damage-rect is the only one of the four options that moves that number.

### The seam argument, which is why LDAC is the right core1 tenant

`pico-link-yz6`'s recorded cost list is what makes UI-on-core1 expensive, and
every item on it is a consequence of putting **Rust** on the second core:

- the `ui-ffi` global allocator must become single-core-locked or per-core;
- `spi1` and its DMA channel move to core1 exclusively;
- `PICO_CORE1_STACK_SIZE` must be set (currently SDK default,
  `firmware/CMakeLists.txt:129`);
- `pl_usb_mutex` becomes mandatory rather than advisory;
- "no Rust from IRQ" must strengthen to "Rust is entered only from core1
  thread context", and `PlUi` must be owned by exactly one execution context
  (which is why `pico-link-6o2` was a prerequisite).

LDAC-on-core1 pays **none** of these. libldac is C, allocates at init only, and
touches no Rust and no `PlUi`. It preserves the strongest invariant we have:
**the entire Rust surface stays single-core and single-context.** That
invariant is cheap to keep and expensive to reintroduce, and it is exactly the
`core`/platform seam this project is supposed to be guarding. Putting the
allocating, `PlUi`-owning, FFI-crossing half of the system on the second core
while leaving the non-allocating C half on the first is the wrong way round.

### Interaction with `pico-link-15n` (multicore flash lockout)

**Core1 makes the flash picture strictly worse, whichever tenant it gets, and
this is a cost the original bead did not record.**

Today the lockout requirement in `pico-link-15n` is *trivially satisfied*:
core1 is never started, so nothing else can touch XIP during a flash write. The
only hazard is the three ~3 ms interrupts-off blackouts against the 2 ms
ISO-OUT re-arm bar, which is what 15n's RAM-resident ISR addresses.

Starting core1 converts a currently-nonexistent hazard into a real one. Core1
executes from XIP, so every flash write must bracket a
`multicore_lockout_start_blocking` / `_end_blocking` handshake with
`multicore_lockout_victim_init` on core1, and core1 must be parked in RAM for
the full ~9 ms. Consequences by tenant:

- **UI on core1:** ~9 ms of dropped frames per flash write. Cosmetic. But the
  handshake itself becomes a new deadlock surface, and it must hold across a
  path that can be triggered from the UI (`persist.c`'s pending queue), i.e.
  core1 must be able to park while a write it requested completes.
- **LDAC on core1:** ~9 ms stall against a hard 1x-realtime encode deadline.
  That is *worse* than today, where the encoder at least runs on the core doing
  the write and is only subject to the interrupts-off windows.

Either way, **`pico-link-15n` becomes a hard prerequisite for starting core1
at all**, and it must land first. Not starting core1 keeps 15n a
single-core problem, which is the easier one.

### Is this worth doing now?

No. Andreas asked a question, not for a project. The honest answer to *"OR
moving rendering to another core?"* is: **it would not fix the thing you are
worried about, and damage-rect blit would.**

## Alternatives considered

**A1. Build core1-owns-display now, as originally specced.** Rejected.
Contradicts Andreas's 2026-08-30 ruling; buys isolation the NVIC already
provides (fact D); does not raise the 26 fps ceiling; costs 77% of core1 for
the meter anyway; pays the full Rust-on-two-cores cost list; and promotes the
flash lockout from trivial to load-bearing. Its strongest supporting argument
(my August preemption model) was measured and refuted.

**A2. Rate-limit the VU meter to 1-2 Hz and change nothing else.** Rejected as
the primary answer, though it is the correct *fallback*. Uma applied exactly
this to the fault strip, and it was right there — but a 1 Hz VU meter is not a
VU meter. Keep it in reserve: if damage-rect render lands and the measured band
cost still will not fit, drop the meter's rate rather than starting a core.

**A3. Merge `pico-link-3uq` and call the ceiling raised.** Rejected as
sufficient. It recovers the minority (~35%) term, and it introduces a real
hazard the investigation on that bead already identified: the DMA reads
directly out of the Rust-owned framebuffer, and once the superloop runs during
the transfer, anything that renders into that buffer between `blit_start` and
`blit_wait` tears or corrupts the frame. Paying a correctness hazard for the
smaller half of the cost is a bad trade *while it has no consumer*.

**A4. Double-buffer the framebuffer.** Not chosen, but note the cost so nobody
re-derives it: a second 240x240 RGB565 buffer is 115 KB of the 520 KB SRAM.
There is 8 MB of PSRAM, but PSRAM-backed framebuffers change the DMA source
timing and would need measuring. Revisit only if damage-rect proves
insufficient.

**A5. Cut the VU meter.** Not chosen. It is commissioned in the design of
record and Andreas re-raised it unprompted. Damage-rect makes it affordable at
~13% duty; that is a good price for the feature that most makes the device feel
alive.

## Consequences

**Accepted now**

- `pico-link-yz6` is re-scoped, not built. Core1 stays unstarted. The whole
  Rust surface stays single-core, the `ui-ffi` allocator stays unlocked,
  `spi1` stays core0's, `pl_usb_mutex` stays advisory, and "no Rust from IRQ"
  remains the rule (it is already satisfied since `pico-link-6o2` closed).
- A new bead is needed for **damage-rect render + partial blit**. It is a
  cross-seam change and should be designed before it is filed:
  - `core` gains a damage region on `Framebuffer` and a way to expose it —
    this is the load-bearing part, and it must not become a special case for
    the VU meter. It is the general "which pixels changed" answer the widget
    tree should have had all along.
  - the FFI gains a rect alongside the pixel pointer (`pl_ui_render` currently
    returns `px`/`px_len` only). This is a **versioned** FFI change; treat it
    with the care `pico-link-a67` established.
  - `st7789_blit_framebuffer` gains CASET/RASET per blit. Note the standing
    contract in `st7789.c:150-155` — the address window is set **once at init**
    and never re-issued, and the current code relies on the panel's write
    pointer wrapping. Partial blit breaks that assumption and must set the
    window every transfer. This is the single most likely place for it to go
    subtly wrong on hardware.
- **Correct the 38.6 ms figure wherever it is quoted as a blit time.** It
  appears at `firmware/src/watchdog_sup.c:24`, in `pico-link-3uq`'s and
  `pico-link-du0`'s framing, and in the current handoff notes. The watchdog
  budget is not wrong (60 ms covers render+blit and it was derived from the
  frame total), but the *label* is, and the label is what people size plans
  against.

**Deferred, with named triggers**

- **Start core1 for LDAC** if `pico-link-cz0.5.6` cannot bring HQ encode under
  ~1100 us on core0. Prerequisite: `pico-link-15n` must land first.
- **Revisit core1 for display** only if, after damage-rect lands, a *measured*
  thread-context starvation reappears that the dirty gate and rectangle
  cannot address. Do not revisit it on the strength of the preemption argument
  — that argument has been tested and lost.
- **Merge `pico-link-3uq`** when a measured consumer needs superloop throughput
  during a repaint. Its own design must state and enforce the
  no-render-between-start-and-wait rule.

**Risks**

- Damage-rect render is only a win if the *render* is genuinely clipped, not
  just the blit. If the widget tree redraws the whole background before drawing
  the band, we save the 35% and keep the 65%. The design must make the clip
  reach the rasteriser, and the acceptance criterion must measure
  `PL_LOOP_PHASE_UI_RENDER`, not just `PL_LOOP_PHASE_BLIT`.
- Damage tracking is a classic source of stale-pixel bugs. The 1 s forced
  full-frame repaint backstop from `pico-link-vxc` already exists and should be
  kept as the safety net — do not remove it as part of this work.
- The VU meter still needs its levels sampled cheaply in the audio path and
  crossed via the existing `bt.c` MPSC ring. Nothing here relaxes the
  no-new-IRQ-context-Rust-calls rule.
