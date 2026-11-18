import fcntl
import json
import os
from pathlib import Path
import resource
import select
import signal
import subprocess
import sys
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]


class CLITests(unittest.TestCase):
    def call(self, *args):
        return subprocess.run([sys.executable, "-m", "mainsense", *args], cwd=ROOT,
                              text=True, capture_output=True, timeout=10)

    def test_json_snapshot_has_real_numbers_and_explicit_units(self):
        result = self.call("--json", "--once", "-i", "0.2")
        self.assertEqual(result.returncode, 0, result.stderr)
        sample = json.loads(result.stdout)
        self.assertIsInstance(sample["cpu"]["used_percent"], (int, float))
        self.assertGreater(sample["memory"]["total_bytes"], 0)
        self.assertIn("gpus", sample)
        self.assertEqual(sample.get("kernel_release"), os.uname().release)

    def test_count_limits_stream(self):
        result = self.call("--json", "--count", "2", "-i", "0.2")
        self.assertEqual(result.returncode, 0, result.stderr)
        samples = [json.loads(line) for line in result.stdout.splitlines()]
        self.assertEqual(len(samples), 2)
        self.assertLess(samples[0]["monotonic"], samples[1]["monotonic"])

    def test_log_appends_without_recording_the_priming_sample(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "rig.jsonl"
            path.write_text('{"previous": true}')
            result = self.call("--log-file", path, "--json", "--count", "2", "-i", "0.2")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stderr, "")
            written = [json.loads(line) for line in path.read_text().splitlines()]
        self.assertEqual(written[0], {"previous": True})
        self.assertEqual(len(written), 3)
        self.assertEqual(written[1:], [json.loads(line) for line in result.stdout.splitlines()])

    def test_log_open_failure_is_reported_before_sampling(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "missing" / "rig.jsonl"
            result = self.call("--log-file", path, "--once", "-i", "0.2")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("could not open log file", result.stderr)
        self.assertIn(str(path), result.stderr)
        self.assertNotIn("Traceback", result.stderr)

    def test_short_write_preserves_existing_log_and_allows_restart(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "rig.jsonl"
            previous = b'{"previous": true}\n'
            path.write_bytes(previous)
            limit = len(previous) + 300
            result = subprocess.run(
                [sys.executable, "-m", "mainsense", "--log-file", path, "--json", "--once", "-i", "0.2"],
                cwd=ROOT, capture_output=True, timeout=10,
                preexec_fn=lambda: resource.setrlimit(resource.RLIMIT_FSIZE, (limit, limit)),
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(path.read_bytes(), previous)
            resumed = self.call("--log-file", path, "--json", "--once", "-i", "0.2")
            self.assertEqual(resumed.returncode, 0, resumed.stderr)
            self.assertEqual(path.read_bytes(), previous + resumed.stdout.encode())

    def test_log_rejects_stdout_alias(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "rig.jsonl"
            alias = Path(folder) / "stdout"
            previous = b'{"previous": true}\n'
            path.write_bytes(previous)
            alias.hardlink_to(path)
            with alias.open("r+b") as stdout:
                result = subprocess.run(
                    [sys.executable, "-m", "mainsense", "--log-file", path, "--once", "-i", "0.2"],
                    cwd=ROOT, stdout=stdout, stderr=subprocess.PIPE, timeout=10,
                )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(b"stdout", result.stderr)
            self.assertEqual(path.read_bytes(), previous)

    def test_rename_rotation_reopens_log_path(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "rig.jsonl"
            rotated = path.with_suffix(".old")
            with subprocess.Popen(
                [sys.executable, "-m", "mainsense", "--log-file", path, "--json", "--count", "6", "-i", "0.2"],
                cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            ) as process:
                try:
                    self.assertTrue(select.select([process.stdout], [], [], 5)[0])
                    deadline = time.monotonic() + 3
                    while not path.stat().st_size and time.monotonic() < deadline:
                        time.sleep(0.01)
                    self.assertGreater(path.stat().st_size, 0)
                    with path.open("rb") as holder:
                        fcntl.flock(holder, fcntl.LOCK_EX)
                        time.sleep(0.25)  # The next snapshot can reach the old lock before rotation.
                        path.rename(rotated)
                        path.touch()
                        stdout, stderr = process.communicate(timeout=8)
                finally:
                    if process.poll() is None:
                        process.kill()
                self.assertEqual(process.returncode, 0, stderr)
            self.assertTrue(path.read_bytes())
            self.assertEqual(sorted((rotated.read_bytes() + path.read_bytes()).splitlines()), sorted(stdout.splitlines()))

    def test_sampling_continues_while_the_log_is_locked(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "rig.jsonl"
            with path.open("wb") as holder:
                fcntl.flock(holder, fcntl.LOCK_EX)
                with subprocess.Popen(
                    [sys.executable, "-m", "mainsense", "--log-file", path, "--json", "--count", "3", "-i", "0.2"],
                    cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0,
                ) as process:
                    try:
                        first_lines = []
                        for _ in range(2):
                            self.assertTrue(select.select([process.stdout], [], [], 3)[0])
                            first_lines.append(process.stdout.readline())
                        self.assertEqual(path.stat().st_size, 0)
                        fcntl.flock(holder, fcntl.LOCK_UN)
                        stdout, stderr = process.communicate(timeout=5)
                    finally:
                        if process.poll() is None:
                            process.kill()
                    self.assertEqual(process.returncode, 0, stderr)
            self.assertEqual(path.read_bytes(), b"".join(first_lines) + stdout)

    def test_sigterm_drains_queued_records(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "rig.jsonl"
            with path.open("wb") as holder:
                fcntl.flock(holder, fcntl.LOCK_EX)
                with subprocess.Popen(
                    [sys.executable, "-m", "mainsense", "--log-file", path, "--json", "-i", "0.2"],
                    cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                ) as process:
                    try:
                        self.assertTrue(select.select([process.stdout], [], [], 5)[0])
                        process.send_signal(signal.SIGTERM)
                        fcntl.flock(holder, fcntl.LOCK_UN)
                        stdout, stderr = process.communicate(timeout=5)
                    finally:
                        if process.poll() is None:
                            process.kill()
                    self.assertEqual(process.returncode, 0, stderr)
            self.assertTrue(stdout)
            self.assertEqual(path.read_bytes(), stdout)

    def test_invalid_intervals_fail_before_sampling(self):
        for interval in ("nan", "inf", "-1", "0", "100", "bad"):
            with self.subTest(interval=interval):
                result = self.call("--interval", interval)
                self.assertEqual(result.returncode, 2)
                self.assertIn("interval must", result.stderr)

    def test_noninteractive_ascii_snapshot_has_no_escape_codes(self):
        result = self.call("--ascii", "--once", "-i", "0.2")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("\x1b", result.stdout)
        result.stdout.encode("ascii")
        for label in ("CPU", "GPU", "RAM", "VRAM"):
            self.assertIn(label, result.stdout)
