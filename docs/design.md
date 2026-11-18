# Terminal design decisions

Comparative OSS research, 2026-10-22. Documentation and published interface
descriptions were studied; no implementation source, themes, or assets were
imported. One author performed both research and implementation; this is not a
claim of separate-person clean-room development.

## Starting gaps

The repository was empty. The current machine exposes 32 CPU threads, AMD GPU
sysfs, k10temp, amdgpu and NVMe hwmon sensors. It reports approximately 31 GiB
of Linux-visible RAM and a 96 GiB VRAM pool. `/dev/dri` is not exposed in this
execution environment, so the core monitor must work directly from procfs and
sysfs without opening a DRM device. Generic memory totals would obscure this
machine's unusual split.

## Reference comparison

| Reference | Surveyed release / license | Useful lesson | Deliberately excluded |
| --- | --- | --- | --- |
| [amdgpu_top](https://github.com/Umio-Yasuno/amdgpu_top) | v0.11.5, MIT; current README and installed CLI help | Keep GPU identity, utilization, temperatures, clocks and memory together. Explain missing readings and distinguish VRAM from GTT. | Register counters and dependency on DRM device handles. |
| [btop](https://github.com/aristocratos/btop) | v1.4.7, Apache-2.0; current README | Use readable resource panels, core detail, visible controls, responsive interaction and selectable graph glyphs. | Disk/network panels, process signaling, extensive settings menus. |
| [nvtop](https://github.com/Syllo/nvtop) | 3.3.2, GPL-3.0-or-later; current README | Give GPUs stable identities and aligned measurements; retain trends alongside current values. | Multi-vendor accelerator coverage and editable chart configuration. |
| [amdtop](https://github.com/lhl/amdtop) | v0.2.6, Apache-2.0; current README | Collapsible detail and explicit APU memory accounting keep the overview approachable. Sleeping/unavailable devices deserve distinct states. | Theme collection, NPU or bandwidth values without verified telemetry. |

## Independent behavior specification

- The default screen answers: how busy are CPU/GPU, how full are RAM/VRAM,
  and how hot are CPU/GPU? Current values remain readable beside trends.
- Wide terminals show CPU and GPU side by side, then RAM and VRAM. Narrow
  terminals stack panels. Details scroll; header and shortcut footer stay put.
- Core detail scales into as many columns as fit. Sensor detail retains original
  labels and device names, including Tctl, edge and NVMe sensors.
- Colors distinguish resources; text and numeric values convey meaning in
  monochrome. ASCII mode supports terminals without Unicode block characters.
- `Space` pauses collection, the displayed snapshot and histories. `+`/`-` change the
  interval. `g` cycles GPUs; `c`, `t`, `p` toggle cores, sensors and processes;
  `?` explains shortcuts and measurement semantics. `q` exits cleanly.
- Unknown values are `--`, never invented zeroes. CPU needs two counter samples.
  GPU memory occupancy uses byte counters, not memory-controller activity.
- RAM used is `MemTotal - MemAvailable`; swap is separate. GTT is system-backed
  and overlaps RAM. Never sum RAM, VRAM and GTT into a purported physical total.
- AMD power is labeled SoC power: on APUs it includes CPU power. Tctl keeps its
  control-temperature label; it is not silently renamed die temperature.
- One collector services the UI. History is bounded. Reading sensors never
  writes fan, power, clock or driver settings. JSON and plain-text snapshots are
  available for diagnostics without a terminal.

Acceptance: fixture tests for counter accounting, units, absent sensors and
device discovery; actual-host snapshots; terminal resize and input tests; a
measured collector cost. Verify the important paths at 80x24, 120x40, a narrow
terminal, and a very small terminal. No external runtime packages are required.

Design informed by these four projects (study attribution; no code derived).
Linux's public procfs/sysfs field names and units necessarily match the kernel
interfaces; the implementation is independently written.

## Acceptance evidence

The initial acceptance results below are supplemented by the later
[sensor and resource review](sensor-resource-review.md), including 40 tests,
additional temperature/clock coverage and measurements of the complete TUI.

- 27 tests pass, including accounting fixtures, device disappearance, missing
  readings, process identity, live CLI output and real terminal input/resize.
- Geometry tests cover 1x1, 20x5, 42x18, 80x24, 120x40 and 240x60. PTY checks
  exercise 80x24, 120x40, 42x18 and 1x1, then restore the normal window.
- Live VRAM total/used byte values matched direct sysfs reads. Linux RAM, CPU
  Tctl, GPU edge, clocks and SoC power were observed on the current host.
- A 60-sample local measurement found a 9.00 ms median and 15.36 ms maximum
  collection time. Rendering took 0.77 ms at 80x24 and 0.47 ms at 120x40.
  These are local observations, not performance guarantees on other hardware.
- Independent code review found no major issues. Narrow help visibility and
  viewport-sized page scrolling were improved and covered by regressions.
- Optional wheel construction was attempted without network access and could
  not run because this environment lacks `setuptools.build_meta`. Direct
  checkout execution and the CLI entry point are verified; wheel installation
  is not. Build the executable from a checkout with `make` and run `./mainsense`.
