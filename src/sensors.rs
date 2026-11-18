//! Read Linux hwmon, power-supply and AMDGPU telemetry.
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
};

#[derive(Default)]
pub struct Readings {
    pub temperatures: Vec<Value>,
    pub voltages: Vec<Value>,
    pub gpus: Vec<Value>,
}

#[derive(Clone, Default)]
struct Channels {
    temperatures: Vec<Value>,
    voltages: Vec<Value>,
}

impl Channels {
    fn set_age(&mut self, age: f64) {
        for reading in self.temperatures.iter_mut().chain(&mut self.voltages) {
            reading["sample_age_seconds"] = json!((age * 1000.0).round() / 1000.0);
        }
    }
}

pub struct Sensors {
    gpu_names: HashMap<String, String>,
    slow_sensors: HashMap<PathBuf, (f64, Channels)>,
}

impl Sensors {
    pub fn new(pci_ids: &Path) -> Self {
        let mut amd_section = false;
        let mut gpu_names = HashMap::new();
        for line in text(pci_ids).lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(device) = line.strip_prefix('\t') {
                if amd_section {
                    if let Some((id, label)) = device.split_once("  ") {
                        if id.len() == 4 && id.bytes().all(|c| c.is_ascii_hexdigit()) {
                            gpu_names.insert(id.to_ascii_lowercase(), label.trim().to_owned());
                        }
                    }
                }
            } else {
                amd_section = line.starts_with("1002 ");
            }
        }
        Self {
            gpu_names,
            slow_sensors: HashMap::new(),
        }
    }

    pub fn collect(&mut self, sys: &Path, now: f64) -> Readings {
        let cards = discover_cards(&sys.join("class/drm"));
        let gpu_sleep = cards
            .iter()
            .flat_map(|card| {
                card.chips
                    .iter()
                    .map(|chip| (canonical(chip), card.sleeping))
            })
            .collect::<HashMap<_, _>>();
        let mut previous = std::mem::take(&mut self.slow_sensors);
        let mut by_chip = HashMap::new();
        let mut seen_temperatures = HashSet::new();
        let mut seen_voltages = HashSet::new();
        let mut readings = Readings::default();

        for chip in prefixed_paths(&sys.join("class/hwmon"), "hwmon") {
            let identity = canonical(&chip);
            if by_chip.contains_key(&identity) {
                continue;
            }
            let name = chip_name(&chip);
            let cached = previous
                .remove(&identity)
                .filter(|(timestamp, _)| name == "nvme" && (0.0..2.0).contains(&(now - timestamp)));
            let (sampled_at, mut channels) = cached.unwrap_or_else(|| {
                let sleeping = gpu_sleep
                    .get(&identity)
                    .copied()
                    .unwrap_or_else(|| device_sleeping(&chip.join("device")));
                (now, read_channels(&chip, &name, sleeping))
            });
            if name == "nvme" {
                self.slow_sensors
                    .insert(identity.clone(), (sampled_at, channels.clone()));
            }
            channels.set_age(now - sampled_at);

            extend_unique(
                &mut readings.temperatures,
                &mut seen_temperatures,
                &channels.temperatures,
            );
            extend_unique(
                &mut readings.voltages,
                &mut seen_voltages,
                &channels.voltages,
            );
            by_chip.insert(identity, channels);
        }

        for card in &cards {
            let hwmon = card
                .chips
                .first()
                .cloned()
                .unwrap_or_else(|| card.device.join("hwmon/unavailable"));
            let mut channels = by_chip
                .entry(canonical(&hwmon))
                .or_insert_with(|| {
                    let mut channels = read_channels(&hwmon, &chip_name(&hwmon), card.sleeping);
                    channels.set_age(0.0);
                    channels
                })
                .clone();
            let gpu = read_gpu(card, &hwmon, &self.gpu_names, &mut channels);
            extend_unique(
                &mut readings.temperatures,
                &mut seen_temperatures,
                &channels.temperatures,
            );
            extend_unique(
                &mut readings.voltages,
                &mut seen_voltages,
                &channels.voltages,
            );
            readings.gpus.push(gpu);
        }

        let supplies = read_supply_voltages(&sys.join("class/power_supply"));
        extend_unique(&mut readings.voltages, &mut seen_voltages, &supplies);
        readings
    }
}

fn extend_unique(target: &mut Vec<Value>, seen: &mut HashSet<String>, readings: &[Value]) {
    for reading in readings {
        if let Some(id) = reading["id"].as_str() {
            if seen.insert(id.to_owned()) {
                target.push(reading.clone());
            }
        }
    }
}

struct Card {
    name: String,
    device: PathBuf,
    chips: Vec<PathBuf>,
    sleeping: bool,
}

