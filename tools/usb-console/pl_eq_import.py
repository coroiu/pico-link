#!/usr/bin/env python3
"""Imports an Equalizer APO / AutoEQ parametric-EQ preset file over the
"Pico Link Config" vendor interface (bead pico-link-ryw.12.5, design
comment on pico-link-ryw.12, section 1: "Transport").

This is the RELEASE-build import path: a zero-endpoint control-only USB
interface (ITF_NUM_CONFIG, see firmware/src/usb_config_itf.h), present in
every firmware build -- unlike eq_import.py in this directory, which drives
the PL_DEBUG_REMOTE-only CDC console and needs a debug build.

Never opens a /dev/cu.usbmodem* node (CLAUDE.md's CDC rules: the tty path
is implicated in kernel panics on this Mac) -- this tool doesn't touch CDC
at all, only plain USB control transfers via pyusb, the same mechanism
cdc_sender.py/cdc_reader.py already use to enumerate this device.

Wire protocol (see usb_config_itf.h for the authoritative doc):
  IMPORT_PRESET (bRequest 0x01), OUT, vendor/interface, wIndex=ITF_NUM_CONFIG:
    payload = proto(1) + name_len(1) + name(name_len) + APO text
  GET_STATUS (bRequest 0x02), IN, vendor/interface, wIndex=ITF_NUM_CONFIG:
    reply = state(1) + error(1) + line(2, little-endian)

Usage:
  python3 pl_eq_import.py xm3-preset.txt
  python3 pl_eq_import.py xm3-preset.txt --name "XM3 tuned"
  python3 pl_eq_import.py xm3-preset.txt --vid 0x2e8a --pid 0xc

Requires the same `pyusb` + libusb dependency as cdc_sender.py.
"""
from __future__ import annotations

import argparse
import struct
import sys
import time
from pathlib import Path

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

# Matches cdc_reader.py/cdc_sender.py's defaults -- same composite device,
# same VID/PID (M3, bd pico-link-cz0.4). PID is NOT bumped for this
# interface addition -- see usb_descriptors.c's bcdDevice comment.
DEFAULT_VID = 0x2E8A
DEFAULT_PID = 0x000C

# ITF_NUM_CONFIG in firmware/src/usb_descriptors.h. Not looked up by class
# code (there's nothing distinguishing about a zero-endpoint vendor
# interface to search for) -- this is the one place, besides the firmware
# itself, that has to agree on the number.
ITF_NUM_CONFIG = 6

REQ_IMPORT_PRESET = 0x01
REQ_GET_STATUS = 0x02

NAME_MAX = 16
IMPORT_BUF_MAX = 1024

# bmRequestType bytes: vendor request, interface recipient, OUT/IN.
BM_REQUEST_TYPE_OUT = 0x21  # host-to-device | vendor | interface
BM_REQUEST_TYPE_IN = 0xA1  # device-to-host | vendor | interface

STATE_NAMES = {
    0: "idle",
    1: "busy",
    2: "saved",
    3: "discarded",
    4: "rejected",
}

# Mirrors ui-ffi/src/lib.rs's eq_apo_error_code / debug_eq_command_error_result
# match arms (magnitudes of PlEqCommandResult.code), plus
# usb_config_itf.h's PL_CFG_ERR_MALFORMED_HEADER for header-level failures
# that never reach the parser.
ERROR_NAMES = {
    0: "ok",
    1: "invalid utf-8 / null args",
    2: "malformed line",
    3: "invalid number",
    4: "unknown filter kind",
    5: "too many bands",
    6: "missing preamp",
    7: "no bands",
    8: "duplicate preamp",
    9: "not in session",
    254: "malformed transport header",
}


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


def find_device(vid, pid):
    backend = get_backend()
    if backend is None:
        print("No libusb backend found. Try: brew install libusb", file=sys.stderr)
        sys.exit(2)

    devs = candidate_devices(vid, pid, backend)
    if not devs and (vid is not None or pid is not None):
        devs = candidate_devices(None, None, backend)
    if not devs:
        print("No USB devices found at all (check cabling/permissions).", file=sys.stderr)
        sys.exit(1)
    if len(devs) > 1:
        print("Multiple candidate devices found:", file=sys.stderr)
        for d in devs:
            print(f"  {describe(d)}", file=sys.stderr)
        print("Narrow with --vid/--pid.", file=sys.stderr)
        sys.exit(1)
    return devs[0]


