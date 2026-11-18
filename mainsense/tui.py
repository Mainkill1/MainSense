"""Responsive terminal presentation, independent of sensor collection."""

from dataclasses import dataclass, field
from collections import deque
import curses
import math
import time
import unicodedata


PROCESS_LIMIT = 30


@dataclass
class State:
    interval: float = 1.0
    paused: bool = False
    cores: bool = True
    sensors: bool = True
    processes: bool = False
    help: bool = False
    gpu: int = 0
    scroll: int = 0
    page_rows: int = 12
    ascii: bool = False
    history: dict = field(default_factory=dict)

    def record(self, snapshot):
        now = snapshot["monotonic"]
        values = {"cpu": snapshot["cpu"]["used_percent"]}
        values.update({g["id"]: g["used_percent"] for g in snapshot["gpus"]})
        active = set(values)
        for key in list(self.history):
            if key not in active:
                del self.history[key]
        for key, value in values.items():
            history = self.history.setdefault(key, deque(maxlen=600))
            history.append((now, value))
            while history and history[0][0] < now - 120:
                history.popleft()


@dataclass
class Row:
    text: str
    styles: list


def clean(text):
    # Every emitted glyph occupies exactly one terminal cell. Sensor/process
    # names cannot inject controls or disturb the panel geometry.
    return "".join(c if c.isprintable() and not unicodedata.combining(c)
                   and unicodedata.east_asian_width(c) not in ("W", "F") else "?" for c in str(text))


class Canvas:
    def __init__(self, width, height=0):
        self.width = width
        self.chars = [[" "] * width for _ in range(height)]
        self.styles = [[0] * width for _ in range(height)]

    def put(self, y, x, text, style=0, limit=None):
        if y < 0 or x >= self.width:
            return
        text = clean(text)
        if limit is not None:
            text = text[:max(0, limit)]
        if x < 0:
            text, x = text[-x:], 0
        while len(self.chars) <= y:
            self.chars.append([" "] * self.width)
            self.styles.append([0] * self.width)
        end = min(self.width, x + len(text))
        self.chars[y][x:end] = list(text[:end-x])
        self.styles[y][x:end] = [style] * (end-x)

    def rows(self):
        return [Row("".join(chars), styles) for chars, styles in zip(self.chars, self.styles)]


def number(value, unit="", precision=1):
    return "--" if value is None else f"{value:.{precision}f}{unit}"


def size(value):
    if value is None:
        return "--"
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if abs(value) < 1024 or unit == "TiB":
            return f"{value:.1f} {unit}"
        value /= 1024


def gauge(value, width, ascii=False):
    if value is None:
        return "-" * width
    eighths = int(max(0, min(100, value)) / 100 * width * 8)
    whole, fraction = divmod(eighths, 8)
    if ascii:
        return ("#" * whole + ("+" if fraction else "")).ljust(width, ".")[:width]
    return ("█" * whole + ("▏▎▍▌▋▊▉"[fraction-1] if fraction else "")).ljust(width, "░")[:width]


def spark(history, width, now, ascii=False):
    buckets = [[] for _ in range(width)]
    for timestamp, value in history:
        index = min(width-1, int((timestamp - (now - 60)) / 60 * width))
        if timestamp >= now - 60 and index >= 0 and value is not None:
            buckets[index].append(value)
    levels = "._:-=+*#@" if ascii else "▁▂▃▄▅▆▇█"
    return "".join(levels[min(7, max(0, round(sum(b) / len(b) * 7 / 100)))] if b else " " for b in buckets)


class Panel:
    def __init__(self, canvas, x, y, width, height, title, style, ascii=False):
        self.canvas, self.x, self.y, self.width = canvas, x, y, width
        self.style, self.ascii = style, ascii
        tl, tr, bl, br, h, v = ("+", "+", "+", "+", "-", "|") if ascii else ("╭", "╮", "╰", "╯", "─", "│")
        canvas.put(y, x, tl + h * (width-2) + tr, 5)
        for row in range(1, height-1):
            canvas.put(y+row, x, v, 5)
            canvas.put(y+row, x+width-1, v, 5)
        canvas.put(y+height-1, x, bl + h * (width-2) + br, 5)
        canvas.put(y, x+2, " " + title + " ", style, width-4)

    def line(self, row, text, style=None):
        self.canvas.put(self.y+row, self.x+2, text, self.style if style is None else style, self.width-4)

    def meter(self, row, label, value):
        bar_width = max(1, self.width-18)
        self.line(row, f"{label:<5} {gauge(value, bar_width, self.ascii)} {number(value, '%'):>6}")


