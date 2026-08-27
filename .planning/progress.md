# Progress

**Last updated:** 2026-08-27

## Where things stand

Epics A and B are **complete**. All host-side work is done. Firmware
architecture flipped C-first on 2026-08-27: pico-sdk owns `main()` and
`runtime_init`; `core/` becomes a `no_std` + `alloc` staticlib called from C
over a narrow FFI. See
[ADR 2026-08-27](decisions/2026-08-27-c-first-pico-sdk-owns-main.md), which
supersedes the 2026-08-26 "Rust owns the binary" ADR. Gate-2 radio bring-up has
been attempted three times against the now-superseded architecture and **not
yet achieved** — that is the next real piece of work, against the new
architecture.

### Epic A — repo reset ✅
Repo squashed to a single orphan initial commit and pushed to
`github.com/coroiu/pico-link` (private). The 403-commit prehistory stays at
`coroiu/bitwarden-hw-key`, never force-pushed; the old remote was dropped rather
than kept as `archive`. A local tag `pre-squash-archive` still pins the old
history in this checkout and can be deleted at will.

Agent definitions and the vision-session skill were rewritten for Pico Link.

**One step left:** outside a running session, `mv esp32-bluetooth-tx pico-link`
and restart Claude Code there. Renaming the cwd mid-session breaks worktree
registrations. The orchestrator's memory directory is keyed to the old path and
must move with it.

### Epic B — core retarget ✅
- **B1** `core` is `no_std` + `alloc` and cross-compiles for
  `thumbv8m.main-none-eabihf`. `std::time::Instant` became a core-owned
  `platform::Instant`; `thread::sleep` moved onto `Clock::sleep`.
- **B2** 240x240 square panel. The render layer was already resolution-parametric,
  so the only production constant was the emulator's `WIDTH`/`HEIGHT`. The real
  work was recomputing the row budget: **5 rows + a 6px peek**, up from 3 rows +
  16px, and handling the aspect flip (width *drops* 80px while height grows 70).
- **B3** Joystick + 4-button `NavIntent` replacing the rotary-encoder vocabulary:
  `Up`/`Down`/`Left`/`Right`, signed `JumpBy(i16)`, `Select`, a real `Back`
  button, `ShortcutX`/`ShortcutY`. `Left`/`Right` are forwarded but unconsumed
  until a horizontal widget exists.
- **B4** All three run modes verified at 240x240 — headless, windowed (live
  `screencapture`, not just "it launched"), and `--dump-png`. 137 tests green.

### Epic C — firmware, in progress; architecture flipped 2026-08-27
**The architecture decision changed twice.** First
[ADR 2026-08-26](decisions/2026-08-26-rust-owns-the-binary-no-usbpods-fork.md):
Rust owns the binary, BTstack/libldac/TinyUSB link in as C static libraries.
That ADR is now **superseded** by
[ADR 2026-08-27 (C-first)](decisions/2026-08-27-c-first-pico-sdk-owns-main.md):
**pico-sdk owns `main()` and `runtime_init`** — the normal, heavily-tested boot
path USBPods already uses on this hardware — and `core/` becomes a `no_std` +
`alloc` staticlib called from C over a narrow FFI, for rendering only.

**Why it flipped.** The 2026-08-26 ADR's verification (76 BTstack symbols
linked, zero undefined, `cortex_m_rt` owning the vector table, no pico-sdk
runtime in the binary) was real and stayed true — but it verified *linking*,
not *running*. Running is what was never tested, and across three sessions and
roughly 1.45M agent tokens the radio was never brought up:
- A HardFault in the RP2350 GPIO coprocessor path (`gpioc_bit_oe_put`,
  `CFSR = NOCP`) because `CPACR` was never enabled — diagnosed on-device, fixed
  with one register write once found.
- A hang in `cyw43_spi_init`'s PIO/DMA claim sequence, suspected uninitialised
  `hardware_claim` spinlocks and PIO/DMA clock setup.
- The smoking gun: two pico-sdk `runtime_init` hooks are linked into the binary
  as inert `.preinit_array` data — nothing calls them, because nothing in the
  Rust-owned boot path is pico-sdk's own entry point.

**What's banked and reusable regardless of the flip:**
- **C1 done.** Stock USBPods ran on the Pimoroni Pico Plus 2 W — the real target,
  not the Pico 2 W reference — and LDAC worked.