fn discover_cards(root: &Path) -> Vec<Card> {
    let mut cards: Vec<_> = prefixed_paths(root, "card")
        .into_iter()
        .filter_map(|path| {
            let name = basename(&path);
            let index = name
                .strip_prefix("card")
                .filter(|value| digits(value))?
                .parse::<u32>()
                .ok()?;
            let device = path.join("device");
            if text(&device.join("vendor")).trim() != "0x1002" {
                return None;
            }
            let chips = prefixed_paths(&device.join("hwmon"), "hwmon");
            let sleeping = device_sleeping(&device);
            Some((
                index,
                Card {
                    name,
                    device,
                    chips,
                    sleeping,
                },
            ))
        })
        .collect();
    cards.sort_by_key(|(index, _)| *index);
    cards.into_iter().map(|(_, card)| card).collect()
}

fn read_gpu(
    card: &Card,
    chip: &Path,
    names: &HashMap<String, String>,
    channels: &mut Channels,
) -> Value {
    let device = &card.device;
    let pci_id = text(&device.join("device"))
        .trim()
        .trim_start_matches("0x")
        .to_ascii_lowercase();
    let mut name = text(&device.join("product_name")).trim().to_owned();
    if name.is_empty() {
        name = names
            .get(&pci_id)
            .cloned()
            .unwrap_or_else(|| format!("AMD GPU [1002:{pci_id}]"));
    }

    let canonical_device = canonical(device);
    let value = |attribute: &str| {
        (!card.sleeping)
            .then(|| unsigned(&device.join(attribute)))
            .flatten()
    };
    let sensor = |attribute: &str, scale: f64| {
        (!card.sleeping)
            .then(|| unsigned(&chip.join(attribute)).map(|raw| raw as f64 / scale))
            .flatten()
    };
    let used = value("mem_info_vram_used");
    let total = value("mem_info_vram_total");
    let mut valid = channels
        .temperatures
        .iter()
        .filter(|reading| reading["temperature_c"].is_number());
    let primary = valid
        .clone()
        .find(|reading| {
            reading["label"]
                .as_str()
                .is_some_and(|label| label.eq_ignore_ascii_case("edge"))
        })
        .or_else(|| valid.next());
    let temperature_c = primary.and_then(|reading| reading["temperature_c"].as_f64());
    let temperature_label = primary
        .and_then(|reading| reading["label"].as_str())
        .unwrap_or("unavailable")
        .to_owned();

    let metrics_path = device.join("gpu_metrics");
    let metrics = (!card.sleeping)
        .then(|| read_metrics(&metrics_path))
        .flatten();
    let metrics_v3 = metrics.as_ref().and_then(|table| table.v3.as_ref());
    if let Some(metrics) = metrics_v3 {
        channels.temperatures.push(json!({
            "id": format!("{}#soc", canonical(&metrics_path).display()),
            "chip": "amdgpu_metrics",
            "device": card.name,
            "kind": "gpu",
            "label": "SoC",
            "temperature_c": metrics.soc_temperature_c,
            "critical_c": null,
            "status": if metrics.soc_temperature_c.is_some() { "ok" } else { "unsupported" },
            "error_code": null,
            "alarm": false,
            "source": metrics_path.to_string_lossy(),
            "sample_age_seconds": 0.0,
        }));
    }
    if let Some(table) = &metrics {
        append_metrics_voltages(&mut channels.voltages, &metrics_path, &card.name, table);
    }
    let (memory_clock_mhz, memory_clock_source) = match sensor("freq2_input", 1_000_000.0) {
        Some(mhz) => (Some(mhz), Some("hwmon freq2_input")),
        None => {
            let clock = metrics_v3
                .and_then(|data| data.memory_clock_mhz)
                .map(f64::from);
            (clock, clock.map(|_| "gpu_metrics v3.0 average UCLK"))
        }
    };
    json!({
        "id": canonical_device.to_string_lossy(),
        "card": card.name,
        "name": name,
        "pci_address": basename(&canonical_device),
        "status": if card.sleeping { "sleeping" } else { "available" },
        "used_percent": value("gpu_busy_percent").filter(|n| *n <= 100),
        "memory_busy_percent": value("mem_busy_percent").filter(|n| *n <= 100),
        "vram_used_bytes": used,
        "vram_total_bytes": total,
        "vram_used_percent": used.zip(total).and_then(|(used, total)| {
            (total != 0).then(|| (1000.0 * used as f64 / total as f64).round_ties_even() / 10.0)
        }),
        "gtt_used_bytes": value("mem_info_gtt_used"),
        "gtt_total_bytes": value("mem_info_gtt_total"),
        "temperature_c": temperature_c,
        "temperature_label": temperature_label,
        "temperatures": channels.temperatures,
        "voltages": channels.voltages,
        "power_w": sensor("power1_average", 1_000_000.0).or_else(|| sensor("power1_input", 1_000_000.0)),
        "clock_mhz": sensor("freq1_input", 1_000_000.0),
        "memory_clock_mhz": memory_clock_mhz,
        "memory_clock_source": memory_clock_source,
        "metrics": match (metrics.as_ref(), metrics_v3) {
            (Some(table), Some(data)) => json!({
                "version": table.version,
                "gfx_temperature_c": data.gfx_temperature_c,
                "soc_temperature_c": data.soc_temperature_c,
                "clock_mhz": data.clock_mhz,
                "memory_clock_mhz": data.memory_clock_mhz,
            }),
            (Some(table), None) => json!({ "version": table.version }),
            (None, _) => json!({}),
        },
        "fan_rpm": (!card.sleeping).then(|| unsigned(&chip.join("fan1_input"))).flatten(),
    })
}

