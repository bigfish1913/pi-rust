"""No native dependency required: ownership, safety, bounded output and coverage."""
from argparse import Namespace
from pathlib import Path
import queue
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

import resource_tui as tui


class FakePty:
    instances = []

    @classmethod
    def spawn(cls, argv, **kwargs):
        proc = cls()
        proc.argv, proc.kwargs = argv, kwargs
        cls.instances.append(proc)
        return proc

    def __init__(self):
        self.pid = 12001
        self.alive = True
        self.exitstatus = None
        self.closed = 0
        self.output = queue.Queue()
        self.output.put("\x1b[2JEnter send · Ctrl+C clear")
        self.writes = []
        self.pasting = False

    def read(self, size):
        value = self.output.get()
        if value is None:
            raise EOFError()
        return value

    def write(self, text):
        self.writes.append(text)
        if "\x1b[200~" in text:
            self.pasting = True
        if "\x1b[201~" in text:
            self.pasting = False
            self.output.put("\x1b[37;2H[paste #1 65536 chars]")
        elif not self.pasting:
            self.output.put("\x1b[37;2H" + text)
        return len(text.encode())

    def isalive(self):
        return self.alive

    def close(self, force=False):
        self.closed += 1
        self.alive = False
        self.exitstatus = 1  # Normal intentional Ctrl+C/termination status is not failure.
        self.output.put(None)


class Sampler:
    memory = 1024

    def __init__(self, pid, interval):
        self.pid, self.error, self.peak, self.count = pid, None, self.memory, 0
        self.closed = False
        self.latest = None

    def take(self):
        self.count += 1
        self.latest = dict(primary_metric="windows_private_commit", primary_bytes=self.memory,
            private_bytes=self.memory, rss_bytes=2048, peak_rss_bytes=2048,
            user_seconds=self.count * .001, kernel_seconds=0,
            wall_seconds=float(self.count))
        return self.latest

    def close(self):
        self.closed = True


class TuiTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / "workspace").mkdir()
        self.args = Namespace(rounds=2, idle_seconds=0, max_memory_mib=512,
                              sample_interval=.01, startup_seconds=.3)
        FakePty.instances = []
        Sampler.memory = 1024

    def run_fake(self, cls=FakePty):
        events = []
        with patch.object(tui, "_pty_class", return_value=cls), patch.object(tui.metrics, "Sampler", Sampler):
            result = tui.run_tui(Path("not-executed.exe"), self.root, {"ISOLATED": "1"}, self.args, events.append)
        return result, events

    def assert_no_threads(self):
        self.assertFalse([t.name for t in threading.enumerate() if t.name.startswith("resource-tui-")])

    def test_rendered_workload_and_owned_cleanup(self):
        unrelated = FakePty()
        result, events = self.run_fake()
        self.assertEqual(result["status"], "PASS", result)
        self.assertEqual(result["roundsCompleted"], 2)
        self.assertEqual(result["exitStatus"], 1)
        proc = FakePty.instances[-1]
        self.assertEqual(proc.closed, 1)
        self.assertTrue(unrelated.alive)
        self.assertEqual(proc.kwargs["dimensions"], (40, 120))
        self.assertEqual(proc.kwargs["backend"], 0)
        self.assertEqual(proc.kwargs["env"], {"ISOLATED": "1"})
        self.assertNotIn("--mode", proc.argv)
        self.assertNotIn("\r", "".join(proc.writes))
        self.assertNotIn("\n", "".join(proc.writes))
        self.assertEqual(result["workload"]["completedInputBytes"], 2 * (65536 + 12 + 64 + 64))
        self.assertEqual(len(events), len(result["samples"]))
        self.assertEqual(result["pid"], proc.pid)
        self.assert_no_threads()

    def test_memory_ceiling_aborts_only_owned_pty(self):
        unrelated = FakePty()
        Sampler.memory = 513 * 1024 * 1024
        self.args.max_memory_mib = 9999  # Hard ceiling cannot be raised.
        result, _ = self.run_fake()
        self.assertEqual(result["status"], "FAIL")
        self.assertEqual(result["category"], "safety")
        self.assertEqual(result["roundsCompleted"], 0)
        self.assertFalse(FakePty.instances[-1].alive)
        self.assertTrue(unrelated.alive)
        self.assert_no_threads()

    def test_unsupported_no_spawn(self):
        with patch.object(tui.sys, "platform", "linux"), patch.object(FakePty, "spawn") as spawn:
            result = tui.run_tui(Path("none"), self.root, {}, self.args, lambda e: None)
        self.assertEqual(result["status"], "SKIP")
        self.assertIsNone(result["baseline"])
        spawn.assert_not_called()

    def test_missing_optional_dependency(self):
        import builtins
        original = builtins.__import__
        def missing(name, *args, **kwargs):
            if name == "winpty":
                raise ImportError("not installed")
            return original(name, *args, **kwargs)
        with patch.object(tui.sys, "platform", "win32"), patch("builtins.__import__", missing):
            result = tui.run_tui(Path("none"), self.root, {}, self.args, lambda e: None)
        self.assertEqual(result["status"], "SKIP")
        self.assertIn("pywinpty", result["reason"])

    def test_unconfirmed_edit_is_skip_not_fake_pass(self):
        class NoPaste(FakePty):
            def write(self, text):
                return len(text.encode())
        result, _ = self.run_fake(NoPaste)
        self.assertEqual(result["status"], "SKIP")
        self.assertEqual(result["startupStatus"], "PASS")
        self.assertEqual(result["pressureStatus"], "SKIP")
        self.assertEqual(result["roundsCompleted"], 0)
        self.assert_no_threads()

    def test_bounded_log_and_cleanup_unblocks_read(self):
        proc = FakePty()
        terminal = tui.Terminal(proc, self.root / "terminal.log")
        proc.output.put("x" * (tui.MAX_LOG + 8192))
        deadline = time.monotonic() + 3
        while terminal.total < tui.MAX_LOG and time.monotonic() < deadline:
            time.sleep(.01)
        self.assertEqual(terminal.close(), [])
        self.assertEqual((self.root / "terminal.log").stat().st_size, tui.MAX_LOG)
        self.assertLessEqual(len(terminal.tail), tui.TAIL)
        self.assert_no_threads()


if __name__ == "__main__":
    unittest.main()
