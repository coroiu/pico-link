#!/usr/bin/env python3
"""RISKY FALLBACK -- opens /dev/cu.usbmodem* through the macOS tty layer.

DO NOT USE THIS UNLESS cdc_reader.py DOES NOT WORK FOR YOUR SITUATION.

USB CDC over the tty path goes through the AppleUSBCDC kext, which is
implicated in kernel panics on this Mac (tinygo-org/tinygo#5531). The
mechanism is undocumented so the risk is treated as real. A crashed laptop
also loses any beads board state since the last `bd dolt push`.

Prefer tools/usb-console/cdc_reader.py, which reads the same data straight
off the CDC-Data bulk endpoint via libusb and never opens a tty node at all.
Reach for this fallback only if pyusb/libusb genuinely cannot claim the
interface on your machine (see README.md's "when the direct path won't work"
section for the two approaches that were tried here and what happened).

If you do use this, follow CLAUDE.md's CDC rules without exception:
  - Open the tty ONCE for a long capture. Never loop open/close/reopen --
    repeated enumeration and driver attach is the pattern most associated
    with the panics.
  - NEVER run this alongside any other reader (this script, cdc_reader.py,
    screen, cu, minicom, ...) on the same device. Two readers silently steal
    bytes from each other -- fragmented lines and counter jumps that look
    exactly like real firmware bugs, not a measurement artifact.
  - DTR must be asserted explicitly (ioctl TIOCMBIS/TIOCM_DTR) -- embassy-usb
    style firmware's wait_connection() blocks on it, and a plain `cat` does
    not reliably assert it on macOS. A silently-blocked device looks
    identical to dead firmware.
  - `timeout` does not exist on this Mac -- this script uses its own
    duration/deadline handling instead of shelling out to it.

Usage:
  python3 tty_fallback.py /dev/cu.usbmodemXXXX --duration 5
  python3 tty_fallback.py /dev/cu.usbmodemXXXX --out capture.log
"""
from __future__ import annotations

import argparse
import fcntl
import os
import sys
import termios
import time

TIOCM_DTR = 0x002
TIOCM_RTS = 0x004


def assert_dtr(fd: int) -> None:
    status = bytearray(4)
    fcntl.ioctl(fd, termios.TIOCMGET, status)
    import struct

    (bits,) = struct.unpack("I", status)
    bits |= TIOCM_DTR | TIOCM_RTS
    fcntl.ioctl(fd, termios.TIOCMSET, struct.pack("I", bits))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("device", help="e.g. /dev/cu.usbmodem8v3_2_71")
    ap.add_argument("--duration", type=float, default=None, help="capture for N seconds then exit (default: run until Ctrl-C)")
    ap.add_argument("--out", type=str, default=None, help="also append raw captured bytes to this file")
    args = ap.parse_args()

    print(
        "# WARNING: opening a tty node via AppleUSBCDC -- the RISKY path.\n"
        "# Prefer cdc_reader.py. Opening this ONCE, no reopen loop.\n"
        f"# {args.device}",
        file=sys.stderr,
    )

    fd = os.open(args.device, os.O_RDWR | os.O_NOCTTY)
    try:
        assert_dtr(fd)
        print("# DTR+RTS asserted", file=sys.stderr)

        outfile = open(args.out, "ab") if args.out else None
        deadline = (time.time() + args.duration) if args.duration is not None else None
        try:
            while True:
                if deadline is not None and time.time() >= deadline:
                    break
                chunk = os.read(fd, 4096)
                if chunk:
                    sys.stdout.buffer.write(chunk)
                    sys.stdout.buffer.flush()
                    if outfile:
                        outfile.write(chunk)
                        outfile.flush()
        except KeyboardInterrupt:
            pass
        finally:
            if outfile:
                outfile.close()
    finally:
        os.close(fd)
        print("\n# tty closed", file=sys.stderr)


if __name__ == "__main__":
    main()
