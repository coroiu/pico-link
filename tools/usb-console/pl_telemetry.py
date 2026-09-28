#!/usr/bin/env python3
"""Polls the "Pico Link Config" vendor interface's GET_TELEMETRY / GET_INFO
CLASS requests (bead `pico-link-jyhk.4`, "ADA DESIGN" comment on
`pico-link-jyhk.1`, sections 3-5) and decodes the page-0 Home snapshot
whose wire layout `core/src/app/telemetry.rs` owns.

Pattern after pl_eq_import.py in this directory: plain pyusb control
transfers on iface 6, never a /dev/cu.usbmodem* node (CLAUDE.md's CDC
rules -- the tty path is implicated in kernel panics on this Mac). This
tool does not touch CDC at all.

Wire protocol (usb_config_itf.h is authoritative; this module's decode
mirrors core/src/app/telemetry.rs's doc comment, "Layout, page 0, proto
1" table):
  GET_INFO (bRequest 0x04), IN, class/interface, wIndex=6:
    reply = pl_cfg_info_wire_t (info_ver, import_proto, status_ver,
            telemetry_proto, telemetry_page_mask(u32), version_len,
            version[32]) -- 40 bytes.
  GET_TELEMETRY (bRequest 0x03), IN, class/interface, wIndex=6,
  wValue=page id (0 = Home, the only page today):
    reply = HOME_SNAPSHOT_LEN (163) bytes, decoded per
    core/src/app/telemetry.rs's layout table. snap_seq == 0 means "not
    ready" (no GET_TELEMETRY poll has reached the superloop generator
    yet, or it has been over 1s since the last one).
    A page id with no bit set in telemetry_page_mask STALLs cleanly.

Usage:
  python3 pl_telemetry.py                      # live 30Hz line, Ctrl-C to stop
  python3 pl_telemetry.py --rate 20             # poll at 20Hz instead
  python3 pl_telemetry.py --duration 120        # measurement mode: run for 120s, print summary
  python3 pl_telemetry.py --count 500           # measurement mode: stop after 500 polls, print summary
  python3 pl_telemetry.py --hammer 60           # robustness: poll as fast as possible for 60s
  python3 pl_telemetry.py --stall-check         # send wValue=1 (unsupported page), confirm STALL

Requires the same pyusb + libusb dependency as pl_eq_import.py.
"""
from __future__ import annotations

import argparse
import struct
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

# Matches cdc_reader.py/pl_eq_import.py's defaults -- same composite
# device, same VID/PID (M3, bd pico-link-cz0.4).
DEFAULT_VID = 0x2E8A
DEFAULT_PID = 0x000C

# ITF_NUM_CONFIG in firmware/src/usb_descriptors.h -- the one place,
# besides the firmware itself, that has to agree on the number.
ITF_NUM_CONFIG = 6

REQ_GET_STATUS = 0x02
REQ_GET_TELEMETRY = 0x03
REQ_GET_INFO = 0x04

TELEMETRY_PAGE_HOME = 0

# bmRequestType bytes: CLASS request (not VENDOR), interface recipient, IN.
# See pl_eq_import.py's module doc for why this must be CLASS on pico-sdk
# 2.1.1's TinyUSB usbd.c.
BM_REQUEST_TYPE_IN = 0xA1  # device-to-host | class | interface

# --- GET_INFO wire format: pl_cfg_info_wire_t (usb_config_itf.h) -------
# u8 info_ver, u8 import_proto, u8 status_ver, u8 telemetry_proto,
# u32 telemetry_page_mask, u8 version_len, u8 version[32].
INFO_WIRE_FORMAT = "<BBBBI B32s"
INFO_WIRE_LEN = struct.calcsize(INFO_WIRE_FORMAT)  # 40 bytes