fn read_channels(chip: &Path, name: &str, sleeping: bool) -> Channels {
    let kind = match name {
        "k10temp" | "coretemp" | "zenpower" | "cpu_thermal" => "cpu",
        "amdgpu" => "gpu",
        _ => "other",
    };
    let mut channels = Channels::default();
    let thermal_limit = canonical(chip).parent().and_then(thermal_critical);
    for input in paths(chip) {
        let filename = basename(&input);
        let Some(channel) = filename.strip_suffix("_input") else {
            continue;
        };
        if let Some(index) = channel.strip_prefix("temp").filter(|index| digits(index)) {
            let state = channel_status(chip, channel, sleeping);
            let measurement = read_measurement(&input, 1000.0, state);
            let critical_c = signed(&chip.join(format!("{channel}_crit")))
                .map(|n| n as f64 / 1000.0)
                .or_else(|| if index == "1" { thermal_limit } else { None });
            let alarm = !sleeping
                && any_alarm(
                    chip,
                    channel,
                    &["alarm", "crit_alarm", "max_alarm", "emergency_alarm"],
                );
            channels.temperatures.push(json!({
                "id": canonical(&input).to_string_lossy(), "source": input.to_string_lossy(),
                "chip": name, "device": basename(chip), "kind": kind, "label": channel_label(chip, channel),
                "temperature_c": measurement.value, "critical_c": critical_c, "alarm": alarm,
                "status": measurement.status, "error_code": measurement.error_code,
            }));
        } else if channel.strip_prefix("in").is_some_and(digits) {
            let label = channel_label(chip, channel);
            if label.strip_prefix("in").is_some_and(digits)
                || (name == "strixhalo_smu" && label.starts_with("SMU rail "))
            {
                continue;
            }
            let state = channel_status(chip, channel, sleeping);
            let mut measurement = read_measurement(&input, 1000.0, state);
            if name == "amdgpu"
                && matches!(channel, "in0" | "in1")
                && measurement.value == Some(0.0)
            {
                measurement = Measurement::unavailable("driver_zero", None);
            }
            let alarm = state == "ok"
                && any_alarm(
                    chip,
                    channel,
                    &[
                        "alarm",
                        "min_alarm",
                        "max_alarm",
                        "lcrit_alarm",
                        "crit_alarm",
                        "lemergency_alarm",
                        "emergency_alarm",
                    ],
                );
            channels.voltages.push(json!({
                "id": canonical(&input).to_string_lossy(), "source": input.to_string_lossy(),
                "chip": name, "device": basename(chip), "kind": kind, "label": label,
                "voltage_v": measurement.value, "alarm": alarm,
                "status": measurement.status, "error_code": measurement.error_code,
            }));
        }
    }
    channels
}

fn read_supply_voltages(root: &Path) -> Vec<Value> {
    paths(root).into_iter().filter_map(|supply| {
        let input = supply.join("voltage_now");
        fs::symlink_metadata(&input).ok()?;
        let supply_type = text(&supply.join("type"));
        let label = if supply_type.trim().is_empty() { "Supply" } else { supply_type.trim() };
        let measurement = read_measurement(&input, 1_000_000.0, "ok");
        Some(json!({
            "id": canonical(&input).to_string_lossy(), "source": input.to_string_lossy(),
            "device": basename(&supply), "chip": "power_supply", "kind": "power_supply",
            "label": format!("{label} voltage"), "voltage_v": measurement.value, "alarm": false,
            "status": measurement.status, "error_code": measurement.error_code, "sample_age_seconds": 0.0,
        }))
    }).collect()
}

struct MetricsV3 {
    gfx_temperature_c: Option<f64>,
    soc_temperature_c: Option<f64>,
    clock_mhz: Option<u16>,
    memory_clock_mhz: Option<u16>,
}

struct VoltageField {
    offset: usize,
    label: &'static str,
    hwmon_labels: &'static [&'static str],
}

// Linux kgd_pp_interface.h native struct offsets verified on x86-64.
const METRICS_V1_3_VOLTAGES: &[VoltageField] = &[
    VoltageField {
        offset: 104,
        label: "SoC",
        hwmon_labels: &["vddnb", "vddsoc"],
    },
    VoltageField {
        offset: 106,
        label: "GFX",
        hwmon_labels: &["vddgfx"],
    },
    VoltageField {
        offset: 108,
        label: "Memory",
        hwmon_labels: &["vddmem"],
    },
];
const METRICS_V2_4_VOLTAGES: &[VoltageField] = &[
    VoltageField {
        offset: 152,
        label: "CPU avg",
        hwmon_labels: &["vddcpu"],
    },
    VoltageField {
        offset: 154,
        label: "SoC avg",
        hwmon_labels: &["vddnb", "vddsoc"],
    },
    VoltageField {
        offset: 156,
        label: "GFX avg",
        hwmon_labels: &["vddgfx"],
    },
];

