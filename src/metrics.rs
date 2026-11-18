use crate::sensors::Sensors;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::ffi::CStr;
use std::fs;
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

type Counters = (u64, u64);
type CpuCounters = BTreeMap<Option<u32>, Counters>;

pub struct Collector {
    proc: PathBuf,
    sys: PathBuf,
    counters: CpuCounters,
    process_counters: HashMap<(u32, u64), (u64, f64)>,
    clock_ticks: Option<f64>,
    page_size: Option<u64>,
    hostname: String,
    kernel_release: String,
    sensors: Sensors,
}

impl Collector {
    pub fn new() -> Self {
        Self {
            proc: "/proc".into(),
            sys: "/sys".into(),
            counters: BTreeMap::new(),
            process_counters: HashMap::new(),
            clock_ticks: sysconf(libc::_SC_CLK_TCK).map(|ticks| ticks as f64),
            page_size: sysconf(libc::_SC_PAGESIZE),
            hostname: hostname(),
            kernel_release: kernel_release(),
            sensors: Sensors::new(Path::new("/usr/share/misc/pci.ids")),
        }
    }

    pub fn reset_baselines(&mut self) {
        self.counters.clear();
        self.process_counters.clear();
    }

    pub fn sample(&mut self) -> Value {
        let now = monotonic();
        let counters = parse_cpu_stat(&read_text(&self.proc.join("stat")));
        let cpuinfo = read_text(&self.proc.join("cpuinfo"));
        let (model, frequency) = cpu_description(&cpuinfo);
        let readings = self.sensors.collect(&self.sys, now);
        let primary = readings
            .temperatures
            .iter()
            .filter(|sensor| sensor["kind"] == "cpu")
            .filter_map(|sensor| {
                Some((sensor["label"].as_str()?, sensor["temperature_c"].as_f64()?))
            })
            .max_by(|a, b| {
                temperature_rank(a.0)
                    .cmp(&temperature_rank(b.0))
                    .then(a.1.total_cmp(&b.1))
            });
        let cores: Vec<_> = counters
            .iter()
            .filter_map(|(id, values)| {
                Some(json!({
                    "id": (*id)?,
                    "used_percent": cpu_percent(self.counters.get(id), Some(values)),
                }))
            })
            .collect();
        let used_percent = cpu_percent(self.counters.get(&None), counters.get(&None));
        let mut warnings = Vec::new();
        if !counters.contains_key(&None) {
            warnings.push("CPU counters unavailable: cannot read /proc/stat");
        }
        if readings.gpus.is_empty() {
            warnings.push("No AMD GPU visible in /sys/class/drm");
        }
        if primary.is_none() {
            warnings.push("CPU temperature unavailable: no readable CPU hwmon sensor");
        }
        self.counters = counters;
        let load_average = read_text(&self.proc.join("loadavg"));
        let load: Vec<_> = load_average.split_whitespace().take(3).collect();
        let uptime = read_text(&self.proc.join("uptime"))
            .split_whitespace()
            .next()
            .and_then(finite_number);
        json!({
            "schema_version": 1,
            "timestamp": SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|time| time.as_secs_f64())
                .unwrap_or_else(|error| -error.duration().as_secs_f64()),
            "monotonic": now,
            "hostname": self.hostname,
            "kernel_release": self.kernel_release,
            "cpu": {
                "model": model,
                "used_percent": used_percent,
                "cores": cores,
                "frequency_mhz": frequency,
                "temperature_c": primary.map(|(_, value)| value),
                "temperature_label": primary.map_or("unavailable", |(label, _)| label),
            },
            "memory": parse_memory(&read_text(&self.proc.join("meminfo"))),
            "gpus": readings.gpus,
            "sensors": readings.temperatures,
            "voltages": readings.voltages,
            "warnings": warnings,
            "load_average": load,
            "uptime_seconds": uptime,
        })
    }

    pub fn processes(&mut self) -> Vec<Value> {
        let now = monotonic();
        let mut current = HashMap::new();
        let mut entries = Vec::new();
        if let Ok(directories) = fs::read_dir(&self.proc) {
            for directory in directories.flatten() {
                let Some(pid) = directory
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<u32>().ok())
                else {
                    continue;
                };
                let text = read_text(&directory.path().join("stat"));
                let Some((name, ticks, started, rss)) = parse_process(&text) else {
                    continue;
                };
                let key = (pid, started);
                current.insert(key, (ticks, now));
                let usage = self
                    .process_counters
                    .get(&key)
                    .and_then(|(previous, time)| {
                        if now <= *time {
                            return None;
                        }
                        Some(round_tenth(
                            ticks.checked_sub(*previous)? as f64 / self.clock_ticks? / (now - time)
                                * 100.0,
                        ))
                    });
                let rss_bytes = self.page_size.and_then(|size| rss.checked_mul(size));
                entries.push((pid, name.to_owned(), usage, rss_bytes));
            }
        }
        self.process_counters = current;
        entries.sort_unstable_by(|a, b| {
            b.2.unwrap_or(-1.0)
                .total_cmp(&a.2.unwrap_or(-1.0))
                .then(b.3.cmp(&a.3))
                .then(a.0.cmp(&b.0))
        });
        entries
            .into_iter()
            .take(30)
            .map(|(pid, name, usage, rss)| {
                json!({"pid": pid, "name": name, "cpu_percent": usage, "rss_bytes": rss})
            })
            .collect()
    }
}

