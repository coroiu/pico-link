#!/usr/bin/env python3
"""Direct-USB CDC command sender -- the debug remote-control host side.

Writes `NAV ...` command lines straight to the CDC-Data bulk OUT endpoint
via libusb (pyusb), the same way cdc_reader.py in this directory reads --
without ever opening a /dev/cu.usbmodem* node. See CLAUDE.md's CDC rules for
why: the tty path is implicated in kernel panics on this Mac.

Pairs with firmware/src/debug_remote.c (bead pico-link-cd3), a debug-only,
compile-gated (PL_DEBUG_REMOTE, OFF by default -- see
firmware/CMakeLists.txt) module that reads these lines from the MAIN LOOP
(never interrupt context) and injects them into the same `pl_ui_input`
event path the GPIO button scan feeds. This exists because code review on
pico-link-cz0.5.2 found no software path to trigger scan/connect, so every
on-device acceptance item needed a human physically present at the d-pad.

Wire protocol (one command per line, LF-terminated):
  NAV UP | NAV DOWN | NAV LEFT | NAV RIGHT
  NAV SELECT | NAV BACK | NAV X | NAV Y
  NAV JUMP <signed-int>
  CONNECT <addr>   -- bead pico-link-g48: bypasses GAP inquiry and connects
                       straight to a known BD_ADDR. See --connect below.

Usage:
  python3 cdc_sender.py UP DOWN SELECT          # send three commands, exit
  python3 cdc_sender.py --list                  # enumerate candidate devices, exit
  python3 cdc_sender.py SELECT --delay 0.5      # 0.5s pause between commands
  python3 cdc_sender.py --raw "NAV JUMP -3"      # send a raw protocol line verbatim
  python3 cdc_sender.py --vid 0x2e8a --pid 0xc  # override device match
  python3 cdc_sender.py --connect AABBCCDDEEFF  # connect directly to a known device address

Command shorthand accepted (case-insensitive): UP, DOWN, LEFT, RIGHT,
SELECT, BACK, X, Y, JUMP:<n> (e.g. JUMP:-3). Each is turned into the matching
`NAV ...` protocol line.

--connect BD_ADDR (bead pico-link-g48): skips discovery entirely and asks
the firmware to connect straight to that address -- useful when the target
headset is on but not in pairing mode (so it will never show up in a scan)
and you already know its address. The address is a CLI argument ONLY: it
is read from argv, put on the wire, and never written to any file, default,
or constant by this script or the firmware it talks to (the firmware
persists no link keys either -- see firmware/src/bt.h's doc comment on
pl_bt_debug_connect). Expect this to often end in a clean BTstack refusal
against a device the board was never (or is no longer) paired with -- that
is a valid, informative result, not a bug in this channel.

Requires: `pip3 install pyusb` and libusb (`brew install libusb`). No sudo.
"""
from __future__ import annotations

import argparse
import sys
import time

try:
    import usb.core
    import usb.util
    import usb.backend.libusb1
except ImportError:
    print(
        "pyusb is not installed. Run: pip3 install pyusb\n"
        "(also needs libusb: brew install libusb)",
        file=sys.stderr,
    )
    sys.exit(2)

# Matches cdc_reader.py's defaults -- same composite device, same VID/PID
# (M3, bd pico-link-cz0.4).
DEFAULT_VID = 0x2E8A
DEFAULT_PID = 0x000C

CDC_DATA_CLASS = 0x0A
CDC_COMM_CLASS = 0x02

_SHORTHAND = {
    "UP": "NAV UP",
    "DOWN": "NAV DOWN",
    "LEFT": "NAV LEFT",
    "RIGHT": "NAV RIGHT",
    "SELECT": "NAV SELECT",
    "BACK": "NAV BACK",
    "X": "NAV X",
    "Y": "NAV Y",
}