struct GpuMetrics {
    version: &'static str,
    v3: Option<MetricsV3>,
    voltages: Vec<(&'static VoltageField, u16)>,
}

fn read_metrics(path: &Path) -> Option<GpuMetrics> {
    // Limit the read to the largest supported layout plus a byte to detect a longer table.
    let mut bytes = Vec::with_capacity(265);
    File::open(path)
        .ok()?
        .take(265)
        .read_to_end(&mut bytes)
        .ok()?;
    let [size_lo, size_hi, format, content] = *bytes.first_chunk::<4>()?;
    let size = usize::from(u16::from_le_bytes([size_lo, size_hi]));
    if bytes.len() != size {
        return None;
    }
    let read = |offset| u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
    let (version, voltage_fields, v3) = match (size, format, content) {
        (120, 1, 3) => ("1.3", METRICS_V1_3_VOLTAGES, None),
        (168, 2, 4) => ("2.4", METRICS_V2_4_VOLTAGES, None),
        (264, 3, 0) => {
            // SMU temperature fields in hundredths of a degree C.
            let available = |offset| {
                let value = read(offset);
                (value != u16::MAX).then_some(value)
            };
            let v3 = MetricsV3 {
                gfx_temperature_c: available(4).map(|value| f64::from(value) / 100.0),
                soc_temperature_c: available(6).map(|value| f64::from(value) / 100.0),
                clock_mhz: available(174),
                memory_clock_mhz: available(186),
            };
            ("3.0", &[][..], Some(v3))
        }
        _ => return None,
    };
    let voltages = voltage_fields
        .iter()
        .filter_map(|field| {
            let millivolts = read(field.offset);
            // Zero and all-ones are placeholders. Also reject out-of-range rail voltages.
            (1..=3000)
                .contains(&millivolts)
                .then_some((field, millivolts))
        })
        .collect();
    Some(GpuMetrics {
        version,
        v3,
        voltages,
    })
}

fn append_metrics_voltages(voltages: &mut Vec<Value>, path: &Path, card: &str, table: &GpuMetrics) {
    if table.voltages.is_empty() {
        return;
    }
    let identity = canonical(path);
    for &(field, millivolts) in &table.voltages {
        let has_native_reading = voltages.iter().any(|reading| {
            reading["status"] == "ok"
                && reading["voltage_v"]
                    .as_f64()
                    .is_some_and(|volts| volts > 0.0)
                && reading["label"].as_str().is_some_and(|label| {
                    field
                        .hwmon_labels
                        .iter()
                        .any(|alias| label.eq_ignore_ascii_case(alias))
                })
        });
        if has_native_reading {
            continue;
        }
        voltages.push(json!({
            "id": format!("{}#voltage-{}", identity.display(), field.offset),
            "source": path.to_string_lossy(),
            "source_offset_bytes": field.offset,
            "chip": "amdgpu_metrics",
            "device": card,
            "kind": "gpu",
            "label": format!("{} (AMDGPU v{})", field.label, table.version),
            "voltage_v": f64::from(millivolts) / 1000.0,
            "status": "ok",
            "error_code": null,
            "alarm": false,
            "sample_age_seconds": 0.0,
        }));
    }
}

struct Measurement {
    value: Option<f64>,
    status: &'static str,
    error_code: Option<&'static str>,
}

impl Measurement {
    fn unavailable(status: &'static str, error_code: Option<&'static str>) -> Self {
        Self {
            value: None,
            status,
            error_code,
        }
    }
}

fn read_measurement(path: &Path, scale: f64, state: &'static str) -> Measurement {
    if state != "ok" {
        return Measurement::unavailable(state, None);
    }
    match fs::read_to_string(path) {
        Ok(value) => match value.trim().parse::<i64>() {
            Ok(value) => Measurement {
                value: Some(value as f64 / scale),
                status: "ok",
                error_code: None,
            },
            Err(_) => Measurement::unavailable("invalid_value", None),
        },
        Err(error) => {
            let status = match error.kind() {
                io::ErrorKind::InvalidData => "invalid_value",
                io::ErrorKind::NotFound => "missing",
                io::ErrorKind::PermissionDenied => "permission_denied",
                _ => "read_error",
            };
            Measurement::unavailable(status, error.raw_os_error().and_then(errno_name))
        }
    }
}

fn errno_name(code: i32) -> Option<&'static str> {
    Some(match code {
        libc::EACCES => "EACCES",
        libc::EAGAIN => "EAGAIN",
        libc::EBADF => "EBADF",
        libc::EBUSY => "EBUSY",
        libc::EFAULT => "EFAULT",
        libc::EINTR => "EINTR",
        libc::EINVAL => "EINVAL",
        libc::EIO => "EIO",
        libc::EISDIR => "EISDIR",
        libc::ELOOP => "ELOOP",
        libc::EMFILE => "EMFILE",
        libc::ENFILE => "ENFILE",
        libc::ENODATA => "ENODATA",
        libc::ENODEV => "ENODEV",
        libc::ENOENT => "ENOENT",
        libc::ENOMEM => "ENOMEM",
        libc::ENOSYS => "ENOSYS",
        libc::ENOTDIR => "ENOTDIR",
        libc::ENXIO => "ENXIO",
        libc::EOPNOTSUPP => "ENOTSUP",
        libc::EOVERFLOW => "EOVERFLOW",
        libc::EPERM => "EPERM",
        libc::EREMOTEIO => "EREMOTEIO",
        libc::ESTALE => "ESTALE",
        libc::ETIMEDOUT => "ETIMEDOUT",
        _ => return None,
    })
}

