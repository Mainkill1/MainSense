# MainSense

A readable terminal monitor for Linux CPU, AMD GPU, RAM, VRAM, temperatures and voltages.
MainSense is a native Rust executable. It uses Linux's read-only monitoring
interfaces and runs as a regular user.

## Install and run

Download the Linux archive for your architecture, extract it, and run the
executable. [GitHub releases](https://github.com/Mainkill1/MainSense/releases)

```sh
./mainsense --help
./mainsense
```

To build from a checkout, use Rust 1.85 or newer, Cargo, and Make.

```sh
git clone https://github.com/Mainkill1/MainSense.git
cd MainSense
make
./mainsense
```

The examples below use `mainsense` from your PATH. Use `./mainsense` to
run it from the current directory.
If you are upgrading a v0.1.0 checkout and an old `mainsense/` cache directory
is still present, move it aside before building.

The overview keeps CPU and GPU activity and temperatures above RAM and VRAM.
Wide terminals use two columns; narrow terminals stack the panels. The header
and controls stay visible while you scroll through thread meters and sensors.
The four main panels fit in 80x24. A 120x40 window shows more detail at once.

## Controls

| Key | Action |
| --- | --- |
| `q`, `Ctrl+C` | Exit and restore the terminal |
| `Space` | Pause/resume collection, displayed readings and histories |
| `+`, `-` | Faster/slower refresh, from 0.2 to 5 seconds |
| `c` | Expand/collapse per-thread CPU meters |
| `t` | Expand/collapse temperature and voltage sensors |
| `p` | Expand/collapse the top 30 visible processes by CPU, then RSS |
| `g` | Cycle detected AMD GPUs |
| `↑`/`↓`, `k`/`j` | Scroll |
| `PgUp`/`PgDn`, `Home`/`End` | Scroll further or jump to an end |
| `?`, `h` | Open/close help |
| `Esc` | Close help |

```sh
mainsense --compact                   # Start with just the overview
mainsense --interval 0.5              # Twice per second
mainsense --ascii --no-color          # Simple terminal / SSH compatibility
mainsense --once                      # Plain-text snapshot
mainsense --json --once               # One machine-readable snapshot
mainsense --json --count 10 -i 0.5     # Ten JSON lines
mainsense --help
```

Output redirected to a file or pipe defaults to one plain-text snapshot.
`--json` streams until interrupted unless `--count` or `--once` is supplied.
Snapshots wait one sampling interval so CPU utilization is measured correctly.
`NO_COLOR` disables color. For color when your shell sets it, use
`env -u NO_COLOR mainsense`. Preferences apply to the current run.

## Recording telemetry

```sh
mainsense --log-file mainsense.jsonl                               # Record while the terminal UI runs
mainsense --json --count 60 -i 1 --log-file mainsense.jsonl         # Record 60 samples without the UI
```

`--log-file` appends a JSON line per snapshot with a Unix timestamp, readings,
and any snapshot warnings. It works with the terminal UI and with `--once` or
`--json`. The log includes up to 30 process entries while the process panel
is active. Pausing stops new samples while queued records finish writing.

Use a regular file for `--log-file` and direct stdout to a separate destination.
The writer follows the log path after it is renamed and recreates the file if
needed. It buffers up to 32 pending samples and attempts to drain them on exit.
MainSense reports an error if the queue fills or shutdown times out waiting for
storage. `--json` also sends snapshots to stdout, which can advance while a
log record waits to be written.

Each snapshot includes `kernel_release`. Temperature and voltage entries include
their source path, `status`, and `error_code` when a sysfs read returns an errno.
Use `mainsense --json --once` for a one-sample diagnostic. Common statuses are
`ok`, `permission_denied`, `missing`, `read_error`, `invalid_value`, `sleeping`,
`disabled`, `fault`, `unsupported`, and `driver_zero` for AMDGPU zero placeholders.
For long-running collection, `--json` can feed a log manager that applies your
retention policy.

## What the readings mean

| Reading | Source and meaning |
| --- | --- |
| CPU busy | Deltas of `/proc/stat`; aggregate and per logical CPU. Idle and I/O wait are excluded from busy time, and guest time is not counted twice. |
| CPU clock | Average of available `/proc/cpuinfo` frequency readings; a sampled value, not a requested boost clock. |
| RAM used | `/proc/meminfo`: `MemTotal - MemAvailable`. Reclaimable cache is not mistaken for unavailable RAM. |
| Cache | Cached + Buffers + SReclaimable − Shmem, clamped at zero; not an additional pool to add to used RAM. |
| Swap | SwapTotal − SwapFree, separately from physical RAM. |
| GPU busy | AMDGPU `gpu_busy_percent`. This is the driver's reported activity. |
| VRAM | AMDGPU `mem_info_vram_used` / `mem_info_vram_total`, in bytes. On APUs this can be a large firmware-reserved pool. |
| GTT | System-backed GPU memory used and driver budget. It overlaps system RAM; never add it to RAM/VRAM to infer physical capacity. |
| Mem activity | `mem_busy_percent` if available. Controller activity is distinct from allocated memory and from bandwidth in GB/s. |
| Memory clock | hwmon `freq2_input`, or average UCLK from a supported `gpu_metrics` table. The UI labels the averaged fallback; JSON includes its source. |
| Temperatures | Raw hwmon readings with the kernel's channel labels. CPU prefers Tdie, then Tctl/package readings. The sensor panel includes NVMe, ACPI and the SoC channel from a supported GPU metrics table. |
| Voltages | Linux hwmon `inN_input` (millivolts) and power-supply `voltage_now` (microvolts), normalized to volts. The voltage panel shows device labels and hwmon alarms. JSON includes channel paths and lists GPU channels under their GPU as well. |
| SoC power | AMDGPU hwmon power, converted from microwatts. **On APUs it includes the CPU**, so it is not GPU-only power. |
| Processes | Readable `/proc/PID/stat`: interval CPU usage and resident system RAM. 100% means one logical CPU; multithreaded processes can exceed 100%. |

All memory sizes use binary units: 1 GiB = 1,073,741,824 bytes. `--` in the UI
and `null` in JSON mean unavailable, unreadable, sleeping or not yet sampled;
they do not mean zero. Critical temperatures come from sensor thresholds or the
same device's critical thermal trip; the monitor does not invent generic limits. Tctl remains
labeled as a control temperature. The 60-second graphs use a fixed 0–100% scale
and retain gaps while paused. Resuming starts fresh CPU/process deltas, so the
first sample is unavailable instead of averaging over the paused period.

CPU and GPU readings follow the selected interval. NVMe health reads can issue
device commands, so temperatures and alarms are cached for up to two seconds.
The expanded sensor table shows their age in sufficiently wide terminals;
JSON always includes `sample_age_seconds`. Cached readings describe the device
when sampled, including its validity and alarm state.

The program reads monitoring files only. It never changes clocks, fan curves,
power limits or drivers. JSON contains every detected GPU and sensor, including
sensor paths for diagnosing mismatches. The UI selects one GPU at a time.

Voltage coverage depends on the installed hardware and drivers. Some board
channels report voltage at the sensor pin and need motherboard-specific scaling
to represent the rail. MainSense shows the driver value and label. AMDGPU graphics
and Northbridge channels that return placeholder zeros are reported as unavailable.
Battery, UPS, and supply drivers can expose live readings via `voltage_now`.
PMBus regulators and power supplies appear automatically when a kernel driver
exposes their hwmon voltage channels. [Linux power supply class](https://docs.kernel.org/power/power_supply_class.html), [Linux PMBus driver](https://docs.kernel.org/hwmon/pmbus.html).

## Verified on this machine

Development checks on 2026-10-22 used Debian 13, kernel 6.12, a Ryzen AI MAX+
395 with 32 logical CPUs and Strix Halo Radeon graphics. The kernel reported
31.0 GiB of Linux-visible RAM and a separate 96.0 GiB VRAM pool. CPU Tctl and
GPU edge and SoC temperatures, GPU clock, average UCLK, SoC power, VRAM/GTT usage,
ACPI and NVMe temperatures were readable. VRAM byte counts matched direct sysfs
reads. All five hwmon temperature inputs are collected; the GPU metrics table
adds a sixth temperature channel. Its units and offsets were checked against
the kernel ABI and amdgpu_top's decoder on the same captured bytes.

This kernel does not expose a GPU fan reading, memory-controller busy percentage,
or a separate VRAM temperature through these hwmon interfaces. The monitor
leaves missing metrics unavailable. It can run in this environment even though
`/dev/dri` is not exposed.

A follow-up on kernel 6.12.115 found `vddgfx` and `vddnb` channels on this
machine. Both returned placeholder zeros during GPU activity. The k10temp,
ACPI and NVMe hwmon devices exposed no voltage channels. The supported
`gpu_metrics` v3.0 layout has no voltage fields. [SMU driver](https://github.com/torvalds/linux/blob/v6.12/drivers/gpu/drm/amd/pm/swsmu/smu14/smu_v14_0_0_ppt.c#L325-L330), [GPU metrics ABI](https://github.com/torvalds/linux/blob/v6.12/drivers/gpu/drm/amd/include/kgd_pp_interface.h#L1096-L1175).

A temporary Linux probe captured three live voltage fields from this machine's
separate SMU firmware table. The physical rail identities still need validation
before MainSense can label those readings. [Capture and findings](docs/sensor-resource-review.md#strix-halo-voltage-follow-up).

The binary GPU metrics decoder currently accepts only the verified v3.0 layout;
other versions keep their sysfs/hwmon readings. Zero-filled core/skin fields on
this machine are not presented as functioning temperature sensors.

Process visibility follows the current PID namespace and access permissions;
inside a container the list can be smaller than the host's. The process table
does not report per-process GPU usage. NPU telemetry, GPU register counters and
DRAM bandwidth are outside this version's scope. These are not inferred from
unrelated counters. On machines without readable AMD sensors, CPU and RAM
monitoring continues with an explanatory message.

## Development

```sh
cargo test --locked
make
python3 -m unittest discover -s tests -v
python3 scripts/benchmark_monitor.py --seconds 30 --soak-seconds 180 --output /tmp/mainsense-benchmark.json
```

Rust tests cover CPU and RAM accounting, process stat parsing, device discovery,
sensor units, unavailable readings, layouts and JSONL logging. The Python
development checks run the compiled binary in a real pseudo-terminal and
exercise CLI output, fast sampling, log rotation and shutdown.

The benchmark measures the actual terminal app, including rendering, in four
refresh/process/pause configurations. CPU percentages use one logical CPU as
100%; memory figures are resident process memory. See the
[sensor and resource review](docs/sensor-resource-review.md) for findings,
measurements and remaining limitations.

Linux collection lives in `src/metrics.rs` and `src/sensors.rs`. The terminal UI
lives in `src/tui.rs`, and the CLI in `src/main.rs`.

Design informed by amdgpu_top, btop, nvtop and amdtop, with independently written
code. See [the design comparison](docs/design.md) for sources and decisions.
Measurement conventions follow the kernel documentation for
[/proc](https://docs.kernel.org/filesystems/proc.html),
[AMDGPU monitoring](https://docs.kernel.org/gpu/amdgpu/thermal.html),
[hwmon units](https://docs.kernel.org/hwmon/sysfs-interface.html), and
[k10temp](https://docs.kernel.org/hwmon/k10temp.html).

## License

MainSense is released under the [MIT License](LICENSE).