fn parse_cpu_stat(text: &str) -> CpuCounters {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let suffix = fields.next()?.strip_prefix("cpu")?;
            let id = if suffix.is_empty() {
                None
            } else {
                Some(suffix.parse::<u32>().ok()?)
            };
            let mut total = 0u64;
            let mut idle = None;
            for (index, text) in fields.take(8).enumerate() {
                let ticks = text.parse::<u64>().ok()?;
                total = total.checked_add(ticks)?;
                // Guest and guest-nice already contribute to user and nice.
                if index == 3 {
                    idle = Some(ticks);
                } else if index == 4 {
                    idle = Some(idle?.checked_add(ticks)?);
                }
            }
            Some((id, (total, idle?)))
        })
        .collect()
}

fn cpu_percent(previous: Option<&Counters>, current: Option<&Counters>) -> Option<f64> {
    let ((old_total, old_idle), (total, idle)) = (previous?, current?);
    let elapsed = total.checked_sub(*old_total)?;
    let busy = elapsed.checked_sub(idle.checked_sub(*old_idle)?)?;
    percent(Some(busy), Some(elapsed))
}

fn cpu_description(text: &str) -> (&str, Option<u64>) {
    let mut model = None;
    let mut total = 0.0;
    let mut count = 0;
    for (key, value) in text.lines().filter_map(|line| line.split_once(':')) {
        match key.trim() {
            "model name" if model.is_none() && !value.trim().is_empty() => {
                model = Some(value.trim())
            }
            "cpu MHz" => {
                if let Some(mhz) = finite_number(value.trim()).filter(|mhz| *mhz >= 0.0) {
                    total += mhz;
                    count += 1;
                }
            }
            _ => {}
        }
    }
    let frequency = (count > 0).then(|| (total / f64::from(count)).round_ties_even() as u64);
    (model.unwrap_or("CPU"), frequency)
}

fn parse_memory(text: &str) -> Value {
    let values: HashMap<_, _> = text
        .lines()
        .filter_map(|line| {
            let (key, fields) = line.split_once(':')?;
            let amount = fields.split_whitespace().next()?.parse::<u64>().ok()?;
            Some((key, amount.checked_mul(1024)?))
        })
        .collect();
    let get = |key| values.get(key).copied();
    let total = get("MemTotal");
    let available = get("MemAvailable");
    let used = difference(total, available);
    let swap_total = get("SwapTotal");
    let cache = (|| {
        let sum = get("Cached")?
            .checked_add(get("Buffers")?)?
            .checked_add(get("SReclaimable")?)?;
        Some(sum.saturating_sub(get("Shmem")?))
    })();
    json!({
        "total_bytes": total,
        "available_bytes": available,
        "used_bytes": used,
        "used_percent": percent(used, total),
        "cache_bytes": cache,
        "swap_total_bytes": swap_total,
        "swap_used_bytes": difference(swap_total, get("SwapFree")),
    })
}

fn difference(total: Option<u64>, available: Option<u64>) -> Option<u64> {
    total?.checked_sub(available?)
}

fn percent(used: Option<u64>, total: Option<u64>) -> Option<f64> {
    let total = total?;
    if total == 0 {
        return None;
    }
    Some(round_tenth(used? as f64 / total as f64 * 100.0))
}

fn round_tenth(value: f64) -> f64 {
    (value * 10.0).round_ties_even() / 10.0
}