fn thermal_critical(zone: &Path) -> Option<f64> {
    if !basename(zone).starts_with("thermal_zone") {
        return None;
    }
    prefixed_paths(zone, "trip_point_")
        .into_iter()
        .filter_map(|path| {
            let name = basename(&path);
            let stem = name.strip_suffix("_type")?;
            (text(&path).trim() == "critical")
                .then(|| signed(&zone.join(format!("{stem}_temp"))))
                .flatten()
        })
        .min()
        .map(|raw| raw as f64 / 1000.0)
}

fn channel_status(chip: &Path, channel: &str, sleeping: bool) -> &'static str {
    if sleeping {
        "sleeping"
    } else if unsigned(&chip.join(format!("{channel}_fault"))) == Some(1) {
        "fault"
    } else if unsigned(&chip.join(format!("{channel}_enable"))) == Some(0) {
        "disabled"
    } else {
        "ok"
    }
}

fn any_alarm(chip: &Path, channel: &str, suffixes: &[&str]) -> bool {
    suffixes
        .iter()
        .any(|suffix| unsigned(&chip.join(format!("{channel}_{suffix}"))) == Some(1))
}

fn channel_label(chip: &Path, channel: &str) -> String {
    let label = text(&chip.join(format!("{channel}_label")));
    if label.trim().is_empty() {
        channel.to_owned()
    } else {
        label.trim().to_owned()
    }
}

fn chip_name(chip: &Path) -> String {
    let name = text(&chip.join("name"));
    if name.trim().is_empty() {
        basename(chip)
    } else {
        name.trim().to_owned()
    }
}

fn device_sleeping(device: &Path) -> bool {
    matches!(
        text(&device.join("power/runtime_status")).trim(),
        "suspended" | "suspending"
    )
}

fn digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|c| c.is_ascii_digit())
}

fn paths(root: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    paths.sort_unstable();
    paths
}

fn prefixed_paths(root: &Path, prefix: &str) -> Vec<PathBuf> {
    paths(root)
        .into_iter()
        .filter(|path| basename(path).starts_with(prefix))
        .collect()
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path)
        .ok()
        .or_else(|| {
            let parent = fs::canonicalize(path.parent()?).ok()?;
            Some(parent.join(path.file_name()?))
        })
        .unwrap_or_else(|| path.to_owned())
}

