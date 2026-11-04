#!/usr/bin/env python3
"""Isolated, stdlib-only process resource regression. No real provider without --live.

Exit codes: PASS 0, FAIL 1, usage 2, unsupported platform SKIP 77.
Memory is Windows PrivateUsage / Linux VmRSS, never virtual address space.
CPU percentages are single-core percentages (and can exceed 100).
"""
import argparse
import ctypes
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import math
import os
from pathlib import Path
import queue
import re
import subprocess
import sys
import threading
import time
from urllib.parse import quote, quote_plus, urlsplit
import uuid

MIB = 1024 * 1024
MAX_LINE = 1024 * 1024
MAX_LOG = 4 * MIB
PASTE_BYTES = MIB
EDITS = 64


class Problem(Exception):
    def __init__(self, category, actual, budget=None):
        self.category, self.actual, self.budget = category, actual, budget
        super().__init__(str(actual))


def require(ok, message, category="protocol"):
    if not ok:
        raise Problem(category, message)


def redact(value, secret=""):
    value = str(value)
    if secret:
        for variant in sorted({secret, quote(secret, safe=""), quote_plus(secret),
                               json.dumps(secret)[1:-1], json.dumps(secret, ensure_ascii=False)[1:-1]},
                              key=len, reverse=True):
            value = value.replace(variant, "[REDACTED]")
    return value


def dump(path, value, secret=""):
    path.write_text(redact(json.dumps(value, indent=2, ensure_ascii=False), secret), encoding="utf-8")


def linux_sample(status, stat, ticks):
    # comm is parenthesized and may itself contain spaces and ')' characters.
    end = stat.rfind(")")
    require(end > stat.find("(") >= 0, "invalid /proc stat", "metrics")
    fields = stat[end + 1:].split()  # first element is field 3 (state)
    values = {}
    for line in status.splitlines():
        key, _, rest = line.partition(":")
        if key in ("VmRSS", "VmHWM"):
            parts = rest.split()
            require(len(parts) == 2 and parts[1] == "kB", "invalid /proc memory unit", "metrics")
            values[key] = int(parts[0]) * 1024
    require("VmRSS" in values and "VmHWM" in values and len(fields) >= 13 and ticks > 0,
            "missing /proc metrics", "metrics")
    return dict(primary_bytes=values["VmRSS"], rss_bytes=values["VmRSS"],
                peak_rss_bytes=values["VmHWM"], user_seconds=int(fields[11]) / ticks,
                kernel_seconds=int(fields[12]) / ticks, primary_metric="linux_rss")


def windows_sample(pid):
    from ctypes import wintypes as w
    size_t = ctypes.c_size_t

    class Counters(ctypes.Structure):
        _fields_ = [("cb", w.DWORD), ("PageFaultCount", w.DWORD)] + [
            (name, size_t) for name in ("PeakWorkingSetSize", "WorkingSetSize",
            "QuotaPeakPagedPoolUsage", "QuotaPagedPoolUsage", "QuotaPeakNonPagedPoolUsage",
            "QuotaNonPagedPoolUsage", "PagefileUsage", "PeakPagefileUsage", "PrivateUsage")]

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    psapi = ctypes.WinDLL("psapi", use_last_error=True)
    kernel.OpenProcess.argtypes = [w.DWORD, w.BOOL, w.DWORD]
    kernel.OpenProcess.restype = w.HANDLE
    kernel.CloseHandle.argtypes = [w.HANDLE]
    kernel.CloseHandle.restype = w.BOOL
    kernel.GetProcessTimes.argtypes = [w.HANDLE] + [ctypes.POINTER(w.FILETIME)] * 4
    kernel.GetProcessTimes.restype = w.BOOL
    psapi.GetProcessMemoryInfo.argtypes = [w.HANDLE, ctypes.POINTER(Counters), w.DWORD]
    psapi.GetProcessMemoryInfo.restype = w.BOOL
    handle = kernel.OpenProcess(0x0400 | 0x0010, False, pid)
    if not handle:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        counters = Counters()
        counters.cb = ctypes.sizeof(counters)
        if not psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
            raise ctypes.WinError(ctypes.get_last_error())
        creation, exit_time, kernel_time, user_time = (w.FILETIME() for _ in range(4))
        if not kernel.GetProcessTimes(handle, ctypes.byref(creation), ctypes.byref(exit_time),
                                      ctypes.byref(kernel_time), ctypes.byref(user_time)):
            raise ctypes.WinError(ctypes.get_last_error())
        def seconds(t):
            return ((t.dwHighDateTime << 32) | t.dwLowDateTime) / 10000000
        return dict(primary_metric="windows_private_commit", primary_bytes=counters.PrivateUsage,
                    private_bytes=counters.PrivateUsage, rss_bytes=counters.WorkingSetSize,
                    peak_rss_bytes=counters.PeakWorkingSetSize,
                    peak_commit_bytes=counters.PeakPagefileUsage,
                    user_seconds=seconds(user_time), kernel_seconds=seconds(kernel_time))
    finally:
        kernel.CloseHandle(handle)


