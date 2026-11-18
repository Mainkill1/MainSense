# Sensor and resource review

Reviewed on 2026-10-22 against the current Linux machine: Debian 13, kernel
6.12, Ryzen AI MAX+ 395 / Strix Halo, 32 logical CPUs. The review included a
separate reviewer, direct procfs/sysfs inventory, fixture tests and measurements
of the running curses application.

## Findings and fixes

| Priority | Finding | Resolution |
| --- | --- | --- |
| Medium | Pause froze the display but continued sensor and process polling. At the fastest refresh rate it still consumed 2.55% of one logical CPU. | Pausing now stops collection. Resume resets CPU/process baselines instead of reporting an average over the pause. Input waits follow the refresh deadline. |
| Medium | The current GPU's binary metrics exposed a SoC temperature and average memory clock that the monitor omitted. | Added a bounded, version-checked v3.0 decoder. The sensor panel includes SoC temperature; VRAM shows average UCLK when hwmon has no memory clock. |
| Medium | The ACPI hwmon channel omitted its critical threshold even though its parent thermal zone exported a 110°C critical trip. | Read critical trips from that same physical device; do not associate unrelated zones or duplicate the temperature. |
| Low | GPU temperature was read twice per snapshot, potentially issuing extra hardware queries and disagreeing between overview and details. | Reuse one reading throughout each snapshot. |
| Low | NVMe health channels were polled at the full UI rate, potentially issuing several device commands per update. | Cache NVMe temperatures and alarms for up to two seconds; expose sample age. CPU/GPU retain the selected rate. |
| Low | Fault/disabled flags and separate critical/maximum alarm flags were not fully honored. | Check validity before reading inputs and include the kernel's channel alarm flags. Sleeping GPUs skip utilization, power, clock and binary-metrics reads. |

No blocking correctness issue remained in the follow-up review. The NVMe cache
holds the state at sampling time: a fault, alarm or power-state transition may
take up to two seconds to appear. The age is explicit in JSON and in wide sensor
tables. This monitor does not control thermal protection.

## Sensor coverage on this host

| Measurement | Verified source / result |
| --- | --- |
| CPU activity | `/proc/stat` aggregate and all 32 logical CPUs; interval deltas exclude idle/I/O wait and avoid double-counting guest time. |
| CPU clock | Mean of readable `/proc/cpuinfo` MHz values. |
| CPU temperature | k10temp `temp1_input`, labeled **Tctl**. No separate Tdie, CCD or per-core channels are exposed through this host's hwmon. |
| GPU activity, clock and power | AMDGPU `gpu_busy_percent`, hwmon `freq1_input`, `power1_average` with `power1_input` fallback. Units are percent, MHz and watts. APU power is labeled SoC power. |
| GPU temperature | AMDGPU hwmon `temp1_input`, labeled **edge**, reused in all views. |
| SoC temperature | `gpu_metrics` v3.0 SoC field; centidegrees converted to Celsius. |
| Memory clock | `gpu_metrics` v3.0 **average UCLK**, in MHz. The host lacks hwmon `freq2_input`; no current frequency is inferred from an unmarked DPM list. |
| System memory | `/proc/meminfo`, using MemTotal − MemAvailable; 33,277,636,608 bytes total, approximately 31.0 GiB. Swap is separate. |
| VRAM | Exact byte counters under AMDGPU; 103,079,215,104 bytes total, or 96 GiB. Memory activity is not substituted for allocation. |
| GTT | Exact used/budget byte counters; 16,638,816,256-byte budget. This overlaps RAM. |
| ACPI | `acpitz` temperature plus the matching thermal zone's 110°C critical trip. |
| NVMe | Both exposed inputs: **Composite** and **Sensor 2**; Composite critical threshold is 79.85°C. Original labels and values are retained. |

All **five** `/sys/class/hwmon/*/temp*_input` channels were accounted for with
no missing inputs. SoC from the GPU metrics table adds a sixth temperature.
The ACPI thermal-zone temperature is the same channel as its hwmon child and
is not counted twice. Channel collection verifies the driver reports; it does
not establish physical sensor calibration.

The host exposes no GPU fan reading, memory-controller busy percentage or
separate VRAM temperature through the examined interfaces. Missing values stay
unavailable. Zero-filled GPU-table core/skin fields are not presented as valid
temperature sensors. NPU activity, DRAM bandwidth and per-process GPU activity
remain outside this version's scope.