# --- GET_TELEMETRY page-0 wire format: core/src/app/telemetry.rs -------
# Header: proto(u8) page(u8) len(u16) uptime_ms(u32) snap_seq(u32)
#         link(u8) flags(u8) kbps(u16)
#         codec_len(u8) codec[8]
#         name_len(u8) name[32]
#         fx_len(u8) fx[16]
#         vol_level(u8) vol_source(u8)
#         peak_l(u8) peak_r(u8) rms_l(u8) rms_r(u8) received_ms(u32)
# Then 6 fault slots x 13 bytes: count(u16) first_seen_ms(u32)
#         last_seen_ms(u32) value_kind(u8) value(u16)
HOME_HEADER_FORMAT = "<BBHII BBH B8s B32s B16s BB BBBB I"
HOME_HEADER_LEN = struct.calcsize(HOME_HEADER_FORMAT)
FAULT_SLOT_FORMAT = "<HIIBH"
FAULT_SLOT_LEN = struct.calcsize(FAULT_SLOT_FORMAT)  # 13 bytes
FAULT_SLOT_COUNT = 6
HOME_SNAPSHOT_LEN = HOME_HEADER_LEN + FAULT_SLOT_COUNT * FAULT_SLOT_LEN

# FaultKey::ALL order (core/src/app/fault.rs).
FAULT_KEY_NAMES = [
    "BufStarved",
    "BufOverflow",
    "UsbSupplyLow",
    "AirCongested",
    "AirLinkLost",
    "EncResync",
]

VALUE_KIND_NAMES = {0: "none", 1: "ratio", 2: "count", 3: "millis"}
VOLUME_SOURCE_NAMES = {0: "host", 1: "sink", 2: "device"}

FLAG_ADAPTIVE = 1 << 0
FLAG_KBPS_IS_LIVE = 1 << 1
FLAG_VOLUME_PRESENT = 1 << 2
FLAG_MUTED = 1 << 3
FLAG_LEVEL_PRESENT = 1 << 4

assert HOME_SNAPSHOT_LEN == 163, f"decoder layout drifted from the design's 163 bytes: got {HOME_SNAPSHOT_LEN}"


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


def get_info(dev) -> dict:
    reply = dev.ctrl_transfer(BM_REQUEST_TYPE_IN, REQ_GET_INFO, 0, ITF_NUM_CONFIG, INFO_WIRE_LEN)
    info_ver, import_proto, status_ver, telemetry_proto, page_mask, version_len, version_raw = struct.unpack(
        INFO_WIRE_FORMAT, bytes(reply)
    )
    version = version_raw[: min(version_len, len(version_raw))].decode("utf-8", errors="replace")
    return {
        "info_ver": info_ver,
        "import_proto": import_proto,
        "status_ver": status_ver,
        "telemetry_proto": telemetry_proto,
        "page_mask": page_mask,
        "version": version,
    }


def get_telemetry_raw(dev, page: int = TELEMETRY_PAGE_HOME) -> bytes:
    return bytes(dev.ctrl_transfer(BM_REQUEST_TYPE_IN, REQ_GET_TELEMETRY, page, ITF_NUM_CONFIG, HOME_SNAPSHOT_LEN))


def _not_ready_snapshot(raw_len: int) -> dict:
    """A too-short (possibly zero-byte) GET_TELEMETRY reply, decoded as
    "not ready" rather than raised as an error (bead `pico-link-s6hh`).

    As of that bead, healthy firmware always replies at least
    HOME_SNAPSHOT_LEN bytes (a synthesized snap_seq-0 header before the
    first real snapshot) -- see usb_config_itf.c's configd_init. A reply
    shorter than that only happens against OLDER firmware that still
    replies zero bytes pre-boot-settle, so this is a same-shaped fallback
    dict (snap_seq 0, everything else zeroed/empty) rather than a second
    error path callers must special-case.
    """
    return {
        "proto": 0,
        "page": 0,
        "len": raw_len,
        "uptime_ms": 0,
        "snap_seq": 0,
        "link_connected": False,
        "adaptive": False,
        "kbps_is_live": False,
        "volume_present": False,
        "muted": False,
        "level_present": False,
        "kbps": 0,
        "codec_word": "",
        "device_name": "",
        "fx_preset_name": "",
        "volume_level": 0,
        "volume_source": "unknown(0)",
        "peak_l": 0,
        "peak_r": 0,
        "rms_l": 0,
        "rms_r": 0,
        "received_ms": 0,
        "faults": [],
    }


