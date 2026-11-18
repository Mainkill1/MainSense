"""Read Linux's procfs and sysfs monitoring interfaces."""

import errno
import math
import os
import re
import socket
import time
from pathlib import Path

from .gpu_metrics import decode_v3


def parse_cpu_stat(text):
    counters = {}
    for line in text.splitlines():
        fields = line.split()
        if not fields or not re.fullmatch(r"cpu\d*", fields[0]):
            continue
        try:
            values = [int(x) for x in fields[1:9]]
        except ValueError:
            continue
        if len(values) < 4 or any(x < 0 for x in values):
            continue
        # guest times (columns 9 and 10) already contribute to user/nice.
        counters[fields[0]] = (sum(values), values[3] + (values[4] if len(values) > 4 else 0))
    return counters


def cpu_percent(previous, current):
    if previous is None or current is None:
        return None
    elapsed, idle = current[0] - previous[0], current[1] - previous[1]
    if elapsed <= 0 or idle < 0 or idle > elapsed:
        return None
    return round(100 * (elapsed - idle) / elapsed, 1)


def parse_memory(text):
    values = {}
    for line in text.splitlines():
        fields = line.split()
        if len(fields) < 2:
            continue
        try:
            value = int(fields[1]) * 1024
        except ValueError:
            continue
        if value >= 0:
            values[fields[0].rstrip(":")] = value
    total, available = values.get("MemTotal"), values.get("MemAvailable")
    used = difference(total, available)
    swap_total, swap_free = values.get("SwapTotal"), values.get("SwapFree")
    cache_keys = ("Cached", "Buffers", "SReclaimable", "Shmem")
    cache = (max(0, values["Cached"] + values["Buffers"] + values["SReclaimable"] - values["Shmem"])
             if all(key in values for key in cache_keys) else None)
    return {"total_bytes": total, "available_bytes": available, "used_bytes": used,
            "used_percent": percent(used, total), "cache_bytes": cache,
            "swap_total_bytes": swap_total, "swap_used_bytes": difference(swap_total, swap_free)}


def difference(total, available):
    if total is None or available is None or available > total:
        return None
    return total - available


def percent(used, total):
    return round(100 * used / total, 1) if used is not None and total else None


