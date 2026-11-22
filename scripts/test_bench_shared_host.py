#!/usr/bin/env python3
"""Offline regressions. Optional real release probe: RPI_SHARED_HOST_BINARY=/abs/path.

python3 -B -m unittest discover -s scripts -p test_bench_shared_host.py -v
"""
import asyncio
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest import mock

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location(
    "bench_shared_host", Path(__file__).with_name("bench-shared-host.py"))
bench = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bench)


class ArgumentTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.binary = self.root / "probe"
        self.binary.write_text("#!/bin/sh\nexit 0\n")
        self.binary.chmod(0o755)
        self.argv = ["--binary", str(self.binary), "--output-root", str(self.root / "output")]

    def rejected(self, extra):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as error:
            bench.parse_args(self.argv + extra)
        self.assertEqual(error.exception.code, 2)

    def test_defaults_and_counts(self):
        args = bench.parse_args(self.argv)
        self.assertEqual((args.agents, args.mode, args.tool_kind), (8, "tool", "async"))
        self.assertIsNone(args.max_blocking_threads)
        self.assertEqual(args.stagger_ms, 0)
        self.assertEqual(bench.parse_args(self.argv + ["--max-blocking-threads", "32"]).max_blocking_threads, 32)
        for extra in (["--agents", "0"], ["--agents", "10001"], ["--workers", "0"],
                      ["--max-blocking-threads", "0"], ["--max-blocking-threads", "-1"],
                      ["--stagger-ms", "-1"], ["--history-entries", "-1"],
                      ["--context-bytes", "-1"], ["--timeout", "0"]):
            self.rejected(extra)

    def test_existing_root_and_dangling_symlink_refused(self):
        output = self.root / "output"
        output.mkdir()
        (output / "keep").write_text("unchanged")
        self.rejected([])
        self.assertEqual((output / "keep").read_text(), "unchanged")
        (output / "keep").unlink()
        output.rmdir()
        output.symlink_to(self.root / "missing")
        self.rejected([])

    def test_missing_output_parent_refused(self):
        self.rejected(["--output-root", str(self.root / "missing" / "output")])

    def test_non_executable_refused(self):
        self.binary.chmod(0o644)
        self.rejected([])

    def test_cgroup_metrics_and_nonempty_refusal(self):
        group = self.root / "group"
        group.mkdir()
        for name, value in {"cgroup.procs": "", "cgroup.events": "populated 0\nfrozen 0\n",
                            "memory.current": "0", "memory.peak": "0",
                            "memory.stat": "anon 8\nfile 2\n", "memory.events": "oom 0\noom_kill 0\n"}.items():
            (group / name).write_text(value)
        with mock.patch.object(bench.platform, "system", return_value="Linux"):
            self.assertEqual(bench.parse_args(self.argv + ["--cgroup", str(group)]).cgroup, group.resolve())
            self.assertEqual(bench.sample_cgroup(group)["memory.stat"], {"anon": 8, "file": 2})
            (group / "memory.peak").write_text("12")
            self.rejected(["--cgroup", str(group)])
            (group / "memory.peak").write_text("0")
            (group / "cgroup.events").write_text("populated 1\n")
            self.rejected(["--cgroup", str(group)])
            (group / "cgroup.events").write_text("populated 0\n")
            (group / "cgroup.procs").write_text("123\n")
            self.rejected(["--cgroup", str(group)])

    @unittest.skipUnless(bench.platform.system() == "Darwin", "Darwin libproc ABI")
    def test_darwin_live_resource_counters(self):
        value = bench.darwin_resources(os.getpid())
        self.assertGreaterEqual(value["hostThreads"], 1)
        self.assertGreaterEqual(value["hostFds"], 3)
        # Footprint counts compressed pages; RSS alone can report sub-megabyte
        # values for a live interpreter.
        self.assertGreaterEqual(value["hostFootprintBytes"], 4 * 1024 * 1024)
        self.assertGreaterEqual(value["hostResidentBytes"], 1024 * 1024)

    def test_rss_sum_is_not_reported_as_physical_memory(self):
        table = SimpleNamespace(stdout="100 100 10\n101 100 20\n102 999 99\n")
        with mock.patch.object(bench.subprocess, "run", return_value=table), \
                mock.patch.object(bench.platform, "system", return_value="Darwin"):
            value = bench.sample_process(100)
        self.assertEqual(value["hostRssBytes"], 10 * 1024)
        self.assertEqual(value["processGroupRssSumBytes"], 30 * 1024)
        self.assertEqual(value["processGroupProcesses"], 2)
        self.assertIn("double-count", value["method"])