def sample(pid):
    if sys.platform == "win32":
        value = windows_sample(pid)
    elif sys.platform.startswith("linux"):
        proc = Path("/proc") / str(pid)
        value = linux_sample((proc / "status").read_text(), (proc / "stat").read_text(),
                             os.sysconf("SC_CLK_TCK"))
    else:
        raise Problem("unsupported", sys.platform)
    value["wall_seconds"] = time.perf_counter()
    validate_sample(value)
    return value


def validate_sample(value, previous=None):
    for key in ("primary_bytes", "rss_bytes", "peak_rss_bytes", "user_seconds", "kernel_seconds", "wall_seconds"):
        require(key in value and isinstance(value[key], (int, float)) and
                math.isfinite(value[key]) and value[key] >= 0, "missing/invalid metric: " + key, "metrics")
    require(value.get("primary_metric") in ("windows_private_commit", "linux_rss"),
            "missing primary metric label", "metrics")
    primary_field = "private_bytes" if value["primary_metric"] == "windows_private_commit" else "rss_bytes"
    require(value.get(primary_field) == value["primary_bytes"], "primary metric/value mismatch", "metrics")
    if previous:
        require(value["primary_metric"] == previous["primary_metric"], "primary metric changed", "metrics")
        monotonic = ("user_seconds", "kernel_seconds", "wall_seconds", "peak_commit_bytes")
        # Linux /proc VmHWM (like VmRSS) is an approximate, inconsistent snapshot,
        # not a monotonic counter (proc_pid_status(5); fs/proc/task_mmu.c:task_mem).
        # Keep the raw value; the memory gate uses Sampler.peak = max(primary_bytes).
        if value["primary_metric"] == "windows_private_commit":
            monotonic += ("peak_rss_bytes",)
        for key in monotonic:
            if key in previous:
                require(key in value and value[key] >= previous[key], "retrograde metric: " + key, "metrics")


def delta(before, after):
    validate_sample(after, before)
    wall = after["wall_seconds"] - before["wall_seconds"]
    user = after["user_seconds"] - before["user_seconds"]
    kernel = after["kernel_seconds"] - before["kernel_seconds"]
    require(wall > 0, "nonpositive measurement window", "metrics")
    return dict(wall_seconds=wall, user_cpu_seconds=user, kernel_cpu_seconds=kernel,
                cpu_seconds=user + kernel, cpu_percent=100 * (user + kernel) / wall,
                memory_delta_bytes=after["primary_bytes"] - before["primary_bytes"])


class Sampler:
    """Bounded retention: latest sample, peak primary, count, and first error only."""
    def __init__(self, pid, interval, reader=sample):
        self.pid, self.interval, self.reader = pid, interval, reader
        self.lock, self.stop_event = threading.Lock(), threading.Event()
        self.latest, self.error, self.peak, self.count = None, None, 0, 0
        self.take()
        self.thread = threading.Thread(target=self.run, name="resource-sampler", daemon=True)
        self.thread.start()

    def take(self):
        with self.lock:
            if self.error:
                raise self.error
            try:
                value = self.reader(self.pid)
                validate_sample(value, self.latest)
                self.latest = value
                self.peak = max(self.peak, value["primary_bytes"])
                self.count += 1
                return dict(value)
            except Exception as error:
                self.error = error if isinstance(error, Problem) else Problem("metrics", str(error))
                raise self.error

    def run(self):
        while not self.stop_event.wait(self.interval):
            try:
                self.take()
            except Exception:
                break

    def close(self):
        self.stop_event.set()
        self.thread.join(timeout=5)
        require(not self.thread.is_alive(), "sampler thread leaked", "cleanup")
        if self.error:
            raise self.error


