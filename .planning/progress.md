# Progress

**Last updated:** 2026-08-28

## Where things stand

Epics A and B are **complete**. All host-side work is done. Firmware
architecture is C-first (flipped 2026-08-27): pico-sdk owns `main()` and
`runtime_init`; `core/` is a `no_std` + `alloc` staticlib called from C over a
narrow FFI. See
[ADR 2026-08-27](decisions/2026-08-27-c-first-pico-sdk-owns-main.md), which
supersedes the 2026-08-26 "Rust owns the binary" ADR.

**Gate-2 radio bring-up is ACHIEVED**, against the C-first architecture, in an
overnight run merged 2026-08-27 into 2026-08-28 (`pico-link-cz0.3`, merged
`4b14cc9`): BTstack Classic GAP inquiry runs over pico-sdk's cyw43 HCI
transport, discovered devices render on the panel with address and RSSI,
webcam-verified. This is the milestone three earlier Rust-first sessions never
reached. See "Session 2026-08-27 into 2026-08-28" below for the full run.

**The same overnight run continued past M2** and, still on `main`
(now at `52c6c53`), fixed the three biggest usability blockers left standing:
input was unusable (8.3s hold needed to register a press), the panel took
over a second per frame, and the UI rendered upside-down relative to how the
board sits with its cable out. The first two are fixed; the rotation attempt
landed MIRRORED instead and is tracked in `pico-link-zzq` — see "Session
2026-08-27 into 2026-08-28" below for the full account, including the
still-open M3 (TinyUSB audio) and why photographs cannot settle screen
orientation.

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

### Epic C — firmware, in progress; C-first, gate-2 achieved
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

**M1a, M1b and M2 of the C-first migration are DONE**, all merged to `main` in
an overnight run 2026-08-27 into 2026-08-28. See "Session 2026-08-27 into
2026-08-28" below for the full account.
- **M1a done** (`pico-link-cz0.1`, `8295a97`) — new `firmware/` CMake project
  on pico-sdk 2.1.1, board `pimoroni_pico_plus2_w_rp2350`, CDC console,
  unattended `picotool reboot -f -u` reflashing. No Rust yet.
- **M1b done** (`pico-link-cz0.2`, `0a8c1e3`) — the architecture proof: Rust
  `core` renders, C blits, on real hardware, no crashes. `ui-ffi` staticlib,
  cbindgen header, CMake-driven cargo cross-build, ST7789 driver, debounced
  input.
- **M2 done** (`pico-link-cz0.3`, `4b14cc9`) — **gate-2 radio bring-up
  achieved**, against the C-first architecture. BTstack Classic GAP inquiry
  over pico-sdk's cyw43 HCI transport; discovered devices render on the panel
  with address and RSSI; webcam-verified. This is the milestone three earlier
  Rust-first sessions never reached.

## Next step

**`pico-link-zzq` (P1) first: the display-rotation fix merged this run is
wrong** — `MADCTL = 0xA0` mirrors the panel instead of rotating it, discovered
by Andreas inspecting the physical board after the earlier webcam-based
verification accepted it. Known good is `MADCTL = 0x60` (upright, cable
exiting LEFT); the actual requirement is that image rotated 180 degrees.
Verify with an asymmetric corner test pattern, not a photo of small text — see
the session write-up below for the full account.

**Then M3 — TinyUSB composite sound card** (`pico-link-cz0.4`), branch
`bd-pico-link-cz0.4`, still unmerged and still the next milestone gating the
MVP. macOS enumerates the device driverlessly as a sound card and its 227-byte
config descriptor was verified byte-by-byte off the live device, but the
firmware hangs when audio actually streams. The leading hypothesis was
`tud_task()` starved by the then-1-second blit; that superloop is now 38.6ms
(`pico-link-14l`, below), so the first action on resuming this branch is to
**rebase onto `main` and simply retry streaming before any new diagnosis.**

Also open, not blocking either: `pico-link-d7k` (the d-pad-select ->
`PL_CMD_CONNECT` path, and the new 180-degree input remap from the rotation
fix, both still need one human press on real hardware to verify — there is no
automated input path on the real target, a standing gap in the three-run-modes
testability story), `pico-link-gap` (panic recorder, applicability under the
C-first `firmware/` project unverified), `pico-link-hfc` (P4, remaining
`.claude` boilerplate).

`pico-link-14l` (panel colour + SPI clock) is now **done**, not open — see the
session write-up below.

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

## Session 2026-08-27 into 2026-08-28 — C-first migration executed, gate 2 achieved

An overnight run took the C-first ADR from decision to working silicon. All
merged to `main` (now at `4b14cc9`) and pushed.

**Merged, in order:**
1. `pico-link-cz0.1` (M1a), `8295a97` — new `firmware/` CMake project on
   pico-sdk 2.1.1, board `pimoroni_pico_plus2_w_rp2350`, CDC console,
   unattended `picotool reboot -f -u` reflashing. No Rust.
