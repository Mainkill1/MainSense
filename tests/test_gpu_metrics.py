import struct
import unittest

from mainsense.gpu_metrics import decode_v3


def metrics_blob():
    # Hand-built public v3.0 ABI fixture. Unknown fields have all bits set.
    data = bytearray(b"\xff" * 264)
    struct.pack_into("<HBB", data, 0, 264, 3, 0)
    struct.pack_into("<HH", data, 4, 3150, 3725)
    struct.pack_into("<H", data, 174, 600)
    struct.pack_into("<H", data, 186, 1000)
    return data


class GPUMetricsTests(unittest.TestCase):
    def test_known_version_temperature_and_clock_units(self):
        values = decode_v3(metrics_blob())
        self.assertEqual(values.get("gfx_temperature_c"), 31.5)
        self.assertEqual(values.get("soc_temperature_c"), 37.25)
        self.assertEqual(values.get("memory_clock_mhz"), 1000)
        self.assertEqual(values.get("clock_mhz"), 600)

    def test_other_versions_bad_sizes_and_truncated_tables_are_rejected(self):
        good = metrics_blob()
        for data in (b"", good[:3], good[:180], good+b"\x00", b"\x08\x01\x02\x00"+good[4:],
                     b"\x08\x01\x03\x01"+good[4:], b"\x04\x01\x03\x00"+good[4:]):
            with self.subTest(header=data[:4]):
                self.assertEqual(decode_v3(data), {})

    def test_unknown_fields_never_become_huge_temperatures_or_clocks(self):
        data = metrics_blob()
        data[4:8] = b"\xff"*4
        data[186:188] = b"\xff"*2
        values = decode_v3(data)
        self.assertIsNone(values.get("gfx_temperature_c"))
        self.assertIsNone(values.get("soc_temperature_c"))
        self.assertIsNone(values.get("memory_clock_mhz"))
        self.assertEqual(values.get("clock_mhz"), 600)
