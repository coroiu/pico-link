# USB console reader

Reads the firmware's USB-CDC debug console (boot banner, heartbeat, log
lines) for the Pico Link verification loop.

**Default / safe path: `cdc_reader.py`.** Talks directly to the CDC-Data
bulk endpoint over libusb (pyusb). It never opens a `/dev/cu.usbmodem*`
node — no tty, no AppleUSBCDC kext involvement at all.

**Fallback / risky path: `tty_fallback.py`.** Opens the tty node the normal
way. Only reach for this if the direct path doesn't work for your situation;
see "When the direct path won't work" below.

## Why this exists

USB CDC over the macOS tty path is implicated in kernel panics on this
machine ([tinygo-org/tinygo#5531](https://github.com/tinygo-org/tinygo/issues/5531)).
The linked issue documents the panics but not the mechanism, so the cause is
unknown and the risk is treated as real, not hypothetical. A crashed laptop
also loses any beads board state since the last `bd dolt push`. C-first
firmware bring-up means many more flash-and-read cycles ahead of us, so this
was made the default now rather than after it bites someone.

Bonus: a direct-USB reader claims the CDC-Data interface exclusively, so
there is structurally only ever one reader. The tty path lets two readers
(e.g. two agents, or a leftover `screen` session) silently steal bytes from
each other — fragmented lines and counter jumps that are indistinguishable
from a real firmware bug. That cost real debugging time on 2026-08-27 (see
CLAUDE.md's CDC rules).

## Setup

```bash
pip3 install pyusb
brew install libusb   # if not already installed
```

No sudo, no kernel-extension changes, no SIP toggling.

## Usage

```bash
# See what's plugged in
python3 tools/usb-console/cdc_reader.py --list

# Stream to stdout until Ctrl-C
python3 tools/usb-console/cdc_reader.py

# Bounded capture, also saved to a file
python3 tools/usb-console/cdc_reader.py --duration 5 --out /tmp/capture.log

# Override device match (firmware descriptors may change post-C-first)
python3 tools/usb-console/cdc_reader.py --vid 0x2e8a --pid 0x000a

# If the target firmware's wait_connection()-style gate needs DTR
python3 tools/usb-console/cdc_reader.py --assert-dtr
```

Exits cleanly on Ctrl-C (open-ended captures) or at `--duration` (bounded
captures); the interface is always released and the device handle disposed
on the way out, whichever path triggered exit — no lingering claim.

## What was actually measured (2026-08-27)

Against the running `pico-link (spike)` embassy-usb firmware
(`0x2e8a:0x000a`, CDC-Data interface #1, bulk IN endpoint `0x82`):

- `usb.core.find()` with the libusb1 backend enumerated the device.
- `dev.is_kernel_driver_active(1)` reported `True` (AppleUSBCDC has it).
- `dev.detach_kernel_driver(1)` failed: `[Errno 13] Access denied
  (insufficient permissions)`. This is expected on macOS without extra
  entitlements/sudo and was NOT a blocker.
- `usb.util.claim_interface(dev, 1)` **succeeded anyway**, immediately after
  the detach failure. macOS's IOKit-backed libusb driver does not require
  the detach step that Linux does.
- A subsequent bulk read (`dev.read(0x82, 64, timeout=200)`) over a 3-second
  window returned 231 bytes of real firmware output:
  `heartbeat 26 btstack_timer_fired=0\r\ns=0x7 p=0x0 l=0x0 c=0x0 d=2\r\n...`
  — genuine heartbeat/status lines from the running firmware, not garbage.
- No `/dev/cu.usbmodem*` node was opened at any point (confirmed via
  `lsof`), and the existing tty node was untouched and unopened by anyone.
- `usb.util.release_interface()` + `usb.util.dispose_resources()` cleanly
  released the claim afterward; the device was immediately available again.

**Conclusion: the direct-USB approach works on this Mac on the first
approach tried** for the thing that actually matters -- reading the
CDC-Data bulk endpoint without ever opening a tty. The `STOPPING RULE`
(stop after ~two failed approaches) was not triggered for that path.

**`--assert-dtr` is a measured partial exception.** Claiming the CDC-Data
interface (#1) and reading from it works cleanly with no sudo/entitlements.
But `SET_CONTROL_LINE_STATE` targets the CDC-*Communication* interface (#0),
which needs its own claim -- and on this Mac, claiming interface 0 fails
with `[Errno 13] Access denied` even after a `detach_kernel_driver` attempt
(also EACCES, and non-fatal by design). Two approaches were tried against a
device re-enumerated as `0x2e8a:0x0009`:
  1. Claim iface 0 directly, no detach attempt first.
  2. Attempt `detach_kernel_driver(0)` (fails EACCES, ignored), then claim.

Both fail identically with EACCES on interface 0. Per the bead's stopping
rule, this was not pursued further (e.g. no attempt was made to run as root,
which the bead's safety rationale argues against anyway -- privilege
escalation is its own risk). `cdc_reader.py --assert-dtr` reports the
failure clearly and continues without DTR rather than pretending it
succeeded; **firmware whose console blocks on DTR (embassy-usb's
`wait_connection()` style gating) will need either `tty_fallback.py`'s
`ioctl`-based DTR assert, or -- the better fix -- for the firmware itself to
not gate its console on DTR.** The C-first / TinyUSB migration is a natural
point to make that call; flag it to Ada/Ruby rather than working around it
here. This does not affect the core capture path: interface-1 reads work
without DTR against every firmware observed so far (the spike firmware
streamed heartbeats with no DTR assert at all).

One bug this surfaced and fixed: an early version of `cdc_reader.py` opened
its `--out` file *after* claiming the interface; a bad path (e.g. a
read-only filesystem) then threw before the `try/finally` that releases the
interface, leaving a stale claim. Fixed by opening the output file first and
moving the entire claim/read/release sequence inside one `try/finally`.

## When the direct path won't work

Two things to try, per the bead's stopping rule, before falling back to the
tty:

1. Pass `--vid`/`--pid` explicitly — firmware descriptors will change once
   the C-first / TinyUSB firmware lands (ADR
   `.planning/decisions/2026-08-27-usb-device-stack-returns-to-tinyusb.md`),
   and auto-detection assumes the pico-sdk default `0x2e8a:0x000a`.
2. If `claim_interface` itself fails (not just `detach_kernel_driver`,
   which is expected to fail and is handled), something else likely has the
   interface open — check `lsof /dev/cu.usbmodem*` and close it. Do not fall
   back to the tty just because `detach_kernel_driver` logged a warning.

If both genuinely fail, use `tty_fallback.py` and follow CLAUDE.md's CDC
rules (open once, DTR asserted, never two readers) without exception.

## Coordinating with a flashing agent

Never run either script while another agent is actively flashing the board
(it will be enumerated as `0x2e8a:0x0009`, the RP2 BOOTSEL bootloader, or
simply vanish and re-enumerate). Check first:

```bash
system_profiler SPUSBDataType 2>/dev/null | grep -A6 "0x2e8a"
```

If you see product `Pico` / PID `0x0009`, the board is in BOOTSEL mode —
wait for it to re-enumerate as the CDC device before reading.