2. `pico-link-4mc`, `88661d7` — `tools/usb-console/cdc_reader.py`, a
   direct-USB CDC reader that never opens a `/dev/cu.*` node, now the standard
   capture path. `tty_fallback.py` remains a clearly-marked risky fallback.
3. `pico-link-iyf`, `915e980` — the memory-capture hook now requires a
   successful call, a real command position, and a bead ID that resolves.
4. `pico-link-cz0.2` (M1b), `0a8c1e3` — **the architecture proof.** Rust
   `core` renders, C blits, on real hardware. No NOCP UsageFault, no crashes.
   `ui-ffi` staticlib, cbindgen header, CMake-driven cargo cross-build, ST7789
   driver, debounced input. `FrameBuffer565` now backed by `Vec<u16>`; `run.rs`
   refactored into `Runner`/`step()`. Two design corrections found by building
   it: the specified 64KB heap arena is smaller than one 115KB framebuffer
   (raised to 192KB), and the target triple is `eabi`, not `eabihf`, to match
   pico-sdk's softfp ABI.
5. `pico-link-poi`, `ea02758` — dead frontend boilerplate removed from
   `.claude/`. Also verified a negative worth recording: sweeping `.claude/`
   for `bitwarden`, `bhk-core`, `hardware key`, `rust owns`, `owns main`,
   `embassy`, `cortex_m_rt` returns zero hits — no agent definition still
   describes the old product or the abandoned Rust-first architecture.
6. `pico-link-cz0.3` (M2), `4b14cc9` — **Bluetooth radio up.** pico-sdk's
   ready-made HCI transport over cyw43, BTstack Classic with GAP inquiry,
   discovered devices rendered on the panel with address and RSSI,
   webcam-verified. This is the milestone three earlier Rust-first sessions
   never reached.
7. `pico-link-lfm`, `9d90dd9` — the memory-capture hook now captures EVERY
   `LEARNED:` in a Bash call, not just the last. Also fixed a pre-existing
   defect worth recording: the hook's bead-validation watchdog never worked,
   because the lookup ran inside a command substitution, so `kill -9` left a
   grandchild holding the pipe. Measured 60.09s against a hung `bd` on the old
   hook; now 7.16s, bounded inside the 10s hook timeout.
8. `pico-link-5am`, `0c310a4` — input debounce moved OFF the render loop onto
   a 1ms pico-sdk repeating timer feeding a lock-free SPSC ring buffer (cap
   32, drops newest on overflow) that the superloop drains. Nothing calls
   into Rust from interrupt context. Root cause it fixed: input was sampled
   once per ~1.03s frame, and the 8-sample debounce needed ~8.3 seconds of
   held button, so the d-pad appeared dead.
9. `pico-link-14l`, `8381c2b` — **SPI clock 1MHz -> 75MHz** (the `clk_peri`/2
   hardware ceiling), frame time 1.03s -> 38.6ms. Panel colour also settled:
   an exposure-immune single-frame test (half theme BACKGROUND, half pure
   white, compared within one photograph) shows BACKGROUND reading as
   saturated blue, not washed toward white. The earlier alarming absolute
   readings were camera exposure on an emissive panel, as suspected but not
   proven until this test.
10. `pico-link-g7o`, `52c6c53` — attempted display rotation via `MADCTL = 0xA0`
    with the input pin-to-intent table rotated 180 degrees to match.
    **THIS IS WRONG AND IT IS ON `main`.** Andreas inspected the physical
    board: the content is MIRRORED, not rotated. Tracked in `pico-link-zzq`
    (P1). Known good: `MADCTL = 0x60` gives a correct, unshifted, upright
    image with the USB cable exiting LEFT; the requirement is that image
    rotated 180 degrees.

**The verification lesson, and it cost this run twice.** Screen orientation was
judged from webcam photographs, and photographs cannot separate a mirror from a
180-degree rotation when all you can read is small blurry text — both come out
reversed. The first attempt rotated the PHOTO until text read upright and
produced geometrically impossible results; the second accepted a mirrored build
as correct. **Judge orientation with an asymmetric CORNER TEST PATTERN** —
distinct colours in three of the four corners — so one frame decides it by
which corner holds which colour, with no dependence on legibility. Andreas
turning the board in his hand is the other reliable oracle and takes seconds.

**Two hard-won environment facts from this run:**
- pico-sdk's `stdio_usb` gates ALL console output on DTR
  (`stdio_usb_connected()` returns `tud_cdc_connected()`), and the direct-USB
  reader cannot assert DTR on macOS — a healthy board therefore reads as
  permanently silent. Fix: build with
  `PICO_STDIO_USB_CONNECTION_WITHOUT_DTR=1` (DTR, not DTE). This cost a
  supervisor a whole hang hunt; see CLAUDE.md's "Environment & workflow
  gotchas" for the full account.
- The Homebrew `arm-none-eabi-gcc` lacks newlib specs. The working toolchain
  is at `/Applications/ArmGNUToolchain/15.2.rel1/arm-none-eabi/bin`.

**Done as of this run:** `pico-link-lfm` (memory-capture hook, every LEARNED
captured, watchdog fixed) and `pico-link-14l` (SPI clock, panel colour) —
both merged, see the numbered list above.

