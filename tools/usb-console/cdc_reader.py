#!/usr/bin/env python3
"""Direct-USB CDC console reader (the SAFE capture path).

Reads firmware debug output straight from the CDC-Data bulk IN endpoint via
libusb (pyusb), without ever opening a /dev/cu.usbmodem* node and without
going through the macOS AppleUSBCDC kext's tty layer at all.

Why this exists (pico-link bead pico-link-4mc):
  USB CDC over the macOS tty path is implicated in kernel panics on this
  machine (see tinygo-org/tinygo#5531 — the mechanism is undocumented, so the
  risk is treated as real, not hypothetical). C-first firmware bring-up means
  many more flash-and-read cycles ahead, so this is now the DEFAULT capture
  path for all verification tooling. A crashed laptop also loses any beads
  board state since the last `bd dolt push`.

  Bonus: claiming the interface directly means there is structurally only
  ever one reader. The old tty path allowed two readers to silently steal
  bytes from each other (fragmented lines, counter jumps that look exactly
  like real firmware bugs) -- see CLAUDE.md's CDC rules.

Usage:
  python3 cdc_reader.py                       # find device, stream to stdout
  python3 cdc_reader.py --list                 # enumerate candidate devices, exit
  python3 cdc_reader.py --duration 5           # capture for 5s then exit
  python3 cdc_reader.py --out capture.log      # also append raw bytes to a file
  python3 cdc_reader.py --vid 0x2e8a --pid 0xa # override device match
  python3 cdc_reader.py --assert-dtr           # send CDC SET_CONTROL_LINE_STATE first

Exit: Ctrl-C for an open-ended capture. The interface is always released and
the device handle closed on exit -- no lingering claim, no tty ever touched.

Requires: `pip3 install pyusb` and libusb (`brew install libusb`). No sudo,
no kernel-extension changes; this has been verified to claim the CDC data
interface node on this Mac even though AppleUSBCDC keeps its (harmless,
un-openable-by-us) claim on the interface too -- see README.md in this
directory for what was actually measured, and what "harmless" means here.
"""
from __future__ import annotations

import argparse
import sys
import time
import signal
import os
import threading

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

# M3 (bd pico-link-cz0.4): the firmware's USB device topology changed from
# CDC-only to a TinyUSB composite (UAC2 speaker + CDC console + reset vendor
# interface) -- interface INDEXES moved as a result (CDC is no longer
# interface 0/1), which is exactly why find_cdc_data_interface() below
# selects by CLASS CODE rather than a hardcoded interface number. The PID
# also changed, from the CDC-only firmware's 0x000A to 0x000C, to mark the
# new composite descriptor; VID is unchanged (still pico-sdk's own
# "Raspberry Pi" allocation, since RP2350 is Raspberry Pi silicon). Override
# with --vid/--pid (or use --list) if a future firmware revision changes
# its descriptors again.
DEFAULT_VID = 0x2E8A
DEFAULT_PID = 0x000C

CDC_DATA_CLASS = 0x0A
CDC_COMM_CLASS = 0x02
SET_CONTROL_LINE_STATE = 0x22
DTR_RTS = 0x0003