def to_protocol_line(token: str) -> str:
    """Turn one CLI token into a `NAV ...` protocol line, or raise ValueError."""
    upper = token.strip().upper()
    if upper in _SHORTHAND:
        return _SHORTHAND[upper]
    if upper.startswith("JUMP:") or upper.startswith("JUMP "):
        n = upper[5:].strip()
        try:
            int(n)
        except ValueError:
            raise ValueError(f"JUMP needs a signed integer, got {n!r}")
        return f"NAV JUMP {n}"
    raise ValueError(
        f"unrecognized command {token!r} -- expected one of "
        f"UP/DOWN/LEFT/RIGHT/SELECT/BACK/X/Y/JUMP:<n>"
    )


def to_connect_line(addr: str) -> str:
    """Turn a BD_ADDR string (bead pico-link-g48) into a `CONNECT ...`
    protocol line. Accepts "AABBCCDDEEFF" or "AA:BB:CC:DD:EE:FF" /
    "AA-BB-CC-DD-EE-FF". Raises ValueError on anything else -- this is
    deliberately strict (exactly 6 bytes of hex) rather than best-effort,
    since a malformed address silently truncated or padded would connect
    to the wrong device.

    The address is a CLI argument only -- it is read from argv, placed on
    the wire, and discarded. Nothing in this file stores it as a default,
    a constant, or writes it to any file (see bt.h's doc comment on the
    firmware side, pl_bt_debug_connect, for why that matters)."""
    hex_only = addr.strip().replace(":", "").replace("-", "")
    if len(hex_only) != 12 or not all(c in "0123456789abcdefABCDEF" for c in hex_only):
        raise ValueError(
            f"--connect needs a 6-byte BD_ADDR, got {addr!r} "
            f"(expected e.g. AABBCCDDEEFF or AA:BB:CC:DD:EE:FF)"
        )
    return f"CONNECT {hex_only.upper()}"


def get_backend():
    for candidate in (
        None,
        "/opt/homebrew/lib/libusb-1.0.dylib",
        "/usr/local/lib/libusb-1.0.dylib",
    ):
        find_library = (lambda path: path) if candidate is None else (lambda _p, c=candidate: c)
        backend = usb.backend.libusb1.get_backend(find_library=find_library) if candidate else usb.backend.libusb1.get_backend()
        if backend is not None:
            return backend
    return None


def find_cdc_data_interface(dev):
    """Return (config, data_iface, out_ep) -- the bulk OUT endpoint on the
    CDC-Data interface, mirroring cdc_reader.py's IN-endpoint lookup."""
    for cfg in dev:
        data_iface = None
        for intf in cfg:
            if intf.bInterfaceClass == CDC_DATA_CLASS and data_iface is None:
                data_iface = intf
        if data_iface is not None:
            out_ep = None
            for ep in data_iface:
                if usb.util.endpoint_direction(ep.bEndpointAddress) == usb.util.ENDPOINT_OUT:
                    out_ep = ep
                    break
            if out_ep is not None:
                return cfg, data_iface, out_ep
    return None, None, None


def candidate_devices(vid, pid, backend):
    kwargs = {"backend": backend}
    if vid is not None:
        kwargs["idVendor"] = vid
    if pid is not None:
        kwargs["idProduct"] = pid
    return list(usb.core.find(find_all=True, **kwargs))