**Still open, not done:** `pico-link-zzq` (**P1** — the merged rotation fix
is wrong: `MADCTL = 0xA0` mirrors the image rather than rotating it; known
good is `MADCTL = 0x60`, upright with the cable exiting LEFT, and the actual
requirement is that image rotated 180 degrees; judge with an asymmetric
corner test pattern, not by reading text in a photo). `pico-link-cz0.4` (M3,
TinyUSB composite sound card) — branch `bd-pico-link-cz0.4`, unmerged, hangs
when audio streams; the leading hypothesis (`tud_task()` starved by the
then-1-second blit) is now moot since the blit is 38.6ms, so the next step is
a rebase-and-retry before any new diagnosis. `pico-link-d7k` — the
d-pad-select to `PL_CMD_CONNECT` path, and the new 180-degree input remap
from the rotation fix, both need one human press to exercise on real
hardware, because there is no automated input path on the real target — a
standing gap in the three-run-modes testability story.

## Open beads

- `pico-link-zzq` — **P1.** The merged display-rotation fix (`pico-link-g7o`,
  on `main` as of `52c6c53`) is wrong: `MADCTL = 0xA0` mirrors the image
  instead of rotating it. Known good: `MADCTL = 0x60` gives an upright image
  with the cable exiting LEFT; the actual requirement is that image rotated
  180 degrees. Judge with an asymmetric corner test pattern (distinct colours
  in three of four corners), never by reading small text in a photo.
- `pico-link-cz0.4` — M3, TinyUSB composite sound card. Branch
  `bd-pico-link-cz0.4`, unmerged; hangs when audio streams. Next step:
  rebase onto `main` (now 38.6ms/frame, not ~1s) and retry before new
  diagnosis.
- `pico-link-d7k` — d-pad-select -> `PL_CMD_CONNECT`, and the new
  180-degree input remap, never exercised on real hardware; no automated
  input path on the real target
- `pico-link-gap` — panic recorder: survive the reboot, report on next boot.
  Filed against the retired Rust-owns-`main()` `hal_shim`; applicability under
  the C-first `firmware/` project is unverified this session, not re-checked.
- `pico-link-hfc` — P4, remaining `.claude` boilerplate.
- `pico-link-46w` (core affinity on the IRQ depth counter) and `pico-link-1rp`
  (watchdog blind to a core1 lockup while core0 keeps feeding it) are **NOT ON
  THE BOARD** — verified 2026-08-28: `bd show` resolves neither, and the board
  holds 10 beads total. They were presumably lost in the beads 1.2.2 recovery.
  Both described the retired Rust-owns-`main()` `hal_shim` and neither has been
  re-examined against the C-first `firmware/` project, so re-file them only if
  the concern turns out to apply to the current code — do not restore them
  blindly.

## Hardware workflow — read this before touching the board

Earned expensively on 2026-08-27, against the now-retired embassy-usb /
Rust-owns-`main()` firmware-spike. The build/tooling facts below still apply
to the C-first `firmware/` project; the CDC-specific one is superseded by the
2026-08-27-into-08-28 entry right after it.

- **`PICO_SDK_PATH=/Users/andreas/pico-sdk` is REQUIRED for every firmware build
  and is NOT set in the environment.** The build fails immediately without it.
- **The Homebrew `arm-none-eabi-gcc` lacks newlib specs and cannot build the
  firmware.** The working toolchain is
  `/Applications/ArmGNUToolchain/15.2.rel1/arm-none-eabi/bin`.
- **`picotool uf2 convert` needs `-t elf`.** Without it picotool silently writes
  a ZERO-BYTE .uf2 and reports success. You then flash nothing and debug a
  phantom.
- **(C-first `firmware/`, 2026-08-28) pico-sdk's `stdio_usb` gates ALL console
  output on DTR**, and the direct-USB reader (`cdc_reader.py`) cannot assert
  DTR on macOS — a healthy board reads as permanently silent, indistinguishable
  from a hang. Build with `PICO_STDIO_USB_CONNECTION_WITHOUT_DTR=1` (DTR, not
  DTE) so `stdio_usb_connected()` falls back to `tud_ready()`. Cost a
  supervisor a whole hang hunt; keep the flag set in `firmware/CMakeLists.txt`.
- **(Retired embassy-usb architecture) reading the CDC console needed DTR
  asserted explicitly.** embassy-usb's `wait_connection()` blocked until DTR,
  and a plain `cat /dev/cu.usbmodem*` on macOS did not reliably assert it. This
  applied to the Rust-owns-`main()` firmware-spike, not the current
  `firmware/` project.
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
- **Dev affordances that must NOT ship (as filed against the retired
  `hal_shim` firmware-spike; not re-verified against the current `firmware/`
  project):** the watchdog, the panic-to-BOOTSEL handler, the borrowed
  0x2e8a/0x000a VID/PID (Raspberry Pi's, used because picotool only scans that
  vendor ID), and the USB_STAGE display instrumentation.

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
