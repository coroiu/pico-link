#!/usr/bin/env python3
"""Loads an Equalizer APO / AutoEQ parametric-EQ preset file over the
PL_DEBUG_REMOTE CDC console's `EQ BEGIN`/`EQ <line>`/`EQ END` commands
(bead pico-link-ryw.11).

Reads a plain-text preset file (the same format Equalizer APO's
`Config Editor > Export` or AutoEQ produce: an optional `Name: <text>`
line, a `Preamp: <n> dB` line, and one `Filter N: ...` line per band) and
sends it to the board as one `EQ BEGIN` / `EQ <line>` per non-blank line /
`EQ END` session, via cdc_sender.py's `--raw` path -- never opening a
`/dev/cu.usbmodem*` node (see CLAUDE.md's CDC rules).

One line per `--raw`, ~1s apart (`--delay 1.0` by default): the console is
line-based and this gives the firmware's line parser + core::dsp::eqapo
session plenty of headroom, deliberately slower than cdc_sender.py's own
0.3s default for terser NAV/CONNECT commands.

Usage:
  python3 eq_import.py xm3-preset.txt
  python3 eq_import.py xm3-preset.txt --delay 0.5
  python3 eq_import.py xm3-preset.txt --vid 0x2e8a --pid 0xc

Requires the same `pyusb` + libusb dependency as cdc_sender.py (this
script shells out to it, in the same directory).
"""
from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path


def load_preset_lines(path: Path) -> list[str]:
    """Reads `path` and returns its non-blank, comment-stripped lines,
    trimmed. Blank lines are dropped rather than sent -- core::dsp::eqapo's
    `EqApoSession::feed_line` tolerates and simply ignores blank lines
    itself (still counting them for its own line numbers), but there's no
    reason to spend a ~1s `--raw` slot sending one over the wire.
    """
    lines = []
    for raw_line in path.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if not line:
            continue
        lines.append(line)
    return lines


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("preset_file", type=Path, help="path to an Equalizer APO / AutoEQ preset text file")
    ap.add_argument("--delay", type=float, default=1.0, help="seconds between lines (default 1.0)")
    ap.add_argument("--vid", type=lambda s: int(s, 0), default=None, help="USB vendor ID, forwarded to cdc_sender.py")
    ap.add_argument("--pid", type=lambda s: int(s, 0), default=None, help="USB product ID, forwarded to cdc_sender.py")
    ap.add_argument("--quiet", action="store_true", help="forwarded to cdc_sender.py")
    args = ap.parse_args()

    if not args.preset_file.is_file():
        print(f"error: {args.preset_file} is not a file", file=sys.stderr)
        return 2

    preset_lines = load_preset_lines(args.preset_file)
    if not preset_lines:
        print(f"error: {args.preset_file} has no non-blank lines", file=sys.stderr)
        return 2

    raw_commands = ["EQ BEGIN"]
    raw_commands.extend(f"EQ {line}" for line in preset_lines)
    raw_commands.append("EQ END")

    sender = Path(__file__).resolve().parent / "cdc_sender.py"
    cmd = [sys.executable, str(sender), "--delay", str(args.delay)]
    for line in raw_commands:
        cmd.extend(["--raw", line])
    if args.vid is not None:
        cmd.extend(["--vid", str(args.vid)])
    if args.pid is not None:
        cmd.extend(["--pid", str(args.pid)])
    if args.quiet:
        cmd.append("--quiet")

    if not args.quiet:
        print(f"# eq_import: sending {len(raw_commands)} lines from {args.preset_file} ({args.delay}s apart)", file=sys.stderr)

    return subprocess.call(cmd)


if __name__ == "__main__":
    raise SystemExit(main())
