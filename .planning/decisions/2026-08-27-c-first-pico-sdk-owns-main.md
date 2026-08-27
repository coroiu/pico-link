# C-first: pico-sdk owns `main()`; the Rust core becomes a staticlib called over FFI

**Date:** 2026-08-27
**Status:** Accepted
**Supersedes:** [2026-08-26 — Rust owns the firmware binary; C libraries are linked in, not forked](2026-08-26-rust-owns-the-binary-no-usbpods-fork.md)

## Context

ADR 2026-08-26 decided Cargo builds the `.elf`, Rust owns `main()`, the vector
table and crt0, and BTstack/libldac/TinyUSB link in underneath as plain C
static libraries. That decision was verified empirically before being taken:
the resulting ELF was inspected directly — 76 BTstack symbols, zero undefined,
`cortex_m_rt` (not pico-sdk) owning the vector table, no `runtime_init` in the
binary. That verification was real, and it is still true today: BTstack does
link into a Rust-owned binary.

But linking was never the risk. **Running was**, and it went untested for three
sessions and roughly 1.45M agent tokens, with the radio never brought up.
pico-sdk's C code assumes its own `runtime_init` ran before `main()` — clock
configuration, coprocessor enables, peripheral resets, hardware-claim spinlock
init. `cortex_m_rt` ran instead. Each pico-sdk subsystem underneath then failed
in its own way, discovered one at a time on real silicon:

- A HardFault in the RP2350 GPIO coprocessor path, `PC` inside
  `gpioc_bit_oe_put`, `CFSR = NOCP` — the GPIO coprocessor was never enabled
  because `CPACR` was never written. Diagnosed on-device; fixed with one
  register write once found.
- A hang in `cyw43_spi_init`'s PIO/DMA claim sequence, suspected to be
  uninitialised `hardware_claim` spinlocks and PIO/DMA clock setup that
  `runtime_init` normally performs.
- The smoking gun: two of pico-sdk's `runtime_init` hooks are present in the
  linked binary, but only as inert `.preinit_array` data — nothing calls them,
  because nothing in the Rust-owned boot path is pico-sdk's `_entry_point`.

Separately, ADR 2026-08-27 (TinyUSB owns the USB device controller) already
forced part of this question when TinyUSB's `rp2040` device controller driver
turned out to require `pico.h` and the hardware register headers to link at
all — its Consequences section named this explicitly and deferred the
resolution to `pico-link-xkp.2`. The evidence below settles it instead of
deferring it further.

The C surface of this device is, and always was, nearly the whole thing:
BTstack (A2DP host stack), libldac, FDK-AAC, TinyUSB (UAC2 + CDC + reset), the
cyw43 driver, and pico-sdk's own ready-made BTstack HCI transport glue. Rust
owning `main()` to host that surface inverted the real weight of the system:
the part doing the least novel work (display rendering) was made responsible
for booting the part doing the most (radio, USB, audio).

## Decision

**pico-sdk owns `main()` and `runtime_init`.** This is the normal, supported,
heavily-tested boot path — the one USBPods already runs successfully on this
exact hardware (Pimoroni Pico Plus 2 W, RP2350B + RM2). C is in charge of the
binary's entry point and its runtime bring-up.

**`core/` becomes a `no_std` + `alloc` staticlib, called from C over a narrow
FFI.** The render layer, widget/focus model, screen + navigator, theme and
chrome stay Rust — that is exactly the platform-free, decoupled shape `core/`
was already built in, not a compromise forced onto it. C calls into it to
render frames; C blits the result over SPI.

| Layer | Owner |
|---|---|
| `main()`, `runtime_init`, boot | **C — pico-sdk** |
| A2DP host stack — L2CAP, SDP, AVDTP, AVRCP | C — BTstack |
| LDAC encoder | C — libldac |
| USB Audio Class 2 + CDC + reset | C — TinyUSB |
| cyw43 driver (RM2 radio) | C — pico-sdk's `cyw43-driver` |
| Scheduling / run loop | **C — BTstack's run loop** |
| App, screens, navigation, theme, chrome (`core`) | Rust, `no_std` + `alloc` staticlib |
| Display blit over SPI | C, calling into the Rust-rendered framebuffer |

Whether this means literally forking/vendoring USBPods, or writing an
independent C `main()` against pico-sdk directly that happens to look
structurally similar, is migration detail for the architect's design — not
decided by this ADR. This ADR decides only who owns `main()` and which
language the runtime bring-up trusts.

## Rationale