HELP = [
    "KEYBOARD", "q / Ctrl+C      Quit", "Space           Pause / resume the display",
    "+ / -           Faster / slower refresh (0.2 to 5 seconds)",
    "c               Show / hide CPU thread meters", "t               Show / hide temperature and voltage sensors",
    "p               Show / hide CPU process table", "g               Select next AMD GPU",
    "Up/Down, j/k    Scroll", "PgUp/PgDn       Scroll one page", "Home / End      First / last panel",
    "? / h           Help", "Esc             Close help", "",
    "READING THE DISPLAY", "CPU uses the interval between two /proc/stat samples.",
    "RAM used = MemTotal - MemAvailable; cache is reclaimable.",
    "VRAM is the driver-reported GPU memory pool, including APU reservations.",
    "GTT is system-backed GPU memory and overlaps system RAM.",
    "Mem activity is controller busy time, not allocated VRAM or bandwidth.",
    "SoC power includes CPU power on AMD APUs.",
    "Tctl is a CPU control reading; Tdie is preferred if exposed.",
    "-- means missing, unreadable, sleeping or not yet sampled.",
    "Graphs show the last 60 seconds on a fixed 0-100% scale.",
    "Paused samples are frozen; gaps in history remain empty.",
    "Pausing stops collection. CPU counters re-prime on resume.",
    "NVMe temperatures refresh every 2 seconds; AGE shows time since reading.",
    "UCLK avg is a firmware-filtered memory-controller clock, not data rate.",
    "Process CPU uses 100% per logical CPU and can exceed 100%.",
    "Only processes visible and readable in this /proc namespace appear.",
    "Run with --ascii or --no-color for simpler terminals.",
]