def get_status(dev) -> tuple[int, int, int]:
    """Sends GET_STATUS and returns (state, error, line)."""
    reply = dev.ctrl_transfer(BM_REQUEST_TYPE_IN, REQ_GET_STATUS, 0, ITF_NUM_CONFIG, 4)
    state, error, line = struct.unpack("<BBH", bytes(reply))
    return state, error, line


def format_status(state: int, error: int, line: int) -> str:
    state_name = STATE_NAMES.get(state, f"unknown({state})")
    if state != 4:  # not REJECTED
        return f"state={state_name}"
    error_name = ERROR_NAMES.get(error, f"unknown({error})")
    return f"state={state_name} error={error_name} line={line}"


def build_payload(name: str, apo_text: str) -> bytes:
    name_bytes = name.encode("utf-8")[:NAME_MAX]
    text_bytes = apo_text.encode("utf-8")
    payload = bytes([1, len(name_bytes)]) + name_bytes + text_bytes
    if len(payload) > IMPORT_BUF_MAX:
        raise ValueError(
            f"payload is {len(payload)} bytes, over the firmware's {IMPORT_BUF_MAX}-byte "
            f"IMPORT_PRESET buffer -- trim the preset text"
        )
    return payload


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("preset_file", type=Path, help="Equalizer APO / AutoEQ text file to import")
    ap.add_argument(
        "--name",
        default=None,
        help=(
            "preset name to send (Andreas's 2026-09-25 ruling on pico-link-ryw.12: "
            "the name is chosen on the computer). Default: the file's stem "
            "(e.g. xm3-preset.txt -> \"xm3-preset\"), truncated to 16 UTF-8 bytes."
        ),
    )
    ap.add_argument("--vid", type=lambda s: int(s, 0), default=DEFAULT_VID, help=f"USB vendor ID (default {DEFAULT_VID:#06x})")
    ap.add_argument("--pid", type=lambda s: int(s, 0), default=DEFAULT_PID, help=f"USB product ID (default {DEFAULT_PID:#06x})")
    ap.add_argument("--poll-timeout", type=float, default=3.0, help="seconds to poll GET_STATUS for a terminal state (default 3.0)")
    ap.add_argument("--poll-interval", type=float, default=0.05, help="seconds between GET_STATUS polls (default 0.05)")
    args = ap.parse_args()

    if not args.preset_file.is_file():
        print(f"No such file: {args.preset_file}", file=sys.stderr)
        return 1

    apo_text = args.preset_file.read_text(encoding="utf-8")
    name = args.name if args.name is not None else args.preset_file.stem

    try:
        payload = build_payload(name, apo_text)
    except ValueError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1

    dev = find_device(args.vid, args.pid)
    print(f"Using {describe(dev)}")

    try:
        dev.set_configuration()
    except usb.core.USBError:
        pass  # already configured -- benign on macOS

    state, _error, _line = get_status(dev)
    if state == 1:  # busy
        print("error: device reports an import already in progress (state=busy)", file=sys.stderr)
        return 1

    print(f"Sending IMPORT_PRESET: name={name!r}, {len(apo_text.splitlines())} source lines, {len(payload)} wire bytes")
    dev.ctrl_transfer(BM_REQUEST_TYPE_OUT, REQ_IMPORT_PRESET, 0, ITF_NUM_CONFIG, payload)

    deadline = time.monotonic() + args.poll_timeout
    state, error, line = get_status(dev)
    while state == 1 and time.monotonic() < deadline:  # busy
        time.sleep(args.poll_interval)
        state, error, line = get_status(dev)

    print(f"GET_STATUS: {format_status(state, error, line)}")

    if state == 2:  # saved
        return 0
    if state == 1:
        print("error: still busy after poll timeout -- device may be stuck", file=sys.stderr)
        return 1
    return 1


if __name__ == "__main__":
    sys.exit(main())
