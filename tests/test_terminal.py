"""Exercise curses, actual terminal input and resize signals in a real PTY."""

import fcntl
import json
import os
from pathlib import Path
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unittest


class TerminalTests(unittest.TestCase):
    def test_keyboard_resize_and_terminal_restoration(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        log_path = Path(folder.name) / "mainsense.jsonl"
        master, slave = os.openpty()
        process = None

        def resize(width, height):
            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
            if process is not None:
                os.kill(process.pid, signal.SIGWINCH)

        def receive(seconds=0.2):
            data = b""
            deadline = time.monotonic()+seconds
            while time.monotonic() < deadline:
                if select.select([master], [], [], max(0, deadline-time.monotonic()))[0]:
                    try:
                        chunk = os.read(master, 65536)
                    except OSError:
                        break
                    if not chunk:
                        break
                    data += chunk
            return data

        try:
            resize(80, 24)
            env = dict(os.environ, TERM="xterm-256color")
            env.pop("NO_COLOR", None)
            process = subprocess.Popen([sys.executable, "-m", "mainsense", "-i", "0.2", "--log-file", log_path],
                                       cwd=Path(__file__).resolve().parents[1], env=env,
                                       stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
            os.close(slave)
            slave = None
            captured = receive(0.5)
            self.assertIn(b"VRAM", captured)
            self.assertIn(b"q quit", captured)
            self.assertIn(b"\x1b[36m", captured)  # Color-capable path actually used.
            with log_path.open("rb") as held_log:
                fcntl.flock(held_log, fcntl.LOCK_EX)
                receive(0.3)
                os.write(master, b" ")
                paused = receive()
                self.assertIn(b"PAUSED", paused)
            receive(0.3)  # Allow samples queued before the pause to finish writing.
            paused_log = log_path.read_text()
            self.assertTrue(paused_log)
            captured += paused
            os.write(master, b"?")
            help_screen = receive()
            self.assertIn(b"KEYBOARD", help_screen)
            captured += help_screen
            self.assertEqual(log_path.read_text(), paused_log)
            os.write(master, b"? ")
            for width, height in [(120, 40), (42, 18), (1, 1), (80, 24)]:
                resize(width, height)
                captured += receive()
                self.assertIsNone(process.poll(), captured[-2000:])
            os.write(master, b"ctpgg+--jjjjjjjjjjjjjjjj")
            captured += receive(0.3)
            os.write(master, b"q")
            captured += receive(0.5)
            self.assertEqual(process.wait(timeout=2), 0, captured[-2000:])
            self.assertNotIn(b"Traceback", captured)
            self.assertIn(b"\x1b[?1049l", captured)  # Original screen restored.
            samples = [json.loads(line) for line in log_path.read_text().splitlines()]
            self.assertGreater(len(samples), len(paused_log.splitlines()))
            self.assertIn("voltages", samples[-1])
        finally:
            if process is not None and process.poll() is None:
                process.kill()
                process.wait()
            os.close(master)
            if slave is not None:
                os.close(slave)