fn basename(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn text(path: &Path) -> String {
    fs::read(path)
        .map(|data| String::from_utf8_lossy(&data).into_owned())
        .unwrap_or_default()
}

fn unsigned(path: &Path) -> Option<u64> {
    text(path).trim().parse().ok()
}

fn signed(path: &Path) -> Option<i64> {
    text(path).trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::symlink,
        sync::atomic::{AtomicU64, Ordering},
    };

    const GPU: &str = "class/drm/card2/device";
    const GPU_MON: &str = "class/drm/card2/device/hwmon/hwmon3";
    const BOARD: &str = "class/hwmon/hwmon8";

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "mainsense-sensors-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn write(&self, path: &str, data: impl AsRef<[u8]>) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, data).unwrap();
        }

        fn gpu(&self) {
            self.write(&format!("{GPU}/vendor"), "0x1002");
            self.write(&format!("{GPU}/device"), "0x1586");
            self.write(&format!("{GPU_MON}/name"), "amdgpu");
            self.write(&format!("{GPU_MON}/temp1_input"), "42500");
            self.write(&format!("{GPU_MON}/temp1_label"), "edge");
            fs::create_dir_all(self.0.join("class/hwmon")).unwrap();
            symlink(self.0.join(GPU_MON), self.0.join("class/hwmon/hwmon3")).unwrap();
        }

        fn collector(&self) -> Sensors {
            Sensors::new(&self.0.join("pci.ids"))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn hides_voltage_channels_without_a_rail_identity() {
        let fixture = Fixture::new();
        fixture.gpu();
        fixture.write(&format!("{GPU_MON}/in0_label"), "vddgfx");
        fixture.write(&format!("{GPU_MON}/in0_input"), "875");
        fixture.write(&format!("{GPU_MON}/in1_input"), "625");
        fixture.write(&format!("{BOARD}/in0_label"), "in0");
        fixture.write(&format!("{BOARD}/in0_input"), "1200");
        let probe = "class/hwmon/hwmon9";
        fixture.write(&format!("{probe}/name"), "strixhalo_smu");
        fixture.write(&format!("{probe}/in0_label"), "SMU rail 1 (0x320)");
        fixture.write(&format!("{probe}/in0_input"), "1070");
        fixture.write(&format!("{probe}/in1_label"), "GFX voltage (SMU)");
        fixture.write(&format!("{probe}/in1_input"), "940");

        let readings = fixture.collector().collect(&fixture.0, 1.0);
        assert_eq!(readings.voltages.len(), 2);
        assert_eq!(readings.voltages[0]["label"], "vddgfx");
        assert_eq!(readings.voltages[1]["label"], "GFX voltage (SMU)");
        assert_eq!(readings.voltages[1]["voltage_v"], 0.94);
        assert_eq!(readings.gpus[0]["voltages"], json!(readings.voltages[..1]));
    }

    #[test]
    fn voltage_units_labels_alarms_and_identity_match_driver() {
        let fixture = Fixture::new();
        fixture.gpu();
        fixture.write(&format!("{GPU_MON}/in0_label"), "vddgfx");
        fixture.write(&format!("{GPU_MON}/in0_input"), "875");
        fixture.write(&format!("{GPU_MON}/in1_label"), "vddnb");
        fixture.write(&format!("{GPU_MON}/in1_input"), "0");
        fixture.write(&format!("{BOARD}/in2_label"), "-12V");
        fixture.write(&format!("{BOARD}/in2_input"), "-11900");
        fixture.write(&format!("{BOARD}/in2_min_alarm"), "1");
        fixture.write(&format!("{BOARD}/in3_label"), "Standby");
        fixture.write(&format!("{BOARD}/in3_input"), "0");
        fixture.write(&format!("{BOARD}/in4_label"), "Vcore");
        fixture.write(&format!("{BOARD}/in4_input"), "bad");
        fixture.write(&format!("{BOARD}/in_bad_input"), "1234");
        fixture.write("class/power_supply/BAT0/type", "Battery");
        fixture.write("class/power_supply/BAT0/voltage_now", "12050000");
        fixture.write("class/power_supply/AC/voltage_max_design", "19000000");

        let readings = fixture.collector().collect(&fixture.0, 1.0);
        let values: Vec<_> = readings
            .voltages
            .iter()
            .map(|v| v["voltage_v"].as_f64())
            .collect();
        assert_eq!(
            values,
            [Some(0.875), None, Some(-11.9), Some(0.0), None, Some(12.05)]
        );
        assert_eq!(readings.voltages[0]["label"], "vddgfx");
        assert_eq!(readings.voltages[1]["status"], "driver_zero");
        assert_eq!(readings.voltages[2]["alarm"], true);
        assert_eq!(readings.voltages[4]["status"], "invalid_value");
        assert_eq!(readings.voltages[5]["label"], "Battery voltage");
        assert_eq!(
            readings.voltages[0]["id"],
            fixture
                .0
                .join(GPU_MON)
                .join("in0_input")
                .to_string_lossy()
                .as_ref()
        );
        assert_eq!(
            readings.gpus[0]["voltages"].as_array().unwrap(),
            &readings.voltages[..2]
        );
        assert_eq!(readings.temperatures.len(), 1);
    }

    #[test]
    fn sleeping_or_disabled_inputs_are_not_opened() {
        let fixture = Fixture::new();
        fixture.gpu();
        fixture.write(&format!("{GPU}/power/runtime_status"), "suspended");
        fixture.write(&format!("{GPU_MON}/in0_label"), "vddgfx");
        fixture.write(&format!("{BOARD}/in0_label"), "Vcore");
        fixture.write(&format!("{BOARD}/in0_enable"), "0");
        fixture.write(&format!("{BOARD}/temp1_fault"), "1");
        // Broken symlinks would be reported as missing if the input were read.
        fs::remove_file(fixture.0.join(GPU_MON).join("temp1_input")).unwrap();
        for input in [
            format!("{GPU_MON}/in0_input"),
            format!("{GPU_MON}/temp1_input"),
            format!("{BOARD}/in0_input"),
            format!("{BOARD}/temp1_input"),
        ] {
            symlink(fixture.0.join("absent"), fixture.0.join(input)).unwrap();
        }
        let readings = fixture.collector().collect(&fixture.0, 1.0);
        assert_eq!(readings.gpus[0]["status"], "sleeping");
        assert_eq!(readings.voltages[0]["status"], "sleeping");
        assert_eq!(readings.voltages[1]["status"], "disabled");
        assert_eq!(readings.temperatures[0]["status"], "sleeping");
        assert_eq!(readings.temperatures[1]["status"], "fault");
        assert!(readings.voltages.iter().all(|v| v["voltage_v"].is_null()));
        assert!(readings
            .temperatures
            .iter()
            .all(|v| v["temperature_c"].is_null()));
    }

    #[test]
    fn nvme_cache_ages_and_evicts_removed_devices() {
        let fixture = Fixture::new();
        let nvme = "class/hwmon/hwmon0";
        fixture.write(&format!("{nvme}/name"), "nvme");
        fixture.write(&format!("{nvme}/temp1_input"), "30000");
        fixture.write(&format!("{BOARD}/name"), "coretemp");
        fixture.write(&format!("{BOARD}/temp1_input"), "30000");
        let mut sensors = fixture.collector();
        sensors.collect(&fixture.0, 1.0);
        fixture.write(&format!("{nvme}/temp1_input"), "40000");
        fixture.write(&format!("{BOARD}/temp1_input"), "50000");
        let readings = sensors.collect(&fixture.0, 1.5);
        assert_eq!(readings.temperatures[0]["temperature_c"], 30.0);
        assert_eq!(readings.temperatures[0]["sample_age_seconds"], 0.5);
        assert_eq!(readings.temperatures[1]["temperature_c"], 50.0);
        assert_eq!(
            sensors.collect(&fixture.0, 3.0).temperatures[0]["temperature_c"],
            40.0
        );
        fs::remove_dir_all(fixture.0.join(nvme)).unwrap();
        assert_eq!(sensors.collect(&fixture.0, 3.2).temperatures.len(), 1);
        fixture.write(&format!("{nvme}/name"), "nvme");
        fixture.write(&format!("{nvme}/temp1_input"), "60000");
        assert_eq!(
            sensors.collect(&fixture.0, 3.5).temperatures[0]["temperature_c"],
            60.0
        );
    }

    fn metrics_blob(size: u16, version: [u8; 2], fields: &[(usize, u16)]) -> Vec<u8> {
        let mut data = vec![0xff; usize::from(size)];
        data[..2].copy_from_slice(&size.to_le_bytes());
        data[2..4].copy_from_slice(&version);
        for &(offset, value) in fields {
            data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        data
    }

    fn gpu_metrics_blob() -> Vec<u8> {
        metrics_blob(
            264,
            [3, 0],
            &[(4, 3150), (6, 3725), (174, 600), (186, 1000)],
        )
    }

    #[test]
    fn gpu_metrics_v1_3_fills_voltages_without_overriding_usable_hwmon() {
        let fixture = Fixture::new();
        fixture.gpu();
        fixture.write(&format!("{GPU_MON}/in0_label"), "VddGfx");
        fixture.write(&format!("{GPU_MON}/in0_input"), "820");
        fixture.write(&format!("{GPU_MON}/in1_label"), "vddnb");
        fixture.write(&format!("{GPU_MON}/in1_input"), "0");
        let path = format!("{GPU}/gpu_metrics");
        fixture.write(
            &path,
            metrics_blob(120, [1, 3], &[(104, 812), (106, 900), (108, 1100)]),
        );

        let readings = fixture.collector().collect(&fixture.0, 1.0);
        let voltages = &readings.voltages;
        assert_eq!(voltages.len(), 4);
        assert_eq!(voltages[0]["voltage_v"], 0.82);
        assert_eq!(voltages[1]["status"], "driver_zero");
        assert_eq!(voltages[2]["label"], "SoC (AMDGPU v1.3)");
        assert_eq!(voltages[2]["voltage_v"], 0.812);
        assert_eq!(voltages[2]["source_offset_bytes"], 104);
        assert_eq!(
            voltages[2]["source"],
            fixture.0.join(&path).to_str().unwrap()
        );
        assert_eq!(voltages[3]["label"], "Memory (AMDGPU v1.3)");
        assert_eq!(voltages[3]["voltage_v"], 1.1);
        assert_eq!(voltages[3]["source_offset_bytes"], 108);
        assert_eq!(readings.gpus[0]["voltages"], json!(voltages));
    }

    #[test]
    fn gpu_metrics_v2_4_labels_reported_averages_and_ignores_invalid_values() {
        let fixture = Fixture::new();
        fixture.gpu();
        let path = format!("{GPU}/gpu_metrics");
        fixture.write(
            &path,
            metrics_blob(168, [2, 4], &[(152, 850), (154, 950), (156, 1050)]),
        );
        let mut sensors = fixture.collector();
        let readings = sensors.collect(&fixture.0, 1.0);
        let observed: Vec<_> = readings
            .voltages
            .iter()
            .map(|entry| {
                (
                    entry["label"].as_str().unwrap(),
                    entry["voltage_v"].as_f64().unwrap(),
                    entry["source_offset_bytes"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            observed,
            [
                ("CPU avg (AMDGPU v2.4)", 0.85, 152),
                ("SoC avg (AMDGPU v2.4)", 0.95, 154),
                ("GFX avg (AMDGPU v2.4)", 1.05, 156),
            ]
        );
        assert_eq!(readings.gpus[0]["voltages"], json!(readings.voltages));

        fixture.write(
            &path,
            metrics_blob(168, [2, 4], &[(152, 0), (154, u16::MAX), (156, 3001)]),
        );
        assert!(sensors.collect(&fixture.0, 1.2).voltages.is_empty());
    }

    #[test]
    fn gpu_metrics_voltages_require_exact_layout_and_an_awake_gpu() {
        let fixture = Fixture::new();
        fixture.gpu();
        let path = format!("{GPU}/gpu_metrics");
        let valid = metrics_blob(168, [2, 4], &[(152, 950)]);
        let mut sensors = fixture.collector();
        fixture.write(&path, &valid);
        assert_eq!(sensors.collect(&fixture.0, 1.0).voltages.len(), 1);

        let mut wrong_header_size = valid.clone();
        wrong_header_size[0] -= 8;
        let mut wrong_revision = valid.clone();
        wrong_revision[3] = 3;
        let mut oversized = valid.clone();
        oversized.push(0);
        for unsupported in [
            &valid[..167],
            &wrong_header_size,
            &wrong_revision,
            &oversized,
            &gpu_metrics_blob(),
        ] {
            fixture.write(&path, unsupported);
            assert!(sensors.collect(&fixture.0, 1.2).voltages.is_empty());
        }

        fixture.write(&path, valid);
        fixture.write(&format!("{GPU}/power/runtime_status"), "suspended");
        let readings = sensors.collect(&fixture.0, 1.4);
        assert!(readings.voltages.is_empty());
        assert_eq!(readings.gpus[0]["voltages"], json!([]));
    }

    #[test]
    fn amdgpu_uses_strict_v3_metrics_without_losing_integer_memory() {
        let fixture = Fixture::new();
        fixture.gpu();
        fixture.write("pci.ids", "1002  AMD\n\t1586  Strix Halo\n");
        fixture.write("class/drm/card10/device/vendor", "0x1002");
        fixture.write("class/drm/card1-DP-1/device/vendor", "0x1002");
        fixture.write("class/drm/card+3/device/vendor", "0x1002");
        fixture.write("class/drm/card1/device/vendor", "0x8086");
        fixture.write(&format!("{GPU}/mem_info_vram_total"), "9007199254740993");
        fixture.write(&format!("{GPU}/gpu_metrics"), gpu_metrics_blob());
        let mut sensors = fixture.collector();
        let readings = sensors.collect(&fixture.0, 1.0);
        let cards: Vec<_> = readings
            .gpus
            .iter()
            .map(|gpu| gpu["card"].as_str().unwrap())
            .collect();
        assert_eq!(cards, ["card2", "card10"]);
        let gpu = &readings.gpus[0];
        assert_eq!(gpu["name"], "Strix Halo");
        assert_eq!(gpu["vram_total_bytes"], 9007199254740993u64);
        assert_eq!(gpu["temperature_c"], 42.5);
        assert_eq!(gpu["memory_clock_mhz"], 1000.0);
        assert_eq!(gpu["memory_clock_source"], "gpu_metrics v3.0 average UCLK");
        assert_eq!(readings.temperatures[1]["temperature_c"], 37.25);
        let mut bad = gpu_metrics_blob();
        bad.push(0);
        fixture.write(&format!("{GPU}/gpu_metrics"), bad);
        let readings = sensors.collect(&fixture.0, 2.0);
        assert!(readings.gpus[0]["memory_clock_mhz"].is_null());
        assert_eq!(readings.temperatures.len(), 1);
    }

    #[test]
    fn gpu_falls_back_to_first_valid_temperature_without_an_edge_label() {
        let fixture = Fixture::new();
        fixture.gpu();
        fixture.write(&format!("{GPU_MON}/temp1_label"), "first");
        fixture.write(&format!("{GPU_MON}/temp2_input"), "50000");
        let readings = fixture.collector().collect(&fixture.0, 1.0);
        assert_eq!(readings.gpus[0]["temperature_c"], 42.5);
        assert_eq!(readings.gpus[0]["temperature_label"], "first");
    }

    #[test]
    fn reports_missing_and_read_error_with_errno_and_uses_same_zone_critical_trip() {
        let fixture = Fixture::new();
        let thermal = "devices/virtual/thermal/thermal_zone1";
        fixture.write(&format!("{thermal}/hwmon1/temp1_input"), "33000");
        fixture.write(&format!("{thermal}/trip_point_0_type"), "passive");
        fixture.write(&format!("{thermal}/trip_point_0_temp"), "80000");
        fixture.write(&format!("{thermal}/trip_point_1_type"), "critical");
        fixture.write(&format!("{thermal}/trip_point_1_temp"), "110000");
        fixture.write(&format!("{BOARD}/name"), "board");
        fixture.write(&format!("{BOARD}/in0_label"), "Vcore");
        fixture.write(&format!("{BOARD}/in1_label"), "Vmem");
        symlink(
            fixture.0.join(thermal).join("hwmon1"),
            fixture.0.join("class/hwmon/hwmon1"),
        )
        .unwrap();
        symlink(
            fixture.0.join("absent"),
            fixture.0.join(BOARD).join("in0_input"),
        )
        .unwrap();
        fs::create_dir(fixture.0.join(BOARD).join("in1_input")).unwrap();

        let readings = fixture.collector().collect(&fixture.0, 1.0);
        assert_eq!(readings.temperatures[0]["critical_c"], 110.0);
        assert_eq!(readings.voltages[0]["status"], "missing");
        assert_eq!(readings.voltages[0]["error_code"], "ENOENT");
        assert_eq!(readings.voltages[1]["status"], "read_error");
        assert_eq!(readings.voltages[1]["error_code"], "EISDIR");
    }
}
