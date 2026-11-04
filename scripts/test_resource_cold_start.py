#!/usr/bin/env python3
"""Offline controller regressions; opt in to real RPC with RPI_COLD_START_BINARY.

Run: python -B -m unittest discover -s scripts -p test_resource_cold_start.py -v
All generated files live in temporary directories outside the checkout. No large
historical fixture, cargo build, provider, or real session is used by default.
"""
import sys

sys.dont_write_bytecode = True

import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().with_name("resource-cold-start.py")
REPOSITORY = SCRIPT.parent.parent
spec = importlib.util.spec_from_file_location("tested_resource_cold_start", SCRIPT)
cold = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cold)

external_spec = importlib.util.spec_from_file_location(
    "tested_external_tools_smoke", SCRIPT.with_name("external-tools-smoke.py"))
external = importlib.util.module_from_spec(external_spec)
external_spec.loader.exec_module(external)


def parameters(**changes):
    values = dict(baseline=None, shapes=["legacy", "modern"], frames=80,
                  entries=12, repeats=1, frame_bytes=8, max_ready_ms=2000,
                  max_single_ready_ms=3000, max_get_messages_ms=100,
                  max_get_state_ms=1500)
    values.update(changes)
    return SimpleNamespace(**values)


def idle_state(**changes):
    result = dict(activeOperation=None, queue=dict(steering=[], followUp=[],
                                                  steeringCount=0, followUpCount=0))
    result.update(changes)
    return result


def observation(shape="legacy", variant="candidate", repeat=1, **changes):
    result = dict(shape=shape, variant=variant, repeat=repeat, status="PASS",
                  errors=[], cleanup=[], metrics=dict(readyMs=100,
                  getMessages40Ms=10, getStateMs=20, peakPrimaryBytes=1024,
                  peakPrivateBytes=None, osPeakCommitBytes=None))
    result.update(changes)
    return result


class TemporaryCase(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="rpi-cold-start-test-")
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name).resolve()
        self.assertNotIn(REPOSITORY, self.base.parents)
        self.binary = self.base / "candidate.exe"
        self.binary.write_bytes(b"not executed: controller unit test\n")
        self.baseline = self.base / "baseline.exe"
        self.baseline.write_bytes(b"not executed: baseline unit test\n")

    def argv(self, output=None, paired=False, extra=()):
        result = ["--candidate", str(self.binary), "--output",
                  str(output or self.base / "output")]
        if paired:
            result += ["--baseline", str(self.baseline)]
        return result + list(extra)

    def parse(self, argv):
        with contextlib.redirect_stderr(io.StringIO()):
            return cold.parse_args(argv)

    def rejected(self, argv):
        with self.assertRaises(SystemExit) as raised:
            self.parse(argv)
        self.assertEqual(raised.exception.code, 2)


class ArgumentTests(TemporaryCase):
    def test_locked_historical_defaults_and_candidate_only(self):
        args = self.parse(self.argv())
        self.assertIsNone(args.baseline)
        self.assertEqual((args.frames, args.entries, args.repeats, args.frame_bytes),
                         (701000, 21733, 5, 256))
        self.assertEqual(args.shapes, ["legacy", "modern"])
        self.assertEqual((args.max_ready_ms, args.max_single_ready_ms,
                          args.max_get_messages_ms, args.max_get_state_ms),
                         (2000, 3000, 100, 1500))
        self.assertEqual(args.candidate, self.binary)
        self.assertFalse(args.output.exists())

    def test_paired_args_and_small_overrides(self):
        args = self.parse(self.argv(paired=True, extra=("--frames", "80", "--entries",
            "12", "--repeats", "1", "--frame-bytes", "4", "--shapes", "modern")))
        self.assertEqual(args.baseline, self.baseline)
        self.assertEqual((args.frames, args.entries, args.repeats, args.frame_bytes), (80, 12, 1, 4))
        self.assertEqual(args.shapes, ["modern"])

    def test_existing_outputs_and_repository_are_not_modified(self):
        directory = self.base / "existing"
        directory.mkdir()
        sentinel = directory / "sentinel.bin"
        sentinel.write_bytes(b"keep me\x00")
        for output in (directory, sentinel, REPOSITORY, REPOSITORY / "cold-test-must-not-exist"):
            with self.subTest(output=output):
                self.rejected(self.argv(output))
        self.assertEqual(sentinel.read_bytes(), b"keep me\x00")
        self.assertEqual(list(directory.iterdir()), [sentinel])
        self.assertFalse((REPOSITORY / "cold-test-must-not-exist").exists())

    def test_bad_binaries_missing_candidate_and_duplicate_shapes(self):
        for key in ("--candidate", "--baseline"):
            for path in (self.base / "missing.exe", self.base):
                with self.subTest(key=key, path=path):
                    args = self.argv(paired=True)
                    args[args.index(key) + 1] = str(path)
                    self.rejected(args)
        self.rejected(["--output", str(self.base / "unused")])
        self.rejected(self.argv(extra=("--shapes", "legacy", "legacy")))

    def test_nonpositive_and_nonfinite_limits(self):
        for key in ("frames", "entries", "repeats", "frame-bytes", "max-ready-ms",
                    "max-single-ready-ms", "max-get-messages-ms", "max-get-state-ms"):
            for value in ("0", "-1", "nan", "inf", "-inf"):
                with self.subTest(key=key, value=value):
                    self.rejected(self.argv(extra=("--" + key + "=" + value,)))
        self.assertFalse((self.base / "output").exists())


