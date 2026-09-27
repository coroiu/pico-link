#!/usr/bin/env python3
"""Drives the web-companion EQ management protocol -- GET_LIBRARY (0x05),
HOST_OP (0x06), GET_OP_STATUS (0x07) -- on the "Pico Link Config" vendor
interface (bead pico-link-jyhk.21, Task 4 of
.planning/design/2026-09-27-iface6-eq-management-protocol.md). Companion to
pl_eq_import.py (0x01/0x02): same interface, same transport discipline (CLASS
bmRequestType, never VENDOR -- see that file's module doc for why), same "no
/dev/cu.usbmodem* node" rule.

Wire layouts (authoritative source: firmware/src/usb_config_itf.h,
core/src/app/library.rs, core/src/app/host_op.rs -- this tool copies them
opaquely and never invents a field):
  GET_LIBRARY (0x05), IN: header 14B (lib_proto, reserved, len u16,
    library_rev u16, flags, effect_count, effect_rec_len, device_count,
    device_rec_len, max_effects, max_devices, reserved), then effect records
    (84B: id u16, persisted_seq u16, blob[80]) then device records (42B:
    addr[6], preset_id u16, flags, name_len, name[32]).
  HOST_OP (0x06), OUT: op_proto(1)=1, op(1), seq(1), flags(1), then a
    per-op body (see OPS below).
  GET_OP_STATUS (0x07), IN: op_proto, seq, op, state (0 none/1 done/2
    rejected), error, reserved, effect_id/library_rev/persisted_seq/line/band
    (u16 each), value (f32), payload_len (u8), payload.

Blob (Preset::to_wire v2, 80B, core/src/dsp/preset.rs): version(1)=2,
name[16], flags(1), preamp_cdb i16, then band records -- this tool only ever
copies a blob whole (read from GET_LIBRARY or a PARSE_APO reply) or patches
the name field in place (rename); it never re-derives band data by hand.

Usage:
  python3 pl_eq_ops.py list
  python3 pl_eq_ops.py status [--seq N]
  python3 pl_eq_ops.py delete --id ID --base-seq N
  python3 pl_eq_ops.py assign --addr AA:BB:CC:DD:EE:FF --effect-id ID
  python3 pl_eq_ops.py rename --id ID --base-seq N --name "New Name"
  python3 pl_eq_ops.py save-apo preset.txt [--name NAME] [--replace-id ID --base-seq N]
  python3 pl_eq_ops.py preview-keepalive   # re-touches the interface (GET_LIBRARY poll) to hold an existing lease
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

DEFAULT_VID = 0x2E8A
DEFAULT_PID = 0x000C
ITF_NUM_CONFIG = 6

REQ_GET_LIBRARY = 0x05
REQ_HOST_OP = 0x06
REQ_GET_OP_STATUS = 0x07

BM_REQUEST_TYPE_OUT = 0x21  # host-to-device | class | interface
BM_REQUEST_TYPE_IN = 0xA1  # device-to-host | class | interface

MAILBOX_LEN = 1024
LIBRARY_BUF_LEN = 1536
OP_STATUS_MAX_LEN = 120

OP_PROTO = 1
BLOB_LEN = 80
NAME_MAX = 16

OP_SAVE_EFFECT = 1
OP_DELETE_EFFECT = 2
OP_ASSIGN = 3
OP_PREVIEW = 4
OP_PREVIEW_END = 5
OP_PARSE_APO = 6

FLAG_BYPASS = 1 << 0

# core/src/app/host_op.rs's OpError enum, in declaration order.
OP_ERROR_NAMES = {
    0: "none",
    1: "invalid request",
    2: "unknown op",
    3: "not ready (presets not yet loaded from flash)",
    4: "store full",
    5: "not found",
    6: "conflict (stale base_seq)",
    7: "editor open on device",
    8: "name taken",
    9: "name invalid",
    10: "blob version",
    11: "band count",
    12: "reserved band kind",
    13: "gain out of range",
    14: "freq out of range",
    15: "Q out of range",
    16: "preamp out of range",
    17: "unknown device",
    18: "parse error",
    19: "APO text too large",
}

OP_NAMES = {
    OP_SAVE_EFFECT: "SAVE_EFFECT",
    OP_DELETE_EFFECT: "DELETE_EFFECT",
    OP_ASSIGN: "ASSIGN",
    OP_PREVIEW: "PREVIEW",
    OP_PREVIEW_END: "PREVIEW_END",
    OP_PARSE_APO: "PARSE_APO",
}

OP_STATUS_HEADER_FMT = "<BBBBBBHHHHHf B"  # trailing payload_len, payload appended separately
OP_STATUS_HEADER_LEN = 21


# --- device discovery (shared shape with pl_eq_import.py) ------------------


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


# --- wire helpers ------------------------------------------------------------


def get_library(dev) -> bytes:
    return bytes(dev.ctrl_transfer(BM_REQUEST_TYPE_IN, REQ_GET_LIBRARY, 0, ITF_NUM_CONFIG, LIBRARY_BUF_LEN))


def decode_library(buf: bytes) -> dict:
    if len(buf) < 14:
        return {"ready": False}
    lib_proto, _reserved, length, library_rev, flags, effect_count, effect_rec_len, device_count, device_rec_len, max_effects, max_devices, _reserved2 = struct.unpack(
        "<BBHHBBBBBBBB", buf[:14]
    )
    effects = []
    off = 14
    for _ in range(effect_count):
        rec = buf[off:off + effect_rec_len]
        eid, pseq = struct.unpack("<HH", rec[:4])
        blob = rec[4:4 + BLOB_LEN]
        name = decode_blob_name(blob)
        effects.append({"id": eid, "persisted_seq": pseq, "name": name, "blob": blob})
        off += effect_rec_len
    devices = []
    for _ in range(device_count):
        rec = buf[off:off + device_rec_len]
        addr = rec[0:6]
        preset_id, dflags, name_len = struct.unpack("<HBB", rec[6:10])
        name = rec[10:10 + name_len].decode("utf-8", errors="replace")
        devices.append({
            "addr": ":".join(f"{b:02X}" for b in addr),
            "preset_id": preset_id,
            "connected": bool(dflags & 1),
            "name": name,
        })
        off += device_rec_len
    return {
        "ready": True,
        "lib_proto": lib_proto,
        "len": length,
        "library_rev": library_rev,
        "presets_ready": bool(flags & 1),
        "max_effects": max_effects,
        "max_devices": max_devices,
        "effects": effects,
        "devices": devices,
    }


def decode_blob_name(blob: bytes) -> str:
    name_bytes = blob[1:1 + NAME_MAX]
    end = name_bytes.find(b"\x00")
    if end < 0:
        end = len(name_bytes)
    return name_bytes[:end].decode("utf-8", errors="replace")


def patch_blob_name(blob: bytes, name: str) -> bytes:
    name_bytes = name.encode("utf-8")[:NAME_MAX]
    padded = name_bytes + b"\x00" * (NAME_MAX - len(name_bytes))
    out = bytearray(blob)
    out[1:1 + NAME_MAX] = padded
    return bytes(out)


def send_host_op(dev, op: int, seq: int, flags: int, body: bytes) -> None:
    payload = bytes([OP_PROTO, op, seq, flags]) + body
    if len(payload) > MAILBOX_LEN:
        raise ValueError(f"HOST_OP payload is {len(payload)} bytes, over the {MAILBOX_LEN}-byte mailbox")
    dev.ctrl_transfer(BM_REQUEST_TYPE_OUT, REQ_HOST_OP, 0, ITF_NUM_CONFIG, payload)


def get_op_status(dev) -> dict:
    reply = bytes(dev.ctrl_transfer(BM_REQUEST_TYPE_IN, REQ_GET_OP_STATUS, 0, ITF_NUM_CONFIG, OP_STATUS_MAX_LEN))
    if len(reply) < OP_STATUS_HEADER_LEN:
        return {"state": 0, "raw_len": len(reply)}
    op_proto, seq, op, state, error, _reserved, effect_id, library_rev, persisted_seq, line, band, value, payload_len = struct.unpack(
        "<BBBBBBHHHHHfB", reply[:OP_STATUS_HEADER_LEN]
    )
    payload = reply[OP_STATUS_HEADER_LEN:OP_STATUS_HEADER_LEN + payload_len]
    return {
        "op_proto": op_proto,
        "seq": seq,
        "op": op,
        "op_name": OP_NAMES.get(op, f"unknown({op})"),
        "state": state,
        "error": error,
        "error_name": OP_ERROR_NAMES.get(error, f"unknown({error})"),
        "effect_id": effect_id,
        "library_rev": library_rev,
        "persisted_seq": persisted_seq,
        "line": line,
        "band": band,
        "value": value,
        "payload": payload,
    }


def format_status(status: dict) -> str:
    state = status.get("state", 0)
    if state == 0:
        return "state=none (no HOST_OP has completed yet)"
    if state == 1:
        return (
            f"state=done op={status['op_name']} seq={status['seq']} "
            f"effect_id={status['effect_id']} library_rev={status['library_rev']} "
            f"persisted_seq={status['persisted_seq']}"
        )
    # rejected
    detail = f"error={status['error_name']}"
    if status["line"]:
        detail += f" line={status['line']}"
    if status["band"]:
        detail += f" band={status['band']} value={status['value']:.3g}"
    return f"state=rejected op={status['op_name']} seq={status['seq']} {detail}"


def poll_op_status(dev, seq: int, timeout: float, interval: float) -> dict:
    """Polls GET_OP_STATUS until it reports `seq` in a terminal state
    (design section 4: "the host polls GET_OP_STATUS until seq matches its
    own and state != 0"). A stale reply from a PREVIOUS op still shows the
    old seq -- never mistaken for this op's result."""
    deadline = time.monotonic() + timeout
    status = get_op_status(dev)
    while (status.get("seq") != seq or status.get("state", 0) == 0) and time.monotonic() < deadline:
        time.sleep(interval)
        status = get_op_status(dev)
    return status


def connect(vid, pid):
    dev = find_device(vid, pid)
    print(f"Using {describe(dev)}")
    try:
        dev.set_configuration()
    except usb.core.USBError:
        pass  # already configured -- benign on macOS
    return dev


# --- subcommands -------------------------------------------------------------


def cmd_list(args) -> int:
    dev = connect(args.vid, args.pid)
    lib = decode_library(get_library(dev))
    if not lib["ready"]:
        print("GET_LIBRARY: not ready yet (no snapshot published)")
        return 1
    print(f"library_rev={lib['library_rev']} presets_ready={lib['presets_ready']} "
          f"effects={len(lib['effects'])}/{lib['max_effects']} devices={len(lib['devices'])}/{lib['max_devices']}")
    for e in lib["effects"]:
        print(f"  effect id={e['id']:3d} persisted_seq={e['persisted_seq']:5d} name={e['name']!r}")
    for d in lib["devices"]:
        conn = "connected" if d["connected"] else "not connected"
        print(f"  device {d['addr']} preset_id={d['preset_id']:3d} ({conn}) name={d['name']!r}")
    return 0


def cmd_status(args) -> int:
    dev = connect(args.vid, args.pid)
    status = get_op_status(dev)
    print(format_status(status))
    return 0


def cmd_delete(args) -> int:
    dev = connect(args.vid, args.pid)
    body = struct.pack("<HH", args.id, args.base_seq)
    send_host_op(dev, OP_DELETE_EFFECT, args.seq, 0, body)
    status = poll_op_status(dev, args.seq, args.poll_timeout, args.poll_interval)
    print(format_status(status))
    return 0 if status.get("state") == 1 else 1


def parse_addr(text: str) -> bytes:
    parts = text.split(":")
    if len(parts) != 6:
        raise ValueError(f"not a MAC address: {text!r}")
    return bytes(int(p, 16) for p in parts)


def cmd_assign(args) -> int:
    dev = connect(args.vid, args.pid)
    body = parse_addr(args.addr) + struct.pack("<H", args.effect_id)
    send_host_op(dev, OP_ASSIGN, args.seq, 0, body)
    status = poll_op_status(dev, args.seq, args.poll_timeout, args.poll_interval)
    print(format_status(status))
    return 0 if status.get("state") == 1 else 1


def cmd_rename(args) -> int:
    dev = connect(args.vid, args.pid)
    lib = decode_library(get_library(dev))
    if not lib["ready"]:
        print("error: GET_LIBRARY not ready -- can't fetch the current blob to rename", file=sys.stderr)
        return 1
    effect = next((e for e in lib["effects"] if e["id"] == args.id), None)
    if effect is None:
        print(f"error: no effect with id {args.id} in the current library", file=sys.stderr)
        return 1
    new_blob = patch_blob_name(effect["blob"], args.name)
    body = struct.pack("<HH", args.id, args.base_seq) + new_blob
    send_host_op(dev, OP_SAVE_EFFECT, args.seq, 0, body)
    status = poll_op_status(dev, args.seq, args.poll_timeout, args.poll_interval)
    print(format_status(status))
    return 0 if status.get("state") == 1 else 1


def cmd_save_apo(args) -> int:
    dev = connect(args.vid, args.pid)
    apo_text = args.preset_file.read_text(encoding="utf-8")
    name = args.name if args.name is not None else args.preset_file.stem
    name_bytes = name.encode("utf-8")[:NAME_MAX]

    parse_body = bytes([len(name_bytes)]) + name_bytes + apo_text.encode("utf-8")
    send_host_op(dev, OP_PARSE_APO, args.seq, 0, parse_body)
    parse_status = poll_op_status(dev, args.seq, args.poll_timeout, args.poll_interval)
    if parse_status.get("state") != 1:
        print(f"PARSE_APO: {format_status(parse_status)}")
        return 1
    payload = parse_status["payload"]
    if len(payload) < BLOB_LEN + 2 + 1:
        print(f"error: PARSE_APO reply payload too short ({len(payload)} bytes)", file=sys.stderr)
        return 1
    blob = payload[:BLOB_LEN]
    collides_with = struct.unpack("<H", payload[BLOB_LEN:BLOB_LEN + 2])[0]
    print(f"PARSE_APO ok (collides_with={collides_with})")

    save_seq = (args.seq + 1) & 0xFF
    if args.replace_id is not None:
        save_id = args.replace_id
        base_seq = args.base_seq
    else:
        save_id = 0
        base_seq = 0
    save_body = struct.pack("<HH", save_id, base_seq) + blob
    send_host_op(dev, OP_SAVE_EFFECT, save_seq, 0, save_body)
    save_status = poll_op_status(dev, save_seq, args.poll_timeout, args.poll_interval)
    print(f"SAVE_EFFECT: {format_status(save_status)}")
    return 0 if save_status.get("state") == 1 else 1


def cmd_preview_keepalive(args) -> int:
    """Design section 7's lease: C stamps s_last_host_setup_us on EVERY
    iface-6 SETUP, so any request -- not just PREVIEW -- holds an active
    host preview open. A plain GET_LIBRARY read is the lightest touch that
    still counts."""
    dev = connect(args.vid, args.pid)
    get_library(dev)
    print("touched iface-6 (GET_LIBRARY) -- any active host preview lease is renewed")
    return 0


def cmd_preview_end(args) -> int:
    dev = connect(args.vid, args.pid)
    send_host_op(dev, OP_PREVIEW_END, args.seq, 0, b"")
    status = poll_op_status(dev, args.seq, args.poll_timeout, args.poll_interval)
    print(format_status(status))
    return 0 if status.get("state") == 1 else 1


def add_common_args(sub):
    sub.add_argument("--vid", type=lambda s: int(s, 0), default=DEFAULT_VID, help=f"USB vendor ID (default {DEFAULT_VID:#06x})")
    sub.add_argument("--pid", type=lambda s: int(s, 0), default=DEFAULT_PID, help=f"USB product ID (default {DEFAULT_PID:#06x})")


def add_op_args(sub):
    sub.add_argument("--seq", type=int, default=1, help="HOST_OP seq byte (default 1) -- the host picks it, C never parses it")
    sub.add_argument("--poll-timeout", type=float, default=3.0, help="seconds to poll GET_OP_STATUS for a terminal state (default 3.0)")
    sub.add_argument("--poll-interval", type=float, default=0.05, help="seconds between GET_OP_STATUS polls (default 0.05)")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("list", help="GET_LIBRARY: list effects and paired devices")
    add_common_args(p)
    p.set_defaults(func=cmd_list)

    p = sub.add_parser("status", help="GET_OP_STATUS: show the last HOST_OP's result")
    add_common_args(p)
    p.set_defaults(func=cmd_status)

    p = sub.add_parser("delete", help="HOST_OP DELETE_EFFECT")
    add_common_args(p)
    add_op_args(p)
    p.add_argument("--id", type=int, required=True)
    p.add_argument("--base-seq", type=int, required=True, help="the effect's current persisted_seq (from `list`) -- stale values reject with CONFLICT")
    p.set_defaults(func=cmd_delete)

    p = sub.add_parser("assign", help="HOST_OP ASSIGN (effect-id 0 = Off)")
    add_common_args(p)
    add_op_args(p)
    p.add_argument("--addr", required=True, help="paired device's BD_ADDR, e.g. 94:DB:56:54:7C:F2")
    p.add_argument("--effect-id", type=int, required=True)
    p.set_defaults(func=cmd_assign)

    p = sub.add_parser("rename", help="HOST_OP SAVE_EFFECT with only the name byte patched (blob otherwise unchanged)")
    add_common_args(p)
    add_op_args(p)
    p.add_argument("--id", type=int, required=True)
    p.add_argument("--base-seq", type=int, required=True)
    p.add_argument("--name", required=True)
    p.set_defaults(func=cmd_rename)

    p = sub.add_parser("save-apo", help="PARSE_APO then SAVE_EFFECT (create, or --replace-id to update)")
    add_common_args(p)
    add_op_args(p)
    p.add_argument("preset_file", type=Path)
    p.add_argument("--name", default=None, help="default: the file's stem")
    p.add_argument("--replace-id", type=int, default=None, help="update this existing effect instead of creating a new one")
    p.add_argument("--base-seq", type=int, default=0, help="required (and meaningful) only with --replace-id")
    p.set_defaults(func=cmd_save_apo)

    p = sub.add_parser("preview-end", help="HOST_OP PREVIEW_END")
    add_common_args(p)
    add_op_args(p)
    p.set_defaults(func=cmd_preview_end)

    p = sub.add_parser("preview-keepalive", help="touch iface-6 (GET_LIBRARY) to renew an active host-preview lease without changing anything")
    add_common_args(p)
    p.set_defaults(func=cmd_preview_keepalive)

    args = ap.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