class Host:
    """Owns only its Popen PID. Drop partial snapshots, never retain an event backlog."""
    def __init__(self, command, root, env, timeout, secret="", raw=False):
        self.timeout, self.secret, self.raw = timeout, secret, raw
        self.queue = queue.Queue(maxsize=32)
        self.error, self.counter, self.closed = None, 0, False
        self.log_lock, self.log_bytes = threading.Lock(), 0
        self.log = (root / "process.jsonl").open("w", encoding="utf-8")
        try:
            self.proc = subprocess.Popen(command, cwd=root / "workspace", env=env,
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        except BaseException:
            self.log.close()
            raise
        self.threads = [threading.Thread(target=self.pump, args=(pipe, name), daemon=True)
                        for pipe, name in ((self.proc.stdout, "stdout"), (self.proc.stderr, "stderr"))]
        for thread in self.threads:
            thread.start()

    def record(self, name, line):
        data = json.dumps({"pipe": name, "line": redact(line, self.secret)}, ensure_ascii=False) + "\n"
        with self.log_lock:
            if self.log_bytes + len(data.encode()) <= MAX_LOG:
                self.log.write(data)
                self.log.flush()
                self.log_bytes += len(data.encode())

    def pump(self, pipe, name):
        try:
            while True:
                line = pipe.readline(MAX_LINE + 1)
                if not line:
                    break
                require(len(line) <= MAX_LINE, "oversize process line", "protocol")
                line = line.decode("utf-8", errors="strict").rstrip("\r\n")
                self.record(name, line)
                if name == "stdout" and line:
                    frame = {"type": "version", "text": line} if self.raw else json.loads(line)
                    require(isinstance(frame, dict), "non-object process frame")
                    if frame.get("type") in {"ready", "response", "agent_settled", "prepared",
                            "round_complete", "reset_complete", "stopped", "version"}:
                        try:
                            self.queue.put_nowait(frame)
                        except queue.Full:
                            raise Problem("protocol", "control event retention limit exceeded")
        except Exception as error:
            if self.error is None:
                self.error = error

    def wait(self, predicate):
        deadline = time.monotonic() + self.timeout
        while time.monotonic() < deadline:
            if self.error:
                raise self.error
            try:
                frame = self.queue.get(timeout=min(.05, max(.001, deadline - time.monotonic())))
            except queue.Empty:
                if self.proc.poll() is not None and not self.threads[0].is_alive():
                    raise Problem("process", "unexpected process exit: " + str(self.proc.returncode))
                continue
            if predicate(frame):
                return frame
            # Only prompt acceptance and settlement can race. Anything else is stale.
            if frame.get("type") == "agent_settled":
                require(not hasattr(self, "settled"), "duplicate settlement")
                self.settled = frame
        raise Problem("timeout", "process response deadline exceeded", self.timeout)

    def send(self, value):
        line = (json.dumps(value, ensure_ascii=False) + "\n").encode()
        require(len(line) <= 4096, "command exceeds 4096 bytes")
        self.proc.stdin.write(line)
        self.proc.stdin.flush()

    def call(self, kind, **fields):
        self.counter += 1
        ident = "resource-" + str(self.counter)
        self.send(dict(type=kind, id=ident, **fields))
        response = self.wait(lambda f: f.get("type") == "response" and f.get("id") == ident)
        require(response.get("command") == kind and response.get("success") is True,
                "RPC command failed: " + kind)
        return response.get("data")

    def settlement(self):
        frame = self.__dict__.pop("settled", None)
        if frame is None:
            frame = self.wait(lambda f: f.get("type") == "agent_settled")
        require(frame.get("outcome") == "completed", "agent_settled not completed", "semantic")

    def exit_ok(self):
        self.proc.stdin.close()
        try:
            code = self.proc.wait(timeout=self.timeout)
        except subprocess.TimeoutExpired:
            raise Problem("timeout", "process did not stop", self.timeout) from None
        require(code == 0, "nonzero process exit: " + str(code), "process")

    def close(self):
        if self.closed:
            return
        self.closed = True
        try:
            if self.proc.poll() is None:
                self.proc.terminate()
                try:
                    self.proc.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    self.proc.kill()
                    self.proc.wait(timeout=5)
        finally:
            # Closing a buffered stdin can raise BrokenPipeError; still close everything else.
            cleanup_error = None
            for pipe in (self.proc.stdin,):
                try:
                    if not pipe.closed:
                        pipe.close()
                except OSError as error:
                    cleanup_error = error
            for thread in self.threads:
                thread.join(timeout=5)
            for pipe in (self.proc.stdout, self.proc.stderr, self.log):
                try:
                    pipe.close()
                except OSError as error:
                    cleanup_error = cleanup_error or error
        require(not any(t.is_alive() for t in self.threads), "process reader leaked", "cleanup")
        if self.error:
            raise self.error
        if cleanup_error:
            raise Problem("cleanup", str(cleanup_error))


def text(content):
    if isinstance(content, str):
        return content
    return "".join(block.get("text", "") for block in content or [] if isinstance(block, dict))


def expected(round_number, final):
    return {"round": round_number, "sum": round_number * (round_number + 1) // 2,
            "final": final, "marker": "RESOURCE_SMOKE_FINAL" if final else "RESOURCE_SMOKE_ROUND"}


def prompt_for(round_number, final):
    return ("Keep a running sum, initially zero. Add this round's integer to the sum from earlier rounds. "
            "Return ONLY a JSON object with round, sum, final, marker; no markdown. "
            "Use marker RESOURCE_SMOKE_FINAL when final is true, otherwise RESOURCE_SMOKE_ROUND. "
            "Round input: " + json.dumps({"round": round_number, "final": final}, separators=(",", ":")))


class Provider:
    """Independent bounded loopback streaming fixture, with no retained request bodies."""
    def __init__(self, max_requests, expected_api_key=None, expected_max_tokens=None):
        self.calls, self.error, self.max_requests = 0, None, max_requests
        owner = self
        class Handler(BaseHTTPRequestHandler):
            def setup(self):
                super().setup()
                self.connection.settimeout(5)

            def log_message(self, *_):
                pass

            def do_POST(self):
                self.connection.settimeout(5)
                try:
                    owner.calls += 1
                    require(owner.calls <= owner.max_requests, "fixture request budget exceeded")
                    require(self.path == "/v1/chat/completions", "wrong fixture route")
                    if expected_api_key is not None:
                        require(self.headers.get("Authorization") == "Bearer " + expected_api_key,
                                "fixture authorization mismatch", "provider")
                    size = int(self.headers.get("Content-Length", "0"))
                    require(0 < size <= MAX_LINE, "fixture input size invalid")
                    body = json.loads(self.rfile.read(size))
                    require(body.get("stream") is True and not body.get("tools"), "stream/tools contract")
                    if expected_max_tokens is not None:
                        require(body.get("max_tokens", body.get("max_completion_tokens")) == expected_max_tokens,
                                "fixture output-token budget mismatch", "provider")
                    users = [m for m in body["messages"] if m.get("role") == "user"]
                    prompt = text(users[-1]["content"])
                    task = json.loads(prompt[prompt.index("{"):])
                    require(set(task) == {"round", "final"} and type(task["round"]) is int and
                            0 < task["round"] <= 1000 and type(task["final"]) is bool, "fixture task mismatch")
                    answer = expected(task["round"], task["final"])
                    output = json.dumps(answer, separators=(",", ":"))
                    # Small controlled whitespace padding exercises incremental updates, bounded to 2 KiB.
                    output = " " * 2048 + output
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Connection", "close")
                    self.end_headers()
                    for index in range(0, len(output), 64):
                        frame = {"choices": [{"index": 0, "delta": {"content": output[index:index+64]},
                                               "finish_reason": None}]}
                        self.wfile.write(("data: " + json.dumps(frame) + "\n\n").encode())
                        self.wfile.flush()
                    frame = {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                             "usage": {"prompt_tokens": 30, "completion_tokens": 40, "total_tokens": 70}}
                    self.wfile.write(("data: " + json.dumps(frame) + "\n\ndata: [DONE]\n\n").encode())
                    self.wfile.flush()
                    self.close_connection = True
                except Exception as error:
                    owner.error = str(error)
                    self.close_connection = True
        self.server = HTTPServer(("127.0.0.1", 0), Handler)
        self.server.timeout = .1
        self.thread = threading.Thread(target=lambda: self.server.serve_forever(poll_interval=.05), daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=6)
        require(not self.thread.is_alive(), "fixture thread leaked", "cleanup")
        require(self.error is None, self.error, "provider")


def environment(root, args, base_url, secret=""):
    allowed = {"SYSTEMROOT", "WINDIR", "COMSPEC", "PATH", "PATHEXT", "SYSTEMDRIVE",
               "PROCESSOR_ARCHITECTURE", "NUMBER_OF_PROCESSORS"}
    env = {k: v for k, v in os.environ.items() if k.upper() in allowed}
    for leaf in ("home", "agent", "tmp", "sessions", "workspace", "appdata", "localappdata"):
        (root / leaf).mkdir()
    home = root / "home"
    env.update(HOME=str(home), USERPROFILE=str(home), APPDATA=str(root / "appdata"),
               LOCALAPPDATA=str(root / "localappdata"), XDG_CONFIG_HOME=str(home / ".config"),
               XDG_CACHE_HOME=str(home / ".cache"), XDG_DATA_HOME=str(home / ".local/share"),
               TEMP=str(root / "tmp"), TMP=str(root / "tmp"), TMPDIR=str(root / "tmp"),
               RPI_CODING_AGENT_DIR=str(root / "agent"), NO_COLOR="1", RPI_VOICE_AUTO_TTS="off")
    if args.live:
        env[args.api_key_env] = secret
    else:
        env["RPI_OFFLINE"] = "1"
    dump(root / "agent/auth.json", {})
    dump(root / "agent/settings.json", {"defaultProvider": "resource-fixture",
        "defaultModel": args.model or "synthetic", "defaultThinkingLevel": "off",
        "retry": {"enabled": False, "maxRetries": 0}, "compaction": {"enabled": False}})
    dump(root / "agent/models.json", {"providers": {"resource-fixture": {
        "api": args.api if args.live else "openai-completions", "baseUrl": base_url,
        "apiKey": "$" + args.api_key_env if args.live else "local-dummy-not-a-credential",
        "models": [{"id": args.model or "synthetic", "name": "resource smoke", "reasoning": False,
                    "input": ["text"], "contextWindow": 100000, "maxTokens": args.max_output_tokens}]}}})
    return env


class Report:
    def __init__(self, root, args, secret=""):
        self.root, self.args, self.secret = root, args, secret
        self.phase, self.round, self.warmup_index = "startup", None, None
        self.data = {"status": "RUNNING", "firstProblemRound": None, "firstProblem": None,
                     "live": {"status": "RUNNING" if args.live else "SKIP", "reason": None if args.live else "no --live"},
                     "platform": sys.platform, "profile": args.profile, "cases": [], "metrics": []}
        self.save()

    def save(self):
        dump(self.root / "results.json", self.data, self.secret)

    def event(self, value):
        with (self.root / "metrics.jsonl").open("a", encoding="utf-8") as file:
            file.write(redact(json.dumps(value, ensure_ascii=False), self.secret) + "\n")
        self.save()

    def fail(self, error):
        if self.data["firstProblem"] is None:
            self.data["firstProblemRound"] = self.round
            self.data["firstProblem"] = {"category": getattr(error, "category", "running"),
                "phase": self.phase, "round": self.round, "warmup_index": self.warmup_index,
                "actual": str(getattr(error, "actual", error)),
                "budget": getattr(error, "budget", None)}
        self.data["status"] = "FAIL"
        if self.args.live:
            self.data["live"]["status"] = "FAIL"
        self.save()

    def measure(self, phase, number, before, after, sampler, baseline, warmup=False, warmup_index=None):
        self.phase, self.round, self.warmup_index = phase, number, warmup_index
        change = delta(before, after)
        record = dict(phase=phase, round=number, warmup=warmup, warmup_index=warmup_index,
                      sample=after, delta=change,
                      peak_primary_bytes=sampler.peak, sample_count=sampler.count,
                      baseline_delta_bytes=after["primary_bytes"] - baseline["primary_bytes"])
        self.data["metrics"].append(record)
        self.event(record)
        checks = [(sampler.peak / MIB, self.args.max_memory_mib, "memory_ceiling")]
        if not warmup:
            if phase.endswith("idle"):
                checks.append((change["cpu_percent"], self.args.max_idle_cpu_percent, "idle_cpu"))
            else:
                checks.append((change["cpu_seconds"], self.args.max_round_cpu_seconds, "round_cpu"))
            growth = self.args.max_editor_growth_mib if phase.startswith("editor") else self.args.max_rpc_growth_mib
            checks.append((record["baseline_delta_bytes"] / MIB, growth, "memory_growth"))
        for actual, budget, category in checks:
            if actual > budget:
                raise Problem(category, actual, budget)


def validate_answer(data, prompt, number, final, previous, args):
    require(isinstance(data, dict), "invalid message query response", "semantic")
    messages = data.get("messages", [])
    require(data.get("pendingResume") is False and isinstance(messages, list) and len(messages) == 2 and
            all(isinstance(m, dict) for m in messages), "missing current message pair", "semantic")
    user, assistant = messages
    ids = [m.get("entryId") for m in messages]
    require(all(isinstance(i, str) and i for i in ids) and len(set(ids)) == 2 and not set(ids) & previous,
            "stale/missing native message identity", "semantic")
    require(user.get("role") == "user" and text(user.get("content")) == prompt,
            "current user prompt mismatch", "semantic")
    require(assistant.get("role") == "assistant" and assistant.get("provider") == "resource-fixture" and
            assistant.get("model") == (args.model or "synthetic") and
            assistant.get("api") == (args.api if args.live else "openai-completions"),
            "native assistant identity mismatch", "semantic")
    require(assistant.get("stopReason") == "stop" and not assistant.get("errorMessage"),
            "assistant abnormal/truncated stop", "semantic")
    blocks = assistant.get("content")
    require(isinstance(blocks, list) and blocks and
            all(isinstance(b, dict) and b.get("type") == "text" and isinstance(b.get("text"), str) for b in blocks),
            "non-text assistant content", "semantic")
    try:
        answer = json.loads(text(blocks))
    except (ValueError, TypeError):
        raise Problem("semantic", "assistant final JSON invalid") from None
    require(isinstance(answer, dict) and answer == expected(number, final) and
            type(answer.get("round")) is int and type(answer.get("sum")) is int and type(answer.get("final")) is bool,
            "round/sum/final marker mismatch", "semantic")
    return set(ids)


def run_case(args, report, kind, encoding, secret):
    label = kind + ("-" + encoding if encoding else "")
    root = report.root / label
    root.mkdir()
    host = sampler = provider = None
    failure = None
    started = time.perf_counter()
    warmup_rounds = args.warmup_rounds if kind == "rpc" else 0
    final_conclusion = {"status": "NOT_COMPLETED" if kind == "rpc" else "NOT_APPLICABLE",
                        "round": None, "answer": None}
    observed_message_bytes = 0
    report.phase, report.round, report.warmup_index = label + "-startup", None, None
    try:
        total = args.rounds + warmup_rounds
        if kind == "rpc" and not args.live:
            provider = Provider(total)
        base = args.base_url if args.live else (f"http://127.0.0.1:{provider.server.server_port}/v1" if provider else "http://127.0.0.1:1/v1")
        env = environment(root, args, base, secret)
        if kind == "editor":
            command = [str(args.editor_probe)]
        else:
            command = [str(args.rpi), "--mode", "rpc", "--provider", "resource-fixture", "--model",
                args.model or "synthetic", "--session-dir", str(root / "sessions"), "--session-id", str(uuid.uuid4()),
                "--rpc-message-updates", encoding, "--no-extensions", "--no-skills", "--no-context-files",
                "--no-prompt-templates", "--no-tools", "--thinking", "off", "--local-only",
                "--timeout", str(args.request_timeout)]
        host = Host(command, root, env, args.request_timeout, secret)
        sampler = Sampler(host.proc.pid, args.sample_interval)
        ready = host.wait(lambda f: f.get("type") == "ready")
        if kind == "editor":
            require(ready.get("ok") is True, "editor not ready")
        else:
            require(ready.get("semantics", {}).get("messageUpdates") == encoding, "RPC encoding mismatch")
        baseline = sampler.take()  # Editor baseline MUST precede prepare; no extra editor warmup.
        report.event(dict(phase=report.phase, round=None, sample=baseline,
                          event="baseline", warmupRounds=warmup_rounds))
        if kind == "editor":
            report.phase = "editor-prepare"
            host.send(dict(type="prepare", pasteBytes=PASTE_BYTES, edits=EDITS))
            frame = host.wait(lambda f: f.get("type") == "prepared")
            require(frame.get("ok") is True and frame.get("pasteBytes") == PASTE_BYTES and frame.get("edits") == EDITS,
                    "editor prepare contract mismatch")
            report.measure("editor-prepare", None, baseline, sampler.take(), sampler, baseline)
        previous = set()
        for index in range(1, total + 1):
            warmup = index <= warmup_rounds
            number = None if warmup else index - warmup_rounds
            report.phase, report.round = label + ("-warmup" if warmup else "-round"), number
            report.warmup_index = index if warmup else None
            before = sampler.take()
            if kind == "editor":
                host.send(dict(type="round", round=index))
                frame = host.wait(lambda f: f.get("type") == "round_complete")
                require(frame.get("ok") is True and frame.get("round") == index and
                        frame.get("pasteBytes") == PASTE_BYTES and frame.get("edits") == EDITS,
                        "editor round contract mismatch")
            else:
                prompt = prompt_for(index, index == total)
                require(host.call("prompt", message=prompt) == {"accepted": True}, "prompt not accepted")
                host.settlement()
                data = host.call("get_messages", limit=2)
                previous = validate_answer(data, prompt, index, index == total, previous, args)
                observed_message_bytes += len(json.dumps(data["messages"], ensure_ascii=False,
                                                         separators=(",", ":")).encode("utf-8"))
                assistant = data["messages"][1]
                answer = json.loads(text(assistant["content"]))
                report.event(dict(event="completed-answer", phase=report.phase, round=number,
                    warmup=warmup, warmup_index=report.warmup_index, taskRound=index,
                    answer=answer, entryIds=[m["entryId"] for m in data["messages"]],
                    usage=assistant.get("usage"),
                    cumulativeObservedMessageJsonBytes=observed_message_bytes))
                if index == total:
                    final_conclusion = {"status": "PASS", "round": number, "answer": answer}
            after = sampler.take()
            report.measure(report.phase, number, before, after, sampler, baseline, warmup,
                           report.warmup_index)
            if warmup:
                if index == warmup_rounds:
                    baseline = after
                    report.event(dict(event="baseline", phase=label + "-warmup-complete", round=None,
                                      warmup_index=index, sample=baseline, warmupRounds=warmup_rounds))
                continue
            report.phase, report.round = label + "-idle", number
            before = sampler.take()
            time.sleep(args.idle_seconds)
            idle_end = sampler.take()
            idle_delta = delta(before, idle_end)
            report.measure(report.phase, number, before, idle_end, sampler, baseline)
        report.phase, report.round = (label + "-reset" if kind == "editor" else label + "-stop"), None
        if kind == "editor":
            host.send({"type": "reset"})
            require(host.wait(lambda f: f.get("type") == "reset_complete").get("ok") is True, "editor reset failed")
            after = sampler.take()  # No requirement that allocator RSS falls after reset.
            report.event(dict(phase=report.phase, round=None, sample=after))
        final_sample = sampler.take()
        peak_primary, sample_count = sampler.peak, sampler.count
        sampler.close()
        sampler = None
        if kind == "editor":
            host.send({"type": "stop"})
            require(host.wait(lambda f: f.get("type") == "stopped").get("ok") is True, "editor stop failed")
        else:
            host.call("stop")
        host.exit_ok()
        if provider:
            require(provider.calls == total, "unexpected offline request count", "provider")
        summary = {"case": label, "status": "PASS", "rounds": args.rounds,
            "warmupRounds": warmup_rounds, "totalWallSeconds": time.perf_counter() - started,
            "userCpuSeconds": final_sample["user_seconds"], "kernelCpuSeconds": final_sample["kernel_seconds"],
            "actualCpuSeconds": final_sample["user_seconds"] + final_sample["kernel_seconds"],
            "cpuMeasurementEndsBeforeStop": True, "idle": dict(idle_delta, round=args.rounds),
            "measuredWindow": delta(baseline, final_sample), "finalConclusion": final_conclusion,
            "cumulativeObservedMessageJsonBytes": observed_message_bytes,
            "peakPrimaryBytes": peak_primary, "sampleCount": sample_count}
    except BaseException as error:
        failure = error
        report.fail(error)
    finally:
        for owned in (sampler, host, provider):
            if owned:
                try:
                    owned.close()
                except Exception as error:
                    if failure is None:
                        failure = error
                        report.phase, report.round = label + "-cleanup", None
                        report.fail(error)
        if failure:
            if kind == "rpc":
                final_conclusion = dict(final_conclusion, status="FAIL",
                    round=final_conclusion["round"] if final_conclusion["answer"] is not None
                          else report.data["firstProblemRound"])
            report.data["cases"].append(dict(case=label, status="FAIL", rounds=args.rounds,
                warmupRounds=warmup_rounds, finalConclusion=final_conclusion,
                cumulativeObservedMessageJsonBytes=observed_message_bytes))
            report.save()
            raise failure
    # A case is never counted PASS until every owned resource has closed successfully.
    report.data["cases"].append(summary)
    report.event(dict(phase=label + "-complete", **summary))


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rpi", type=Path, required=True)
    parser.add_argument("--editor-probe", type=Path)
    parser.add_argument("--diagnostics-dir", type=Path, required=True, help="new directory only; never overwritten")
    parser.add_argument("--case", choices=("all", "editor", "rpc"), default="all")
    parser.add_argument("--live", action="store_true", help="explicit opt-in; otherwise live is reported SKIP")
    parser.add_argument("--encoding", choices=("compact-deltas", "snapshot", "both"))
    parser.add_argument("--api", choices=("openai-completions", "openai-responses", "anthropic-messages"))
    parser.add_argument("--base-url")
    parser.add_argument("--model")
    parser.add_argument("--api-key-env")
    for name, default, typ in (("rounds", None, int), ("warmup-rounds", None, int),
        ("max-output-tokens", 256, int), ("max-requests", 5, int), ("request-timeout", None, int),
        ("idle-seconds", 1, float), ("sample-interval", .2, float), ("max-memory-mib", 512, float),
        ("max-rpc-growth-mib", 96, float), ("max-editor-growth-mib", 32, float),
        ("max-round-cpu-seconds", 5, float), ("max-idle-cpu-percent", 25, float)):
        parser.add_argument("--" + name, type=typ, default=default)
    parser.add_argument("--profile", default="release", help="build profile metadata (not a build command)")
    args = parser.parse_args(argv)
    defaults = {"rounds": 5 if args.live else 20, "warmup_rounds": 0 if args.live else 2,
                "encoding": "snapshot" if args.live else "both", "request_timeout": 60 if args.live else 25}
    for name, value in defaults.items():
        if getattr(args, name) is None:
            setattr(args, name, value)
    for name in ("rounds", "warmup_rounds", "max_output_tokens", "max_requests", "request_timeout",
                 "idle_seconds", "sample_interval", "max_memory_mib", "max_rpc_growth_mib",
                 "max_editor_growth_mib", "max_round_cpu_seconds", "max_idle_cpu_percent"):
        value = getattr(args, name)
        if not math.isfinite(value) or value < 0 or (value == 0 and name != "warmup_rounds"):
            parser.error("--" + name.replace("_", "-") + " must be positive (warmup may be zero)")
    if args.rounds + args.warmup_rounds > 1000:
        parser.error("at most 1000 total rounds per encoding")
    if args.case != "rpc" and args.editor_probe is None:
        parser.error("--editor-probe is required unless --case rpc")
    if args.live:
        if args.case == "editor":
            parser.error("--live requires a RPC case; editor-only cannot validate a model")
        if not all((args.api, args.base_url, args.model, args.api_key_env)):
            parser.error("--live requires --api, --base-url, --model and --api-key-env")
        if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", args.api_key_env):
            parser.error("invalid --api-key-env name")
        if args.api == "openai-responses" and args.max_output_tokens < 64:
            parser.error("openai-responses requires --max-output-tokens >= 64")
        env_name = args.api_key_env.upper()
        if env_name.startswith(("RPI_", "XDG_")) or env_name in {
                "PATH", "PATHEXT", "HOME", "USERPROFILE", "TMP", "TEMP", "TMPDIR", "SYSTEMDRIVE",
                "SYSTEMROOT", "WINDIR", "APPDATA", "LOCALAPPDATA", "COMSPEC", "NO_COLOR",
                "PROCESSOR_ARCHITECTURE", "NUMBER_OF_PROCESSORS"}:
            parser.error("--api-key-env conflicts with isolated environment")
        key = os.environ.get(args.api_key_env, "")
        if not key or any(ord(char) < 32 for char in key):
            parser.error("named live key environment variable is missing/empty or contains control characters")
        try:
            url = urlsplit(args.base_url)
            local = url.hostname in ("localhost", "127.0.0.1", "::1")
            valid = (url.scheme == "https" or (url.scheme == "http" and local)) and url.hostname and url.port != 0
            valid = valid and not (url.username or url.password or url.query or url.fragment)
        except ValueError:
            valid = False
        if not valid:
            parser.error("base URL requires HTTPS (loopback HTTP allowed), no userinfo/query/fragment")
        requests = (args.rounds + args.warmup_rounds) * (2 if args.encoding == "both" else 1)
        if args.case != "editor" and requests > args.max_requests:
            parser.error("scheduled requests across encodings and warmup exceed --max-requests")
    elif any((args.api, args.base_url, args.model, args.api_key_env)):
        parser.error("provider arguments require explicit --live")
    for name in ("rpi", "editor_probe"):
        path = getattr(args, name)
        if path is not None:
            path = path.resolve()
            if not path.is_file():
                parser.error("--" + name.replace("_", "-") + " must name an existing file")
            setattr(args, name, path)
    args.diagnostics_dir = args.diagnostics_dir.absolute()
    if args.diagnostics_dir.exists():
        parser.error("diagnostics directory already exists; refusing to overwrite")
    return args


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for block in iter(lambda: file.read(MIB), b""):
            digest.update(block)
    return digest.hexdigest()


def controller_repository():
    """Optional read-only controller checkout identity, NOT binary build provenance."""
    root = Path(__file__).resolve().parent.parent
    result = {"scope": "controller repository only; not binary build source", "head": None, "dirty": None}
    try:
        env = dict(os.environ, GIT_OPTIONAL_LOCKS="0")
        def git(*args):
            return subprocess.run(["git", "--no-optional-locks", "-C", str(root), *args],
                capture_output=True, text=True, timeout=5, check=True, env=env).stdout.strip()
        result["head"] = git("rev-parse", "HEAD")
        result["dirty"] = bool(git("status", "--porcelain", "--untracked-files=normal"))
    except (OSError, subprocess.SubprocessError, UnicodeError):
        pass  # Git is optional; null values explicitly mean unknown.
    return result


def main(argv=None):
    args = parse_args(argv)
    secret = os.environ[args.api_key_env] if args.live else ""
    try:
        args.diagnostics_dir.mkdir(parents=True, exist_ok=False)
    except OSError:
        print("FAIL: could not exclusively create diagnostics directory", file=sys.stderr)
        return 1
    report = Report(args.diagnostics_dir, args, secret)
    if sys.platform != "win32" and not sys.platform.startswith("linux"):
        report.data.update(status="SKIP", reason="unsupported platform: " + sys.platform)
        report.save()
        return 77
    version_host = None
    try:
        version_root = report.root / "version"
        version_root.mkdir()
        env = environment(version_root, args, args.base_url or "http://127.0.0.1:1/v1", secret)
        version_host = Host([str(args.rpi), "--version"], version_root, env, args.request_timeout, secret, raw=True)
        version = version_host.wait(lambda f: f.get("type") == "version")["text"]
        version_host.exit_ok()
        version_host.close()
        version_host = None
        metadata = {"phase": "initial", "version": version, "binary": str(args.rpi), "sha256": sha256_file(args.rpi),
            "editorProbe": {"binary": str(args.editor_probe), "sha256": sha256_file(args.editor_probe)} if args.editor_probe else None,
            "controllerRepository": controller_repository(),
            "validationScope": {"task": "exact prompted arithmetic JSON fixture; not general reasoning quality",
                "providerCoverage": "loopback openai-completions tested; openai-responses/anthropic-messages untested"},
            "profile": args.profile, "platform": sys.platform, "primaryMetric": "windows_private_commit" if sys.platform == "win32" else "linux_rss",
            "workload": {"case": args.case, "rounds": args.rounds,
                "warmupRounds": args.warmup_rounds if args.case == "rpc" else 0 if args.case == "editor" else None,
                "warmupRoundsByCase": {"rpc": args.warmup_rounds, "editor": 0},
                "encoding": args.encoding, "live": args.live, "pasteBytes": PASTE_BYTES, "edits": EDITS,
                "maxOutputTokens": args.max_output_tokens, "maxRequests": args.max_requests},
            "budgets": {k: v for k, v in vars(args).items() if k.startswith("max_")},
            "sampling": {"intervalSeconds": args.sample_interval, "idleSeconds": args.idle_seconds,
                         "requestTimeoutSeconds": args.request_timeout, "cpuPercentBasis": "single-core"}}
        report.data["metadata"] = metadata
        report.event(metadata)
        if args.case in ("all", "editor"):
            run_case(args, report, "editor", None, secret)
        if args.case in ("all", "rpc"):
            for encoding in (("compact-deltas", "snapshot") if args.encoding == "both" else (args.encoding,)):
                run_case(args, report, "rpc", encoding, secret)
        report.data["status"] = "PASS"
        if args.live:
            report.data["live"]["status"] = "PASS"
        report.save()
        print("PASS: " + str(report.root / "results.json"))
        return 0
    except BaseException as error:
        report.fail(error)
        print("FAIL: " + redact(str(error), secret), file=sys.stderr)
        return 1
    finally:
        if version_host:
            try:
                version_host.close()
            except Exception as error:
                report.fail(error)


if __name__ == "__main__":
    sys.exit(main())