- **Linking was never the risk; running was, and it went untested.** The
  2026-08-26 verification proved the ELF was well-formed. It did not prove the
  ELF would run, because it never ran on hardware. Three sessions of real
  bring-up work — a HardFault, a hang, and the inert `.preinit_array` hooks as
  direct evidence — showed the actual failure mode: pico-sdk C code silently
  assumes a runtime it never got.
- **The C surface is nearly the whole device.** BTstack, libldac, FDK-AAC,
  TinyUSB, the cyw43 driver, and pico-sdk's ready-made BTstack HCI transport
  are all C, and none of that is changing. Rust owning `main()` to host that
  weight was backwards; C hosting Rust's render layer matches where the actual
  engineering mass sits.
- **`core/` was designed for exactly this.** It survived a full product pivot
  (from a Bitwarden hardware key) because it had zero coupling to anything
  above the render layer. A `no_std` + `alloc` staticlib behind a narrow FFI is
  the use case that decoupling was built for — not a downgrade from "Rust owns
  the binary."
- **The flip is cheapest now and gets more expensive every session.** `core/`
  and `emulator/` are host-native and completely untouched by this decision —
  137 tests keep passing regardless of who owns `main()`. `firmware-spike/` is
  explicitly a spike; its Rust boot/runtime code is largely disposable. Every
  additional session spent building further C-linked-into-Rust bring-up work
  raises the cost of this same flip later.
- **This reverses a deliberate ADR, honestly.** ADR 2026-08-26 itself rejected
  taking USBPods as a base and migrating downward, on the grounds that it
  "puts C in charge of the program" — precisely what this ADR now chooses. That
  rejection was reasoned from a real verification (the ELF inspection), but the
  verification tested linkage, not execution. The evidence gathered since
  argues the original rejection was made on the wrong test, not that the
  original reasoning about the fork question was itself wrong — the fork/no-fork
  question is reopened, not resolved, by this ADR.

### Licensing note (bead `pico-link-kq9`)

`cyw43-driver`'s default licence is non-commercial, but its `LICENSE.RP`
applies instead here, because RP2350 is Raspberry Pi Ltd silicon. This keeps
Pico Link commercially fine as long as it stays RP-only — a constraint worth
keeping visible, not a blocker to this decision.

## Alternatives considered

**Keep pushing the Rust-owns-`main()` architecture through the runtime
bring-up problem** — reimplement the pieces of `runtime_init` that pico-sdk's C
code needs (CPACR, clocks, hardware-claim spinlocks, PIO/DMA setup) in Rust, by
hand, one HardFault at a time. Rejected: this is reinventing a well-tested,
already-working runtime from scratch, discovered reactively by crashing on
each missing piece, against a moving target of "whatever pico-sdk assumes
today." Three sessions of exactly this produced two confirmed bugs and zero
radio bring-up.

**Stay Rust-owns-`main()` but vendor just `runtime_init` as a C shim, called
once from Rust before anything else runs.** Considered more favorably, but
this is a narrower version of the same problem: pico-sdk's assumption isn't
confined to one function call, it is pervasive (hardware-claim state,
peripheral reset ordering, clock trees that other C subsystems read back
later). A single shim call does not establish that the rest of the C stack
was initialised in the order and manner it expects throughout its lifetime.

## Consequences

- **The embassy async executor and `embassy-usb` CDC console are retired.**
  The latter was already being retired by ADR 2026-08-27 in favour of TinyUSB;
  this ADR retires the executor itself, since C's `main()` and BTstack's run
  loop now own scheduling.
- **BTstack's run loop becomes the scheduler.** Whatever cooperative or
  interrupt-driven scheduling the firmware needs is BTstack's run loop's job,
  not an `embassy` executor's.
- **The display seam becomes Rust-renders-framebuffer / C-blits-over-SPI.**
  Rust's `core/` produces frame content into a buffer it owns; C is
  responsible for getting that buffer onto the ST7789 over SPI. The exact
  shape of this FFI boundary is migration design, not decided here.
- **`core/` and `emulator/` and their 137 host tests must keep working
  unchanged.** This decision is scoped to the firmware binary's entry point
  and runtime ownership; it does not touch the platform-free render/layout/
  input code or its desktop verification story.
- **The detailed migration design is separate work.** The architect (Ada) is
  producing the concrete migration plan, which will land as beads. This ADR
  intentionally does not specify staticlib build steps, the FFI header shape,
  or whether USBPods is forked, vendored, or read-only-referenced going
  forward — those are implementation choices for that design, not this
  decision record.