class Collector:
    def reset_baselines(self):
        """Resume with fresh deltas instead of averaging over a paused period."""
        self.previous = {}
        self.previous_processes = {}

    def processes(self, now=None):
        now = time.monotonic() if now is None else now
        entries, current = [], {}
        for directory in self.proc.glob("[0-9]*"):
            if not directory.name.isdigit():
                continue
            text = read_text(directory / "stat")
            end = text.rfind(")")
            start = text.find("(")
            if start < 0 or end < start:
                continue
            fields = text[end+1:].split()
            try:
                ticks = int(fields[11]) + int(fields[12])
                born, rss = int(fields[19]), int(fields[21])
            except (IndexError, ValueError):
                continue
            pid = int(directory.name)
            key = (pid, born)
            current[key] = (ticks, now)
            old = self.previous_processes.get(key)
            usage = None
            if old and now > old[1] and ticks >= old[0]:
                usage = round((ticks-old[0]) / self.clock_ticks / (now-old[1]) * 100, 1)
            entries.append({"pid": pid, "name": text[start+1:end], "cpu_percent": usage,
                            "rss_bytes": max(0, rss) * self.page_size})
        self.previous_processes = current
        return sorted(entries, key=lambda p: (p["cpu_percent"] if p["cpu_percent"] is not None else -1,
                                               p["rss_bytes"]), reverse=True)

    def __init__(self, proc_root="/proc", sys_root="/sys", pci_ids=None):
        self.proc = Path(proc_root)
        self.sys = Path(sys_root)
        self.previous = {}
        self.previous_processes = {}
        self.slow_sensors = {}
        self.clock_ticks = os.sysconf("SC_CLK_TCK")
        self.page_size = os.sysconf("SC_PAGE_SIZE")
        self.kernel_release = os.uname().release
        self.gpu_names = {}
        database = Path(pci_ids) if pci_ids else Path("/usr/share/misc/pci.ids")
        in_amd = False
        for line in read_text(database).splitlines():
            if line and not line.startswith(("\t", "#")):
                in_amd = line.startswith("1002 ")
            if in_amd and re.match(r"\t[0-9a-fA-F]{4}  ", line):
                device, name = line.strip().split(None, 1)
                self.gpu_names[device.lower()] = name

    def sample(self, now=None):
        now = time.monotonic() if now is None else now
        counters = parse_cpu_stat(read_text(self.proc / "stat"))
        cpuinfo = read_text(self.proc / "cpuinfo")
        model = re.search(r"^model name\s*:\s*(.+)$", cpuinfo, re.MULTILINE)
        frequencies = [float(v) for v in re.findall(r"^cpu MHz\s*:\s*([0-9.]+)$", cpuinfo, re.MULTILINE)
                       if re.fullmatch(r"\d+(?:\.\d+)?", v)]
        sensors, voltages, sensors_by_chip, current_slow = [], {}, {}, {}
        for chip in sorted((self.sys / "class/hwmon").glob("hwmon*")):
            identity = str(chip.resolve())
            name = read_text(chip / "name").strip()
            # NVMe temperature/alarm reads can each submit a device command.
            # Keep CPU/GPU live, but query drive health at most every 2 seconds.
            cached = self.slow_sensors.get(identity) if name == "nvme" else None
            if cached and 0 <= now-cached[0] < 2:
                sampled_at, readings = cached
            else:
                sampled_at, readings = now, read_sensors(chip, name=name)
            if name == "nvme":
                current_slow[identity] = (sampled_at, readings)
            age = round(now-sampled_at, 3)
            temperatures, chip_voltages = (
                [dict(sensor, sample_age_seconds=age) for sensor in channels] for channels in readings
            )
            sensors_by_chip[identity] = temperatures, chip_voltages
            sensors.extend(temperatures)
            voltages.update((sensor["id"], sensor) for sensor in chip_voltages)
        self.slow_sensors = current_slow  # Evict removed devices; keep cache bounded.
        cpu_sensors = [s for s in sensors if s["kind"] == "cpu" and s["temperature_c"] is not None]
        # A die reading is preferable to a control reading. Across packages,
        # display the hottest preferred sensor and retain every sensor in details.
        preferred = [s for s in cpu_sensors if s["label"] == "Tdie"]
        preferred = preferred or [s for s in cpu_sensors if s["label"] == "Tctl" or s["label"].startswith("Package")]
        primary = max(preferred or cpu_sensors, key=lambda s: s["temperature_c"], default={})
        cpu = {"model": model[1].strip() if model else "CPU",
               "used_percent": cpu_percent(self.previous.get("cpu"), counters.get("cpu")),
               "cores": [{"id": int(key[3:]), "used_percent": cpu_percent(self.previous.get(key), counters[key])}
                         for key in sorted((k for k in counters if k != "cpu"), key=lambda k: int(k[3:]))],
               "frequency_mhz": round(sum(frequencies) / len(frequencies)) if frequencies else None,
               "temperature_c": primary.get("temperature_c"),
               "temperature_label": primary.get("label", "unavailable")}
        self.previous = counters
        gpus = []
        cards = [p for p in (self.sys / "class/drm").glob("card*") if re.fullmatch(r"card\d+", p.name)]
        for card in sorted(cards, key=lambda p: int(p.name[4:])):
            device = card / "device"
            if read_text(device / "vendor").strip() != "0x1002":
                continue
            gpu = self.read_gpu(card, sensors_by_chip)
            gpus.append(gpu)
            known = {s["id"] for s in sensors}
            sensors.extend(s for s in gpu["temperatures"] if s["id"] not in known)
            voltages.update((sensor["id"], sensor) for sensor in gpu["voltages"])
        voltages.update((sensor["id"], sensor)
                        for sensor in read_supply_voltages(self.sys / "class/power_supply"))
        warnings = []
        if "cpu" not in counters:
            warnings.append("CPU counters unavailable: cannot read /proc/stat")
        if not gpus:
            warnings.append("No AMD GPU visible in /sys/class/drm")
        if not cpu_sensors:
            warnings.append("CPU temperature unavailable: no readable CPU hwmon sensor")
        return {"schema_version": 1, "timestamp": time.time(), "monotonic": now,
                "hostname": socket.gethostname(), "kernel_release": self.kernel_release, "cpu": cpu,
                "memory": parse_memory(read_text(self.proc / "meminfo")),
                "gpus": gpus, "sensors": sensors, "voltages": list(voltages.values()), "warnings": warnings,
                "load_average": read_text(self.proc / "loadavg").split()[:3],
                "uptime_seconds": first_number(read_text(self.proc / "uptime"))}

    def read_gpu(self, card, sensors_by_chip):
        device = card / "device"
        device_id = read_text(device / "device").strip().removeprefix("0x")
        name = read_text(device / "product_name").strip() or self.gpu_names.get(device_id, f"AMD GPU [1002:{device_id}]")
        status = read_text(device / "power/runtime_status").strip()
        sleeping = status in ("suspended", "suspending")
        chips = sorted((device / "hwmon").glob("hwmon*"))
        chip = chips[0] if chips else device / "hwmon/unavailable"
        readings = sensors_by_chip.get(str(chip.resolve()))
        if readings is None:
            readings = ([], read_voltages(chip, "amdgpu", "gpu", sleeping=True)) if sleeping else read_sensors(chip)
        temperatures, voltages = ([dict(s, sample_age_seconds=s.get("sample_age_seconds", 0)) for s in group]
                                  for group in readings)
        if sleeping:
            temperatures = [dict(s, temperature_c=None, status="sleeping", error_code=None) for s in temperatures]
            voltages = [dict(s, voltage_v=None, status="sleeping", error_code=None, alarm=False) for s in voltages]
        valid = [s for s in temperatures if s["temperature_c"] is not None]
        primary = next((s for s in valid if s["label"].lower() == "edge"), valid[0] if valid else {})
        def value(name, scale=1):
            return None if sleeping else read_number(device / name, scale)
        def sensor(name, scale=1):
            return None if sleeping else read_number(chip / name, scale)
        used, total = value("mem_info_vram_used"), value("mem_info_vram_total")
        power = sensor("power1_average", 1_000_000)
        metrics = {}
        metrics_path = device / "gpu_metrics"
        if not sleeping:
            try:
                # A bounded read also rejects unexpected longer versions.
                with metrics_path.open("rb") as stream:
                    metrics = decode_v3(stream.read(265))
            except OSError:
                pass
        if metrics:
            soc_temperature = metrics["soc_temperature_c"]
            temperatures.append({"id": str(metrics_path.resolve())+"#soc", "chip": "amdgpu_metrics",
                                 "device": card.name, "kind": "gpu", "label": "SoC",
                                 "temperature_c": soc_temperature, "critical_c": None,
                                 "status": "ok" if soc_temperature is not None else "unsupported", "error_code": None,
                                 "alarm": False, "source": str(metrics_path), "sample_age_seconds": 0})
        memory_clock = sensor("freq2_input", 1_000_000)
        clock_source = "hwmon freq2_input" if memory_clock is not None else None
        if memory_clock is None and metrics.get("memory_clock_mhz") is not None:
            memory_clock, clock_source = metrics["memory_clock_mhz"], "gpu_metrics v3.0 average UCLK"
        return {"id": str(device.resolve()), "card": card.name, "name": name,
                "pci_address": device.resolve().name, "status": "sleeping" if sleeping else "available",
                "used_percent": bounded_percent(value("gpu_busy_percent")),
                "memory_busy_percent": bounded_percent(value("mem_busy_percent")),
                "vram_used_bytes": used, "vram_total_bytes": total,
                "vram_used_percent": percent(used, total),
                "gtt_used_bytes": value("mem_info_gtt_used"), "gtt_total_bytes": value("mem_info_gtt_total"),
                "temperature_c": primary.get("temperature_c"), "temperature_label": primary.get("label", "unavailable"),
                "temperatures": temperatures, "voltages": voltages,
                "power_w": power if power is not None else sensor("power1_input", 1_000_000),
                "clock_mhz": sensor("freq1_input", 1_000_000),
                "memory_clock_mhz": memory_clock, "memory_clock_source": clock_source,
                "metrics": metrics, "fan_rpm": sensor("fan1_input")}


