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

## Open beads

- `pico-link-8v3.2` — hardware bring-up (above)
- `pico-link-b4o` — picotool-only flash workflow; rides with 8v3.2
- `pico-link-poi` — dead `.claude` template files missed by A7
- `pico-link-iyf` — memory-capture hook fires on any Bash text mentioning it

## Environment notes

- **GitHub SSH port 22 is blocked on this machine** (measured 0/10; port 443
  10/10). Push with
  `git push ssh://git@ssh.github.com:443/coroiu/pico-link.git main`. Andreas
  declined a `~/.ssh/config` change and handles it himself.
- **The beads board has no remote backup.** It lives only in
  `.beads/embeddeddolt/`, which is gitignored. JSONL auto-export is OFF by
  default in beads 1.2.2, so `.beads/issues.jsonl` is never generated. `bd dolt
  push` fails for the same port-22 reason.