- **Firmware documents describing "Rust owns the binary" or "no pico-sdk
  runtime" as live architectural facts are stale as of this ADR.** They remain
  accurate as a record of what was believed and verified on 2026-08-26; they
  are not accurate descriptions of the current architecture.

## Update (same day, 2026-08-27): the fork/no-fork question is now decided — Route B

The Rationale above left the fork/no-fork question "reopened, not resolved."
It is now decided. The architect (Ada) analysed the two routes; the
orchestrator accepted her recommendation. **We write our own C against
pico-sdk (Route B). USBPods stays a reference to read, never to vendor or
fork.** This is consistent with, not a reversal of, Andreas's position in ADR
2026-08-26 ("I DON'T WANT A FORK") — that position was never a licensing
objection, so it survives the C-first pivot untouched.

**There is no partial fork.** GPL-3 is viral across the link boundary:
vendoring even one non-trivial USBPods `.c` file makes the entire linked
binary GPL-3 — `core/`, `ui-ffi/`, and all our own C included. The intuitive
middle ground of "just borrow the hard parts" is not a licensing middle ground
at all. This is the one wrong intuition here that is expensive and one-way,
so it is stated plainly rather than left implicit.

**Why the C-first pivot did not settle this on its own.** C-first changed who
owns `main()`. It did not change who owns the design. Under Rust-first the
fork boundary was self-enforcing, because USBPods' C simply did not fit
inside a Cargo-owned binary — there was no way to vendor it even carelessly.
C-first removes that friction: USBPods' files would now compile straight into
our CMake build if anyone reached for them. A constraint previously enforced
by architecture must from now on be enforced by discipline. That is exactly
why it needs to be a written decision instead of an assumption.

**Route B is affordable because the hard parts already have licence-clean,
first-party references vendored in the SDK we build against:**
- UAC2 descriptors and the explicit-feedback clock loop —
  `PICO_SDK_PATH/lib/tinyusb/examples/device/uac2_headset/` (MIT).
- A2DP source, AVDTP, codec plumbing —
  `PICO_SDK_PATH/lib/btstack/example/a2dp_source_demo.c` (BTstack's own
  terms, which we already link under).
- LDAC encoder — Sony `libldac`, vendored from source (Apache-2.0).

USBPods' unique contribution reduces to the *integration* of those three —
and integration is the part we want to own anyway, because it's where our
product differs: a screen and d-pad driving codec selection and device
switching, which USBPods does not have.

**Cost of Route B, stated honestly:** roughly a few extra days, concentrated
in the M3 milestone, on the explicit-feedback clock loop — the one place with
a genuinely high floor for getting it subtly wrong. Not weeks, and not spread
across the project.

**What Route A (forking) would have cost beyond the licence itself.** USBPods
ships a `LICENSE-EXCEPTIONS.md` granting a GPL-3 section 7 linking exception
— it exists because BTstack's licence isn't GPL-3 compatible alone, and
`CLAUDE.md` records that it already names the Rust `core`/`compiler-builtins`
libraries. Forking would mean inheriting responsibility for maintaining that
exception *and* extending it, since our link graph adds our own Rust
staticlib plus `embedded-alloc`, neither of which the exception was written
for. Under Route B this entire exercise disappears.

**Obligations we do carry under Route B, none of them viral:** pico-sdk
(BSD-3-Clause), TinyUSB (MIT), libldac (Apache-2.0), BTstack (its own terms),
`cyw43-driver` under `LICENSE.RP` (applies because RP2350 is Raspberry Pi Ltd
silicon; commercially fine while Pico Link stays RP-only — bead
`pico-link-kq9`). We keep a free choice of licence for our own code.

**Three safeguards, recorded as consequences of this decision:**
- **Do not clone USBPods into the repository tree**, gitignored or otherwise.
  Read it in a scratch directory outside the checkout. This removes any
  possibility of accidental vendoring by a build glob or a stray
  `git add -A`, and keeps the history unambiguous if provenance is ever
  questioned seriously.
- **Every non-trivial C file we write carries a one-line provenance header**
  naming what it was derived from, in the style `firmware-spike/src/main.rs`
  already uses for `reset_iface` (reimplemented "from the wire protocol
  only," none of pico-sdk's code copied). Protocol constants, register
  sequences and spec-mandated descriptor values are facts, not expression.
- **A provenance gate before the M4 milestone merges:** one pass confirming
  no file is USBPods-derived and every third-party licence is recorded.
  code-reviewer's existing quality gate is the natural home for this.
