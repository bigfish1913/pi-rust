#!/usr/bin/env python3
"""Resource controller unit tests; all network traffic stays on loopback."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import Mock, patch
from urllib.parse import quote, quote_plus

SPEC = importlib.util.spec_from_file_location("resource_smoke", Path(__file__).with_name("resource-smoke.py"))
smoke = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(smoke)


def metric(wall=1, user=1, kernel=1, memory=10, **extra):
    return dict(primary_metric="linux_rss", primary_bytes=memory, rss_bytes=memory,
                peak_rss_bytes=100, user_seconds=user, kernel_seconds=kernel, wall_seconds=wall, **extra)


class Base(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.binary = Path(sys.executable)

    def tearDown(self):
        self.temp.cleanup()

    def argv(self, *more):
        return ["--rpi", str(self.binary), "--case", "rpc", "--diagnostics-dir", str(self.root / "out"), *more]

    def args(self, *more):
        return smoke.parse_args(self.argv(*more))

    def report(self, *more):
        args = self.args(*more)
        args.diagnostics_dir.mkdir()
        return args, smoke.Report(args.diagnostics_dir, args)

    def usage_error(self, *more):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as caught:
            self.args(*more)
        self.assertEqual(caught.exception.code, 2)


class MetricsTests(Base):
    def test_linux_spaces_parentheses_and_ticks(self):
        fields = ["S"] + ["0"] * 10 + ["123", "45"] + ["0"] * 10
        value = smoke.linux_sample("Name: anything\nVmRSS: 17 kB\nVmHWM: 21 kB\nVmSize: 999999 kB\n",
                                   "1 (some ) tricky (name)) " + " ".join(fields), 100)
        self.assertEqual(value["primary_bytes"], 17 * 1024)
        self.assertEqual(value["peak_rss_bytes"], 21 * 1024)
        self.assertEqual(value["user_seconds"], 1.23)
        self.assertEqual(value["kernel_seconds"], .45)
        self.assertEqual(value["primary_metric"], "linux_rss")

    def test_missing_linux_metrics_fail(self):
        with self.assertRaises(smoke.Problem):
            smoke.linux_sample("VmRSS: 1 kB", "1 (x) S", 100)

    def test_cpu_math_not_wall_or_multicore_normalized(self):
        change = smoke.delta(metric(), metric(wall=3, user=4, kernel=2, memory=5))
        self.assertEqual(change["cpu_seconds"], 4)
        self.assertEqual(change["cpu_percent"], 200)
        self.assertEqual(change["memory_delta_bytes"], -5)

    def test_missing_decreasing_invalid_metrics(self):
        for key in ("user_seconds", "kernel_seconds", "wall_seconds", "peak_rss_bytes"):
            with self.subTest(key=key), self.assertRaises(smoke.Problem):
                value = metric()
                value[key] -= 1
                smoke.validate_sample(value, metric())
        for key in metric():
            value = metric()
            del value[key]
            with self.subTest(missing=key), self.assertRaises(smoke.Problem):
                smoke.validate_sample(value)
        for bad in (float("nan"), float("inf"), -1):
            with self.assertRaises(smoke.Problem):
                smoke.validate_sample(metric(memory=bad))
        with self.assertRaises(smoke.Problem):
            smoke.delta(metric(), metric())

    @unittest.skipUnless(sys.platform == "win32" or sys.platform.startswith("linux"), "supported OS only")
    def test_actual_process_metrics_and_primary(self):
        before = smoke.sample(os.getpid())
        # Sample actual CPU work, but impose no platform-dependent speed threshold.
        sum(i * i for i in range(100000))
        after = smoke.sample(os.getpid())
        smoke.delta(before, after)
        self.assertGreater(after["primary_bytes"], 0)
        self.assertEqual(after["primary_bytes"], after["private_bytes"] if sys.platform == "win32" else after["rss_bytes"])
        if sys.platform == "win32":
            self.assertIn("peak_commit_bytes", after)

    def test_sampler_stops_and_retains_first_error(self):
        calls, failed = [], threading.Event()
        def reader(_):
            calls.append(1)
            if len(calls) > 2:
                failed.set()
                raise OSError("lost metrics")
            return metric(wall=len(calls))
        sampler = smoke.Sampler(0, .005, reader)
        try:
            self.assertTrue(failed.wait(timeout=5), "sampler never reached error condition")
        finally:
            with self.assertRaisesRegex(smoke.Problem, "lost metrics"):
                sampler.close()
        self.assertFalse(sampler.thread.is_alive())
        self.assertLessEqual(sampler.count, 2)

    def test_memory_ceiling_growth_cpu_idle_and_first_failure(self):
        for flag, phase, after, category in [
            ("--max-memory-mib", "rpc-round", metric(wall=2), "memory_ceiling"),
            ("--max-rpc-growth-mib", "rpc-round", metric(wall=2, memory=50), "memory_growth"),
            ("--max-editor-growth-mib", "editor-prepare", metric(wall=2, memory=50), "memory_growth"),
            ("--max-round-cpu-seconds", "rpc-round", metric(wall=2, user=2), "round_cpu"),
            ("--max-idle-cpu-percent", "rpc-idle", metric(wall=2, user=2), "idle_cpu")]:
            with self.subTest(category=category):
                args = self.args(flag, ".000001")
                folder = self.root / category / phase
                folder.mkdir(parents=True)
                report = smoke.Report(folder, args)
                sampler = type("Sampler", (), {"peak": after["primary_bytes"], "count": 1})()
                with self.assertRaises(smoke.Problem) as error:
                    report.measure(phase, 3, metric(), after, sampler, metric())
                self.assertEqual(error.exception.category, category)
                report.fail(error.exception)
                report.phase, report.round = "cleanup", None
                report.fail(smoke.Problem("cleanup", "later"))
                self.assertEqual(report.data["firstProblemRound"], 3)
                self.assertEqual(report.data["firstProblem"]["category"], category)
                self.assertIn("budget", report.data["firstProblem"])

    def test_rpc_warmup_excludes_growth_cpu_but_not_ceiling(self):
        args, report = self.report("--max-rpc-growth-mib", ".000001", "--max-round-cpu-seconds", ".000001")
        after = metric(wall=2, user=20, memory=100)
        sampler = Mock(peak=100, count=2)
        report.measure("rpc-warmup", None, metric(), after, sampler, metric(), True, 1)
        self.assertIsNone(report.round)
        self.assertEqual(report.warmup_index, 1)
        args.max_memory_mib = .000001
        with self.assertRaises(smoke.Problem) as caught:
            report.measure("rpc-warmup", None, metric(), after, sampler, metric(), True, 1)
        report.fail(caught.exception)
        self.assertEqual(caught.exception.category, "memory_ceiling")
        self.assertIsNone(report.data["firstProblemRound"])
        self.assertEqual(report.data["firstProblem"]["warmup_index"], 1)

    def test_startup_failure_has_no_fabricated_round(self):
        _, report = self.report()
        report.fail(OSError("could not launch"))
        self.assertIsNone(report.data["firstProblemRound"])
        self.assertEqual(report.data["firstProblem"]["phase"], "startup")
        self.assertEqual(report.data["status"], "FAIL")
        self.assertEqual(report.data["live"], {"status": "SKIP", "reason": "no --live"})


class ArgumentsTests(Base):
    def test_defaults_and_usage(self):
        args = self.args()
        self.assertEqual((args.rounds, args.warmup_rounds, args.encoding, args.request_timeout),
                         (20, 2, "both", 25))
        self.usage_error("--case", "all")
        for flag, value in (("--rounds", "0"), ("--rounds", "1001"), ("--sample-interval", "nan"),
                            ("--idle-seconds", "0"), ("--warmup-rounds", "-1")):
            self.usage_error(flag, value)

    def test_request_timeout_is_positive_integer(self):
        args = self.args("--request-timeout", "60")
        self.assertIs(type(args.request_timeout), int)
        self.assertEqual(str(args.request_timeout), "60")
        for value in ("60.0", "1.5", "0", "-1"):
            self.usage_error("--request-timeout", value)

    def test_optional_controller_git_and_sha256(self):
        probe = self.root / "probe"
        probe.write_bytes(b"abc")
        self.assertEqual(smoke.sha256_file(probe),
                         "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        with patch.object(smoke.subprocess, "run", side_effect=FileNotFoundError):
            repo = smoke.controller_repository()
        self.assertIsNone(repo["head"])
        self.assertIsNone(repo["dirty"])
        self.assertIn("not binary build source", repo["scope"])
        with patch.object(smoke.subprocess, "run", side_effect=[Mock(stdout="abc123\n"), Mock(stdout=" M scripts/resource-smoke.py\n")]) as git:
            repo = smoke.controller_repository()
        self.assertEqual(repo["head"], "abc123")
        self.assertTrue(repo["dirty"])
        self.assertTrue(all(c.kwargs["env"]["GIT_OPTIONAL_LOCKS"] == "0" for c in git.call_args_list))

    def test_unknown_binary_and_existing_output_not_overwritten(self):
        self.usage_error("--rpi", str(self.root / "unknown"))
        out = self.root / "out"
        out.mkdir()
        canary = out / "canary"
        canary.write_text("preserve")
        self.usage_error()
        self.assertEqual(canary.read_text(), "preserve")
        self.assertEqual(list(out.iterdir()), [canary])

    def live(self, *more):
        return ["--live", "--api", "openai-completions", "--base-url", "http://127.0.0.1:123/v1",
                "--model", "synthetic", "--api-key-env", "RESOURCE_TEST_KEY", *more]

    def test_live_requires_optin_explicit_values_and_named_key(self):
        self.usage_error("--model", "synthetic")
        self.usage_error("--live")
        with patch.dict(os.environ, {}, clear=True):
            self.usage_error(*self.live())
        with patch.dict(os.environ, {"RESOURCE_TEST_KEY": "test-only-secret"}):
            args = self.args(*self.live())
            self.assertEqual((args.rounds, args.warmup_rounds, args.encoding, args.request_timeout), (5, 0, "snapshot", 60))
            self.usage_error(*self.live("--encoding", "both"))
            self.usage_error(*self.live("--warmup-rounds", "1"))
            self.usage_error(*self.live("--api-key-env", "PATH"))
            self.usage_error(*self.live("--api-key-env", "bad-name"))
            self.usage_error(*self.live("--case", "editor", "--editor-probe", str(self.binary)))
            self.usage_error(*self.live("--api", "openai-responses", "--max-output-tokens", "63"))
            self.assertEqual(self.args(*self.live("--api", "openai-responses", "--max-output-tokens", "64")).max_output_tokens, 64)
            args = self.args(*self.live("--encoding", "both", "--rounds", "2", "--max-requests", "4"))
            self.assertEqual(args.max_requests, 4)

    def test_live_url_security(self):
        with patch.dict(os.environ, {"RESOURCE_TEST_KEY": "test"}):
            for url in ("http://example.com/v1", "https://user:pw@example.com/v1", "https://example.com/?key=secret",
                        "https://example.com/#key", "file:///tmp/x", "https://example.com:bad/v1"):
                self.usage_error(*self.live("--base-url", url))
            self.args(*self.live("--base-url", "https://example.com/v1"))
            self.args(*self.live("--base-url", "http://[::1]:123/v1"))

    def test_isolated_env_config_no_key_on_disk(self):
        key = 'secret +/"雪'
        with patch.dict(os.environ, {"RESOURCE_TEST_KEY": key, "ANTHROPIC_API_KEY": "unrelated", "RPI_OFFLINE": "bad"}):
            args = self.args(*self.live())
            env = smoke.environment(self.root, args, args.base_url, key)
        self.assertEqual(env["RESOURCE_TEST_KEY"], key)
        self.assertNotIn("ANTHROPIC_API_KEY", env)
        self.assertNotIn("RPI_OFFLINE", env)
        self.assertEqual(env["HOME"], str(self.root / "home"))
        models_text = (self.root / "agent/models.json").read_text(encoding="utf-8")
        self.assertNotIn(key, models_text)
        provider = json.loads(models_text)["providers"]["resource-fixture"]
        self.assertEqual(provider["apiKey"], "$RESOURCE_TEST_KEY")
        self.assertEqual(provider["models"][0]["maxTokens"], 256)
        settings = json.loads((self.root / "agent/settings.json").read_text())
        self.assertEqual(settings["retry"], {"enabled": False, "maxRetries": 0})
        self.assertFalse(settings["compaction"]["enabled"])
        self.assertEqual(json.loads((self.root / "agent/auth.json").read_text()), {})

    def test_redaction_variants(self):
        secret = 'hello +/"雪'
        variants = (secret, quote(secret, safe=""), quote_plus(secret), json.dumps(secret)[1:-1],
                    json.dumps(secret, ensure_ascii=False)[1:-1])
        for variant in variants:
            self.assertEqual(smoke.redact("error:" + variant, secret), "error:[REDACTED]")


class ProtocolTests(Base):
    def make_host(self, code, timeout=.8):
        (self.root / "workspace").mkdir(exist_ok=True)
        return smoke.Host([sys.executable, "-u", "-c", code], self.root, dict(os.environ), timeout)

    def test_irrelevant_partial_snapshots_discarded(self):
        host = self.make_host("import json,sys,time\nfor i in range(3000): print(json.dumps({'type':'message_update','data':'x'*100}))\nprint(json.dumps({'type':'ready','ok':True}));sys.stdout.flush();time.sleep(1)")
        try:
            self.assertTrue(host.wait(lambda f: f.get("type") == "ready")["ok"])
            self.assertEqual(host.queue.qsize(), 0)
            self.assertFalse(hasattr(host, "pending"))
            self.assertLessEqual(host.log_bytes, smoke.MAX_LOG)
        finally:
            host.close()
        self.assertIsNotNone(host.proc.poll())
        self.assertTrue(all(not t.is_alive() for t in host.threads))
        self.assertTrue(all(p.closed for p in (host.proc.stdin, host.proc.stdout, host.proc.stderr)))

    def test_oversize_message_and_timeout_cleanup(self):
        for code, category in [("import time;time.sleep(5)", "timeout"),
                               ("import sys,time;sys.stdout.write('x'*(1048576+2));sys.stdout.flush();time.sleep(5)", "protocol")]:
            with self.subTest(category=category):
                host = self.make_host(code, .3)
                try:
                    with self.assertRaises(smoke.Problem) as caught:
                        host.wait(lambda _: True)
                    self.assertEqual(caught.exception.category, category)
                finally:
                    try:
                        host.close()
                    except smoke.Problem:
                        pass
                self.assertIsNotNone(host.proc.poll())
                self.assertFalse(any(t.is_alive() for t in host.threads))

    def test_failed_command_and_eof_not_pass(self):
        code = "import json,sys\nr=json.loads(input());print(json.dumps({'type':'response','id':r['id'],'command':r['type'],'success':False}));sys.stdout.flush()"
        host = self.make_host(code)
        try:
            with self.assertRaises(smoke.Problem):
                host.call("prompt", message="small")
        finally:
            host.close()
        host = self.make_host("raise SystemExit(7)")
        try:
            with self.assertRaises(smoke.Problem) as caught:
                host.wait(lambda _: True)
            self.assertEqual(caught.exception.category, "process")
        finally:
            host.close()

    def test_task_prompt_does_not_supply_the_expected_sum(self):
        prompt = smoke.prompt_for(20, True)
        task = json.loads(prompt[prompt.index("{"):])
        self.assertEqual(task, {"round": 20, "final": True})
        self.assertNotIn("sum", task)
        self.assertNotIn("210", prompt)
        self.assertEqual(smoke.expected(20, True)["sum"], 210)

    def test_native_identity_and_semantic_validation(self):
        args = self.args()
        prompt = smoke.prompt_for(1, True)
        user = {"entryId": "u1", "role": "user", "content": prompt}
        assistant = {"entryId": "a1", "role": "assistant", "provider": "resource-fixture", "model": "synthetic",
                     "api": "openai-completions", "stopReason": "stop",
                     "content": [{"type": "text", "text": json.dumps(smoke.expected(1, True))}]}
        data = {"pendingResume": False, "messages": [user, assistant]}
        self.assertEqual(smoke.validate_answer(data, prompt, 1, True, set(), args), {"u1", "a1"})
        for change in ({"stopReason": "length"}, {"errorMessage": "failed"}, {"provider": "other"},
                       {"entryId": "u1"}, {"content": [{"type": "text", "text": "{}"}]}):
            bad = dict(data, messages=[user, dict(assistant, **change)])
            with self.assertRaises(smoke.Problem) as caught:
                smoke.validate_answer(bad, prompt, 1, True, set(), args)
            self.assertEqual(caught.exception.category, "semantic")
        with self.assertRaises(smoke.Problem):
            smoke.validate_answer(data, prompt, 1, True, {"a1"}, args)

    def test_editor_baseline_before_prepare_warmup_reset_cleanup(self):
        args, report = self.report("--case", "editor", "--editor-probe", str(self.binary),
                                   "--rounds", "2", "--warmup-rounds", "1", "--idle-seconds", ".02")
        real_host = smoke.Host
        created = []
        code = '''import json,sys
print(json.dumps({'type':'ready','ok':True}),flush=True)
for line in sys.stdin:
 r=json.loads(line); t=r['type']
 answer={'type':{'prepare':'prepared','round':'round_complete','reset':'reset_complete','stop':'stopped'}[t],'ok':True}
 if t in ('prepare','round'): answer.update(pasteBytes=1048576,edits=64)
 if t=='round': answer['round']=r['round']
 print(json.dumps(answer),flush=True)
 if t=='stop': break
'''
        def host_factory(command, root, env, timeout, secret):
            self.assertEqual(command, [str(self.binary)])  # no probe flags
            host = real_host([sys.executable, "-u", "-c", code], root, env, timeout, secret)
            created.append(host)
            return host
        with patch.object(smoke, "Host", side_effect=host_factory):
            smoke.run_case(args, report, "editor", None, "")
        metrics = report.data["metrics"]
        self.assertEqual(metrics[0]["phase"], "editor-prepare")
        self.assertIsNone(metrics[0]["round"])
        rounds = [m for m in metrics if m["phase"] == "editor-round"]
        self.assertEqual([m["warmup"] for m in rounds], [False, False])
        self.assertEqual([m["round"] for m in rounds], [1, 2])
        self.assertEqual([m["round"] for m in metrics if m["phase"] == "editor-idle"], [1, 2])
        self.assertEqual(report.data["cases"][0]["warmupRounds"], 0)
        self.assertEqual(report.data["cases"][0]["finalConclusion"],
                         {"status": "NOT_APPLICABLE", "round": None, "answer": None})
        self.assertEqual(report.data["cases"][0]["status"], "PASS")
        self.assertTrue(created[0].closed)
        self.assertEqual(created[0].proc.returncode, 0)
        events = [json.loads(line) for line in (report.root / "metrics.jsonl").read_text().splitlines()]
        self.assertEqual(events[0]["phase"], "editor-startup")
        self.assertIn("sample", events[-2])
        self.assertEqual(events[-2]["phase"], "editor-reset")
        self.assertEqual(events[-1]["phase"], "editor-complete")
        summary = report.data["cases"][0]
        self.assertGreater(summary["totalWallSeconds"], 0)
        self.assertEqual(summary["actualCpuSeconds"], summary["userCpuSeconds"] + summary["kernelCpuSeconds"])
        self.assertIn("cpu_percent", summary["idle"])

    def test_editor_prepare_growth_not_hidden_by_requested_warmup(self):
        args, report = self.report("--case", "editor", "--editor-probe", str(self.binary),
                                   "--warmup-rounds", "2", "--max-editor-growth-mib", ".000001")
        host = Mock()
        host.proc.pid = 123
        host.wait.side_effect = [{"type": "ready", "ok": True},
            {"type": "prepared", "ok": True, "pasteBytes": smoke.PASTE_BYTES, "edits": smoke.EDITS}]
        sampler = Mock(peak=100, count=2)
        sampler.take.side_effect = [metric(wall=1, memory=10), metric(wall=2, memory=50)]
        with patch.object(smoke, "Host", return_value=host), patch.object(smoke, "Sampler", return_value=sampler):
            with self.assertRaises(smoke.Problem) as caught:
                smoke.run_case(args, report, "editor", None, "")
        self.assertEqual(caught.exception.category, "memory_growth")
        self.assertEqual(report.data["firstProblem"]["phase"], "editor-prepare")
        self.assertIsNone(report.data["firstProblemRound"])
        self.assertEqual(report.data["cases"][0]["warmupRounds"], 0)
        self.assertEqual(host.send.call_args_list[0].args[0]["type"], "prepare")
        self.assertEqual(host.send.call_count, 1)
        sampler.close.assert_called_once()
        host.close.assert_called_once()

    def test_rpc_baseline_reset_and_first_idle_failure(self):
        args, report = self.report("--rounds", "3", "--warmup-rounds", "1",
                                   "--max-rpc-growth-mib", "2", "--max-round-cpu-seconds", "1")
        state = {"index": 0, "wall": 0, "cpu": 0}
        host = Mock()
        host.proc.pid = 123
        host.wait.return_value = {"semantics": {"messageUpdates": "snapshot"}}
        def call(kind, **fields):
            if kind == "prompt":
                state["index"] += 1
                state["prompt"] = fields["message"]
                state["cpu"] += 10 if state["index"] == 1 else .01
                return {"accepted": True}
            if kind == "get_messages":
                i = state["index"]
                return {"pendingResume": False, "messages": [
                    {"entryId": f"u{i}", "role": "user", "content": state["prompt"]},
                    {"entryId": f"a{i}", "role": "assistant", "provider": "resource-fixture",
                     "model": "synthetic", "api": "openai-completions", "stopReason": "stop",
                     "content": [{"type": "text", "text": json.dumps(smoke.expected(i, i == 4))}]}]}
        host.call.side_effect = call
        sampler = Mock(count=1, peak=101 * smoke.MIB)
        def take():
            state["wall"] += 1
            memory = (100 if state["index"] else 10) * smoke.MIB
            if state["index"] > 1:
                memory += smoke.MIB
            return metric(wall=state["wall"], user=state["cpu"], memory=memory,
                          ) | {"peak_rss_bytes": 101 * smoke.MIB}
        sampler.take.side_effect = take
        provider = Mock(calls=4)
        provider.server.server_port = 1
        idle_windows = []
        def idle(_):
            idle_windows.append(state["index"] - 1)
            if len(idle_windows) == 2:
                state["cpu"] += 2  # idle spin begins at measured round 2
        with patch.object(smoke, "Host", return_value=host), patch.object(smoke, "Sampler", return_value=sampler), \
             patch.object(smoke, "Provider", return_value=provider), patch.object(smoke.time, "sleep", side_effect=idle):
            with self.assertRaises(smoke.Problem) as caught:
                smoke.run_case(args, report, "rpc", "snapshot", "")
        self.assertEqual(caught.exception.category, "idle_cpu")
        self.assertEqual(idle_windows, [1, 2])
        self.assertEqual(report.data["firstProblemRound"], 2)
        self.assertEqual(report.data["firstProblem"]["phase"], "rpc-snapshot-idle")
        self.assertEqual(report.data["cases"][0]["finalConclusion"],
                         {"status": "FAIL", "round": 2, "answer": None})
        measured = [m for m in report.data["metrics"] if m["phase"] == "rpc-snapshot-round"]
        self.assertEqual([m["baseline_delta_bytes"] for m in measured], [smoke.MIB, smoke.MIB])
        sampler.close.assert_called_once()
        host.close.assert_called_once()
        provider.close.assert_called_once()

    def test_main_initial_probe_hash_and_case_warmups(self):
        host = Mock()
        host.wait.return_value = {"text": "rpi test"}
        argv = self.argv("--case", "all", "--editor-probe", str(self.binary))
        with patch.object(smoke, "Host", return_value=host), patch.object(smoke, "run_case"), \
             patch.object(smoke, "controller_repository", return_value={"head": None, "dirty": None}), \
             contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(smoke.main(argv), 0)
        meta = json.loads((self.root / "out/results.json").read_text())["metadata"]
        self.assertEqual(meta["editorProbe"]["sha256"], smoke.sha256_file(self.binary))
        self.assertEqual(meta["workload"]["warmupRoundsByCase"], {"rpc": 2, "editor": 0})
        self.assertIsNone(meta["workload"]["warmupRounds"])
        self.assertIn("untested", meta["validationScope"]["providerCoverage"])

    def test_running_error_and_cleanup_error_preserves_first(self):
        args, report = self.report("--rounds", "1", "--warmup-rounds", "0")
        class BrokenHost:
            def __init__(self, *_):
                raise smoke.Problem("process", "launch failed")
        provider = Mock()
        provider.server.server_port = 1
        provider.close.side_effect = smoke.Problem("cleanup", "later cleanup failure")
        with patch.object(smoke, "Host", BrokenHost), patch.object(smoke, "Provider", return_value=provider), self.assertRaises(smoke.Problem):
            smoke.run_case(args, report, "rpc", "snapshot", "")
        provider.close.assert_called_once()
        self.assertEqual(report.data["status"], "FAIL")
        self.assertEqual(report.data["cases"][0]["status"], "FAIL")
        self.assertEqual(report.data["cases"][0]["finalConclusion"]["status"], "FAIL")
        self.assertIsNone(report.data["firstProblemRound"])
        self.assertEqual(report.data["firstProblem"]["category"], "process")

    def test_log_cap_control_retention_and_settlement(self):
        code = "import json,sys,time\nfor i in range(10000): print(json.dumps({'type':'message_update','data':'x'*600}))\nprint(json.dumps({'type':'agent_settled','outcome':'failed'}));sys.stdout.flush();time.sleep(1)"
        host = self.make_host(code, 3)
        try:
            with self.assertRaises(smoke.Problem) as caught:
                host.settlement()
            self.assertEqual(caught.exception.category, "semantic")
            self.assertLessEqual(host.log_bytes, smoke.MAX_LOG)
            self.assertGreater(host.log_bytes, smoke.MAX_LOG - 1000)
            with self.assertRaises(smoke.Problem):
                host.send({"message": "x" * 4096})
        finally:
            host.close()
        host = self.make_host("import json,sys,time\nfor i in range(100): print(json.dumps({'type':'ready'}))\nsys.stdout.flush();time.sleep(1)")
        try:
            deadline = time.monotonic() + 5
            while host.error is None and time.monotonic() < deadline:
                time.sleep(.01)
            self.assertLessEqual(host.queue.qsize(), 32)
            self.assertIsInstance(host.error, smoke.Problem)
        finally:
            with self.assertRaises(smoke.Problem):
                host.close()

    def test_main_running_failure_not_pass(self):
        with patch.object(smoke, "Host", side_effect=OSError("launch failed")), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(smoke.main(self.argv()), 1)
        report = json.loads((self.root / "out/results.json").read_text())
        self.assertEqual(report["status"], "FAIL")
        self.assertEqual(report["firstProblem"]["category"], "running")

    def test_unsupported_skip(self):
        with patch.object(smoke.sys, "platform", "unsupported"):
            self.assertEqual(smoke.main(self.argv()), 77)
        self.assertEqual(json.loads((self.root / "out/results.json").read_text())["status"], "SKIP")


# Optional integration uses an already-built CLI only, never cargo and never a real provider.
ROOT = Path(__file__).resolve().parent.parent
REAL_BINARY = Path(os.environ.get("RPI_RESOURCE_SMOKE_BINARY", ROOT / "target/debug" / ("rpi.exe" if os.name == "nt" else "rpi")))


@unittest.skipUnless(REAL_BINARY.is_file(), "no prebuilt CLI; set RPI_RESOURCE_SMOKE_BINARY")
class IntegrationTests(Base):
    def test_offline_real_cli_both_encodings(self):
        args, report = self.report("--rpi", str(REAL_BINARY), "--rounds", "2", "--warmup-rounds", "1",
                                   "--idle-seconds", ".05", "--request-timeout", "60")
        for encoding in ("compact-deltas", "snapshot"):
            smoke.run_case(args, report, "rpc", encoding, "")
        self.assertEqual(len(report.data["cases"]), 2)
        self.assertTrue(all(c["status"] == "PASS" for c in report.data["cases"]))
        events = [json.loads(line) for line in (report.root / "metrics.jsonl").read_text().splitlines()]
        for encoding in ("compact-deltas", "snapshot"):
            label = "rpc-" + encoding
            self.assertEqual([m["round"] for m in report.data["metrics"] if m["phase"] == label + "-idle"], [1, 2])
            warmup = next(m for m in report.data["metrics"] if m["phase"] == label + "-warmup")
            self.assertIsNone(warmup["round"])
            self.assertEqual(warmup["warmup_index"], 1)
            answers = [e for e in events if e.get("event") == "completed-answer" and e["phase"].startswith(label)]
            self.assertEqual(len(answers), 3)
            self.assertEqual([e["round"] for e in answers], [None, 1, 2])
            self.assertTrue(all(len(e["entryIds"]) == 2 and e["usage"] for e in answers))
            sizes = [e["cumulativeObservedMessageJsonBytes"] for e in answers]
            self.assertTrue(0 < sizes[0] < sizes[1] < sizes[2])
            self.assertTrue(any(e.get("event") == "baseline" and e["phase"] == label + "-warmup-complete" for e in events))
            summary = next(c for c in report.data["cases"] if c["case"] == label)
            self.assertEqual(summary["idle"]["round"], 2)
            self.assertEqual(summary["cumulativeObservedMessageJsonBytes"], sizes[-1])
            self.assertEqual(summary["finalConclusion"], {"status": "PASS", "round": 2, "answer": smoke.expected(3, True)})

    def test_live_mode_uses_only_local_fake_endpoint(self):
        provider = smoke.Provider(2, expected_api_key="local-secret-not-real", expected_max_tokens=256)
        try:
            with patch.dict(os.environ, {"RESOURCE_TEST_KEY": "local-secret-not-real"}):
                args, report = self.report("--rpi", str(REAL_BINARY), "--live", "--api", "openai-completions",
                    "--base-url", f"http://127.0.0.1:{provider.server.server_port}/v1", "--model", "synthetic",
                    "--api-key-env", "RESOURCE_TEST_KEY", "--rounds", "2", "--idle-seconds", ".05")
                smoke.run_case(args, report, "rpc", "snapshot", "local-secret-not-real")
            self.assertEqual(provider.calls, 2)
            self.assertEqual(report.data["cases"][0]["status"], "PASS")
            for path in report.root.rglob("*"):
                if path.is_file():
                    self.assertNotIn("local-secret-not-real", path.read_text(encoding="utf-8"))
        finally:
            provider.close()


if __name__ == "__main__":
    unittest.main()