def request(agent=0, second=False):
    messages = [{"role": "user", "content": f"probe-agent:{agent}\nprompt"}]
    if second:
        messages.append({"role": "tool", "tool_call_id": f"call-{agent}",
                         "content": f"tool completed {agent}"})
    return {"model": "probe", "stream": True, "messages": messages}


class FixtureTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.fixture = bench.FixtureServer(2, "tool", 4096)
        self.base = await self.fixture.start()
        self.addAsyncCleanup(self.fixture.close)

    async def post(self, value, declared_length=None):
        port = int(self.base.split(":")[-1].split("/")[0])
        reader, writer = await asyncio.open_connection("127.0.0.1", port)
        body = json.dumps(value).encode()
        length = len(body) if declared_length is None else declared_length
        writer.write(f"POST /v1/chat/completions HTTP/1.1\r\nContent-Length: {length}\r\n\r\n".encode() + body)
        await writer.drain()
        response = await reader.read()
        writer.close()
        await writer.wait_closed()
        return response

    async def test_counts_complete_request_bodies_and_holds_all(self):
        first = asyncio.create_task(self.post(request(0)))
        second = asyncio.create_task(self.post(request(1)))
        try:
            await asyncio.wait_for(self.fixture.ready.wait(), 2)
            self.assertEqual((self.fixture.active, self.fixture.peak), (2, 2))
            self.assertFalse(first.done())
            self.assertFalse(second.done())
            self.fixture.release.set()
            replies = await asyncio.gather(first, second)
            self.assertTrue(all(b"probe_barrier" in response for response in replies))
            await asyncio.gather(self.post(request(0, True)), self.post(request(1, True)))
            self.assertEqual(self.fixture.summary()["toolResults"], 2)
            self.assertEqual(self.fixture.errors, [])
        finally:
            first.cancel()
            second.cancel()
            await asyncio.gather(first, second, return_exceptions=True)

    async def test_wrong_agent_tool_result_refused(self):
        value = request(0, True)
        value["messages"][-1]["tool_call_id"] = "call-1"
        response = await self.post(value)
        self.assertIn(b"400 Bad Request", response)
        self.assertEqual(self.fixture.requests, 0)
        self.assertIn("cross-agent", self.fixture.errors[0])

    async def test_oversized_request_refused_without_reading_declared_body(self):
        response = await asyncio.wait_for(self.post({}, declared_length=5000), 2)
        self.assertIn(b"400 Bad Request", response)
        self.assertEqual(self.fixture.requests, 0)

    async def test_duplicate_request_is_not_a_new_agent(self):
        self.fixture.release.set()
        await self.post(request(0))
        response = await self.post(request(0))
        self.assertIn(b"400 Bad Request", response)
        self.assertEqual(len(self.fixture.first), 1)
        self.assertIn("duplicate", self.fixture.errors[0])


class ResultTests(unittest.TestCase):
    def case(self):
        args = SimpleNamespace(mode="tool", agents=2, tool_kind="async")
        server = SimpleNamespace(first={0, 1}, final={0, 1}, tool_results={0, 1},
                                 requests=4, peak=2, errors=[])
        phases = [{"phase": "ready", "agents": 2},
                  {"phase": "tools_held", "entered": 2, "live": 2},
                  {"phase": "settled", "agents": 2, "completed": 2, "toolEntered": 2, "toolPeak": 2},
                  {"phase": "closed", "agents": 0},
                  {"phase": "complete", "success": True}]
        return args, phases, server

    def test_complete_success(self):
        bench.validate_result(*self.case())

    def test_objects_are_not_concurrency(self):
        args, phases, server = self.case()
        server.peak = 1
        with self.assertRaisesRegex(ValueError, "model concurrency"):
            bench.validate_result(args, phases, server)
        server.peak = 2
        phases[2]["toolPeak"] = 1
        with self.assertRaisesRegex(ValueError, "tool concurrency"):
            bench.validate_result(args, phases, server)

    def test_missing_terminal_or_persistence_completion_fails(self):
        args, phases, server = self.case()
        with self.assertRaises(ValueError):
            bench.validate_result(args, phases[:-1], server)
        phases[2]["completed"] = 1
        with self.assertRaisesRegex(ValueError, "not all agents"):
            bench.validate_result(args, phases, server)


