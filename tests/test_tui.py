import unittest
import curses
from unittest.mock import Mock, patch

from mainsense.metrics import Collector
from mainsense.tui import State, handle_key, render, run


class LayoutTests(unittest.TestCase):
    def setUp(self):
        # Empty roots exercise the actual unavailable-data contract.
        self.sample = Collector("/nonexistent/mainsense/proc", "/nonexistent/mainsense/sys").sample()

    def test_overview_keeps_all_four_resources_visible_at_80_by_24(self):
        lines = render(self.sample, State(), 80, 24)
        self.assertEqual(len(lines), 24)
        content = "\n".join(line.text for line in lines)
        for label in ("CPU", "GPU", "RAM", "VRAM", "--", "q quit"):
            self.assertIn(label, content)

    def test_all_sizes_are_clipped_and_scroll_can_reach_last_sensor(self):
        self.sample["sensors"] = [
            {"chip": "test", "device": "hwmon0", "label": f"Sensor {n}",
             "temperature_c": 42.5, "critical_c": None, "alarm": False}
            for n in range(60)]
        for width, height in [(1, 1), (20, 5), (42, 18), (80, 24), (120, 40), (240, 60)]:
            with self.subTest(width=width, height=height):
                state = State(scroll=10000, ascii=True)
                lines = render(self.sample, state, width, height)
                self.assertEqual(len(lines), height)
                self.assertTrue(all(len(line.text) == width for line in lines))
                if width >= 42 and height >= 18:
                    self.assertIn("Sensor 59", "\n".join(line.text for line in lines))

    def test_voltage_panel_displays_volts_and_respects_sensor_toggle(self):
        self.sample["voltages"] = [
            {"chip": "amdgpu", "device": "hwmon3", "label": "vddgfx", "voltage_v": 0.875, "alarm": False},
            {"chip": "amdgpu", "device": "hwmon3", "label": "vddnb", "voltage_v": None, "alarm": False},
        ]
        state = State(scroll=10000, ascii=True)
        lines = [row.text for row in render(self.sample, state, 38, 18)]
        self.assertTrue(any("vddgfx" in row and "0.875 V" in row for row in lines))
        self.assertTrue(any("vddnb" in row and "--" in row for row in lines))
        handle_key(state, ord("t"))
        state.scroll = 10000
        self.assertFalse(any("vddgfx" in row.text for row in render(self.sample, state, 38, 18)))

    def test_pause_and_detail_controls_do_not_change_sampling_interval(self):
        state = State()
        handle_key(state, ord(" "))
        self.assertTrue(state.paused)
        handle_key(state, ord("c"))
        self.assertFalse(state.cores)
        handle_key(state, ord("?"))
        self.assertTrue(state.help)
        handle_key(state, 27)
        self.assertFalse(state.help)
        self.assertEqual(state.interval, 1.0)
        self.assertFalse(handle_key(state, ord("q")))

    def test_interval_controls_stay_bounded(self):
        state = State()
        for _ in range(80):
            handle_key(state, ord("+"))
        self.assertEqual(state.interval, 0.2)
        for _ in range(80):
            handle_key(state, ord("-"))
        self.assertEqual(state.interval, 5.0)

    def test_sensor_labels_cannot_inject_terminal_controls(self):
        self.sample["cpu"]["model"] = "CPU\x1b[31m\tunsafe\nlabel"
        lines = render(self.sample, State(ascii=True), 80, 24)
        self.assertEqual(len(lines), 24)
        text = "".join(line.text for line in lines)
        self.assertNotIn("\x1b", text)
        self.assertNotIn("\t", text)
        self.assertNotIn("\n", text)

    def test_narrow_window_keeps_quit_and_help_discoverable(self):
        for width in (38, 42, 60, 80):
            with self.subTest(width=width):
                lines = render(self.sample, State(), width, 18)
                footer = "\n".join(row.text for row in lines[-2:])
                self.assertIn("q quit", footer)
                self.assertIn("? help", footer)

    def test_page_navigation_moves_by_the_current_viewport(self):
        for height in (14, 24, 40):
            with self.subTest(height=height):
                state = State()
                render(self.sample, state, 80, height)
                handle_key(state, curses.KEY_NPAGE)
                self.assertEqual(state.scroll, height-5)
                handle_key(state, curses.KEY_PPAGE)
                self.assertEqual(state.scroll, 0)

    def test_paused_display_stops_polling_hardware_and_processes(self):
        collector = Collector("/nonexistent/mainsense/proc", "/nonexistent/mainsense/sys")
        screen = Mock()
        screen.getmaxyx.return_value = (24, 80)
        now = [0]
        keys = iter([ord(" "), ord("q")])
        def get_key():
            now[0] += 10
            return next(keys)
        screen.getch.side_effect = get_key
        with patch("mainsense.tui.time.monotonic", side_effect=lambda: now[0]), \
             patch("mainsense.tui.curses.curs_set"), patch("mainsense.tui.curses.doupdate"), \
             patch.object(collector, "sample", wraps=collector.sample) as samples, \
             patch.object(collector, "processes", wraps=collector.processes) as processes:
            run(screen, collector, State(processes=True), no_color=True)
        self.assertEqual(samples.call_count, 1, "Only the initial sample should touch hardware")
        self.assertEqual(processes.call_count, 0)

    def test_history_is_bounded_and_removed_gpus_are_evicted(self):
        state = State()
        self.sample["gpus"] = [{"id": "gone", "used_percent": 10}]
        state.record(self.sample)
        self.sample["gpus"] = []
        for index in range(5000):
            self.sample["monotonic"] = index*0.2
            state.record(self.sample)
        self.assertNotIn("gone", state.history)
        self.assertLessEqual(len(state.history["cpu"]), 600)
        self.assertGreaterEqual(state.history["cpu"][0][0], self.sample["monotonic"]-120)

    def test_logged_process_sample_is_limited_to_displayed_entries(self):
        collector = Mock()
        collector.sample.return_value = self.sample
        collector.processes.return_value = [
            {"pid": pid, "name": "worker", "cpu_percent": 1, "rss_bytes": 4096} for pid in range(50)
        ]
        screen = Mock()
        screen.getmaxyx.return_value = (24, 80)
        screen.getch.side_effect = [-1, ord("q")]
        recorded = []
        with patch("mainsense.tui.time.monotonic", side_effect=range(10)), \
             patch("mainsense.tui.curses.curs_set"), patch("mainsense.tui.curses.doupdate"):
            run(screen, collector, State(processes=True), no_color=True,
                record_sample=lambda sample: recorded.append(sample.get("processes", [])))
        self.assertEqual([p["pid"] for p in recorded[-1]], list(range(30)))


if __name__ == "__main__":
    unittest.main()
