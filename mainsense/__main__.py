"""Command-line entry point."""

import argparse
from contextlib import nullcontext
import curses
import json
import locale
import math
import os
import shutil
import signal
import sys
import threading
import time

from . import __version__
from .metrics import Collector
from .snapshot_log import SnapshotLog, SnapshotLogError
from .tui import State, render, run


def interval(value):
    try:
        seconds = float(value)
    except ValueError:
        raise argparse.ArgumentTypeError("interval must be a number of seconds") from None
    if not math.isfinite(seconds) or not 0.2 <= seconds <= 5:
        raise argparse.ArgumentTypeError("interval must be between 0.2 and 5 seconds")
    return seconds


def stop_on_sigterm(signum, frame):
    # Unwind curses and let the log writer drain on service shutdown.
    raise KeyboardInterrupt


def main(argv=None):
    parser = argparse.ArgumentParser(
        prog="mainsense", description="A readable CPU / AMD GPU / RAM / VRAM / sensor monitor for Linux.",
        epilog="Keys: q quit, Space pause, +/- refresh, c cores, t sensors, p processes, g GPU, ? help.")
    parser.add_argument("--version", action="version", version=f"mainsense {__version__}")
    parser.add_argument("-i", "--interval", type=interval, default=1.0, metavar="SECONDS", help="refresh period, 0.2 to 5 seconds (default: 1)")
    parser.add_argument("--once", action="store_true", help="print one snapshot and exit")
    parser.add_argument("--json", action="store_true", help="stream one JSON object per sample; combine with --once for one")
    parser.add_argument("--count", type=int, default=0, metavar="N", help="stop JSON output after N samples (0: unlimited)")
    parser.add_argument("--log-file", metavar="PATH", help="append timestamped telemetry snapshots as JSON lines")
    parser.add_argument("--ascii", action="store_true", help="use ASCII borders and graphs")
    parser.add_argument("--no-color", action="store_true", help="disable terminal colors (also honors NO_COLOR)")
    parser.add_argument("--compact", action="store_true", help="start with core and sensor detail collapsed")
    args = parser.parse_args(argv)
    if args.count < 0 or (args.count and not args.json):
        parser.error("--count requires --json and a nonnegative number")
    if not sys.platform.startswith("linux"):
        parser.error("mainsense reads Linux /proc and /sys interfaces")
    locale.setlocale(locale.LC_ALL, "")
    log = SnapshotLog(args.log_file, sys.stdout) if args.log_file is not None else nullcontext()
    previous_sigterm = None
    if threading.current_thread() is threading.main_thread():
        previous_sigterm = signal.signal(signal.SIGTERM, stop_on_sigterm)
    started = False
    try:
        with log as stream:
            started = True
            monitor(args, stream.record if stream is not None else None)
    except KeyboardInterrupt:
        return 0
    except curses.error as error:
        print(f"mainsense: terminal setup failed: {error}. Try --once or --ascii.", file=sys.stderr)
        return 1
    except OSError as error:
        if not started:
            parser.error(f"could not open log file {args.log_file!r}: {error}")
        where = f" for log file {args.log_file!r}" if isinstance(error, SnapshotLogError) else ""
        print(f"mainsense: I/O error{where}: {error}", file=sys.stderr)
        return 1
    finally:
        if previous_sigterm is not None:
            signal.signal(signal.SIGTERM, previous_sigterm)
    return 0


def monitor(args, record_sample=None):
    ascii_mode = args.ascii or "utf" not in (sys.stdout.encoding or "").lower()
    collector = Collector()
    state = State(interval=args.interval, ascii=ascii_mode, cores=not args.compact, sensors=not args.compact)

    if args.json or args.once or not sys.stdin.isatty() or not sys.stdout.isatty() or os.environ.get("TERM") in (None, "dumb"):
        collector.sample()  # Prime interval counters; never show a lifetime CPU average.
        count = 1 if args.once or not args.json else args.count
        emitted = 0
        while True:
            time.sleep(args.interval)
            snapshot = collector.sample()
            if record_sample is not None:
                record_sample(snapshot)
            if args.json:
                output = json.dumps(snapshot, allow_nan=False)
            else:
                width = max(38, shutil.get_terminal_size((100, 40)).columns)
                height = ((24 if width >= 80 else 40) + math.ceil(len(snapshot["cpu"]["cores"])/max(1, width//20))
                          + len(snapshot["sensors"]) + len(snapshot["warnings"]) + 8)
                if snapshot["voltages"]:
                    height += len(snapshot["voltages"]) + 3
                state.record(snapshot)
                output = "\n".join(row.text.rstrip() for row in render(snapshot, state, width, height))
            try:
                print(output, flush=True)
            except BrokenPipeError:
                # Avoid a second BrokenPipeError during interpreter shutdown.
                sys.stdout = open(os.devnull, "w")
                return
            emitted += 1
            if count and emitted >= count:
                break
    else:
        curses.wrapper(run, collector, state, args.no_color or "NO_COLOR" in os.environ, record_sample)


if __name__ == "__main__":
    raise SystemExit(main())