def decode_home_snapshot(raw: bytes) -> dict:
    if len(raw) < HOME_SNAPSHOT_LEN:
        return _not_ready_snapshot(len(raw))

    (
        proto, page, length, uptime_ms, snap_seq,
        link, flags, kbps,
        codec_len, codec_raw,
        name_len, name_raw,
        fx_len, fx_raw,
        vol_level, vol_source,
        peak_l, peak_r, rms_l, rms_r,
        received_ms,
    ) = struct.unpack(HOME_HEADER_FORMAT, raw[:HOME_HEADER_LEN])

    faults = []
    for i in range(FAULT_SLOT_COUNT):
        off = HOME_HEADER_LEN + i * FAULT_SLOT_LEN
        count, first_seen_ms, last_seen_ms, value_kind, value = struct.unpack(
            FAULT_SLOT_FORMAT, raw[off : off + FAULT_SLOT_LEN]
        )
        if count == 0:
            continue
        faults.append(
            {
                "key": FAULT_KEY_NAMES[i],
                "count": count,
                "first_seen_ms": first_seen_ms,
                "last_seen_ms": last_seen_ms,
                "value_kind": VALUE_KIND_NAMES.get(value_kind, f"unknown({value_kind})"),
                "value": value,
            }
        )

    return {
        "proto": proto,
        "page": page,
        "len": length,
        "uptime_ms": uptime_ms,
        "snap_seq": snap_seq,
        "link_connected": bool(link),
        "adaptive": bool(flags & FLAG_ADAPTIVE),
        "kbps_is_live": bool(flags & FLAG_KBPS_IS_LIVE),
        "volume_present": bool(flags & FLAG_VOLUME_PRESENT),
        "muted": bool(flags & FLAG_MUTED),
        "level_present": bool(flags & FLAG_LEVEL_PRESENT),
        "kbps": kbps,
        "codec_word": codec_raw[: min(codec_len, len(codec_raw))].decode("utf-8", errors="replace"),
        "device_name": name_raw[: min(name_len, len(name_raw))].decode("utf-8", errors="replace"),
        "fx_preset_name": fx_raw[: min(fx_len, len(fx_raw))].decode("utf-8", errors="replace"),
        "volume_level": vol_level,
        "volume_source": VOLUME_SOURCE_NAMES.get(vol_source, f"unknown({vol_source})"),
        "peak_l": peak_l,
        "peak_r": peak_r,
        "rms_l": rms_l,
        "rms_r": rms_r,
        "received_ms": received_ms,
        "faults": faults,
    }


def format_compact_line(snap: dict, poll_num: int, rtt_ms: float) -> str:
    if snap["snap_seq"] == 0:
        return f"#{poll_num:6d} not-ready (snap_seq=0) rtt={rtt_ms:5.1f}ms"

    link = "LINK" if snap["link_connected"] else "----"
    codec = snap["codec_word"] or "-"
    kbps = f"{snap['kbps']}kbps" if snap["link_connected"] else "-"
    live = "L" if snap["kbps_is_live"] else " "
    adaptive = "A" if snap["adaptive"] else " "
    device = snap["device_name"] or "-"
    fx = snap["fx_preset_name"] or "Off"
    vol = f"{snap['volume_level']}/127" if snap["volume_present"] else "-"
    muted = "MUTE" if snap["muted"] else "    "
    levels = (
        f"pk={snap['peak_l']:3d}/{snap['peak_r']:3d} rms={snap['rms_l']:3d}/{snap['rms_r']:3d}"
        if snap["level_present"]
        else "pk=  -/  - rms=  -/  -"
    )
    faults = ",".join(f"{f['key']}={f['count']}" for f in snap["faults"]) or "none"

    return (
        f"#{poll_num:6d} {link} {codec:<4s} {kbps:>9s}{live}{adaptive} dev={device:<16.16s} "
        f"fx={fx:<10.10s} vol={vol:>7s}{muted} {levels} seq={snap['snap_seq']:10d} "
        f"faults=[{faults}] rtt={rtt_ms:5.1f}ms"
    )


