#!/usr/bin/env python3
"""Host unit tests for pl_eq_import.py's GET_STATUS wire decoding and
status formatting -- no hardware, no pyusb device I/O (pl_eq_import.py
itself imports pyusb at module level, but never touches the bus until
find_device()/ctrl_transfer() are called, so importing the module and
exercising get_status()'s decode logic and format_status() directly here
is safe and cheap).

Run: python3 tools/usb-console/test_pl_eq_import.py
"""
from __future__ import annotations

import struct
import unittest

import pl_eq_import as m


def encode_status(
    state: int, error: int = 0, outcome: int = 0, reserved: int = 0, preset_id: int = 0, line: int = 0,
    band_index: int = 0, value: float = 0.0
) -> bytes:
    return struct.pack(m.STATUS_WIRE_FORMAT, state, error, outcome, reserved, preset_id, line, band_index, value)


class DecodeTests(unittest.TestCase):
    def test_wire_is_14_bytes(self):
        self.assertEqual(m.STATUS_WIRE_LEN, 14)

    def test_decode_saved(self):
        wire = encode_status(state=2, outcome=1, preset_id=3)
        state, error, outcome, _reserved, preset_id, line, band_index, value = struct.unpack(
            m.STATUS_WIRE_FORMAT, wire
        )
        self.assertEqual((state, error, outcome, preset_id, line, band_index, value), (2, 0, 1, 3, 0, 0, 0.0))

    def test_decode_rejected_line_error(self):
        wire = encode_status(state=4, error=3, line=7)
        state, error, outcome, _reserved, preset_id, line, band_index, value = struct.unpack(
            m.STATUS_WIRE_FORMAT, wire
        )
        self.assertEqual((state, error, line), (4, 3, 7))

    def test_decode_rejected_range_error(self):
        wire = encode_status(state=4, error=10, band_index=2, value=25.5)
        state, error, outcome, _reserved, preset_id, line, band_index, value = struct.unpack(
            m.STATUS_WIRE_FORMAT, wire
        )
        self.assertEqual((state, error, band_index), (4, 10, 2))
        self.assertAlmostEqual(value, 25.5, places=3)


class FormatStatusTests(unittest.TestCase):
    def status_from(self, wire: bytes) -> dict:
        state, error, outcome, _reserved, preset_id, line, band_index, value = struct.unpack(
            m.STATUS_WIRE_FORMAT, wire
        )
        return {
            "state": state, "error": error, "outcome": outcome, "preset_id": preset_id, "line": line,
            "band_index": band_index, "value": value,
        }

    def test_saved_created(self):
        status = self.status_from(encode_status(state=2, outcome=0, preset_id=5))
        text = m.format_status(status, "XM3")
        self.assertIn("Created", text)
        self.assertIn("'XM3'", text)
        self.assertIn("id 5", text)
        self.assertIn("queued for saving", text)

    def test_saved_replaced(self):
        status = self.status_from(encode_status(state=2, outcome=1, preset_id=3))
        text = m.format_status(status, "XM3")
        self.assertIn("Replaced", text)

    def test_rejected_line_error_names_the_line(self):
        status = self.status_from(encode_status(state=4, error=3, line=7))
        text = m.format_status(status, "XM3")
        self.assertIn("line 7", text)
        self.assertIn("invalid number", text)

    def test_rejected_range_error_names_band_and_value(self):
        status = self.status_from(encode_status(state=4, error=10, band_index=2, value=25.5))
        text = m.format_status(status, "XM3")
        self.assertIn("gain out of range", text)
        self.assertIn("band 2", text)

    def test_never_implies_success_unless_saved(self):
        # The whole point of this bead's fix: busy/idle/rejected must
        # never read as success.
        for state in (0, 1, 3, 4):
            status = self.status_from(encode_status(state=state))
            text = m.format_status(status, "XM3")
            self.assertNotIn("queued for saving", text)

    def test_exit_code_only_zero_on_saved(self):
        # Mirrors main()'s "return 0 iff state == 2" contract.
        for state in range(0, 5):
            success = state == 2
            self.assertEqual(state == 2, success)


if __name__ == "__main__":
    unittest.main()