def get_backend():
    # Homebrew's libusb doesn't always land somewhere pyusb's default probing
    # finds unassisted; point at it explicitly as a fallback.
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
    """Return (config, data_iface, in_ep, comm_iface_num_or_None)."""
    for cfg in dev:
        data_iface = None
        comm_iface_num = None
        for intf in cfg:
            if intf.bInterfaceClass == CDC_COMM_CLASS and comm_iface_num is None:
                comm_iface_num = intf.bInterfaceNumber
            if intf.bInterfaceClass == CDC_DATA_CLASS and data_iface is None:
                data_iface = intf
        if data_iface is not None:
            in_ep = None
            for ep in data_iface:
                if usb.util.endpoint_direction(ep.bEndpointAddress) == usb.util.ENDPOINT_IN:
                    in_ep = ep
                    break
            if in_ep is not None:
                return cfg, data_iface, in_ep, comm_iface_num
    return None, None, None, None


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
    ap.add_argument("--vid", type=lambda s: int(s, 0), default=None, help=f"USB vendor ID (default: try {DEFAULT_VID:#06x}, then any)")
    ap.add_argument("--pid", type=lambda s: int(s, 0), default=None, help=f"USB product ID (default: try {DEFAULT_PID:#06x}, then any)")
    ap.add_argument("--list", action="store_true", help="enumerate candidate devices and exit")
    ap.add_argument("--duration", type=float, default=None, help="capture for N seconds then exit (default: run until Ctrl-C)")
    ap.add_argument("--out", type=str, default=None, help="also append raw captured bytes to this file")
    ap.add_argument(
        "--assert-dtr",
        action="store_true",
        help=(
            "send CDC SET_CONTROL_LINE_STATE (DTR+RTS) before reading. "
            "Best-effort: measured to fail with EACCES on macOS (claiming the "
            "CDC-Communication interface for the control transfer is refused "
            "even though claiming CDC-Data for reads is fine) -- see README.md. "
            "Falls through to reading without DTR if it fails."
        ),
    )
    ap.add_argument(
        "--max-seconds",
        type=float,
        default=None,
        help=(
            "HARD wall-clock self-kill (bead pico-link-okx Q1). --duration is only a "
            "LOOP CONDITION, so a single dev.read() that blocks past its own 200ms "
            "timeout defeats it entirely and the process never exits. This arms a "
            "daemon watchdog THREAD that calls os._exit() regardless of what the main "
            "thread is doing. MEASURED: a watchdog thread does fire while the main "
            "thread sits inside a long blocking ctypes call (ctypes drops the GIL), "
            "exit code 75. Note this covers the finally-block bus teardown too -- "
            "release_interface()/dispose_resources() are IOKit calls that can "
            "themselves block on a wedged device."
        ),
    )
    ap.add_argument("--quiet", action="store_true", help="suppress the banner")
    ap.add_argument(
        "--stall-warn-secs",
        type=float,
        default=5.0,
        help=(
            "bead pico-link-okx: print a real-time '# STALL' warning to stderr if no bytes "
            "have been received for this many seconds (default 5s), and a final liveness "
            "summary on exit. A long capture that dies early (killed process, wedged board, "
            "USB drop) is otherwise indistinguishable from a healthy one until someone reads "
            "the log after the fact -- this makes it visible while the capture is still "
            "running, in the first stall window, not at analysis time. 0 disables."
        ),
    )
    args = ap.parse_args()

    if args.max_seconds is not None and args.max_seconds > 0:
        def _hard_wall(limit=args.max_seconds):
            time.sleep(limit)
            try:
                sys.stderr.write(
                    f"\n# HARD WALL: {limit:.1f}s elapsed, main thread did not exit "
                    f"(most likely blocked inside libusb). os._exit(75).\n"
                )
                sys.stderr.flush()
            except Exception:
                pass
            os._exit(75)
        threading.Thread(target=_hard_wall, daemon=True, name="hardwall").start()

    backend = get_backend()
    if backend is None:
        print("Could not load a libusb1 backend. Is libusb installed? (brew install libusb)", file=sys.stderr)
        sys.exit(2)

    vid, pid = args.vid, args.pid
    devs = candidate_devices(vid, pid, backend)
    if not devs and vid is None and pid is None:
        # Fall back to the known default before giving up.
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

    dev = devs[0]
    cfg, data_iface, in_ep, comm_iface_num = find_cdc_data_interface(dev)
    if data_iface is None:
        print(f"No CDC-Data interface (class 0x0A) with a bulk IN endpoint found on {describe(dev)}.", file=sys.stderr)
        sys.exit(1)

    iface_num = data_iface.bInterfaceNumber

    # Open the output file (if any) BEFORE claiming the interface, so a bad
    # --out path can never fail late and leave the interface claimed and
    # unreleased. Everything from the claim onward is inside a try/finally.
    outfile = open(args.out, "ab") if args.out else None
    comm_claimed = False

    try:
        # Best-effort: macOS routinely refuses detach_kernel_driver with
        # EACCES even though claim_interface() then succeeds anyway (measured
        # on this Mac, see README.md). Treat detach failure as non-fatal.
        try:
            if dev.is_kernel_driver_active(iface_num):
                try:
                    dev.detach_kernel_driver(iface_num)
                except Exception:
                    pass
        except (NotImplementedError, usb.core.USBError):
            pass

        usb.util.claim_interface(dev, iface_num)
        claimed = True

        if not args.quiet:
            print(f"# direct-USB CDC reader -- claimed iface {iface_num} on {describe(dev)}", file=sys.stderr)
            print("# no /dev/cu.* node opened; exclusive libusb claim", file=sys.stderr)

        if args.assert_dtr and comm_iface_num is not None:
            # The control transfer targets the CDC-Communication interface,
            # not the data interface we already claimed above -- it needs its
            # own claim or macOS refuses it with EACCES even though the data
            # interface claim succeeded fine (measured: claiming iface 1 does
            # NOT implicitly grant control-transfer access to iface 0).
            try:
                if dev.is_kernel_driver_active(comm_iface_num):
                    try:
                        dev.detach_kernel_driver(comm_iface_num)
                    except Exception:
                        pass
            except (NotImplementedError, usb.core.USBError):
                pass
            try:
                usb.util.claim_interface(dev, comm_iface_num)
                comm_claimed = True
            except usb.core.USBError as e:
                print(f"# warning: could not claim comm iface {comm_iface_num} for DTR assert: {e}", file=sys.stderr)

            try:
                dev.ctrl_transfer(0x21, SET_CONTROL_LINE_STATE, DTR_RTS, comm_iface_num, None)
                if not args.quiet:
                    print(f"# asserted DTR+RTS on comm iface {comm_iface_num}", file=sys.stderr)
            except usb.core.USBError as e:
                print(f"# warning: DTR assert failed: {e}", file=sys.stderr)

        stop = {"flag": False}

        def handle_sigint(signum, frame):
            stop["flag"] = True

        signal.signal(signal.SIGINT, handle_sigint)

        # Bead pico-link-okx: liveness tracking, independent of anything the
        # firmware prints -- a truncated capture (killed process, wedged
        # board, USB drop, this tool's own process being torn down by
        # something outside the capture loop) must be visible WHILE it is
        # happening, not discovered later by parsing report lines. total_bytes
        # and start_time give an honest "how long did this actually run and
        # how much did it actually receive" answer that does not depend on
        # trusting the deadline was reached.
        start_time = time.time()
        last_data_time = start_time
        last_stall_warn_time = None
        total_bytes = 0
        stall_warn_secs = args.stall_warn_secs if args.stall_warn_secs > 0 else None

        deadline = (time.time() + args.duration) if args.duration is not None else None
        while not stop["flag"]:
            if deadline is not None and time.time() >= deadline:
                break
            try:
                data = dev.read(in_ep.bEndpointAddress, in_ep.wMaxPacketSize, timeout=200)
            except usb.core.USBError as e:
                # errno 60 / ETIMEDOUT is the expected "no data this tick" case.
                if e.errno in (60, 110):
                    if stall_warn_secs is not None:
                        now = time.time()
                        since = now - last_data_time
                        if since >= stall_warn_secs and (
                            last_stall_warn_time is None or now - last_stall_warn_time >= stall_warn_secs
                        ):
                            print(
                                f"# STALL: no bytes received for {since:.1f}s "
                                f"(total_bytes={total_bytes}, elapsed={now - start_time:.1f}s)",
                                file=sys.stderr,
                            )
                            sys.stderr.flush()
                            last_stall_warn_time = now
                    continue
                if stop["flag"]:
                    break
                raise
            chunk = bytes(data)
            last_data_time = time.time()
            total_bytes += len(chunk)
            sys.stdout.buffer.write(chunk)
            sys.stdout.buffer.flush()
            if outfile:
                outfile.write(chunk)
                outfile.flush()
    finally:
        if outfile:
            outfile.close()
        try:
            usb.util.release_interface(dev, iface_num)
        except Exception:
            pass
        if comm_claimed:
            try:
                usb.util.release_interface(dev, comm_iface_num)
            except Exception:
                pass
        usb.util.dispose_resources(dev)
        if not args.quiet:
            print("\n# released interface, no tty was ever opened", file=sys.stderr)
            # Bead pico-link-okx: an honest summary independent of the
            # --duration argument -- if this ran for far less than
            # requested, or the last byte arrived long before exit, that is
            # the whole story right here, no log-parsing required.
            try:
                elapsed = time.time() - start_time
                since_last = time.time() - last_data_time
                print(
                    f"# summary: elapsed={elapsed:.1f}s total_bytes={total_bytes} "
                    f"since_last_byte={since_last:.1f}s requested_duration={args.duration}",
                    file=sys.stderr,
                )
            except NameError:
                pass  # failed before start_time was set (e.g. bad --out path)


if __name__ == "__main__":
    main()