def run_stall_check(dev) -> int:
    """Sends GET_TELEMETRY with wValue=1 (an unsupported page) and confirms
    the device STALLs cleanly (usb_config_itf.h: "Stalls if wValue names an
    unsupported page")."""
    try:
        dev.ctrl_transfer(BM_REQUEST_TYPE_IN, REQ_GET_TELEMETRY, 1, ITF_NUM_CONFIG, HOME_SNAPSHOT_LEN)
    except usb.core.USBError as e:
        print(f"GET_TELEMETRY wValue=1: STALLed as expected ({e})")
        return 0
    print("error: GET_TELEMETRY wValue=1 did NOT stall -- unsupported page accepted", file=sys.stderr)
    return 1


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--vid", type=lambda s: int(s, 0), default=DEFAULT_VID)
    ap.add_argument("--pid", type=lambda s: int(s, 0), default=DEFAULT_PID)
    ap.add_argument("--rate", type=float, default=30.0, help="poll rate in Hz (default 30)")
    ap.add_argument("--count", type=int, default=None, help="stop after N polls; print a summary instead of a live line per poll")
    ap.add_argument("--duration", type=float, default=None, help="stop after N seconds; print a summary instead of a live line per poll")
    ap.add_argument("--hammer", type=float, default=None, help="robustness mode: poll as fast as possible for N seconds, ignore --rate")
    ap.add_argument("--stall-check", action="store_true", help="send GET_TELEMETRY wValue=1 (unsupported page), confirm STALL, exit")
    ap.add_argument("--quiet", action="store_true", help="measurement modes only: suppress the per-poll line, print just the summary")
    args = ap.parse_args()

    dev = find_device(args.vid, args.pid)
    print(f"Using {describe(dev)}")
    try:
        dev.set_configuration()
    except usb.core.USBError:
        pass  # already configured -- benign on macOS

    if args.stall_check:
        return run_stall_check(dev)

    info = get_info(dev)
    print(
        f"GET_INFO: info_ver={info['info_ver']} import_proto={info['import_proto']} "
        f"status_ver={info['status_ver']} telemetry_proto={info['telemetry_proto']} "
        f"page_mask={info['page_mask']:#x} fw={info['version']!r}"
    )
    if not (info["page_mask"] & (1 << TELEMETRY_PAGE_HOME)):
        print("error: firmware reports Home page not supported (page_mask bit0 clear)", file=sys.stderr)
        return 1

    measurement_mode = args.count is not None or args.duration is not None or args.hammer is not None
    hammer_mode = args.hammer is not None
    interval = 0.0 if hammer_mode else 1.0 / args.rate
    deadline = time.monotonic() + (args.hammer if hammer_mode else args.duration) if (hammer_mode or args.duration is not None) else None

    poll_num = 0
    errors = 0
    not_ready = 0
    seq_seen = set()
    seq_regressions = 0
    last_seq = None
    rtts = []
    last_snap = None

    try:
        while True:
            if args.count is not None and poll_num >= args.count:
                break
            if deadline is not None and time.monotonic() >= deadline:
                break

            t0 = time.monotonic()
            try:
                raw = get_telemetry_raw(dev)
                snap = decode_home_snapshot(raw)
            except (usb.core.USBError, ValueError) as e:
                errors += 1
                poll_num += 1
                if not args.quiet:
                    print(f"#{poll_num:6d} ERROR: {e}")
                continue
            rtt_ms = (time.monotonic() - t0) * 1000.0
            rtts.append(rtt_ms)
            poll_num += 1
            last_snap = snap

            if snap["snap_seq"] == 0:
                not_ready += 1
            else:
                if last_seq is not None and snap["snap_seq"] < last_seq:
                    seq_regressions += 1
                last_seq = snap["snap_seq"]
                seq_seen.add(snap["snap_seq"])

            if not measurement_mode or not args.quiet:
                print(format_compact_line(snap, poll_num, rtt_ms))

            if not hammer_mode and interval > 0:
                sleep_for = interval - (time.monotonic() - t0)
                if sleep_for > 0:
                    time.sleep(sleep_for)
    except KeyboardInterrupt:
        pass

    if measurement_mode:
        print()
        print("--- summary ---")
        print(f"polls: {poll_num}  errors: {errors}  not_ready: {not_ready}  distinct_snap_seq: {len(seq_seen)}  seq_regressions: {seq_regressions}")
        if rtts:
            rtts_sorted = sorted(rtts)
            n = len(rtts_sorted)
            p50 = rtts_sorted[n // 2]
            p99 = rtts_sorted[min(n - 1, int(n * 0.99))]
            print(f"rtt_ms: min={rtts_sorted[0]:.2f} p50={p50:.2f} p99={p99:.2f} max={rtts_sorted[-1]:.2f}")
        if last_snap is not None:
            print(f"last snapshot: {format_compact_line(last_snap, poll_num, rtts[-1] if rtts else 0.0)}")

    return 0 if errors == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