def render(snapshot, state, width, height):
    width, height = max(1, width), max(1, height)
    frame = Canvas(width, height)
    if width < 38 or height < 10:
        frame.put(0, 0, "MainSense", 1)
        if height > 1:
            frame.put(1, 0, "Resize to at least 38 x 10", 4)
        if height > 2:
            frame.put(2, 0, "q quit | ? help", 5)
        return frame.rows()
    body = Canvas(width)
    cpu, memory = snapshot["cpu"], snapshot["memory"]
    gpus = snapshot["gpus"]
    gpu = gpus[state.gpu % len(gpus)] if gpus else {}
    now = snapshot["monotonic"]
    if state.help:
        y = 0
        import textwrap
        for line in HELP:
            for part in textwrap.wrap(line, width-4) or [""]:
                body.put(y, 2, part, 1 if line.isupper() else 0)
                y += 1
    else:
        wide = width >= 80
        panel_width = (width-1)//2 if wide else width
        right = panel_width+1 if wide else 0
        p = Panel(body, 0, 0, panel_width, 9, f"CPU / {len(cpu['cores'])} threads", 1, state.ascii)
        p.meter(1, "BUSY", cpu["used_percent"])
        p.line(2, spark(state.history.get("cpu", []), panel_width-4, now, state.ascii))
        p.line(3, "60s history                 0-100%", 5)
        p.line(4, f"Temp  {number(cpu['temperature_c'], ' C')}  {cpu['temperature_label']}")
        p.line(5, f"Clock {number(cpu['frequency_mhz'], ' MHz', 0)} avg")
        p.line(6, "Load  " + "  ".join(snapshot["load_average"]), 0)
        p.line(7, "Aggregate CPU across all threads", 5)
        p = Panel(body, right, 0 if wide else 9, width-right if wide else width, 9,
                  f"GPU / {gpu.get('card', '--')}", 2, state.ascii)
        if gpu:
            p.meter(1, "BUSY", gpu["used_percent"])
            p.line(2, spark(state.history.get(gpu["id"], []), p.width-4, now, state.ascii))
            p.line(3, "60s history                 0-100%", 5)
            p.line(4, f"Temp  {number(gpu['temperature_c'], ' C')}  {gpu['temperature_label']}")
            p.line(5, f"Clock {number(gpu['clock_mhz'], ' MHz', 0)}  SoC {number(gpu['power_w'], ' W')}")
            p.line(6, f"Fan {number(gpu['fan_rpm'], ' RPM', 0)}  Mem activity {number(gpu['memory_busy_percent'], '%', 0)}", 0)
            p.line(7, f"{gpu['pci_address']}  {gpu['status']}", 5)
        else:
            p.line(2, "No AMD GPU visible", 4)
            p.line(4, "GPU / VRAM / temperature: --", 5)
            p.line(6, "CPU and RAM monitoring continues", 5)
        y = 9 if wide else 18
        p = Panel(body, 0, y, panel_width, 7, "RAM / Linux system memory", 3, state.ascii)
        p.line(1, f"{size(memory['used_bytes'])} / {size(memory['total_bytes'])}")
        p.meter(2, "USED", memory["used_percent"])
        p.line(3, f"Available  {size(memory['available_bytes'])}", 0)
        p.line(4, f"Cache      {size(memory['cache_bytes'])}", 5)
        p.line(5, f"Swap  {size(memory['swap_used_bytes'])} / {size(memory['swap_total_bytes'])}", 0)
        p = Panel(body, right, y if wide else y+7, width-right if wide else width, 7,
                  "VRAM / GPU memory pool", 2, state.ascii)
        p.line(1, f"{size(gpu.get('vram_used_bytes'))} / {size(gpu.get('vram_total_bytes'))}")
        p.meter(2, "USED", gpu.get("vram_used_percent"))
        p.line(3, f"GTT {size(gpu.get('gtt_used_bytes'))} / {size(gpu.get('gtt_total_bytes'))}", 0)
        clock_label = "UCLK avg" if "average UCLK" in (gpu.get("memory_clock_source") or "") else "Mem clock"
        p.line(4, f"{clock_label}  {number(gpu.get('memory_clock_mhz'), ' MHz', 0)}", 5)
        p.line(5, "GTT overlaps RAM; do not add.", 5)
        y += 7 if wide else 14
        body.put(y, 1, "GPU: " + gpu.get("name", "unavailable"), 5)
        y += 1
        if state.cores:
            columns = max(1, width//20)
            rows = max(1, math.ceil(len(cpu["cores"])/columns))
            p = Panel(body, 0, y, width, rows+2, "CPU THREADS / c to collapse", 1, state.ascii)
            cell_width = (width-4)//columns
            for index, core in enumerate(cpu["cores"]):
                row, col = divmod(index, columns)
                text = f"{core['id']:>3} {gauge(core['used_percent'], max(1, cell_width-11), state.ascii)} {number(core['used_percent'], '%', 0):>4}"
                body.put(y+1+row, 2+col*cell_width, text, 1, cell_width-1)
            y += rows+2
        if state.sensors:
            sensors = snapshot["sensors"]
            p = Panel(body, 0, y, width, max(1, len(sensors))+3, "TEMPERATURES / t to collapse", 4, state.ascii)
            label_width = max(8, min(40, width-24))
            p.line(1, f"{'SENSOR / DEVICE':<{label_width}} {'CURRENT':>8} {'CRITICAL':>9}" + ("    AGE" if width >= 72 else ""), 5)
            for i, sensor in enumerate(sensors):
                label = f"{sensor['label']} / {sensor['chip']}:{sensor['device']}"
                temp = number(sensor["temperature_c"], " C")
                critical = number(sensor["critical_c"], " C")
                alert = sensor["alarm"] or (sensor["critical_c"] is not None and sensor["temperature_c"] is not None and sensor["temperature_c"] >= sensor["critical_c"])
                age = f" {sensor.get('sample_age_seconds', 0):>5.1f}s" if width >= 72 else ""
                p.line(i+2, f"{clean(label)[:label_width]:<{label_width}} {temp:>8} {critical:>9}" + age + (" !" if alert else ""), 6 if alert else 0)
            if not sensors:
                p.line(2, "No readable hwmon temperature sensors", 5)
            y += max(1, len(sensors))+3
            voltages = snapshot.get("voltages", [])
            if voltages:
                p = Panel(body, 0, y, width, len(voltages)+3, "VOLTAGES / t to collapse", 4, state.ascii)
                label_width = min(40, width-16)
                p.line(1, f"{'SENSOR / DEVICE':<{label_width}} {'CURRENT':>9}", 5)
                for i, sensor in enumerate(voltages):
                    label = clean(f"{sensor['label']} / {sensor['chip']}:{sensor['device']}")[:label_width]
                    voltage = number(sensor["voltage_v"], " V", 3)
                    alarm = sensor["alarm"]
                    p.line(i+2, f"{label:<{label_width}} {voltage:>9}" + (" !" if alarm else ""), 6 if alarm else 0)
                y += len(voltages)+3
        if state.processes:
            entries = snapshot.get("processes", [])[:PROCESS_LIMIT]
            p = Panel(body, 0, y, width, max(1, len(entries))+3, "PROCESSES / CPU order / p to collapse", 3, state.ascii)
            p.line(1, "    PID   CPU%       RAM  COMMAND", 5)
            for i, proc in enumerate(entries):
                p.line(i+2, f"{proc['pid']:>7} {number(proc['cpu_percent'], '', 1):>6} {size(proc['rss_bytes']):>9}  {proc['name']}", 0)
            if not entries:
                p.line(2, "Waiting for readable processes...", 5)
            y += max(1, len(entries))+3
        for warning in snapshot["warnings"]:
            body.put(y, 1, warning, 4)
            y += 1
    viewport = height-5
    state.page_rows = viewport
    state.scroll = max(0, min(state.scroll, len(body.chars)-viewport))
    rows = body.rows()
    for index, row in enumerate(rows[state.scroll:state.scroll+viewport]):
        frame.chars[index+3] = list(row.text)
        frame.styles[index+3] = row.styles
    title = "MainSense"
    status = ("PAUSED" if state.paused else "LIVE") + f"  {state.interval:g}s"
    frame.put(0, 1, title, 1)
    frame.put(0, width-len(status)-1, status, 4 if state.paused else 1)
    frame.put(1, 1, f"{snapshot['hostname']} / {cpu['model']}", 5, width-2)
    frame.put(2, 1, f"c cores {'on' if state.cores else 'off'}  t sensors {'on' if state.sensors else 'off'}  p processes {'on' if state.processes else 'off'}", 5)
    position = f"{state.scroll+1}-{min(state.scroll+viewport, len(rows))}/{len(rows)}"
    frame.put(height-2, 1, f"? help  {position}  Up/Down scroll  g GPU", 5)
    frame.put(height-1, 1, "q quit  Space pause  +/- refresh  c cores  t sensors  p procs", 0)
    return frame.rows()


def handle_key(state, key):
    if key in (ord("q"), ord("Q")):
        return False
    if key == 27:
        state.help, state.scroll = False, 0
    elif key in (ord("?"), ord("h")):
        state.help, state.scroll = not state.help, 0
    elif key == ord(" "):
        state.paused = not state.paused
    elif key in (ord("+"), ord("=")):
        state.interval = max(0.2, round(state.interval/1.5, 2))
    elif key in (ord("-"), ord("_")):
        state.interval = min(5.0, round(state.interval*1.5, 2))
    elif key == ord("g"):
        state.gpu += 1
    elif key in (ord("c"), ord("t"), ord("p")):
        name = {ord("c"): "cores", ord("t"): "sensors", ord("p"): "processes"}[key]
        setattr(state, name, not getattr(state, name))
        state.scroll = 0
    elif key in (curses.KEY_DOWN, ord("j")):
        state.scroll += 1
    elif key in (curses.KEY_UP, ord("k")):
        state.scroll = max(0, state.scroll-1)
    elif key == curses.KEY_NPAGE:
        state.scroll += state.page_rows
    elif key == curses.KEY_PPAGE:
        state.scroll = max(0, state.scroll-state.page_rows)
    elif key == curses.KEY_HOME:
        state.scroll = 0
    elif key == curses.KEY_END:
        state.scroll = 1000000
    return True


def run(screen, collector, state, no_color=False, record_sample=None):
    try:
        curses.curs_set(0)
    except curses.error:
        pass
    screen.keypad(True)
    attributes = {0: curses.A_NORMAL, 5: curses.A_DIM}
    if not no_color and curses.has_colors():
        curses.start_color()
        try:
            curses.use_default_colors()
            background = -1
        except curses.error:
            background = curses.COLOR_BLACK
        for pair, color in enumerate((curses.COLOR_CYAN, curses.COLOR_MAGENTA, curses.COLOR_GREEN,
                                      curses.COLOR_YELLOW, curses.COLOR_WHITE, curses.COLOR_RED), 1):
            curses.init_pair(pair, color, background)
            attributes[pair] = curses.color_pair(pair) | (curses.A_DIM if pair == 5 else curses.A_NORMAL)
    snapshot = collector.sample()
    if record_sample is not None:
        record_sample(snapshot)
    state.record(snapshot)
    deadline = time.monotonic()+state.interval
    dirty = True
    previous_size = None
    while True:
        if not state.paused and time.monotonic() >= deadline:
            snapshot = collector.sample()
            if state.processes:
                snapshot["processes"] = collector.processes()[:PROCESS_LIMIT]
            if record_sample is not None:
                record_sample(snapshot)
            state.record(snapshot)
            dirty = True
            deadline = time.monotonic()+state.interval
        dimensions = screen.getmaxyx()
        if dirty or dimensions != previous_size:
            height, width = dimensions
            screen.erase()
            for y, row in enumerate(render(snapshot, state, width, height)):
                start = 0
                while start < width:
                    style = row.styles[start]
                    end = start+1
                    while end < width and row.styles[end] == style:
                        end += 1
                    try:
                        screen.addstr(y, start, row.text[start:end], attributes.get(style, curses.A_BOLD))
                    except curses.error:
                        # Resize races and the bottom-right cell are normal.
                        pass
                    start = end
            screen.noutrefresh()
            curses.doupdate()
            dirty, previous_size = False, dimensions
        # Input and resize signals interrupt getch immediately. Avoid waking
        # ten times per second when only one update (or none) is needed.
        screen.timeout(1000 if state.paused else max(1, min(1000, math.ceil((deadline-time.monotonic())*1000))))
        key = screen.getch()
        if key != -1:
            before, was_paused = state.interval, state.paused
            if not handle_key(state, key):
                break
            if was_paused and not state.paused:
                collector.reset_baselines()
                deadline = time.monotonic()
            elif before != state.interval:
                deadline = time.monotonic()+state.interval
            dirty = True
