#!/usr/bin/env python3
"""Measure the real curses process in a PTY; discard output instead of retaining it.

CPU percentages use one logical CPU = 100%. This does not stress the machine.
"""

import argparse
import fcntl
import json
import os
from pathlib import Path
import select
import struct
import subprocess
import sys
import termios
import time


def usage(pid):
    text = Path(f"/proc/{pid}/stat").read_text()
    fields = text[text.rfind(")")+1:].split()
    status = dict(line.split(":", 1) for line in Path(f"/proc/{pid}/status").read_text().splitlines())
    return ((int(fields[11])+int(fields[12])) / os.sysconf("SC_CLK_TCK"),
            int(status["VmRSS"].split()[0]) * 1024)


def benchmark(project, interval, seconds, keys=b""):
    master, slave = os.openpty()
    process = None
    output_bytes = 0

    def drain(duration):
        nonlocal output_bytes
        deadline = time.monotonic()+duration
        while time.monotonic() < deadline:
            if select.select([master], [], [], max(0, deadline-time.monotonic()))[0]:
                try:
                    data = os.read(master, 65536)
                except OSError:
                    break
                if not data:
                    break
                output_bytes += len(data)

    try:
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        env = dict(os.environ, TERM="xterm-256color")
        env.pop("NO_COLOR", None)
        process = subprocess.Popen([sys.executable, "-m", "mainsense", "-i", str(interval)],
                                   cwd=project, env=env, stdin=slave, stdout=slave, stderr=slave,
                                   start_new_session=True)
        os.close(slave)
        slave = None
        drain(1)
        if keys:
            os.write(master, keys)
        drain(2)
        if process.poll() is not None:
            raise RuntimeError(f"monitor exited early: {process.returncode}")
        cpu_start, rss_start = usage(process.pid)
        started = time.monotonic()
        bytes_start = output_bytes
        observations = []
        while time.monotonic()-started < seconds:
            drain(min(1, seconds-(time.monotonic()-started)))
            cpu, rss = usage(process.pid)
            observations.append({"seconds": round(time.monotonic()-started, 3), "rss_bytes": rss})
        elapsed = time.monotonic()-started
        cpu_end, rss_end = usage(process.pid)
        result = {"interval_seconds": interval, "process_panel": b"p" in keys, "paused": b" " in keys,
                  "elapsed_seconds": round(elapsed, 3), "cpu_one_core_percent": round((cpu_end-cpu_start)/elapsed*100, 3),
                  "rss_start_bytes": rss_start, "rss_end_bytes": rss_end,
                  "rss_peak_bytes": max([rss_start]+[o["rss_bytes"] for o in observations]),
                  "output_bytes_per_second": round((output_bytes-bytes_start)/elapsed), "rss_samples": observations}
        os.write(master, b"q")
        drain(0.3)
        result["exit_code"] = process.wait(timeout=5)
        return result
    finally:
        if process is not None and process.poll() is None:
            process.kill()
            process.wait()
        os.close(master)
        if slave is not None:
            os.close(slave)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--seconds", type=float, default=30)
    parser.add_argument("--soak-seconds", type=float, default=0)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.seconds <= 0 or args.soak_seconds < 0:
        parser.error("durations must be positive (soak can be zero)")
    cases = [(1.0, b"", args.seconds), (0.2, b"", args.seconds),
             (0.2, b"p", args.seconds), (0.2, b"p ", args.seconds)]
    if args.soak_seconds:
        cases.append((0.2, b"p", args.soak_seconds))
    results = []
    for interval, keys, seconds in cases:
        result = benchmark(args.project, interval, seconds, keys)
        results.append(result)
        args.output.write_text(json.dumps(results, indent=2)+"\n")
        print(json.dumps({k: v for k, v in result.items() if k != "rss_samples"}), flush=True)


if __name__ == "__main__":
    main()