class GenerationTests(TemporaryCase):
    def test_shapes_sequences_commit_boundaries_and_entry_order(self):
        # 4001 exercises a complete 4000-frame stream and a final partial stream.
        # A single entry exercises commit references to the FINAL canonical branch.
        for shape in ("legacy", "modern"):
            for frames, entries in ((80, 12), (4000, 3), (4001, 3), (4001, 1), (3, 8)):
                with self.subTest(shape=shape, frames=frames, entries=entries):
                    path = self.base / (shape + "-" + str(frames) + "-" + str(entries) + ".jsonl")
                    args = parameters(frames=frames, entries=entries, frame_bytes=3)
                    metadata = cold.generate(path, self.base / "workspace", shape, args)
                    raw = path.read_bytes()
                    rows = [json.loads(line) for line in raw.splitlines()]
                    header, records = rows[0], rows[1:]
                    expected = dict(kind="header", version=4, id=cold.SESSION_ID,
                                    createdAt=0, cwd=str(self.base / "workspace"))
                    if shape == "modern":
                        expected["progressFormat"] = cold.FORMAT
                    self.assertEqual(header, expected)
                    self.assertTrue(raw.endswith(b"\n"))
                    self.assertEqual([r["seq"] for r in records], list(range(1, len(records) + 1)))
                    self.assertEqual(len({r["id"] for r in records}), len(records))
                    appends = [r for r in records if r.get("op") == "append"]
                    messages = [r for r in records if r["kind"] == "entry"]
                    commits = [r for r in records if r.get("op") == "commit_stream"]
                    self.assertEqual(len(appends), frames)
                    self.assertEqual(len(commits), (frames + 3999) // 4000)
                    self.assertEqual([r["id"] for r in messages], ["entry-" + str(i) for i in range(1, entries + 1)])
                    self.assertEqual([r["message"]["content"] for r in messages],
                                     ["synthetic-entry-" + str(i) for i in range(1, entries + 1)])
                    emitted = appended = committed = 0
                    for row in records:
                        if row.get("op") == "append":
                            appended += 1
                            self.assertEqual(row["frame"]["delta"], "xxx")
                            self.assertEqual(row["runId"], "synthetic-run-" + str((appended - 1) // 4000))
                        elif row["kind"] == "entry":
                            emitted += 1
                            # ceil(entry_index * frames / entries), also when entries > frames.
                            self.assertEqual(appended, (emitted * frames + entries - 1) // entries)
                            self.assertEqual(row["parentId"], "entry-" + str(emitted - 1) if emitted > 1 else None)
                        else:
                            committed += 1
                            self.assertEqual(emitted, appended * entries // frames)
                            self.assertEqual(appended, min(committed * 4000, frames))
                            self.assertEqual(row["frame"]["entryId"], "entry-" + str(max(1, emitted)))
                            self.assertIn(row["frame"]["entryId"], {r["id"] for r in messages})
                            self.assertEqual(row["runId"], "synthetic-run-" + str(committed - 1))
                    self.assertEqual(metadata["historySha256"], hashlib.sha256(raw).hexdigest())
                    self.assertEqual(metadata["historyBytes"], len(raw))
                    self.assertEqual((metadata["appendFrames"], metadata["entries"], metadata["lastSeq"]),
                                     (frames, entries, len(records)))
                    self.assertEqual(metadata["commitStreams"], len(commits))
                    self.assertFalse(metadata["existingIndex"])
                    companion = Path(str(path) + ".progress.jsonl")
                    if shape == "modern":
                        self.assertEqual(len(companion.read_bytes().splitlines()), 1)
                        self.assertTrue(companion.read_bytes().endswith(b"\n"))
                        self.assertEqual(json.loads(companion.read_text(encoding="utf-8")),
                            {"format": cold.FORMAT, "session_id": cold.SESSION_ID,
                             "state": {"sequence": 0, "frames": []}})
                        self.assertEqual(metadata["companionSha256"], cold.sha256(companion))
                    else:
                        self.assertFalse(companion.exists())
                        self.assertIsNone(metadata["companionSha256"])
                    before = path.read_bytes()
                    with self.assertRaises(FileExistsError):
                        cold.generate(path, self.base, shape, args)
                    self.assertEqual(path.read_bytes(), before)


class SummaryTests(unittest.TestCase):
    def summary(self, runs, **changes):
        return cold.summarize(runs, parameters(shapes=["legacy"], **changes))

    def test_candidate_only_and_inclusive_ready_median_max(self):
        runs = [observation(repeat=i + 1) for i in range(3)]
        for run, ready in zip(runs, (1000, 2000, 3000)):
            run["metrics"]["readyMs"] = ready
        summaries = self.summary(runs, repeats=3)
        self.assertEqual([s["variant"] for s in summaries], ["candidate"])
        item = summaries[0]
        self.assertEqual(item["metrics"]["readyMs"], {"median": 2000, "min": 1000, "max": 3000, "n": 3})
        self.assertEqual(item["gate"]["status"], "PASS")
        self.assertTrue(item["gate"]["gatesExit"])
        for timings in ((2001, 2001, 2001), (1, 1, 3001)):
            for run, ready in zip(runs, timings):
                run["metrics"]["readyMs"] = ready
            self.assertEqual(self.summary(runs, repeats=3)[0]["gate"]["status"], "FAIL")

    def test_message_and_state_max_not_median_gate(self):
        for key, limit in (("getMessages40Ms", 100), ("getStateMs", 1500)):
            runs = [observation(repeat=i + 1) for i in range(3)]
            for run in runs:
                run["metrics"][key] = limit
            self.assertEqual(self.summary(runs, repeats=3)[0]["gate"]["status"], "PASS")
            runs[-1]["metrics"][key] = limit + .01
            self.assertEqual(self.summary(runs, repeats=3)[0]["gate"]["status"], "FAIL")

    def test_missing_nonfinite_and_functional_fail_never_pass(self):
        missing = object()
        for key in ("readyMs", "getMessages40Ms", "getStateMs"):
            for value in (missing, None, float("nan"), float("inf"), -float("inf"), -1, True, "1"):
                with self.subTest(key=key, value=value):
                    run = observation()
                    if value is missing:
                        del run["metrics"][key]
                    else:
                        run["metrics"][key] = value
                    self.assertEqual(self.summary([run])[0]["gate"]["status"], "FAIL")
        self.assertEqual(self.summary([observation(status="FAIL")])[0]["gate"]["status"], "FAIL")
        self.assertEqual(self.summary([])[0]["gate"]["status"], "FAIL")
        self.assertEqual(self.summary([observation()], repeats=2)[0]["gate"]["status"], "FAIL")

    def test_slow_baseline_informational(self):
        baseline = observation(variant="baseline")
        baseline["metrics"].update(readyMs=9000, getMessages40Ms=9000, getStateMs=9000)
        baseline_summary, candidate_summary = self.summary([baseline, observation()], baseline=Path("old.exe"))
        self.assertEqual(baseline_summary["gate"]["status"], "FAIL")
        self.assertFalse(baseline_summary["gate"]["gatesExit"])
        self.assertEqual(candidate_summary["gate"]["status"], "PASS")


class MainTests(TemporaryCase):
    def invoke(self, paired=False, mutate=None, repeats=2):
        output = self.base / ("output-" + str(len(list(self.base.glob("output-*")))))
        argv = self.argv(output, paired, ("--frames", "80", "--entries", "12",
                                         "--repeats", str(repeats)))
        calls = []

        def fake_run(binary, root, args, shape, variant, repeat):
            self.assertFalse(root.exists(), "each observation needs a fresh root")
            self.assertEqual(binary, self.baseline if variant == "baseline" else self.binary)
            root.mkdir()
            calls.append((shape, variant, repeat, root))
            result = observation(shape, variant, repeat)
            if mutate:
                mutate(result)
            cold.dump(root / "report.json", result)
            return result

        with mock.patch.object(cold, "run_once", side_effect=fake_run), \
             mock.patch.object(cold.sys, "platform", "linux"), \
             mock.patch.object(cold.sys, "argv", ["unrelated-host", "--do-not-use-host-argv"]), \
             contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            code = cold.main(argv)
        data = json.loads((output / "results.json").read_text(encoding="utf-8"))
        reproduce = json.loads((output / "reproduce.json").read_text(encoding="utf-8"))
        self.assertEqual(reproduce["argv"][3:], argv)
        self.assertEqual(reproduce["generatorSha256"], cold.sha256(SCRIPT))
        self.assertEqual(reproduce["parameters"], data["parameters"])
        self.assertEqual(data["scenario"], "historical-frame-heavy-v1")
        self.assertFalse(data["diskCacheFlushed"])
        self.assertEqual(len(calls), len(data["runs"]))
        self.assertEqual(len({r[3] for r in calls}), len(calls))
        for _, _, _, root in calls:
            self.assertTrue((root / "report.json").is_file())
        return code, data, calls

    def test_candidate_only_new_roots_both_shapes_and_reports(self):
        code, data, calls = self.invoke()
        self.assertEqual(code, 0)
        self.assertEqual(data["status"], "PASS")
        self.assertEqual([(s, v, n) for s, v, n, _ in calls],
                         [(s, "candidate", n) for s in ("legacy", "modern") for n in (1, 2)])
        self.assertEqual(set(data["executables"]), {"candidate"})
        self.assertEqual({s["variant"] for s in data["summaries"]}, {"candidate"})
        self.assertEqual(set(data["legacyMinusModernReadyMedianMs"]), {"candidate"})
        self.assertIsNone(data["parameters"]["baseline"])

    def test_paired_abba_order_and_slow_baseline_is_informational(self):
        def slow_baseline(run):
            if run["variant"] == "baseline":
                run["metrics"].update(readyMs=9999, getMessages40Ms=9999, getStateMs=9999)
        code, data, calls = self.invoke(paired=True, mutate=slow_baseline)
        self.assertEqual(code, 0)
        self.assertEqual(set(data["executables"]), {"baseline", "candidate"})
        self.assertEqual([(s, v, n) for s, v, n, _ in calls],
            [(s, v, n) for s in ("legacy", "modern") for n, order in
             ((1, ("baseline", "candidate")), (2, ("candidate", "baseline"))) for v in order])

    def test_candidate_latency_failures_exit_nonzero(self):
        for metric, value in (("readyMs", 3001), ("getMessages40Ms", 101), ("getStateMs", 1501)):
            with self.subTest(metric=metric):
                def fail(run):
                    run["metrics"][metric] = value
                code, data, _ = self.invoke(mutate=fail, repeats=1)
                self.assertEqual(code, 1)
                self.assertEqual(data["status"], "FAIL")

    def test_baseline_functional_failure_fails_whole_run(self):
        def fail_baseline(run):
            if run["variant"] == "baseline":
                run.update(status="FAIL", errors=[dict(phase="get_messages", category="semantic",
                                                       message="wrong message order")])
        code, data, _ = self.invoke(paired=True, mutate=fail_baseline)
        self.assertEqual(code, 1)
        self.assertEqual(data["status"], "FAIL")

    def test_cleanup_failure_stops_before_another_observation(self):
        for phase, category in (("history-cleanup", "runtime"), ("shutdown", "cleanup")):
            with self.subTest(phase=phase, category=category):
                def fail_cleanup(run):
                    run.update(status="FAIL", errors=[dict(phase=phase, category=category,
                                                           message="owned cleanup failed")])
                code, data, calls = self.invoke(paired=True, mutate=fail_cleanup)
                self.assertEqual(code, 1)
                self.assertEqual(len(calls), 1)
                self.assertEqual(data["status"], "FAIL")
                self.assertIn("cleanup", data["fatalError"]["message"])


class RunOnceTests(TemporaryCase):
    def run_fake(self, entries=12, change_messages=None, state=None, close_error=False, samples=None):
        root = self.base / ("observation-" + str(entries) + "-" +
                            str(len(list(self.base.glob("observation-*")))))
        calls = []

        class FakeHost:
            def __init__(host, command, root, env, timeout):
                host.proc = SimpleNamespace(pid=123, returncode=0)
                host.start_ns, host.ready_ns = 0, 1000000
                host.tree_cleanup = []

            def wait(host, predicate):
                self.assertTrue(predicate({"type": "ready"}))

            def call(host, name, **kwargs):
                calls.append(name)
                if name == "get_messages":
                    self.assertEqual(kwargs, {"limit": 40})
                    rows = [dict(role="user", content="synthetic-entry-" + str(i))
                            for i in range(max(1, entries - 39), entries + 1)]
                    if change_messages:
                        change_messages(rows)
                    return dict(messages=rows, pendingResume=False)
                if name == "get_state":
                    return idle_state() if state is None else state
                self.assertEqual(name, "stop")
                return {}

            def exit_ok(host):
                pass

            def close(host):
                if close_error:
                    raise cold.smoke.Problem("cleanup", "mock tree cleanup failure")
                host.tree_cleanup.append(dict(method="fake-owned-tree", ok=True))

        real_sampler = cold.smoke.Sampler
        samplers = []
        reader = mock.Mock(side_effect=samples)
        thread = mock.Mock()
        thread.is_alive.return_value = False

        def make_sampler(pid, interval):
            if samples is None:
                return SimpleNamespace(peak=1024, close=lambda: None,
                    take=lambda: dict(primary_metric="rss_bytes", peak_commit_bytes=None))
            sampler = real_sampler(pid, interval, reader=reader)
            samplers.append(sampler)
            return sampler

        with mock.patch.object(cold, "ColdHost", FakeHost), \
             mock.patch.object(cold.smoke, "Sampler", side_effect=make_sampler), \
             mock.patch.object(cold.smoke.threading, "Thread", return_value=thread):
            result = cold.run_once(self.binary, root, parameters(entries=entries), "modern", "candidate", 1)
        if samples is not None:
            self.assertEqual(len(samplers), 1)
            self.assertTrue(samplers[0].stop_event.is_set())
            thread.start.assert_called_once()
            thread.join.assert_called_once_with(timeout=5)
            self.assertEqual(reader.call_count, len(samples))
        self.assertEqual(result, json.loads((root / "report.json").read_text(encoding="utf-8")))
        self.assertTrue((root / "fixture.json").is_file())
        history = root / "sessions" / "fixture" / "history.jsonl"
        for path in (history, Path(str(history) + ".progress.jsonl"), Path(str(history) + ".lock")):
            self.assertEqual(path.exists(), close_error and path != Path(str(history) + ".lock"))
        if not close_error:
            self.assertTrue(any(item.get("ok") for item in result["cleanup"]))
        return result, calls

    def test_small_entries_and_last_40_messages_idle_and_cleanup(self):
        for entries in (1, 12, 45):
            with self.subTest(entries=entries):
                # A fresh independent root is mandatory; retain prior diagnostics.
                result, calls = self.run_fake(entries=entries)
                self.assertEqual(result["status"], "PASS")
                self.assertEqual(result["messageCount"], min(entries, 40))
                self.assertEqual(calls, ["get_messages", "get_state", "stop"])
                self.assertEqual(result["stateFlags"], dict(activeOperation=None, steeringCount=0,
                                                          followUpCount=0, pendingResume=False))

    def test_linux_approximate_peak_decrease_does_not_fail_ready_or_cleanup(self):
        samples = [dict(primary_metric="linux_rss", primary_bytes=rss, rss_bytes=rss,
                        peak_rss_bytes=hwm, user_seconds=1, kernel_seconds=1, wall_seconds=i)
                   for i, (rss, hwm) in enumerate(
                       ((4096, 8192), (2048, 4096), (3072, 6144), (1024, 2048)), 1)]
        result, calls = self.run_fake(samples=samples)
        self.assertEqual(result["status"], "PASS")
        self.assertEqual(result["errors"], [])
        self.assertEqual(result["primaryMetric"], "linux_rss")
        self.assertEqual(result["metrics"]["peakPrimaryBytes"], 4096)
        self.assertEqual(calls, ["get_messages", "get_state", "stop"])
        self.assertTrue(all(item.get("ok") or item.get("deleted") or item.get("absent")
                            for item in result["cleanup"]))

    def test_cpu_and_wall_decreases_still_fail_ready_and_clean_up(self):
        before = dict(primary_metric="linux_rss", primary_bytes=4096, rss_bytes=4096,
                      peak_rss_bytes=8192, user_seconds=1, kernel_seconds=1, wall_seconds=1)
        for key in ("user_seconds", "kernel_seconds", "wall_seconds"):
            with self.subTest(key=key):
                after = before | {"peak_rss_bytes": 4096, key: 0}
                result, calls = self.run_fake(samples=[before, after])
                self.assertEqual(result["status"], "FAIL")
                self.assertEqual(result["errors"][0]["phase"], "ready")
                self.assertEqual(result["errors"][0]["message"], "retrograde metric: " + key)
                self.assertEqual(result["metrics"]["peakPrimaryBytes"], 4096)
                self.assertEqual(calls, [])
                self.assertTrue(any(item.get("ok") for item in result["cleanup"]))

    def test_process_cleanup_failure_retains_owned_history(self):
        result, _ = self.run_fake(close_error=True)
        self.assertEqual(result["status"], "FAIL")
        self.assertEqual(result["errors"][0]["phase"], "process-cleanup")
        self.assertTrue(all(item.get("retained") for item in result["cleanup"]))

    def test_wrong_message_order_is_functional_failure(self):
        result, _ = self.run_fake(change_messages=lambda rows: rows.reverse())
        self.assertEqual(result["status"], "FAIL")
        self.assertEqual(result["errors"][0]["phase"], "get_messages")

    def test_nonidle_or_missing_state_is_functional_failure(self):
        states = [{}, idle_state(activeOperation={"runId": "synthetic-active"}),
                  {"activeOperation": None}, {"queue": idle_state()["queue"]}]
        for key in ("steeringCount", "followUpCount", "steering", "followUp"):
            missing = idle_state()
            del missing["queue"][key]
            states.append(missing)
            malformed = idle_state()
            malformed["queue"][key] = 1 if key.endswith("Count") else ["synthetic-queued"]
            states.append(malformed)
        for state in states:
            with self.subTest(state=state):
                result, _ = self.run_fake(state=state)
                self.assertEqual(result["status"], "FAIL")
                self.assertEqual(result["errors"][0]["phase"], "get_state")




class HostLifecycleTests(TemporaryCase):
    def test_cold_reader_protocol_errors_are_not_silenced(self):
        for value in (b"not-json\n", b"[]\n", b"\xff\n", b"x" * (cold.smoke.MAX_LINE + 1)):
            with self.subTest(prefix=value[:8]):
                host = cold.ColdHost.__new__(cold.ColdHost)
                host.error = None
                host.record = mock.Mock()
                host.pump(io.BytesIO(value), "stdout")
                self.assertIsNotNone(host.error)
                self.assertIn("reader-error", [call.args[0] for call in host.record.call_args_list])

    def test_cold_tree_cleanup_after_parent_exit_and_failure_propagation(self):
        host = cold.ColdHost.__new__(cold.ColdHost)
        host.closed, host.proc, host.tree_cleanup = False, SimpleNamespace(pid=123), []
        host.job = mock.Mock()
        with mock.patch.object(cold.smoke.Host, "close") as parent_close:
            host.close()
            host.job.close.assert_called_once()
            parent_close.assert_called_once()
            self.assertTrue(host.tree_cleanup[-1]["ok"])
            host.job.close.side_effect = OSError("job failure")
            with self.assertRaisesRegex(cold.smoke.Problem, "job failure"):
                host.close()
            self.assertFalse(host.tree_cleanup[-1]["ok"])
        host.job = None
        with mock.patch.object(cold.os, "killpg", create=True) as killpg, \
             mock.patch.object(cold.signal, "SIGKILL", 9, create=True), \
             mock.patch.object(cold.smoke.Host, "close"):
            host.close()
            killpg.assert_called_once_with(123, 9)

    def test_external_native_startup_and_tree_cleanup_use_only_owned_process(self):
        root = self.base / "external-native"
        root.mkdir()
        process = mock.Mock(pid=123, returncode=0)
        process.stdin, process.stdout = io.StringIO(), io.StringIO()
        process.wait.return_value = process.poll.return_value = 0
        reader = mock.Mock(ident=1)
        reader.is_alive.return_value = False
        job = mock.Mock()
        with mock.patch.object(external, "environment", return_value={}), \
             mock.patch.object(external, "windows_job", return_value=job), \
             mock.patch.object(external.subprocess, "Popen", return_value=process) as popen, \
             mock.patch.object(external.threading, "Thread", return_value=reader), \
             mock.patch.object(external.os, "killpg", create=True) as killpg:
            host = external.Native(self.binary, root, "snapshot", None)
            self.assertEqual(popen.call_args.kwargs["start_new_session"], os.name != "nt")
            host.close()
            host.close()
            if os.name == "nt":
                job.assign.assert_called_once_with(process)
                job.close.assert_called_once()
            else:
                killpg.assert_called_once_with(123, external.signal.SIGKILL)
            self.assertTrue(host.stderr.closed)
            reader.join.assert_called_once()
        with mock.patch.object(external, "environment", return_value={}), \
             mock.patch.object(external, "windows_job", return_value=mock.Mock()), \
             mock.patch.object(external.subprocess, "Popen", side_effect=OSError("spawn failed")):
            with self.assertRaisesRegex(OSError, "spawn failed"):
                external.Native(self.binary, root, "snapshot", None)

    def test_external_failed_job_close_is_not_swallowed(self):
        host = external.Native.__new__(external.Native)
        host.closed, host.process, host.reader = False, None, None
        host.stderr, host.job = io.StringIO(), mock.Mock()
        host.job.close.side_effect = OSError("job failure")
        with self.assertRaisesRegex(external.OwnedCleanupError, "job failure"):
            host.close()
        self.assertTrue(host.stderr.closed)
        self.assertFalse(host.closed)

    def test_external_environment_has_no_inherited_secrets(self):
        model = SimpleNamespace(server=SimpleNamespace(server_port=1234))
        with mock.patch.dict(os.environ, {"HOME": "DO_NOT_READ", "RPI_SECRET": "synthetic-secret",
                                        "API_KEY": "synthetic-secret"}, clear=True):
            env = external.environment(self.base, model)
        self.assertNotIn("RPI_SECRET", env)
        self.assertNotIn("API_KEY", env)
        self.assertEqual(env["HOME"], str(self.base / "home"))
        self.assertEqual(json.loads((self.base / "agent/auth.json").read_text()), {})
        external.cleanup_environment(self.base)
        self.assertFalse(any((self.base / name).exists() for name in external.OWNED_DIRECTORIES))
        self.assertTrue(self.binary.exists(), "cleanup must not remove unrelated files")



    def test_external_scenarios_clean_data_and_abort_after_cleanup_failure(self):
        for cleanup_failure in (False, True):
            with self.subTest(cleanup_failure=cleanup_failure):
                root = self.base / ("external-run-" + str(cleanup_failure))
                root.mkdir()
                calls = []

                class FakeModel:
                    def __init__(model, case):
                        model.server = SimpleNamespace(server_port=1234)
                        model.stopping, model.release = mock.Mock(), mock.Mock()
                    def close(model):
                        if cleanup_failure:
                            raise RuntimeError("model cleanup sentinel")

                class FakeNative:
                    def __init__(host, binary, case, encoding, model):
                        calls.append(case)
                        external.environment(case, model)
                    def ready(host, encoding):
                        pass
                    def close(host):
                        pass

                scenario = mock.Mock(side_effect=ValueError("scenario sentinel") if cleanup_failure else None)
                with mock.patch.object(external, "Model", FakeModel), \
                     mock.patch.object(external, "Native", FakeNative), \
                     mock.patch.object(external, "roundtrip", scenario), \
                     mock.patch.object(external, "eof_pending", scenario), \
                     mock.patch.object(external, "eof_model_only", scenario), \
                     contextlib.redirect_stdout(io.StringIO()):
                    if cleanup_failure:
                        with self.assertRaisesRegex(RuntimeError, "stopping further scenarios"):
                            external.run(self.binary, root)
                    else:
                        self.assertEqual(len(external.run(self.binary, root)), 6)
                self.assertEqual(len(calls), 1 if cleanup_failure else 6)
                result = json.loads((root / "results.json").read_text(encoding="utf-8"))
                if cleanup_failure:
                    self.assertIn("scenario sentinel", result["failed"][0]["error"])
                    self.assertIn("model cleanup sentinel", result["failed"][0]["error"])
                    self.assertTrue((calls[0] / "sessions").exists())
                else:
                    self.assertEqual(result["failed"], [])
                    self.assertFalse(any((case / name).exists() for case in calls
                                         for name in external.OWNED_DIRECTORIES))


    def test_external_cli_rejects_existing_and_repository_outputs_before_run(self):
        for output in (self.base, REPOSITORY / "external-must-not-exist"):
            argv = ["external-tools-smoke.py", "--rpi", str(self.binary),
                    "--diagnostics-dir", str(output)]
            with self.subTest(output=output), mock.patch.object(external.sys, "argv", argv),                  mock.patch.object(external, "run") as run,                  contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as raised:
                external.main()
            self.assertEqual(raised.exception.code, 2)
            run.assert_not_called()
        self.assertFalse((REPOSITORY / "external-must-not-exist").exists())


@unittest.skipUnless(os.environ.get("RPI_COLD_START_BINARY", "").strip(),
                     "set RPI_COLD_START_BINARY for isolated real RPC integration")
@unittest.skipUnless(sys.platform == "win32" or sys.platform.startswith("linux"),
                     "resource sampling supports Windows/Linux")
class RealRpcTests(TemporaryCase):
    def test_real_candidate_legacy_modern_small_fixture(self):
        binary = Path(os.environ["RPI_COLD_START_BINARY"]).expanduser().resolve()
        self.assertTrue(binary.is_file(), str(binary))
        output = self.base / "real-rpc-output"
        argv = ["--candidate", str(binary), "--output", str(output), "--frames", "80",
                "--entries", "12", "--repeats", "1", "--shapes", "legacy", "modern"]
        with contextlib.redirect_stdout(io.StringIO()):
            code = cold.main(argv)
        data = json.loads((output / "results.json").read_text(encoding="utf-8"))
        self.assertEqual(code, 0, json.dumps(data, ensure_ascii=False, indent=2))
        self.assertEqual(data["status"], "PASS")
        self.assertEqual(data["scenario"], "historical-frame-heavy-v1")
        self.assertEqual([(r["shape"], r["variant"]) for r in data["runs"]],
                         [("legacy", "candidate"), ("modern", "candidate")])
        for run in data["runs"]:
            self.assertEqual(run["status"], "PASS")
            self.assertEqual(run["errors"], [])
            self.assertEqual(run["messageCount"], 12)
            self.assertEqual(run["stateFlags"], dict(activeOperation=None, steeringCount=0,
                                                    followUpCount=0, pendingResume=False))
            self.assertLessEqual(run["metrics"]["getMessages40Ms"], 100)
            self.assertLessEqual(run["metrics"]["getStateMs"], 1500)
            self.assertTrue(run["cleanup"])
            self.assertTrue(any(item.get("ok") is True for item in run["cleanup"]))
            for item in run["cleanup"]:
                if "path" in item:
                    path = Path(item["path"])
                    self.assertIn(output, path.parents)
                    self.assertFalse(path.exists())
                    self.assertTrue(item.get("deleted") or item.get("absent"))
        self.assertFalse(list(output.rglob("history.jsonl*")))
        self.assertTrue(all(s["gate"]["status"] == "PASS" for s in data["summaries"]))


if __name__ == "__main__":
    unittest.main()