def read_text(path, *, strict=False):
    try:
        return path.read_text(errors="replace")
    except OSError:
        if strict:
            raise
        return ""


def first_number(text):
    try:
        value = float(text.split()[0])
        return value if math.isfinite(value) else None
    except (ValueError, IndexError):
        return None


def read_number(path, scale=1, signed=False):
    try:
        value = int(read_text(path).strip())
    except ValueError:
        return None
    if value < 0 and not signed:
        return None
    return value if scale == 1 else value / scale


def bounded_percent(value):
    return value if value is not None and 0 <= value <= 100 else None


def channel_status(chip, channel, sleeping):
    if sleeping:
        return "sleeping"
    if read_number(chip / f"{channel}_fault") == 1:
        return "fault"
    if read_number(chip / f"{channel}_enable") == 0:
        return "disabled"
    return "ok"


def read_measurement(path, scale, status="ok"):
    if status != "ok":
        return None, status, None
    try:
        return int(read_text(path, strict=True)) / scale, "ok", None
    except PermissionError as error:
        return None, "permission_denied", errno.errorcode.get(error.errno)
    except FileNotFoundError as error:
        return None, "missing", errno.errorcode.get(error.errno)
    except OSError as error:
        return None, "read_error", errno.errorcode.get(error.errno)
    except (ValueError, OverflowError):
        return None, "invalid_value", None


