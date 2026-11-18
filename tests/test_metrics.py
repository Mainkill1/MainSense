import errno
import unittest
import tempfile
import os
from unittest.mock import patch
from pathlib import Path

from mainsense.metrics import Collector, cpu_percent, parse_cpu_stat, parse_memory
from mainsense.metrics import read_text
from test_gpu_metrics import metrics_blob


class AccountingTests(unittest.TestCase):
    def test_guest_time_is_not_counted_twice(self):
        # guest and guest_nice are already included in user and nice.
        self.assertEqual(
            parse_cpu_stat("cpu 100 20 30 400 10 5 5 0 80 10\ncpu0 1 2 3 4\nintr 99"),
            {"cpu": (570, 410), "cpu0": (10, 4)},
        )

    def test_cpu_uses_interval_deltas_and_treats_iowait_as_idle(self):
        self.assertEqual(cpu_percent((1000, 800), (1200, 900)), 50.0)
        self.assertEqual(cpu_percent((1000, 800), (1100, 900)), 0.0)
        self.assertEqual(cpu_percent((1000, 800), (1100, 800)), 100.0)

    def test_first_sample_reset_and_no_elapsed_ticks_are_unknown(self):
        for old, new in [(None, (10, 4)), ((10, 4), (10, 4)),
                         ((100, 40), (10, 4)), ((10, 9), (20, 8))]:
            with self.subTest(old=old, new=new):
                self.assertIsNone(cpu_percent(old, new))

    def test_malformed_cpu_rows_do_not_break_other_cores(self):
        self.assertEqual(parse_cpu_stat("cpu nope\ncpu0 1 2 3 4\ncpu1 -1 0 0 4"),
                         {"cpu0": (10, 4)})

    def test_ram_uses_available_and_keeps_swap_separate(self):
        memory = parse_memory("MemTotal: 1000 kB\nMemAvailable: 600 kB\n"
                              "MemFree: 100 kB\nCached: 500 kB\nBuffers: 50 kB\n"
                              "SReclaimable: 40 kB\nShmem: 10 kB\n"
                              "SwapTotal: 200 kB\nSwapFree: 150 kB\n")
        self.assertEqual(memory.get("total_bytes"), 1024000)
        self.assertEqual(memory.get("used_bytes"), 409600)
        self.assertEqual(memory.get("available_bytes"), 614400)
        self.assertEqual(memory.get("used_percent"), 40.0)
        self.assertEqual(memory.get("cache_bytes"), 593920)
        self.assertEqual(memory.get("swap_used_bytes"), 51200)

    def test_missing_available_is_unknown_not_a_false_full_ram_reading(self):
        memory = parse_memory("MemTotal: 1000 kB\nMemFree: 100 kB")
        self.assertIsNone(memory.get("used_bytes"))
        self.assertIsNone(memory.get("used_percent"))

    def test_no_swap_is_zero_and_bad_values_are_unknown(self):
        memory = parse_memory("MemTotal: bad kB\nSwapTotal: 0 kB\nSwapFree: 0 kB")
        self.assertIsNone(memory.get("total_bytes"))
        self.assertEqual(memory.get("swap_used_bytes"), 0)


class CollectorTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.write("proc/stat", "cpu 100 0 0 900\ncpu0 100 0 0 900\n")
        self.write("proc/cpuinfo", "model name : Test Ryzen\ncpu MHz : 2400\n")
        self.write("proc/meminfo", "MemTotal: 1000 kB\nMemAvailable: 750 kB\n")
        self.write("proc/loadavg", "0.10 0.20 0.30 1/2 99")
        self.write("proc/uptime", "1234.0 999.0")
        self.collector = Collector(self.root / "proc", self.root / "sys", pci_ids=self.root / "pci.ids")

    def write(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def gpu(self, card="card0"):
        base = f"sys/class/drm/{card}/device"
        for name, value in {"vendor": "0x1002", "device": "0x1586", "gpu_busy_percent": "17",
                            "mem_busy_percent": "95", "mem_info_vram_total": "103079215104",
                            "mem_info_vram_used": "32212254720", "mem_info_gtt_used": "1048576",
                            "mem_info_gtt_total": "8388608"}.items():
            self.write(f"{base}/{name}", value)
        hwmon = f"{base}/hwmon/hwmon3"
        for name, value in {"name": "amdgpu", "temp1_input": "42500", "temp1_label": "edge",
                            "power1_average": "26000000", "freq1_input": "600000000"}.items():
            self.write(f"{hwmon}/{name}", value)
        link = self.root / "sys/class/hwmon/hwmon3"
        link.parent.mkdir(parents=True, exist_ok=True)
        if not link.exists():
            link.symlink_to(self.root / hwmon)
        return base

    def test_sample_reads_actual_files_and_interval_usage(self):
        initial = self.collector.sample()
        self.assertIn("cpu", initial)
        self.assertIsNone(initial["cpu"]["used_percent"])
        self.write("proc/stat", "cpu 150 0 0 950\ncpu0 150 0 0 950\n")
        sample = self.collector.sample()
        self.assertEqual(sample["cpu"]["used_percent"], 50.0)
        self.assertEqual(sample["cpu"]["cores"], [{"id": 0, "used_percent": 50.0}])
        self.assertEqual(sample["cpu"]["model"], "Test Ryzen")
        self.assertEqual(sample["cpu"]["frequency_mhz"], 2400)
        self.assertEqual(sample["memory"]["used_bytes"], 256000)

    def test_gpu_vram_is_bytes_not_memory_activity(self):
        self.gpu()
        sample = self.collector.sample()
        self.assertIn("gpus", sample)
        gpu = sample["gpus"][0]
        self.assertEqual(gpu["used_percent"], 17)
        self.assertEqual(gpu["vram_total_bytes"], 103079215104)
        self.assertEqual(gpu["vram_used_percent"], 31.2)
        self.assertEqual(gpu["memory_busy_percent"], 95)
        self.assertEqual(gpu["temperature_c"], 42.5)
        self.assertEqual(gpu["power_w"], 26.0)
        self.assertEqual(gpu["clock_mhz"], 600.0)
        self.assertIsNone(gpu["fan_rpm"])

    def test_discovery_excludes_connectors_and_non_amd_devices(self):
        self.gpu("card2")
        self.gpu("card10")
        self.gpu("card2-DP-1")
        self.write("sys/class/drm/card1/device/vendor", "0x8086")
        sample = self.collector.sample()
        self.assertIn("gpus", sample)
        self.assertEqual([g["card"] for g in sample["gpus"]], ["card2", "card10"])

    def test_cpu_sensor_label_and_die_preference_are_preserved(self):
        for name, value in {"name": "k10temp", "temp1_input": "60000", "temp1_label": "Tctl",
                            "temp2_input": "50000", "temp2_label": "Tdie"}.items():
            self.write(f"sys/class/hwmon/hwmon2/{name}", value)
        sample = self.collector.sample()
        self.assertIn("cpu", sample)
        self.assertEqual(sample["cpu"]["temperature_c"], 50.0)
        self.assertEqual(sample["cpu"]["temperature_label"], "Tdie")
        self.assertEqual(len(sample["sensors"]), 2)

    def test_removed_gpu_and_unreadable_sensor_are_not_stale(self):
        base = self.gpu()
        self.collector.sample()
        self.write(f"{base}/gpu_busy_percent", "broken")
        self.write(f"{base}/hwmon/hwmon3/temp1_input", "broken")
        sample = self.collector.sample()
        self.assertIn("gpus", sample)
        self.assertIsNone(sample["gpus"][0]["used_percent"])
        self.assertIsNone(sample["gpus"][0]["temperature_c"])
        (self.root / base / "vendor").unlink()
        self.assertEqual(self.collector.sample()["gpus"], [])

    def test_cpu_hotplug_does_not_report_a_lifetime_average(self):
        self.collector.sample()
        self.write("proc/stat", "cpu 150 0 0 950\ncpu3 150 0 0 950\n")
        sample = self.collector.sample()
        self.assertIn("cpu", sample)
        self.assertEqual(sample["cpu"]["cores"], [{"id": 3, "used_percent": None}])

    def process(self, ticks, start=100):
        fields = ["0"] * 22
        fields[0], fields[11], fields[12], fields[19], fields[21] = "S", str(ticks), "0", str(start), "10"
        self.write("proc/123/stat", "123 (worker (test)) " + " ".join(fields))

    def test_process_usage_uses_time_deltas_and_handles_parentheses(self):
        ticks = os.sysconf("SC_CLK_TCK")
        self.process(ticks)
        first = self.collector.processes(now=10)
        self.assertEqual(len(first), 1)
        self.assertIsNone(first[0]["cpu_percent"])
        self.process(3*ticks)
        second = self.collector.processes(now=12)
        self.assertEqual(second[0]["cpu_percent"], 100.0)
        self.assertEqual(second[0]["name"], "worker (test)")
        self.assertEqual(second[0]["rss_bytes"], 10*os.sysconf("SC_PAGE_SIZE"))

    def test_reused_pid_and_exited_process_do_not_inherit_usage(self):
        self.process(100)
        self.collector.processes(now=1)
        self.process(5000, start=200)
        entries = self.collector.processes(now=2)
        self.assertEqual(len(entries), 1)
        self.assertIsNone(entries[0]["cpu_percent"])
        (self.root / "proc/123/stat").unlink()
        self.assertEqual(self.collector.processes(now=3), [])

    def test_each_gpu_temperature_is_polled_once_per_snapshot(self):
        base = self.gpu()
        target = (self.root / base / "hwmon/hwmon3/temp1_input").resolve()
        with patch("mainsense.metrics.read_text", wraps=read_text) as reads:
            snapshot = self.collector.sample()
        calls = [call for call in reads.call_args_list if call.args[0].resolve() == target]
        self.assertEqual(len(calls), 1, "A hwmon input read can trigger hardware I/O")
        detail = next(s for s in snapshot["sensors"] if s["chip"] == "amdgpu")
        self.assertEqual(snapshot["gpus"][0]["temperature_c"], detail["temperature_c"])

    def test_voltage_channels_keep_driver_labels_units_and_identity(self):
        base = self.gpu()
        self.write(f"{base}/hwmon/hwmon3/in0_label", "vddgfx")
        self.write(f"{base}/hwmon/hwmon3/in0_input", "875")
        self.write(f"{base}/hwmon/hwmon3/in1_input", "0")
        self.write("sys/class/hwmon/hwmon8/name", "board-monitor")
        self.write("sys/class/hwmon/hwmon8/in2_input", "-11900")
        self.write("sys/class/hwmon/hwmon8/in3_input", "0")
        self.write("sys/class/hwmon/hwmon8/in4_input", "unreadable")
        self.write("sys/class/hwmon/hwmon8/in2_min_alarm", "1")
        sample = self.collector.sample()
        voltages = sample.get("voltages", [])
        self.assertEqual([(v["label"], v["voltage_v"]) for v in voltages],
                         [("vddgfx", 0.875), ("in1", None), ("in2", -11.9),
                          ("in3", 0.0), ("in4", None)])
        self.assertEqual([v.get("status") for v in voltages],
                         ["ok", "driver_zero", "ok", "ok", "invalid_value"])
        self.assertEqual(sample["gpus"][0]["voltages"], voltages[:2])
        self.assertEqual(voltages[0]["id"], str((self.root / base / "hwmon/hwmon3/in0_input").resolve()))
        self.assertTrue(voltages[2]["alarm"])

    def test_sleeping_gpu_and_disabled_voltage_inputs_are_not_polled(self):
        base = self.gpu()
        chip = self.root / base / "hwmon/hwmon3"
        (chip / "device").symlink_to(self.root / base)
        self.write(f"{base}/power/runtime_status", "suspended")
        self.write(f"{base}/hwmon/hwmon3/in0_input", "875")
        self.write("sys/class/hwmon/hwmon8/in0_input", "1250")
        self.write("sys/class/hwmon/hwmon8/in0_enable", "0")
        with patch("mainsense.metrics.read_text", wraps=read_text) as reads:
            sample = self.collector.sample()
        self.assertEqual([v["voltage_v"] for v in sample.get("voltages", [])], [None, None])
        self.assertEqual([v.get("status") for v in sample["voltages"]], ["sleeping", "disabled"])
        self.assertFalse(any(call.args[0].name == "in0_input" for call in reads.call_args_list))

    def test_sensor_read_errors_keep_the_kernel_error_code(self):
        chip = "sys/class/hwmon/hwmon8"
        self.write(f"{chip}/in0_input", "1000")
        self.write(f"{chip}/in1_input", "1000")
        self.write(f"{chip}/temp1_input", "30000")
        original_read = Path.read_text

        def failed_inputs(path, **kwargs):
            failures = {"in0_input": errno.EACCES, "in1_input": errno.ENOENT, "temp1_input": errno.EIO}
            if path.name in failures:
                raise OSError(failures[path.name], "fixture read failure")
            return original_read(path, **kwargs)

        with patch.object(Path, "read_text", failed_inputs):
            sample = self.collector.sample()
        readings = sample["voltages"] + sample["sensors"]
        self.assertEqual([(s.get("status"), s.get("error_code")) for s in readings],
                         [("permission_denied", "EACCES"), ("missing", "ENOENT"), ("read_error", "EIO")])

    def test_power_supply_voltage_uses_live_microvolts(self):
        device = "sys/devices/platform/test/power_supply/BAT0"
        self.write(f"{device}/type", "Battery")
        self.write(f"{device}/voltage_now", "12050000")
        self.write("sys/class/power_supply/AC/type", "Mains")
        self.write("sys/class/power_supply/AC/voltage_max_design", "19500000")
        (self.root / "sys/class/power_supply/BAT0").symlink_to(self.root / device)
        voltages = self.collector.sample()["voltages"]
        self.assertEqual([(v["device"], v["label"], v["voltage_v"]) for v in voltages],
                         [("BAT0", "Battery voltage", 12.05)])
        self.assertTrue(voltages[0]["source"].endswith("BAT0/voltage_now"))
        (self.root / device / "voltage_now").unlink()
        self.assertEqual(self.collector.sample()["voltages"], [])

    def test_nvme_polling_is_throttled_but_cpu_temperature_stays_live(self):
        for chip, name in [("hwmon1", "nvme"), ("hwmon2", "k10temp")]:
            self.write(f"sys/class/hwmon/{chip}/name", name)
            self.write(f"sys/class/hwmon/{chip}/temp1_input", "30000")
        self.collector.sample(now=10)
        self.write("sys/class/hwmon/hwmon1/temp1_input", "40000")
        self.write("sys/class/hwmon/hwmon2/temp1_input", "50000")
        snapshot = self.collector.sample(now=10.5)
        drive = next(s for s in snapshot["sensors"] if s["chip"] == "nvme")
        self.assertEqual(drive["temperature_c"], 30.0)
        self.assertEqual(drive.get("sample_age_seconds"), 0.5)
        self.assertEqual(snapshot["cpu"]["temperature_c"], 50.0)
        snapshot = self.collector.sample(now=12)
        drive = next(s for s in snapshot["sensors"] if s["chip"] == "nvme")
        self.assertEqual(drive["temperature_c"], 40.0)
        self.assertEqual(drive.get("sample_age_seconds"), 0.0)

    def test_critical_thermal_trip_is_matched_without_duplicate_sensor(self):
        base = "sys/devices/virtual/thermal/thermal_zone0"
        self.write(f"{base}/hwmon0/name", "acpitz")
        self.write(f"{base}/hwmon0/temp1_input", "33000")
        self.write(f"{base}/trip_point_0_type", "passive")
        self.write(f"{base}/trip_point_0_temp", "80000")
        self.write(f"{base}/trip_point_1_type", "critical")
        self.write(f"{base}/trip_point_1_temp", "110000")
        link = self.root / "sys/class/hwmon/hwmon0"
        link.parent.mkdir(parents=True, exist_ok=True)
        link.symlink_to(self.root / base / "hwmon0")
        snapshot = self.collector.sample()
        self.assertEqual(len(snapshot["sensors"]), 1)
        self.assertEqual(snapshot["sensors"][0]["critical_c"], 110.0)

    def test_fault_disabled_and_sleeping_sensors_do_not_poll_inputs(self):
        base = self.gpu()
        chip = self.root / base / "hwmon/hwmon3"
        (chip / "device").symlink_to(self.root / base)
        for suffix in ("temp1_fault", "temp1_enable", "device/power/runtime_status"):
            with self.subTest(suffix=suffix):
                self.write(f"{base}/hwmon/hwmon3/{suffix}",
                           "suspended" if suffix.endswith("runtime_status") else "0" if suffix.endswith("enable") else "1")
                with patch("mainsense.metrics.read_text", wraps=read_text) as reads:
                    snapshot = self.collector.sample()
                target = (chip / "temp1_input").resolve()
                calls = [call for call in reads.call_args_list if call.args[0].resolve() == target]
                self.assertEqual(len(calls), 0)
                self.assertIsNone(snapshot["gpus"][0]["temperature_c"])
                (chip / suffix).unlink()

    def test_binary_metrics_add_soc_temperature_and_memory_clock(self):
        base = self.gpu()
        (self.root / base / "gpu_metrics").write_bytes(metrics_blob())
        snapshot = self.collector.sample()
        gpu = snapshot["gpus"][0]
        self.assertEqual(gpu["memory_clock_mhz"], 1000)
        self.assertEqual(gpu.get("memory_clock_source"), "gpu_metrics v3.0 average UCLK")
        self.assertEqual(gpu["temperature_c"], 42.5)  # hwmon edge remains authoritative
        soc = [s for s in snapshot["sensors"] if s["label"] == "SoC"]
        self.assertEqual(len(soc), 1)
        self.assertEqual(soc[0]["temperature_c"], 37.25)

    def test_critical_alarm_and_instantaneous_power_fallback(self):
        base = self.gpu()
        chip = f"{base}/hwmon/hwmon3"
        (self.root / chip / "power1_average").unlink()
        self.write(f"{chip}/power1_input", "25500000")
        self.write(f"{chip}/temp1_crit_alarm", "1")
        sample = self.collector.sample()
        self.assertEqual(sample["gpus"][0]["power_w"], 25.5)
        self.assertTrue(sample["gpus"][0]["temperatures"][0]["alarm"])

    def test_reset_after_pause_requires_fresh_cpu_and_process_baselines(self):
        self.assertTrue(callable(getattr(self.collector, "reset_baselines", None)))
        self.process(100)
        self.collector.processes(now=1)
        self.collector.sample()
        self.write("proc/stat", "cpu 150 0 0 950\ncpu0 150 0 0 950\n")
        self.process(200)
        self.collector.reset_baselines()
        self.assertIsNone(self.collector.sample()["cpu"]["used_percent"])
        self.assertIsNone(self.collector.processes(now=100)[0]["cpu_percent"])

    def test_nvme_cache_evicts_disappeared_devices(self):
        self.write("sys/class/hwmon/hwmon1/name", "nvme")
        self.write("sys/class/hwmon/hwmon1/temp1_input", "30000")
        self.collector.sample(now=1)
        import shutil
        shutil.rmtree(self.root / "sys/class/hwmon/hwmon1")
        self.assertEqual(self.collector.sample(now=1.2)["sensors"], [])
        self.assertEqual(self.collector.slow_sensors, {})


if __name__ == "__main__":
    unittest.main()