- **C-spike done, then superseded** (`pico-link-8v3.1`, in `firmware-spike/`).
  Proved Cargo *could* link BTstack + TinyUSB into a Rust-owned binary. Its
  Rust boot/runtime code is now largely disposable; `core/` and `emulator/`
  are completely unaffected (137 host tests, untouched).
- The debug/observability work from the 2026-08-27 session (below) — the
  panic-to-BOOTSEL handler, `picotool` unattended flash loop, and the hardware
  workflow gotchas — is infrastructure, not architecture, and survives the
  flip.
- **TinyUSB decided independently**
  ([ADR 2026-08-27](decisions/2026-08-27-usb-device-stack-returns-to-tinyusb.md)):
  TinyUSB owns the USB device controller; the `embassy-usb` CDC console is
  retired now that it served its bring-up purpose.
- **Licensing finding** (bead `pico-link-kq9`): `cyw43-driver`'s `LICENSE.RP`
  applies (not its default non-commercial licence) because RP2350 is
  Raspberry Pi Ltd silicon — commercially fine while Pico Link stays RP-only.

## Next step

The C-first migration. Detailed design is pending from the architect (Ada) and
will land as beads — do not invent implementation steps ahead of that design.
The shape is known from the ADR: pico-sdk's `main()`/`runtime_init` boots the
device; BTstack's run loop becomes the scheduler; `core/` compiles as a
`no_std` + `alloc` staticlib; the display seam is Rust-renders-framebuffer /
C-blits-over-SPI. Gate-2 radio bring-up (cyw43 BR/EDR) is the acceptance bar,
now to be attempted against this architecture instead of the superseded one.

**The upstream-relationship question (fork vs. no-fork) is decided, same day.**
Ada analysed it, the orchestrator accepted Route B: we write our own C against
pico-sdk; USBPods stays a read-only reference, never forked or vendored — no
partial fork is possible under GPL-3's link-boundary virality. See the "Update
(same day, 2026-08-27)" section appended to
[ADR 2026-08-27](decisions/2026-08-27-c-first-pico-sdk-owns-main.md). This was
pending as of the "Session 2026-08-27" write-up below; treat that section as
historical and this note as current.

## Session 2026-08-27 — the firmware talks back

Gate 2 moved from "nothing runs" to "three beads verified on real silicon". The
unlock was a debug channel: everything downstream had "print it over CDC" as its
acceptance criterion and was therefore unprovable without one.

**Merged to main:**
- `47577dd` — embassy-usb CDC console (`pico-link-8v3.2.7`) plus the picotool
  BOOTSEL reset interface (`pico-link-b4o`). The board enumerates, prints, and
  can be put into BOOTSEL from the host with no human at the laptop.
- `8bb838f` — real `hal_shim` (`pico-link-8v3.2.3`). `hal_time_ms` backed by
  `embassy_time` instead of returning 0, nestable IRQ masking, dead `hal_tick_*`
  deleted. A BTstack 1000ms timer fires once per second, read over CDC.

**The unattended dev loop is closed.** `picotool reboot -f -u` on a running
board, flash, read output — no button press. This is what made overnight work
impossible before.

**Two bugs worth remembering, both about diagnosis rather than code:**

1. A 64-byte control buffer against a 78-byte product string descriptor
   panicked during USB enumeration. `panic-halt`'s bare `loop {}` then froze the
   whole cooperative executor, so even the safety-net timer stopped firing — a
   panic that looked exactly like a hang. It cost most of a session. main now
   has a panic handler that reboots to BOOTSEL instead.
2. A "timer burst" chased for hours was a MEASUREMENT ARTIFACT. The heartbeat
   counter only advances while a host has the console open; the BTstack timer
   advances regardless; so the first lines after a connect show the accumulated
   difference. Two theories were built and disproven along the way — a clock
   reading landing a second in the future, and core1 blocking on CDC. Neither
   existed. A mitigation was written for the phantom and then produced worse
   symptoms than the thing it guarded against.

   The lesson, written down because it will recur: **do not mitigate before the
   root cause is established**, and be suspicious when a measurement and a
   defect have the same signature.

**Next (at the time):** `pico-link-8v3.2.4` — cyw43 bring-up on the RM2 with the
Bluetooth firmware patch, BR/EDR confirmed. It was expected to exercise the run
loop far harder than a synthetic probe, as the real test of the `hal_shim` just
merged.