def read_sensors(chip, name=None):
    name = name or read_text(chip / "name").strip() or chip.name
    kind = "cpu" if name in ("k10temp", "coretemp", "zenpower", "cpu_thermal") else "gpu" if name == "amdgpu" else "other"
    sleeping = read_text(chip / "device/power/runtime_status").strip() in ("suspended", "suspending")
    result = []
    # A thermal-zone hwmon child can omit temp1_crit while the same physical
    # device exports a critical trip. Never borrow thresholds from other zones.
    thermal_critical = None
    thermal_zone = chip.resolve().parent
    if thermal_zone.name.startswith("thermal_zone"):
        limits = [read_number(path.with_name(path.name.removesuffix("_type")+"_temp"), 1000, signed=True)
                  for path in thermal_zone.glob("trip_point_*_type") if read_text(path).strip() == "critical"]
        thermal_critical = min((v for v in limits if v is not None), default=None)
    for path in sorted(chip.glob("temp*_input")):
        stem = path.name.removesuffix("_input")
        temperature, status, error = read_measurement(path, 1000, channel_status(chip, stem, sleeping))
        critical = read_number(chip / f"{stem}_crit", 1000, signed=True)
        if critical is None and stem == "temp1":
            critical = thermal_critical
        alarm = not sleeping and any(read_number(chip / f"{stem}_{suffix}") == 1
                                     for suffix in ("alarm", "crit_alarm", "max_alarm", "emergency_alarm"))
        result.append({"id": str(path.resolve()), "chip": name, "device": chip.name, "kind": kind,
                       "label": read_text(chip / f"{stem}_label").strip() or stem,
                       "temperature_c": temperature, "critical_c": critical,
                       "status": status, "error_code": error,
                       "alarm": alarm, "source": str(path)})
    return result, read_voltages(chip, name, kind, sleeping)


def read_voltages(chip, name, kind, sleeping):
    voltages = []
    for path in sorted(chip.glob("in[0-9]*_input")):
        channel = path.name.removesuffix("_input")
        if not channel[2:].isdigit():
            continue
        state = channel_status(chip, channel, sleeping)
        voltage, status, error = read_measurement(path, 1000, state)
        # Some AMD SMU drivers return a numeric zero for unimplemented rails.
        if name == "amdgpu" and channel in ("in0", "in1") and voltage == 0:
            voltage, status = None, "driver_zero"
        alarms = ("alarm", "min_alarm", "max_alarm", "lcrit_alarm", "crit_alarm", "lemergency_alarm", "emergency_alarm")
        voltages.append({"id": str(path.resolve()), "chip": name, "device": chip.name, "kind": kind,
                         "label": read_text(chip / f"{channel}_label").strip() or channel,
                         "voltage_v": voltage, "source": str(path), "status": status, "error_code": error,
                         "alarm": state == "ok" and any(read_number(chip / f"{channel}_{alarm}") == 1 for alarm in alarms)})
    return voltages


def read_supply_voltages(root):
    for path in sorted(root.glob("*/voltage_now")):
        supply = path.parent
        label = read_text(supply / "type").strip() or "Supply"
        voltage, status, error = read_measurement(path, 1_000_000)
        yield {"id": str(path.resolve()), "chip": "power_supply", "device": supply.name, "kind": "power_supply",
               "label": f"{label} voltage", "voltage_v": voltage, "source": str(path),
               "status": status, "error_code": error, "alarm": False, "sample_age_seconds": 0}