def describe(dev):
    try:
        manu = usb.util.get_string(dev, dev.iManufacturer) if dev.iManufacturer else "?"
        prod = usb.util.get_string(dev, dev.iProduct) if dev.iProduct else "?"
        serial = usb.util.get_string(dev, dev.iSerialNumber) if dev.iSerialNumber else "?"
    except Exception:
        manu, prod, serial = "<unreadable>", "<unreadable>", "<unreadable>"
    return f"{dev.idVendor:#06x}:{dev.idProduct:#06x} {manu!r} / {prod!r} (serial {serial})"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("commands", nargs="*", help="commands to send: UP/DOWN/LEFT/RIGHT/SELECT/BACK/X/Y/JUMP:<n>")
    ap.add_argument("--raw", action="append", default=[], help="send a raw protocol line verbatim (repeatable, interleaved in order given after positional commands)")
    ap.add_argument(
        "--connect",
        default=None,
        metavar="BD_ADDR",
        help=(
            "bead pico-link-g48: connect directly to this BD_ADDR, bypassing GAP inquiry "
            "(e.g. --connect AABBCCDDEEFF or AA:BB:CC:DD:EE:FF). Sent last, after any "
            "positional commands/--raw lines. The address is a CLI argument only -- never "
            "stored anywhere by this tool."
        ),
    )
    ap.add_argument("--vid", type=lambda s: int(s, 0), default=None, help=f"USB vendor ID (default: try {DEFAULT_VID:#06x}, then any)")
    ap.add_argument("--pid", type=lambda s: int(s, 0), default=None, help=f"USB product ID (default: try {DEFAULT_PID:#06x}, then any)")
    ap.add_argument("--list", action="store_true", help="enumerate candidate devices and exit")
    ap.add_argument("--delay", type=float, default=0.3, help="seconds to sleep between commands (default 0.3 -- gives a frame or two to render)")
    ap.add_argument("--quiet", action="store_true", help="suppress the banner/per-command echo")
    args = ap.parse_args()

    backend = get_backend()
    if backend is None:
        print("Could not load a libusb1 backend. Is libusb installed? (brew install libusb)", file=sys.stderr)
        sys.exit(2)

    vid, pid = args.vid, args.pid
    devs = candidate_devices(vid, pid, backend)
    if not devs and vid is None and pid is None:
        devs = candidate_devices(DEFAULT_VID, DEFAULT_PID, backend)

    if args.list:
        if not devs:
            print("No matching USB devices found.")
        for d in devs:
            print(describe(d))
        return

    if not devs:
        print(
            "No matching USB device found. Is the board plugged in and enumerated?\n"
            "Try --list with no --vid/--pid to see everything, or pass --vid/--pid explicitly.",
            file=sys.stderr,
        )
        sys.exit(1)
    if len(devs) > 1:
        print(f"Warning: {len(devs)} matching devices found, using the first: {describe(devs[0])}", file=sys.stderr)

    lines = []
    try:
        for tok in args.commands:
            lines.append(to_protocol_line(tok))
        for raw in args.raw:
            lines.append(raw.strip())
        if args.connect is not None:
            lines.append(to_connect_line(args.connect))
    except ValueError as e:
        print(f"error: {e}", file=sys.stderr)
        sys.exit(2)

    if not lines:
        print(
            "No commands given -- nothing to send. Pass e.g. UP DOWN SELECT, --raw 'NAV JUMP -3', "
            "or --connect AABBCCDDEEFF.",
            file=sys.stderr,
        )
        sys.exit(2)

    dev = devs[0]
    cfg, data_iface, out_ep = find_cdc_data_interface(dev)
    if data_iface is None:
        print(f"No CDC-Data interface (class 0x0A) with a bulk OUT endpoint found on {describe(dev)}.", file=sys.stderr)
        sys.exit(1)

    iface_num = data_iface.bInterfaceNumber

    try:
        try:
            if dev.is_kernel_driver_active(iface_num):
                try:
                    dev.detach_kernel_driver(iface_num)
                except Exception:
                    pass
        except (NotImplementedError, usb.core.USBError):
            pass

        usb.util.claim_interface(dev, iface_num)

        if not args.quiet:
            print(f"# direct-USB CDC sender -- claimed iface {iface_num} on {describe(dev)}", file=sys.stderr)

        for i, line in enumerate(lines):
            payload = (line + "\n").encode("ascii")
            dev.write(out_ep.bEndpointAddress, payload, timeout=1000)
            if not args.quiet:
                print(f"# sent: {line}", file=sys.stderr)
            if i < len(lines) - 1 and args.delay > 0:
                time.sleep(args.delay)
    finally:
        try:
            usb.util.release_interface(dev, iface_num)
        except Exception:
            pass
        usb.util.dispose_resources(dev)
        if not args.quiet:
            print("# released interface, no tty was ever opened", file=sys.stderr)


if __name__ == "__main__":
    main()