**What actually happened next, still 2026-08-27.** Three attempts at gate-2
radio bring-up against this Rust-owns-`main()` architecture — the HardFault
from an unwritten `CPACR`, the hang in `cyw43_spi_init`'s PIO/DMA claim
sequence, and the inert `.preinit_array` `runtime_init` hooks found as the
root cause — led to the C-first pivot
([ADR 2026-08-27](decisions/2026-08-27-c-first-pico-sdk-owns-main.md)). The
`embassy-usb` CDC console and `hal_shim` described above did their job (making
the failures observable at all) and are now retired along with the rest of
the Rust-owns-`main()` boot path; `core/`, `emulator/`, and the hardware
workflow gotchas below are unaffected.

## Open beads

- `pico-link-8v3.2.4` — cyw43 BR/EDR bring-up; attempted against the
  now-superseded Rust-owns-`main()` architecture, root-caused to a missing
  `runtime_init`. Re-cut against C-first once the architect's migration design
  lands.
- `pico-link-4mc` — replace the CDC tty read path with direct USB (SAFETY, P1)
- `pico-link-gap` — panic recorder: survive the reboot, report on next boot
- `pico-link-46w` — enforce core affinity on the IRQ depth counter
- `pico-link-1rp` — watchdog is blind to a core1 lockup; core0 keeps feeding it
- `pico-link-8v3.2.2` — TinyUSB CDC route, DEFERRED not cancelled; the
  embassy-usb console replaced it for now and must move to TinyUSB when UAC2
  audio takes the USB peripheral
- `pico-link-poi` — dead `.claude` template files missed by A7
- `pico-link-iyf` — memory-capture hook fires on any Bash text mentioning it

## Hardware workflow — read this before touching the board

Earned expensively on 2026-08-27. All of it is non-obvious and all of it cost
real time.

- **`PICO_SDK_PATH=/Users/andreas/pico-sdk` is REQUIRED for every firmware build
  and is NOT set in the environment.** The build fails immediately without it.
- **`picotool uf2 convert` needs `-t elf`.** Without it picotool silently writes
  a ZERO-BYTE .uf2 and reports success. You then flash nothing and debug a
  phantom.
- **Reading the CDC console needs DTR asserted explicitly.** embassy-usb's
  `wait_connection()` blocks until DTR, and a plain `cat /dev/cu.usbmodem*` on
  macOS does not reliably assert it — you get an open port and zero bytes, which
  looks exactly like dead firmware. Open the fd and `ioctl(TIOCMBIS, TIOCM_DTR)`.
- **`timeout` does not exist on this Mac.** No coreutils.
- **CDC over the macOS tty path can KERNEL PANIC this machine** (see CLAUDE.md
  for the full rule). Prefer picotool/libusb; one tty open per capture; never
  two readers. `pico-link-4mc` replaces the tty path with direct USB.
- **Two readers on one tty silently corrupt data** — they steal bytes from each
  other, producing fragmented lines AND counter jumps indistinguishable from a
  real firmware bug. The measurement artifact and the defect look identical.
- **The heartbeat counter persists across reconnects** and only advances while a
  host has the console open. Do NOT read heartbeat numbers as seconds since
  connect; that misreading manufactured a phantom bug on 2026-08-27.
- **Nothing added to CORE0's executor may block for >8s.** A hardware watchdog is
  armed for 8s and fed every 3s from core0. Core1 may block freely — that is
  where the risky code belongs.
- **Dev affordances currently in main that must NOT ship:** the watchdog, the
  panic-to-BOOTSEL handler, the borrowed 0x2e8a/0x000a VID/PID (Raspberry Pi's,
  used because picotool only scans that vendor ID), and the USB_STAGE display
  instrumentation.

## Environment notes

- **GitHub SSH port 22 is blocked on this machine** (measured 0/10; port 443
  10/10). Push with
  `git push ssh://git@ssh.github.com:443/coroiu/pico-link.git main`. Andreas
  declined a `~/.ssh/config` change and handles it himself.
- **The beads board has a remote backup as of 2026-08-27.** It lives in
  `.beads/embeddeddolt/`, which is gitignored, and JSONL auto-export is OFF by
  default in beads 1.2.2 so `.beads/issues.jsonl` is never generated — but
  `bd dolt push` now replicates it to `refs/dolt/data` on `coroiu/pico-link`.
  The earlier "push fails" note blamed the port-22 block; the real cause was
  bd's own remote list (separate from git's) still pointing at the pre-pivot
  `bitwarden-hw-key` repo on port 22. Repointed with `bd dolt remote add origin
  git+ssh://git@ssh.github.com:443/coroiu/pico-link.git`.