class ControllerTests(unittest.IsolatedAsyncioTestCase):
    async def test_setup_progress_is_logged_but_not_counted_as_execution(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            script = root / "progress-probe"
            script.write_text(f"#!{sys.executable}\nimport json, sys\n"
                "def emit(value): print(json.dumps(value), flush=True)\n"
                "emit({'phase':'setup_progress','agentsCreated':1})\n"
                "emit({'phase':'ready','agents':1})\n"
                "sys.stdin.readline()\n"
                "emit({'phase':'settled','agents':1,'completed':0,'toolEntered':0,'toolPeak':0})\n"
                "sys.stdin.readline()\n"
                "emit({'phase':'closed','agents':1})\n"
                "sys.stdin.readline()\n"
                "emit({'phase':'complete','success':True})\n")
            script.chmod(0o755)
            args = bench.parse_args(["--binary", str(script), "--output-root", str(root / "out"),
                                     "--agents", "1", "--mode", "idle", "--hold-seconds", "1"])
            result = await bench.run(args)
            self.assertTrue(result["success"], result.get("error"))
            self.assertEqual([p["phase"] for p in result["phases"]],
                             ["ready", "settled", "closed", "complete"])
            logged = [json.loads(line)["phase"] for line in (root / "out/phases.jsonl").read_text().splitlines()]
            self.assertEqual(logged, ["setup_progress", "ready", "settled", "closed", "complete"])
            if bench.platform.system() == "Darwin":
                self.assertGreaterEqual(result["sampledPeakHostThreads"], 1)
                self.assertGreaterEqual(result["sampledPeakHostFds"], 3)

    async def test_signal_denial_only_suppressed_for_absent_group(self):
        process = SimpleNamespace(pid=123, wait=mock.AsyncMock(return_value=0))
        with mock.patch.object(bench.os, "killpg", side_effect=PermissionError("denied")), \
                mock.patch.object(bench, "sample_process", return_value={"processGroupProcesses": 0}):
            await bench.stop_process_group(process)
        with mock.patch.object(bench.os, "killpg", side_effect=PermissionError("denied")), \
                mock.patch.object(bench, "sample_process", return_value={"processGroupProcesses": 1}):
            with self.assertRaises(PermissionError):
                await bench.stop_process_group(process)

    async def test_cleanup_error_does_not_erase_failure_report(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            script = root / "exited-probe"
            script.write_text("#!/bin/sh\nexit 0\n")
            script.chmod(0o755)
            args = bench.parse_args(["--binary", str(script), "--output-root", str(root / "out"),
                                     "--agents", "1", "--mode", "idle"])
            with mock.patch.object(bench, "stop_process_group", side_effect=PermissionError("denied")):
                result = await bench.run(args)
            self.assertFalse(result["success"])
            saved = json.loads((root / "out/results.json").read_text())
            self.assertIn("denied", saved["cleanupErrors"][0])
            self.assertIn("invalid probe phases", saved["error"])

    async def test_timeout_cleans_entire_process_group_and_saves_failure(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            pid_file = root / "descendant.pid"
            script = root / "stuck-probe"
            script.write_text(f"#!{sys.executable}\nimport subprocess, sys, time\n"
                              "from pathlib import Path\n"
                              "p = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])\n"
                              f"Path({str(pid_file)!r}).write_text(str(p.pid))\n"
                              "time.sleep(60)\n")
            script.chmod(0o755)
            args = bench.parse_args(["--binary", str(script), "--output-root", str(root / "out"),
                                     "--agents", "1", "--timeout", "1", "--hold-seconds", "1"])
            result = await bench.run(args)
            self.assertFalse(result["success"])
            self.assertIn("Timeout", result["error"])
            self.assertFalse(json.loads((root / "out/results.json").read_text())["success"])
            samples = (root / "out/memory.jsonl").read_text().splitlines()
            self.assertGreater(len(samples), 0)
            self.assertIn("fixtureFirstRequests", json.loads(samples[0]))
            pid = int(pid_file.read_text())
            for _ in range(30):
                status = subprocess.run(["ps", "-p", str(pid), "-o", "stat="], capture_output=True, text=True)
                if not status.stdout.strip() or status.stdout.strip().startswith("Z"):
                    break
                await asyncio.sleep(0.1)
            else:
                self.fail("descendant survived controller timeout")

    @unittest.skipUnless(os.environ.get("RPI_SHARED_HOST_BINARY"), "opt-in release binary smoke")
    async def test_real_probe_modes(self):
        with tempfile.TemporaryDirectory() as temp:
            for mode, kind in [("empty", "async"), ("idle", "async"), ("model", "async"),
                               ("raw", "async"), ("tool", "async"), ("tool", "bash")]:
                args = bench.parse_args(["--binary", os.environ["RPI_SHARED_HOST_BINARY"],
                    "--output-root", str(Path(temp) / f"{mode}-{kind}"), "--agents", "2",
                    "--mode", mode, "--tool-kind", kind, "--history-entries", "2",
                    "--max-blocking-threads", "2", "--hold-seconds", "1", "--timeout", "45"])
                result = await bench.run(args)
                self.assertTrue(result["success"], result.get("error"))
                ready = next(p for p in result["phases"] if p["phase"] == "ready")
                self.assertEqual(ready["maxBlockingThreads"], 2)


if __name__ == "__main__":
    unittest.main()