fn parse_process(text: &str) -> Option<(&str, u64, u64, u64)> {
    let start = text.find('(')?;
    let end = text.rfind(')')?;
    let name = text.get(start + 1..end)?;
    let mut fields = text.get(end + 1..)?.split_whitespace();
    let user = fields.nth(11)?.parse::<u64>().ok()?;
    let system = fields.next()?.parse::<u64>().ok()?;
    let started = fields.nth(6)?.parse::<u64>().ok()?;
    let rss = fields.nth(1)?.parse::<i64>().ok()?.max(0) as u64;
    Some((name, user.checked_add(system)?, started, rss))
}

fn temperature_rank(label: &str) -> u8 {
    match label {
        "Tdie" => 2,
        label if label == "Tctl" || label.starts_with("Package") => 1,
        _ => 0,
    }
}

fn read_text(path: &Path) -> String {
    fs::read(path)
        .map(|data| String::from_utf8_lossy(&data).into_owned())
        .unwrap_or_default()
}

fn finite_number(text: &str) -> Option<f64> {
    text.parse::<f64>().ok().filter(|value| value.is_finite())
}

fn monotonic() -> f64 {
    let mut time = MaybeUninit::<libc::timespec>::uninit();
    // The output is initialized only after a successful kernel call.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, time.as_mut_ptr()) } != 0 {
        return 0.0;
    }
    let time = unsafe { time.assume_init() };
    time.tv_sec as f64 + time.tv_nsec as f64 / 1_000_000_000.0
}

fn sysconf(name: libc::c_int) -> Option<u64> {
    u64::try_from(unsafe { libc::sysconf(name) })
        .ok()
        .filter(|value| *value > 0)
}

fn hostname() -> String {
    let mut bytes = [0; 256];
    // Linux hostnames are shorter than the buffer and gethostname writes a C string.
    if unsafe { libc::gethostname(bytes.as_mut_ptr(), bytes.len()) } == 0 {
        unsafe { CStr::from_ptr(bytes.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    } else {
        "unknown".into()
    }
}

fn kernel_release() -> String {
    let mut version = MaybeUninit::<libc::utsname>::uninit();
    if unsafe { libc::uname(version.as_mut_ptr()) } != 0 {
        return "unknown".into();
    }
    let version = unsafe { version.assume_init() };
    unsafe { CStr::from_ptr(version.release.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_deltas_ignore_guest_ticks_and_reject_resets() {
        let first = parse_cpu_stat(
            "cpu 100 20 30 400 10 5 5 0 80 10\ncpu2 1 2 3 4\ncpu10 1 2 3 4\ncpu0 nope",
        );
        let second = parse_cpu_stat("cpu 150 20 30 450 10 5 5 0 100 10\ncpu2 0 0 0 0");
        assert_eq!(
            first.keys().copied().collect::<Vec<_>>(),
            [None, Some(2), Some(10)]
        );
        assert_eq!(first[&None], (570, 410));
        assert_eq!(cpu_percent(first.get(&None), second.get(&None)), Some(50.0));
        assert_eq!(cpu_percent(first.get(&Some(2)), second.get(&Some(2))), None);
        assert_eq!(cpu_percent(first.get(&None), first.get(&None)), None);
    }

    #[test]
    fn memory_uses_available_bytes_and_keeps_swap_separate() {
        let memory = parse_memory("MemTotal: 1000 kB\nMemAvailable: 600 kB\nCached: 500 kB\nBuffers: 50 kB\nSReclaimable: 40 kB\nShmem: 10 kB\nSwapTotal: 200 kB\nSwapFree: 150 kB");
        assert_eq!(memory["used_bytes"], 409600);
        assert_eq!(memory["used_percent"], 40.0);
        assert_eq!(memory["cache_bytes"], 593920);
        assert_eq!(memory["swap_used_bytes"], 51200);
        assert!(parse_memory("MemTotal: 1000 kB\nMemFree: 100 kB")["used_bytes"].is_null());
    }

    #[test]
    fn process_stat_preserves_names_and_start_times() {
        let stat = "12 (worker (a) b) S 0 0 0 0 0 0 0 0 0 0 30 4 0 0 0 0 0 0 55 0 -1";
        assert_eq!(parse_process(stat), Some(("worker (a) b", 34, 55, 0)));
        assert!(parse_process("12 (short) S 0").is_none());
    }
}
