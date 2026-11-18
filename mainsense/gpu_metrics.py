"""Decode explicitly supported versions of the kernel gpu_metrics ABI."""

import struct


def decode_v3(data):
    """Read a small, verified projection of Linux's naturally aligned v3.0 ABI.

    Offsets follow Linux v6.12 kgd_pp_interface.h (gpu_metrics_v3_0).
    Temperature units are documented in smu14_driver_if_v14_0_0.h.
    Never interpret a different revision with this layout.
    """
    if len(data) != 264 or struct.unpack_from("<HBB", data) != (264, 3, 0):
        return {}

    def u16(offset, scale=1):
        value = struct.unpack_from("<H", data, offset)[0]
        return None if value == 0xffff else value / scale

    return {"version": "3.0", "gfx_temperature_c": u16(4, 100),
            "soc_temperature_c": u16(6, 100), "clock_mhz": u16(174),
            "memory_clock_mhz": u16(186)}
