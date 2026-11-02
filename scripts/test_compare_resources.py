#!/usr/bin/env python3
"""Offline comparison-controller unit tests; no external model or binaries needed."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

SPEC = importlib.util.spec_from_file_location("compare_resources", Path(__file__).with_name("compare-resources.py"))
compare = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(compare)


def metric(memory=10, wall=1, cpu=0):
    return dict(primary_metric="linux_rss", primary_bytes=memory, rss_bytes=memory,
                peak_rss_bytes=max(memory, 100), user_seconds=cpu, kernel_seconds=0, wall_seconds=wall)


def run(values, status="PASS"):
    return dict(status=status, roundsCompleted=len(values), baseline=metric(),
                final=metric(values[-1], 3, 1) if values else metric(),
                peakPrimaryBytes=max(values, default=10), samples=[
                    dict(phase="idle", round=i, sample=metric(value), delta={"cpu_percent": i})
                    for i, value in enumerate(values, 1)])


class ArithmeticTests(unittest.TestCase):
    def test_median_and_range(self):
        self.assertEqual(compare.median_range([9, 1, 4]), dict(median=4, min=1, max=9, n=3))
        self.assertEqual(compare.median_range([2, 4])["median"], 3)

    def test_missing_nonfinite_not_zero(self):
        for values in ([], [None], [1, None], [float("nan")], [float("inf")], [True]):
            self.assertIsNone(compare.median_range(values))
        self.assertIsNone(compare.excess(None, 0, 1, 1))
        self.assertIsNone(compare.summarize_runs([{"status": "FAIL"}])["peakPrimaryBytes"])

    def test_ratio_floor_and_both_thresholds(self):
        mib = compare.MIB
        value = compare.excess(mib, 0, 2 * mib, 25)
        self.assertEqual(value["percent"], 100)
        self.assertFalse(value["regression"])
        self.assertFalse(compare.excess(102 * mib, 100 * mib, mib, 25)["regression"])
        self.assertTrue(compare.excess(4 * mib, 0, mib, 25)["regression"])
        self.assertFalse(compare.excess(4 * mib, 8 * mib, mib, 25)["regression"])

    def test_first_divergence_matching_completed_rounds(self):
        m = compare.MIB
        ref = [run([m, m, m]), run([m, m, m])]
        cur = [run([m, 4*m, 6*m]), run([m, 5*m, 7*m])]
        found = compare.first_divergence(ref, cur, m, 25)
        self.assertEqual(found["round"], 2)
        self.assertEqual(found["candidate"]["median"], 4.5*m)
        cur[1]["roundsCompleted"] = 1
        self.assertIsNone(compare.first_divergence(ref, cur, m, 25))
        self.assertIsNone(compare.first_divergence(ref, cur[:1], m, 25))

    def test_os_peaks_not_primary(self):
        r = run([100])
        r["final"]["peak_rss_bytes"] = 99999
        stats = compare.summarize_runs([r])
        self.assertEqual(stats["peakPrimaryBytes"]["median"], 100)
        self.assertEqual(stats["osPeakRssBytes"]["median"], 99999)


class ProtocolTests(unittest.TestCase):
    def test_old_notification_not_fake_success(self):
        value = compare.normalize_control({"type": "agent_end", "messages": []}, {"type": "ready"})
        self.assertEqual(value["outcome"], "legacy-notification")
        self.assertTrue(value["legacy"])
        with self.assertRaises(compare.Problem):
            compare.validate_messages({"messages": []}, "prompt", 1, True, set())

    def test_new_agent_end_not_terminal(self):
        ready = {"semantics": {"settlementEvent": "agent_settled", "agentEndIsTerminal": False}}
        self.assertIsNone(compare.normalize_control({"type": "agent_end"}, ready))
        settled = {"type": "agent_settled", "outcome": "completed"}
        self.assertIs(compare.normalize_control(settled, ready), settled)

    def messages(self, ids=True):
        pair = [{"role": "user", "content": "prompt"}, {"role": "assistant", "stopReason": "stop",
                "content": [{"type": "text", "text": json.dumps(compare.smoke.expected(1, True))}],
                "provider": "resource-fixture", "model": "synthetic", "api": "openai-completions"}]
        if ids:
            for index, item in enumerate(pair):
                item["entryId"] = str(index)
        return {"messages": pair}

    def test_legacy_missing_ids_reported_but_not_fabricated(self):
        result = compare.validate_messages(self.messages(False), "prompt", 1, True, set())
        self.assertFalse(result["identityAvailable"])
        self.assertEqual(result["entryIds"], [None, None])

    def test_real_ids_validated_and_not_reused(self):
        seen = set()
        data = self.messages()
        self.assertTrue(compare.validate_messages(data, "prompt", 1, True, seen)["identityAvailable"])
        with self.assertRaises(compare.Problem):
            compare.validate_messages(data, "prompt", 1, True, seen)
        for key, value in (("stopReason", "length"), ("provider", "external")):
            altered = self.messages()
            altered["messages"][1][key] = value
            with self.subTest(key=key), self.assertRaises(compare.Problem):
                compare.validate_messages(altered, "prompt", 1, True, set())

    def test_advertised_native_identity_is_required(self):
        with self.assertRaises(compare.Problem):
            compare.validate_messages(self.messages(False), "prompt", 1, True, set(), require_ids=True)

    def test_failed_settlement_rejected(self):
        host = object.__new__(compare.CompatibleHost)
        host.settled = {"type": "agent_settled", "outcome": "failed"}
        with self.assertRaises(compare.Problem):
            host.completion()


class ControllerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.binary = self.root / "binary with spaces.exe"
        self.binary.write_bytes(b"not invoked")

    def argv(self, *more):
        return ["--variant", "stable0.16.0", str(self.binary), "-", "--variant", "currentaf1de8a",
                str(self.binary), "-", "--diagnostics-dir", str(self.root / "new out"), "--case", "rpc", *more]

    def test_paths_defaults_and_sources(self):
        args = compare.parse_args(self.argv("--source", "currentaf1de8a", "af1de8a"))
        self.assertEqual(args.variant[0], ("stable0.16.0", self.binary.resolve(), None))
        self.assertEqual(args.reference, "stable0.16.0")
        self.assertEqual((args.rounds, args.repeats, args.startup_seconds), (20, 3, 1))
        self.assertFalse(hasattr(args, "model"))

    def test_reject_existing_dir_bad_limits_paths_and_labels(self):
        cases = [("--rounds", "1001"), ("--repeats", "11"), ("--startup-seconds", ".9"),
                 ("--max-memory-mib", "nan"), ("--reference", "missing"),
                 ("--model", "external"), ("--source", "missing", "abc")]
        for extra in cases:
            with self.subTest(extra=extra), contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                compare.parse_args(self.argv(*extra))
        (self.root / "new out").mkdir()
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            compare.parse_args(self.argv())
        self.assertEqual(list((self.root / "new out").iterdir()), [])

    def test_missing_binary_rejected(self):
        self.binary.unlink()
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            compare.parse_args(self.argv())

    def test_safety_abort_not_pass_and_skip_distinct(self):
        args = compare.parse_args(self.argv())
        with self.assertRaises(compare.Problem) as error:
            compare.check_safety(513 * compare.MIB, args)
        self.assertEqual(error.exception.category, "safety_ceiling")
        self.assertEqual(compare.exit_status({"runs": [{"status": "SAFETY_ABORT"}], "comparisons": []}), 1)
        self.assertEqual(compare.exit_status({"runs": [{"status": "SKIP"}], "comparisons": []}), 77)
        self.assertEqual(compare.exit_status({"runs": [{"status": "PASS"}, {"status": "SKIP"}], "comparisons": []}), 0)

    def test_safety_reader_terminates_only_owned_pid_and_retains_trigger(self):
        args = compare.parse_args(self.argv())
        root = self.root / "case"
        root.mkdir()
        host = Mock()
        host.proc.poll.return_value = None
        host.proc.pid = 123
        events = []
        def sampler(pid, interval, reader):
            reader(pid)
            self.fail("safety reader must raise")
        with patch.object(compare, "CompatibleHost", return_value=host), \
             patch.object(compare.smoke, "Provider"), \
             patch.object(compare.smoke, "sample", return_value=metric(513 * compare.MIB)), \
             patch.object(compare.smoke, "Sampler", side_effect=sampler):
            result = compare.run_case(self.binary, None, root, args, "rpc", events.append)
        self.assertEqual(result["status"], "SAFETY_ABORT")
        self.assertEqual(result["firstError"]["category"], "safety_ceiling")
        self.assertEqual(result["final"]["primary_bytes"], 513 * compare.MIB)
        self.assertEqual(events[0]["phase"], "safety-abort")
        host.proc.terminate.assert_called_once_with()
        host.close.assert_called_once_with()

    def test_cleanup_does_not_replace_first_error(self):
        args = compare.parse_args(self.argv())
        root = self.root / "case"
        root.mkdir()
        host = Mock()
        host.wait.side_effect = compare.Problem("protocol", "original")
        host.close.side_effect = RuntimeError("later cleanup error")
        sampler = Mock(peak=10)
        sampler.take.return_value = metric()
        with patch.object(compare, "CompatibleHost", return_value=host), \
             patch.object(compare.smoke, "Provider"), \
             patch.object(compare.smoke, "Sampler", return_value=sampler):
            result = compare.run_case(self.binary, None, root, args, "rpc", lambda event: None)
        self.assertEqual(result["firstError"]["message"], "original")
        self.assertEqual(result["status"], "FAIL")
        self.assertEqual(len(result["samples"]), 1)
        sampler.close.assert_called_once_with()

    def test_only_last_variant_regression_gates(self):
        data = {"runs": [{"status": "PASS"}], "comparisons": [
            {"gatesExit": False, "status": "REGRESSION"}, {"gatesExit": True, "status": "PASS"}]}
        self.assertEqual(compare.exit_status(data), 0)
        data["comparisons"][1]["status"] = "REGRESSION"
        self.assertEqual(compare.exit_status(data), 1)

    def test_main_no_overwrite_on_race(self):
        args = compare.parse_args(self.argv())
        args.diagnostics_dir.mkdir()
        with patch.object(compare, "parse_args", return_value=args), self.assertRaises(FileExistsError):
            compare.main([])
        self.assertEqual(list(args.diagnostics_dir.iterdir()), [])

    def test_sequential_interleaving_and_result_files(self):
        calls = []
        def fake_case(binary, probe, root, args, kind, emit):
            calls.append(root.name)
            emit({"event": "test"})
            r = run([10, 10])
            r["roundsCompleted"] = args.rounds
            return r
        with patch.object(compare, "version_metadata", return_value={"version": "fake", "binarySha256": "abc"}), \
             patch.object(compare, "run_case", side_effect=fake_case):
            self.assertEqual(compare.main(self.argv("--rounds", "2", "--repeats", "2")), 0)
        self.assertEqual(calls, ["repeat-1-rpc-stable0.16.0", "repeat-1-rpc-currentaf1de8a",
                                 "repeat-2-rpc-stable0.16.0", "repeat-2-rpc-currentaf1de8a"])
        root = self.root / "new out"
        data = json.loads((root / "results.json").read_text())
        self.assertEqual(data["status"], "PASS")
        self.assertEqual(len(data["runs"]), 4)
        self.assertIn("do not disprove", (root / "summary.md").read_text())


if __name__ == "__main__":
    unittest.main()
