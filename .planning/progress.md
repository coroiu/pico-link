# Progress

**Last updated:** 2026-08-26

## Where things stand

Epics A and B are **complete**. All host-side work is done. Everything remaining
is firmware and needs hardware.

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

### Epic C — firmware, in progress
**The architecture decision changed.** See
[ADR 2026-08-26](decisions/2026-08-26-rust-owns-the-binary-no-usbpods-fork.md).
There is **no USBPods fork**. Rust owns the binary; BTstack, libldac and TinyUSB
link in as C static libraries.

- **C1 done.** Stock USBPods ran on the Pimoroni Pico Plus 2 W — the real target,
  not the Pico 2 W reference — and LDAC worked.
- **C-spike done** (`pico-link-8v3.1`, in `firmware-spike/`). Proved Cargo can own
  the binary: 76 BTstack symbols linked, zero undefined, `cortex_m_rt` owns the
  vector table, no pico-sdk runtime present. **Link-seam only — nothing has run
  on hardware.**

## Next step

`pico-link-8v3.2` — first real execution. In order: real `hal_time_ms` backed by
the RP2350 timer (BTstack timers are dead without it), link the RP2350 USB device
controller so CDC prints, cyw43 BR/EDR bring-up, replace the dummy
`hci_transport_t` with one backed by cyw43, then `gap_inquiry_start` and print
results. Adds the picotool reset interface in the same pass, so the manual
BOOTSEL hold it needs is the last one.

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

**Next:** `pico-link-8v3.2.4` — cyw43 bring-up on the RM2 with the Bluetooth
firmware patch, BR/EDR confirmed. It will exercise the run loop far harder than
a synthetic probe, which is the real test of the `hal_shim` just merged.

## Open beads

- `pico-link-8v3.2.4` — cyw43 BR/EDR bring-up; the next real step
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
- **The beads board has no remote backup.** It lives only in
  `.beads/embeddeddolt/`, which is gitignored. JSONL auto-export is OFF by
  default in beads 1.2.2, so `.beads/issues.jsonl` is never generated. `bd dolt
  push` fails for the same port-22 reason.
