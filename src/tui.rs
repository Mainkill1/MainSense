//! Terminal presentation and interactive sampling.

use crate::metrics::Collector;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

const MIN_WIDTH: usize = 38;
const MIN_HEIGHT: usize = 10;
const HISTORY_SECONDS: f64 = 60.0;
const MAX_HISTORY_SAMPLES: usize = 600;
const PROCESS_LIMIT: usize = 30;
const SIGNAL_CHECK_INTERVAL: Duration = Duration::from_millis(250);

type History = VecDeque<(f64, Option<f64>)>;

struct State {
    interval: Duration,
    paused: bool,
    cores: bool,
    sensors: bool,
    processes: bool,
    help: bool,
    gpu: usize,
    scroll: usize,
    page_rows: usize,
    ascii: bool,
    history: HashMap<String, History>,
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Ignore,
    Quit,
    Redraw,
    Resume,
    Refresh,
}

impl State {
    fn new(interval: Duration, ascii: bool, compact: bool) -> Self {
        Self {
            interval,
            paused: false,
            cores: !compact,
            sensors: !compact,
            processes: false,
            help: false,
            gpu: 0,
            scroll: 0,
            page_rows: 12,
            ascii,
            history: HashMap::new(),
        }
    }

    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Action {
        if modifiers.contains(KeyModifiers::CONTROL) {
            return match code {
                KeyCode::Char('c' | 'C') => Action::Quit,
                _ => Action::Ignore,
            };
        }
        match code {
            KeyCode::Char('q' | 'Q') => return Action::Quit,
            KeyCode::Char(' ') => {
                self.paused = !self.paused;
                if !self.paused {
                    return Action::Resume;
                }
            }
            KeyCode::Char('+' | '=' | '-' | '_') => {
                let factor = match code {
                    KeyCode::Char('+' | '=') => 1.0 / 1.5,
                    _ => 1.5,
                };
                let hundredths = (self.interval.as_secs_f64() * factor * 100.0).round();
                self.interval = Duration::from_millis(hundredths.clamp(20.0, 500.0) as u64 * 10);
                return Action::Refresh;
            }
            KeyCode::Char('g') => self.gpu = self.gpu.wrapping_add(1),
            KeyCode::Char('c' | 't' | 'p' | '?' | 'h') | KeyCode::Esc => {
                match code {
                    KeyCode::Char('c') => self.cores = !self.cores,
                    KeyCode::Char('t') => self.sensors = !self.sensors,
                    KeyCode::Char('p') => self.processes = !self.processes,
                    KeyCode::Char('?' | 'h') => self.help = !self.help,
                    KeyCode::Esc => self.help = false,
                    _ => unreachable!(),
                }
                self.scroll = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(self.page_rows),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(self.page_rows),
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = usize::MAX,
            _ => return Action::Ignore,
        }
        Action::Redraw
    }

    fn record(&mut self, sample: &Value) {
        let now = sample["monotonic"].as_f64().unwrap_or_default();
        let gpus = list(&sample["gpus"]);
        self.history
            .retain(|id, _| id == "cpu" || gpus.iter().any(|gpu| gpu["id"].as_str() == Some(id)));
        self.record_value("cpu", now, sample["cpu"]["used_percent"].as_f64());
        for gpu in gpus {
            if let Some(id) = gpu["id"].as_str() {
                self.record_value(id, now, gpu["used_percent"].as_f64());
            }
        }
    }

    fn record_value(&mut self, id: &str, now: f64, value: Option<f64>) {
        let history = self.history.entry(id.to_owned()).or_default();
        history.push_back((now, value));
        while history.len() > MAX_HISTORY_SAMPLES
            || history
                .front()
                .is_some_and(|entry| entry.0 < now - HISTORY_SECONDS)
        {
            history.pop_front();
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Plain,
    Cpu,
    Gpu,
    Memory,
    Sensor,
    Muted,
    Alarm,
}

impl Tone {
    fn color(self) -> Color {
        match self {
            Self::Plain => Color::Reset,
            Self::Cpu => Color::Cyan,
            Self::Gpu => Color::Magenta,
            Self::Memory => Color::Green,
            Self::Sensor => Color::Yellow,
            Self::Muted => Color::Grey,
            Self::Alarm => Color::Red,
        }
    }
}

#[derive(Clone, Copy)]
struct Cell {
    glyph: char,
    tone: Tone,
}

const EMPTY: Cell = Cell {
    glyph: ' ',
    tone: Tone::Plain,
};
type Row = Vec<Cell>;

struct Canvas {
    rows: Vec<Row>,
    width: usize,
    ascii: bool,
}

impl Canvas {
    fn new(width: usize, height: usize, ascii: bool) -> Self {
        Self {
            rows: vec![vec![EMPTY; width]; height],
            width,
            ascii,
        }
    }

    fn put(&mut self, y: usize, x: usize, text: &str, tone: Tone, limit: usize) {
        if x >= self.width {
            return;
        }
        if self.rows.len() <= y {
            self.rows.resize_with(y + 1, || vec![EMPTY; self.width]);
        }
        for (cell, glyph) in self.rows[y][x..].iter_mut().take(limit).zip(text.chars()) {
            *cell = Cell {
                glyph: safe_glyph(glyph, self.ascii),
                tone,
            };
        }
    }
}

struct Panel<'a> {
    canvas: &'a mut Canvas,
    x: usize,
    y: usize,
    width: usize,
}

impl<'a> Panel<'a> {
    fn new(
        canvas: &'a mut Canvas,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        title: &str,
        tone: Tone,
    ) -> Self {
        let (tl, tr, bl, br, horizontal, vertical) = if canvas.ascii {
            ('+', '+', '+', '+', '-', "|")
        } else {
            ('╭', '╮', '╰', '╯', '─', "│")
        };
        let rule = horizontal.to_string().repeat(width - 2);
        canvas.put(y, x, &format!("{tl}{rule}{tr}"), Tone::Muted, width);
        for row in 1..height - 1 {
            canvas.put(y + row, x, vertical, Tone::Muted, 1);
            canvas.put(y + row, x + width - 1, vertical, Tone::Muted, 1);
        }
        canvas.put(
            y + height - 1,
            x,
            &format!("{bl}{rule}{br}"),
            Tone::Muted,
            width,
        );
        let mut panel = Self {
            canvas,
            x,
            y,
            width,
        };
        panel.line(0, &format!(" {title} "), tone);
        panel
    }

    fn line(&mut self, row: usize, text: &str, tone: Tone) {
        self.canvas
            .put(self.y + row, self.x + 2, text, tone, self.width - 4);
    }

    fn meter(&mut self, row: usize, label: &str, value: &Value, tone: Tone) {
        let bar = gauge(value, self.width - 18, self.canvas.ascii);
        self.line(
            row,
            &format!("{label:<5} {bar} {:>6}", number(value, "%", 1)),
            tone,
        );
    }

    fn history(&mut self, history: Option<&History>, now: f64, tone: Tone) {
        self.line(
            2,
            &spark(history, self.width - 4, now, self.canvas.ascii),
            tone,
        );
        self.line(3, "60s history                 0-100%", Tone::Muted);
    }
}

fn safe_glyph(glyph: char, ascii: bool) -> char {
    if glyph.is_control() || (ascii && !glyph.is_ascii()) || glyph.width() != Some(1) {
        '?'
    } else {
        glyph
    }
}

fn list(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

fn text<'a>(value: &'a Value, fallback: &'a str) -> &'a str {
    value.as_str().unwrap_or(fallback)
}

fn number(value: &Value, unit: &str, precision: usize) -> String {
    match value.as_f64() {
        Some(value) => format!("{value:.precision$}{unit}"),
        None => "--".to_owned(),
    }
}

fn size(value: &Value) -> String {
    let Some(mut value) = value.as_f64() else {
        return "--".to_owned();
    };
    for unit in ["B", "KiB", "MiB", "GiB"] {
        if value.abs() < 1024.0 {
            return format!("{value:.1} {unit}");
        }
        value /= 1024.0;
    }
    format!("{value:.1} TiB")
}

fn gauge(value: &Value, width: usize, ascii: bool) -> String {
    let Some(value) = value.as_f64() else {
        return "-".repeat(width);
    };
    let eighths = (value.clamp(0.0, 100.0) * width as f64 * 8.0 / 100.0) as usize;
    let (full, remainder) = (eighths / 8, eighths % 8);
    let (filled, empty, fractional) = if ascii {
        ('#', '.', [' ', '+', '+', '+', '+', '+', '+', '+'])
    } else {
        ('█', '░', [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'])
    };
    let partial = (remainder != 0).then_some(fractional[remainder]);
    std::iter::repeat_n(filled, full)
        .chain(partial)
        .chain(std::iter::repeat(empty))
        .take(width)
        .collect()
}

fn spark(history: Option<&History>, width: usize, now: f64, ascii: bool) -> String {
    let mut buckets = vec![(0.0, 0u32); width];
    for &(timestamp, value) in history.into_iter().flatten() {
        if let Some(value) = value.filter(|_| (now - HISTORY_SECONDS..=now).contains(&timestamp)) {
            let elapsed = timestamp - (now - HISTORY_SECONDS);
            let index = ((elapsed * width as f64 / HISTORY_SECONDS) as usize).min(width - 1);
            buckets[index].0 += value;
            buckets[index].1 += 1;
        }
    }
    let levels = if ascii {
        ['.', '_', ':', '-', '=', '+', '*', '#']
    } else {
        ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█']
    };
    buckets
        .into_iter()
        .map(|(sum, count)| {
            if count == 0 {
                ' '
            } else {
                let level = (sum / f64::from(count) * 7.0 / 100.0)
                    .round()
                    .clamp(0.0, 7.0);
                levels[level as usize]
            }
        })
        .collect()
}

fn overview(canvas: &mut Canvas, state: &State, sample: &Value, cpu: &Value, gpu: &Value) -> usize {
    let wide = canvas.width >= 80;
    let panel_width = if wide {
        (canvas.width - 1) / 2
    } else {
        canvas.width
    };
    let right = if wide { panel_width + 1 } else { 0 };
    let gpu_width = canvas.width - right;
    let now = sample["monotonic"].as_f64().unwrap_or_default();
    let mut panel = Panel::new(
        canvas,
        0,
        0,
        panel_width,
        9,
        &format!("CPU / {} threads", list(&cpu["cores"]).len()),
        Tone::Cpu,
    );
    panel.meter(1, "BUSY", &cpu["used_percent"], Tone::Cpu);
    panel.history(state.history.get("cpu"), now, Tone::Cpu);
    panel.line(
        4,
        &format!(
            "Temp  {}  {}",
            number(&cpu["temperature_c"], " C", 1),
            text(&cpu["temperature_label"], "--")
        ),
        Tone::Cpu,
    );
    panel.line(
        5,
        &format!("Clock {} avg", number(&cpu["frequency_mhz"], " MHz", 0)),
        Tone::Cpu,
    );
    let load: Vec<_> = list(&sample["load_average"])
        .iter()
        .filter_map(Value::as_str)
        .collect();
    panel.line(6, &format!("Load  {}", load.join("  ")), Tone::Plain);
    panel.line(7, "Aggregate CPU across all threads", Tone::Muted);

    let mut panel = Panel::new(
        canvas,
        right,
        if wide { 0 } else { 9 },
        gpu_width,
        9,
        &format!("GPU / {}", text(&gpu["card"], "--")),
        Tone::Gpu,
    );
    if gpu.is_null() {
        panel.line(2, "No AMD GPU visible", Tone::Sensor);
        panel.line(4, "GPU / VRAM / temperature: --", Tone::Muted);
        panel.line(6, "CPU and RAM monitoring continues", Tone::Muted);
    } else {
        panel.meter(1, "BUSY", &gpu["used_percent"], Tone::Gpu);
        panel.history(state.history.get(text(&gpu["id"], "")), now, Tone::Gpu);
        panel.line(
            4,
            &format!(
                "Temp  {}  {}",
                number(&gpu["temperature_c"], " C", 1),
                text(&gpu["temperature_label"], "--")
            ),
            Tone::Gpu,
        );
        panel.line(
            5,
            &format!(
                "Clock {}  SoC {}",
                number(&gpu["clock_mhz"], " MHz", 0),
                number(&gpu["power_w"], " W", 1)
            ),
            Tone::Gpu,
        );
        panel.line(
            6,
            &format!(
                "Fan {}  Mem activity {}",
                number(&gpu["fan_rpm"], " RPM", 0),
                number(&gpu["memory_busy_percent"], "%", 0)
            ),
            Tone::Plain,
        );
        panel.line(
            7,
            &format!(
                "{}  {}",
                text(&gpu["pci_address"], "--"),
                text(&gpu["status"], "--")
            ),
            Tone::Muted,
        );
    }

    let memory = &sample["memory"];
    let memory_y = if wide { 9 } else { 18 };
    let mut panel = Panel::new(
        canvas,
        0,
        memory_y,
        panel_width,
        7,
        "RAM / Linux system memory",
        Tone::Memory,
    );
    panel.line(
        1,
        &format!(
            "{} / {}",
            size(&memory["used_bytes"]),
            size(&memory["total_bytes"])
        ),
        Tone::Memory,
    );
    panel.meter(2, "USED", &memory["used_percent"], Tone::Memory);
    panel.line(
        3,
        &format!("Available  {}", size(&memory["available_bytes"])),
        Tone::Plain,
    );
    panel.line(
        4,
        &format!("Cache      {}", size(&memory["cache_bytes"])),
        Tone::Muted,
    );
    panel.line(
        5,
        &format!(
            "Swap  {} / {}",
            size(&memory["swap_used_bytes"]),
            size(&memory["swap_total_bytes"])
        ),
        Tone::Plain,
    );

    let vram_y = if wide { memory_y } else { memory_y + 7 };
    let mut panel = Panel::new(
        canvas,
        right,
        vram_y,
        gpu_width,
        7,
        "VRAM / GPU memory pool",
        Tone::Gpu,
    );
    panel.line(
        1,
        &format!(
            "{} / {}",
            size(&gpu["vram_used_bytes"]),
            size(&gpu["vram_total_bytes"])
        ),
        Tone::Gpu,
    );
    panel.meter(2, "USED", &gpu["vram_used_percent"], Tone::Gpu);
    panel.line(
        3,
        &format!(
            "GTT {} / {}",
            size(&gpu["gtt_used_bytes"]),
            size(&gpu["gtt_total_bytes"])
        ),
        Tone::Plain,
    );
    let clock_label = if text(&gpu["memory_clock_source"], "").contains("average UCLK") {
        "UCLK avg"
    } else {
        "Mem clock"
    };
    panel.line(
        4,
        &format!(
            "{clock_label}  {}",
            number(&gpu["memory_clock_mhz"], " MHz", 0)
        ),
        Tone::Muted,
    );
    panel.line(5, "GTT overlaps RAM", Tone::Muted);
    let y = vram_y + 7;
    canvas.put(
        y,
        1,
        &format!("GPU: {}", text(&gpu["name"], "unavailable")),
        Tone::Muted,
        canvas.width - 2,
    );
    y + 1
}

fn cpu_threads(canvas: &mut Canvas, y: usize, cores: &[Value]) -> usize {
    let columns = canvas.width / 20;
    let rows = cores.len().div_ceil(columns).max(1);
    let width = canvas.width;
    let cell_width = (width - 4) / columns;
    Panel::new(
        canvas,
        0,
        y,
        width,
        rows + 2,
        "CPU THREADS / c to collapse",
        Tone::Cpu,
    );
    for (index, core) in cores.iter().enumerate() {
        let id = core["id"]
            .as_u64()
            .map_or_else(|| "--".to_owned(), |id| id.to_string());
        let bar = gauge(&core["used_percent"], cell_width - 11, canvas.ascii);
        let used = number(&core["used_percent"], "%", 0);
        let x = 2 + index % columns * cell_width;
        canvas.put(
            y + 1 + index / columns,
            x,
            &format!("{id:>3} {bar} {used:>4}"),
            Tone::Cpu,
            cell_width - 1,
        );
    }
    y + rows + 2
}

fn sensor_label(sensor: &Value, width: usize, ascii: bool) -> String {
    format!(
        "{} / {}:{}",
        text(&sensor["label"], "--"),
        text(&sensor["chip"], "--"),
        text(&sensor["device"], "--")
    )
    .chars()
    .take(width)
    .map(|glyph| safe_glyph(glyph, ascii))
    .collect()
}

fn temperatures(canvas: &mut Canvas, y: usize, sensors: &[Value]) -> usize {
    let width = canvas.width;
    let ascii = canvas.ascii;
    let height = sensors.len().max(1) + 3;
    let label_width = (width - 24).min(40);
    let age_header = if width >= 72 { "    AGE" } else { "" };
    let mut panel = Panel::new(
        canvas,
        0,
        y,
        width,
        height,
        "TEMPERATURES / t to collapse",
        Tone::Sensor,
    );
    panel.line(
        1,
        &format!(
            "{:<label_width$} {:>8} {:>9}{age_header}",
            "SENSOR / DEVICE", "CURRENT", "CRITICAL"
        ),
        Tone::Muted,
    );
    for (index, sensor) in sensors.iter().enumerate() {
        let label = sensor_label(sensor, label_width, ascii);
        let reading = &sensor["temperature_c"];
        let critical = &sensor["critical_c"];
        let alarm = sensor["alarm"].as_bool().unwrap_or_default()
            || reading
                .as_f64()
                .zip(critical.as_f64())
                .is_some_and(|(value, threshold)| value >= threshold);
        let age = if width >= 72 {
            format!(
                " {:>5.1}s",
                sensor["sample_age_seconds"].as_f64().unwrap_or_default()
            )
        } else {
            String::new()
        };
        let marker = if alarm { " !" } else { "" };
        let tone = if alarm { Tone::Alarm } else { Tone::Plain };
        panel.line(
            index + 2,
            &format!(
                "{label:<label_width$} {:>8} {:>9}{age}{marker}",
                number(reading, " C", 1),
                number(critical, " C", 1)
            ),
            tone,
        );
    }
    if sensors.is_empty() {
        panel.line(2, "No readable hwmon temperature sensors", Tone::Muted);
    }
    y + height
}

fn voltages(canvas: &mut Canvas, y: usize, voltages: &[Value]) -> usize {
    if voltages.is_empty() {
        return y;
    }
    let width = canvas.width;
    let ascii = canvas.ascii;
    let height = voltages.len() + 3;
    let label_width = (width - 16).min(40);
    let mut panel = Panel::new(
        canvas,
        0,
        y,
        width,
        height,
        "VOLTAGES / t to collapse",
        Tone::Sensor,
    );
    panel.line(
        1,
        &format!("{:<label_width$} {:>9}", "SENSOR / DEVICE", "CURRENT"),
        Tone::Muted,
    );
    for (index, sensor) in voltages.iter().enumerate() {
        let label = sensor_label(sensor, label_width, ascii);
        let reading = number(&sensor["voltage_v"], " V", 3);
        let (alarm, tone) = if sensor["alarm"].as_bool() == Some(true) {
            (" !", Tone::Alarm)
        } else {
            ("", Tone::Plain)
        };
        panel.line(
            index + 2,
            &format!("{label:<label_width$} {reading:>9}{alarm}"),
            tone,
        );
    }
    y + height
}

fn processes(canvas: &mut Canvas, y: usize, entries: &[Value]) -> usize {
    let entries = &entries[..entries.len().min(PROCESS_LIMIT)];
    let height = entries.len().max(1) + 3;
    let mut panel = Panel::new(
        canvas,
        0,
        y,
        canvas.width,
        height,
        "PROCESSES / CPU order / p to collapse",
        Tone::Memory,
    );
    panel.line(1, "    PID   CPU%       RAM  COMMAND", Tone::Muted);
    for (index, entry) in entries.iter().enumerate() {
        let pid = entry["pid"]
            .as_u64()
            .map_or_else(|| "--".to_string(), |pid| pid.to_string());
        panel.line(
            index + 2,
            &format!(
                "{pid:>7} {:>6} {:>9}  {}",
                number(&entry["cpu_percent"], "", 1),
                size(&entry["rss_bytes"]),
                text(&entry["name"], "--")
            ),
            Tone::Plain,
        );
    }
    if entries.is_empty() {
        panel.line(2, "Waiting for readable processes...", Tone::Muted);
    }
    y + height
}

const HELP: &[&str] = &[
    "KEYBOARD",
    "q / Ctrl+C      Quit",
    "Space           Pause / resume collection",
    "+ / -           Faster / slower refresh (0.2 to 5 seconds)",
    "c               Show / hide CPU thread meters",
    "t               Show / hide temperature and voltage sensors",
    "p               Show / hide CPU process table",
    "g               Select next AMD GPU",
    "Up/Down, j/k    Scroll",
    "PgUp/PgDn       Scroll one page",
    "Home / End      First / last panel",
    "? / h           Help",
    "Esc             Close help",
    "",
    "READING THE DISPLAY",
    "CPU uses the interval between two /proc/stat samples.",
    "RAM used = MemTotal - MemAvailable. Cache is reclaimable.",
    "VRAM is the driver-reported GPU memory pool, including APU reservations.",
    "GTT is system-backed GPU memory and overlaps system RAM.",
    "Mem activity is memory controller busy time.",
    "SoC power includes CPU power on AMD APUs.",
    "Tctl is a CPU control reading. Tdie is preferred if exposed.",
    "-- means missing, unreadable, sleeping or awaiting a sample.",
    "Graphs show the last 60 seconds on a fixed 0-100% scale.",
    "Pausing stops recording and leaves gaps in the history.",
    "CPU and process counters start fresh on resume.",
    "NVMe temperatures refresh every 2 seconds. AGE shows time since reading.",
    "UCLK avg is a firmware-filtered memory controller clock.",
    "One fully busy thread shows 100% process CPU use.",
    "Run with --ascii or --no-color for simpler terminals.",
];

fn help(canvas: &mut Canvas) {
    let limit = canvas.width - 4;
    let mut y = 0;
    for &line in HELP {
        let tone = if matches!(line, "KEYBOARD" | "READING THE DISPLAY") {
            Tone::Cpu
        } else {
            Tone::Plain
        };
        let mut rest = line;
        while rest.len() > limit {
            let split = rest[..limit]
                .rfind(' ')
                .filter(|&index| index > 0)
                .unwrap_or(limit);
            canvas.put(y, 2, &rest[..split], tone, limit);
            y += 1;
            rest = rest[split..].trim_start();
        }
        canvas.put(y, 2, rest, tone, limit);
        y += 1;
    }
}

fn render(
    sample: &Value,
    state: &mut State,
    width: usize,
    height: usize,
    viewport: bool,
) -> Vec<Row> {
    if viewport && (width < MIN_WIDTH || height < MIN_HEIGHT) {
        let mut frame = Canvas::new(width, height, state.ascii);
        for (index, line) in ["MainSense", "Resize to at least 38 x 10", "q quit | ? help"]
            .into_iter()
            .take(height)
            .enumerate()
        {
            frame.put(index, 0, line, Tone::Sensor, width);
        }
        return frame.rows;
    }

    let mut canvas = Canvas::new(width, 0, state.ascii);
    let cpu = &sample["cpu"];
    let gpus = list(&sample["gpus"]);
    let gpu = gpus
        .get(state.gpu % gpus.len().max(1))
        .unwrap_or(&Value::Null);
    if state.help {
        help(&mut canvas);
    } else {
        let mut y = overview(&mut canvas, state, sample, cpu, gpu);
        if state.cores {
            y = cpu_threads(&mut canvas, y, list(&cpu["cores"]));
        }
        if state.sensors {
            y = temperatures(&mut canvas, y, list(&sample["sensors"]));
            y = voltages(&mut canvas, y, list(&sample["voltages"]));
        }
        if state.processes {
            y = processes(&mut canvas, y, list(&sample["processes"]));
        }
        for (index, warning) in list(&sample["warnings"])
            .iter()
            .filter_map(Value::as_str)
            .enumerate()
        {
            canvas.put(y + index, 1, warning, Tone::Sensor, width - 2);
        }
    }

    let total = canvas.rows.len();
    let height = if viewport { height } else { total + 5 };
    state.page_rows = height - 5;
    state.scroll = state.scroll.min(total.saturating_sub(state.page_rows));
    let mut frame = Canvas::new(width, height, state.ascii);
    let status = format!(
        "{}  {}s",
        if state.paused { "PAUSED" } else { "LIVE" },
        state.interval.as_secs_f64()
    );
    frame.put(0, 1, "MainSense", Tone::Cpu, width - 2);
    frame.put(
        0,
        width - status.len() - 1,
        &status,
        if state.paused {
            Tone::Sensor
        } else {
            Tone::Cpu
        },
        status.len(),
    );
    frame.put(
        1,
        1,
        &format!(
            "{} / {}",
            text(&sample["hostname"], "--"),
            text(&cpu["model"], "--")
        ),
        Tone::Muted,
        width - 2,
    );
    let on = |enabled| if enabled { "on" } else { "off" };
    frame.put(
        2,
        1,
        &format!(
            "c cores {}  t sensors {}  p procs {}",
            on(state.cores),
            on(state.sensors),
            on(state.processes)
        ),
        Tone::Muted,
        width - 2,
    );
    for (index, row) in canvas
        .rows
        .into_iter()
        .skip(state.scroll)
        .take(state.page_rows)
        .enumerate()
    {
        frame.rows[index + 3] = row;
    }
    let last = (state.scroll + state.page_rows).min(total);
    frame.put(
        height - 2,
        1,
        &format!(
            "? help  {}-{last}/{total}  Up/Down scroll  g GPU",
            state.scroll + 1
        ),
        Tone::Muted,
        width - 2,
    );
    frame.put(
        height - 1,
        1,
        "q quit  Space pause  +/- refresh  c cores  t sensors  p procs",
        Tone::Plain,
        width - 2,
    );
    frame.rows
}

pub fn render_once(sample: &Value, width: u16, ascii: bool, compact: bool) -> String {
    let mut state = State::new(Duration::from_secs(1), ascii, compact);
    state.record(sample);
    render(
        sample,
        &mut state,
        usize::from(width).max(MIN_WIDTH),
        0,
        false,
    )
    .iter()
    .map(|row| {
        row.iter()
            .map(|cell| cell.glyph)
            .collect::<String>()
            .trim_end()
            .to_owned()
    })
    .collect::<Vec<_>>()
    .join("\n")
}

// Construct the guard before sending any screen commands so partial setup is unwound too.
struct TerminalSession {
    no_color: bool,
}

impl TerminalSession {
    fn start(no_color: bool) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let session = Self { no_color };
        execute!(io::stdout(), EnterAlternateScreen, Hide)?;
        Ok(session)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        if !self.no_color {
            let _ = execute!(io::stdout(), ResetColor);
        }
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
    }
}

fn draw(frame: &[Row], no_color: bool) -> io::Result<()> {
    let mut buffer = Vec::new();
    queue!(buffer, Clear(ClearType::All))?;
    let mut tone = Tone::Plain;
    for (y, row) in frame.iter().enumerate() {
        let end = row
            .iter()
            .rposition(|cell| cell.glyph != ' ')
            .map_or(0, |index| index + 1);
        if end == 0 {
            continue;
        }
        queue!(buffer, MoveTo(0, y as u16))?;
        for group in row[..end].chunk_by(|left, right| left.tone == right.tone) {
            if !no_color && tone != group[0].tone {
                tone = group[0].tone;
                queue!(buffer, SetForegroundColor(tone.color()))?;
            }
            let text: String = group.iter().map(|cell| cell.glyph).collect();
            queue!(buffer, Print(text))?;
        }
    }
    if !no_color {
        queue!(buffer, ResetColor)?;
    }
    let mut stdout = io::stdout().lock();
    stdout.write_all(&buffer)?;
    stdout.flush()
}

pub fn run(
    collector: &mut Collector,
    interval: Duration,
    ascii: bool,
    no_color: bool,
    compact: bool,
    stopped: &AtomicBool,
    mut record: impl FnMut(&Value) -> io::Result<()>,
) -> io::Result<()> {
    let _session = TerminalSession::start(no_color)?;
    let mut state = State::new(interval, ascii, compact);
    let mut snapshot = Value::Null;
    let mut deadline = Instant::now();
    let mut dimensions = terminal::size()?;
    let mut dirty = true;
    while !stopped.load(Ordering::Relaxed) {
        if !state.paused && Instant::now() >= deadline {
            snapshot = collector.sample();
            if state.processes {
                snapshot["processes"] = Value::Array(collector.processes());
            }
            record(&snapshot)?;
            state.record(&snapshot);
            deadline = Instant::now() + state.interval;
            dirty = true;
        }
        if dirty {
            let frame = render(
                &snapshot,
                &mut state,
                dimensions.0.into(),
                dimensions.1.into(),
                true,
            );
            draw(&frame, no_color)?;
            dirty = false;
        }
        let timeout = if state.paused {
            SIGNAL_CHECK_INTERVAL
        } else {
            deadline
                .saturating_duration_since(Instant::now())
                .min(SIGNAL_CHECK_INTERVAL)
        };
        let event = match event::poll(timeout) {
            Ok(true) => event::read(),
            Ok(false) => continue,
            Err(error) => Err(error),
        };
        match event {
            Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                match state.key(key.code, key.modifiers) {
                    Action::Quit => break,
                    Action::Resume => {
                        collector.reset_baselines();
                        deadline = Instant::now();
                    }
                    Action::Refresh => deadline = Instant::now() + state.interval,
                    Action::Ignore => continue,
                    Action::Redraw => {}
                }
                dirty = true;
            }
            Ok(Event::Resize(width, height)) => {
                dimensions = (width, height);
                dirty = true;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
            _ => {}
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "hostname": "test-host",
            "monotonic": 99.0,
            "cpu": {"model": "Fixture CPU", "used_percent": 25, "cores": [{"id": 0, "used_percent": 25}]},
            "gpus": [{"id": "gpu0", "card": "card1", "name": "Fixture GPU", "used_percent": 75}],
            "memory": {"used_bytes": 1024, "total_bytes": 4096, "used_percent": 25},
            "sensors": [],
            "voltages": [
                {"label": "vddgfx", "chip": "amdgpu", "device": "hwmon3", "voltage_v": 0.875, "alarm": true},
                {"label": "vddnb", "chip": "amdgpu", "device": "hwmon3", "voltage_v": null, "alarm": false},
            ],
            "warnings": []
        })
    }

    #[test]
    fn plain_layout_shows_all_overview_panels_and_voltage_details() {
        let sample = sample();
        for width in [38, 80] {
            let output = render_once(&sample, width, true, false);
            for label in ["MainSense", "CPU", "GPU", "RAM", "VRAM"] {
                assert!(output.contains(label), "missing {label} at width {width}");
            }
            assert!(output.is_ascii());
            assert!(output.lines().all(|line| line.len() <= width as usize));
            assert!(output
                .lines()
                .any(|line| line.contains("vddgfx") && line.contains("0.875 V !")));
            assert!(output
                .lines()
                .any(|line| line.contains("vddnb") && line.contains("--")));
            let compact = render_once(&sample, width, true, true);
            assert!(!compact.contains("vddgfx"));
            assert!(!compact.contains("CPU THREADS"));
        }
    }

    #[test]
    fn render_sanitizes_terminal_controls_from_external_text() {
        let mut sample = sample();
        sample["voltages"][0]["label"] = json!("vdd\x1b[31m\n\t中");
        sample["processes"] = json!([{"pid": 42, "name": "worker\x1b[31m\n\t中"}]);
        let mut state = State::new(Duration::from_secs(1), false, false);
        state.processes = true;
        let output = render(&sample, &mut state, 80, 24, false)
            .iter()
            .map(|row| row.iter().map(|cell| cell.glyph).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!output.contains(['\x1b', '\t', '中']));
        assert!(output.contains("vdd?[31m???"));
        assert!(output.contains("worker?[31m???"));
    }

    #[test]
    fn viewport_can_reach_last_sensor_and_retains_controls() {
        let mut sample = sample();
        sample["sensors"] = json!((0..60)
            .map(|index| json!({"label": format!("Sensor {index}"), "temperature_c": 42.5}))
            .collect::<Vec<_>>());
        let mut state = State::new(Duration::from_secs(1), true, false);
        state.key(KeyCode::End, KeyModifiers::NONE);
        for (width, height) in [(1, 1), (42, 18), (80, 24), (120, 40)] {
            state.key(KeyCode::End, KeyModifiers::NONE);
            let frame = render(&sample, &mut state, width, height, true);
            assert_eq!(frame.len(), height);
            assert!(frame.iter().all(|row| row.len() == width));
            if width > 1 {
                let row = |index: usize| {
                    frame[index]
                        .iter()
                        .map(|cell| cell.glyph)
                        .collect::<String>()
                };
                assert!(row(height - 1).contains("q quit"));
                assert!(row(height - 2).contains("? help"));
                assert!(frame
                    .iter()
                    .enumerate()
                    .any(|(index, _)| row(index).contains("Sensor 59")));
            }
        }
    }

    #[test]
    fn key_controls_cover_pause_refresh_and_detail_toggles() {
        let mut state = State::new(Duration::from_secs(1), true, false);
        assert_eq!(
            state.key(KeyCode::Char(' '), KeyModifiers::NONE),
            Action::Redraw
        );
        assert!(state.paused);
        assert_eq!(
            state.key(KeyCode::Char(' '), KeyModifiers::NONE),
            Action::Resume
        );
        assert!(!state.paused);
        for _ in 0..80 {
            state.key(KeyCode::Char('+'), KeyModifiers::NONE);
        }
        assert_eq!(state.interval, Duration::from_millis(200));
        for _ in 0..80 {
            state.key(KeyCode::Char('-'), KeyModifiers::NONE);
        }
        assert_eq!(state.interval, Duration::from_secs(5));
        state.key(KeyCode::Char('p'), KeyModifiers::NONE);
        assert!(state.processes);
        state.key(KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(state.help);
        state.key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!state.help);
        assert_eq!(
            state.key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Action::Quit
        );
    }

    #[test]
    fn history_discards_removed_devices_and_old_samples() {
        let mut state = State::new(Duration::from_millis(200), false, false);
        let mut sample = sample();
        state.record(&sample);
        sample["gpus"] = json!([]);
        for i in 500..2500 {
            sample["monotonic"] = json!(i as f64 / 5.0);
            state.record(&sample);
        }
        assert_eq!(state.history.len(), 1);
        let history = &state.history["cpu"];
        assert!(history.len() <= 302);
        assert!(history.front().unwrap().0 >= 439.8);
        assert_eq!(history.back().unwrap().1, Some(25.0));
    }
}