The binary decoder checks the complete 264-byte size and format/content
revision `(3, 0)`, rejects unknown layouts and truncation, and treats `0xffff`
as unavailable. It follows the public
[Linux v6.12 GPU metrics structure](https://github.com/torvalds/linux/blob/v6.12/drivers/gpu/drm/amd/include/kgd_pp_interface.h)
and [SMU14 temperature-unit declarations](https://github.com/torvalds/linux/blob/v6.12/drivers/gpu/drm/amd/pm/swsmu/inc/pmfw_if/smu14_driver_if_v14_0_0.h).
On the **same captured bytes**, the decoder matched installed amdgpu_top 0.11.5:
GFX 33.25°C, SoC 33.25°C, average UCLK 999 MHz and average GFX clock 600 MHz.
The overview continues using hwmon edge temperature and instantaneous hwmon
clock; the binary averages do not silently replace those values.

## Strix Halo voltage follow-up

On 2026-11-18, the GMKtec EVO-X2 (Ryzen AI MAX+ 395, BIOS 1.12, SMU firmware
10.100.6.0, Debian kernel 6.12.115) reported zero for both AMDGPU voltage channels
through hwmon and privileged debugfs. The corresponding AMDGPU code in Linux
7.2.12 also returns zero for both channels. [Stable driver source](https://github.com/gregkh/linux/blob/v7.2.12/drivers/gpu/drm/amd/pm/swsmu/smu14/smu_v14_0_0_ppt.c#L346-L351).

The separate SMU firmware power table is readable through the RSMU mailbox.
A [public Strix Halo capture](https://github.com/FlyGoat/RyzenAdj/issues/363)
suggested a block beginning at byte `0x320` in table revision `0x64020C`
(4,136 bytes). The live capture on this EVO-X2 contained three successive
20-byte records, each with five little-endian floats. The values are consistent
with requested VID, measured voltage, current, power and regulator temperature.
Across the 30 records, the reported power differed from voltage × current by
at most 0.173 W. The voltage field also varied independently of requested VID.
The physical rail identities and calibration remain unverified.

| Table record | Voltage observed |
| --- | --- |
| `0x320` | 1.0671–1.0838 V |
| `0x334` | 0.5977–0.6586 V |
| `0x348` | 0.7879–0.8054 V |

The temporary probe was based on [ryzen_smu v0.1.7](https://github.com/kylon/ryzen_smu/tree/d2a4c942398d46ce5f6b97314cf236409d92bb18).
It matched the host PCI device, CPU family/model and exact table revision. It
used only the RSMU table queries, held a PCI device reference without binding a
driver, and exposed root-readable capture files. An initial load returned an
I/O error. Subsequent multi-read captures sometimes got firmware response
`0xFD` during refresh. Ten single 60-byte positional reads spaced 250 ms apart
all succeeded. AMDGPU stayed bound and reported 93% GPU utilization throughout
the final capture and after the probe unloaded. The out-of-tree and unsigned
module taint flags persist until reboot.

For MainSense, a permanent provider should expose verified rail identities through
hwmon, check the table revision and length, and report a rejected refresh as
unavailable. The temporary module was unloaded and the capture is saved here.
[Live capture](measurements/strix-halo-voltage.json).

## Resource measurements

`scripts/benchmark_monitor.py` runs the real color curses app in a 120×40 PTY,
drains output without retaining it, and measures process CPU ticks and RSS
from `/proc`. Each case warms up for three seconds. The baseline was measured
for 20 seconds per case, the revised app for 30 seconds. **100% CPU means one
logical CPU**, not the entire 32-thread machine. RSS includes the Python runtime.

| Configuration | Before: CPU | After: CPU | After: peak RSS |
| --- | ---: | ---: | ---: |
| Default, 1 update/sec | 0.60% | 0.50% | 18.81 MiB |
| Fastest, 5 updates/sec | 2.45% | 2.93% | 19.12 MiB |
| Fastest with process panel | 3.15% | 3.27% | 19.13 MiB |
| Paused after enabling processes | 2.55% | 0.00% | 18.60 MiB |

The added telemetry and validation have a small cost; these observations do
not show a general speedup at the fastest setting. The pause fix eliminates
measured CPU consumption and terminal output during that case. CPU tick
resolution and changing system load limit precision, especially near zero.

A separate **180-second** run at five updates/sec with the process panel
averaged **2.90% of one CPU**. RSS grew from 18.55 MiB to **19.42 MiB** during
the first 52 seconds, then remained exactly flat through the end, including
after the 120-second history window filled. No continuing memory growth was
observed in this run; this is a short soak, not proof of indefinite behavior.

History buffers have a 600-entry maximum per active CPU/GPU series and prune
old time windows. Removing a GPU also removes its history. Process baselines
replace the previous PID set; NVMe cache entries disappear when their devices
do. Regression tests exercise these bounds.

The environment exposes only a small process namespace, so process-panel cost
on hosts with thousands of processes remains unmeasured. This review did not
generate a heavy CPU/GPU workload or exercise other hardware/driver versions.

## Reproduce

```sh
python3 -m unittest discover -s tests -v
python3 scripts/benchmark_monitor.py --seconds 30 --soak-seconds 180 --output /tmp/mainsense-benchmark.json
python3 -m mainsense --json --once
```

All **40 tests** passed, including counter accounting, sensor units, invalid and
sleeping inputs, unknown GPU-metrics layouts, cache age/eviction, one-read GPU
sampling, pause/resume, bounded histories and real PTY input/resize/cleanup.
The benchmark writes raw per-second RSS observations to the requested JSON
file and exits every monitor process it starts.

Captured evidence: [baseline benchmark](measurements/benchmark-before.json),
[revised benchmark and soak](measurements/benchmark-after.json), and
[sensor inventory and decoder comparison](measurements/sensor-audit.json).
