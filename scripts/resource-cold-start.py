#!/usr/bin/env python3
"""Isolated synthetic JSONL cold-start gate; no prompts, tools or provider requests.

Example (output MUST be a new directory outside this repository):
  python -B scripts/resource-cold-start.py --candidate C:/bin/new-rpi.exe \
    --output D:/bench/cold-start-001 --shapes modern
Add --baseline C:/bin/old-rpi.exe for paired comparison.
Small smoke: add --frames 80 --entries 12 --repeats 1 --shapes legacy modern.

A fresh process/home/project/session is used for every observation. Optional
baseline comparisons alternate AB/BA pairs. This is PROCESS cold start, NOT a
flushed OS disk-cache benchmark.
Legacy ready time includes the locked progress-format upgrade (potential full
history rewrite); modern uses an already-declared, valid empty companion.
Candidate median/max ready and maximum message/state query times gate exit;
baseline latency gates are informational.
All protocol/measurement/cleanup failures, including baseline failures, fail exit.
Only generated history, its companion and its writer lock are deleted; diagnostics
and unknown files are retained. No recursive cleanup. Exit: 0 PASS, 1 FAIL, 2 usage,
77 unsupported platform. Memory: Windows private commit, Linux RSS (not private).
"""
import sys

# Must precede imports of either sibling script (including their nested imports).
sys.dont_write_bytecode = True

import argparse
import ctypes
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import queue
import signal
import subprocess
import threading
import time


def load_comparison():
    path = Path(__file__).with_name("compare-resources.py")
    spec = importlib.util.spec_from_file_location("cold_start_comparison", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


comparison = load_comparison()
smoke = comparison.smoke
require = smoke.require
SESSION_ID = "synthetic-cold-start"
FORMAT = "assistant-progress-v1"


def dump(path, value):
    with path.open("w", encoding="utf-8", newline="\n") as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2, allow_nan=False)
        stream.write("\n")


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def generate(path, workspace, shape, args):
    """Stream one history, never retain a second large copy in memory/on disk.

    Entry i is emitted when floor(append_count * entries / frames) reaches i.
    Every append/entry/commit has a globally unique seq. A distinct stream/run
    per 4000 appends binds to an entry present in the final canonical branch.
    The final partial batch is committed too, so no orphan progress needs replay.
    frame-bytes specifies the ASCII delta payload size, not total JSON row size.
    Companion checkpoint schema is from session/jsonl/progress.rs: Checkpoint
    {format, session_id, state: {sequence, frames}}, not a primary JSONL header.
    """
    seq = emitted = commits = 0
    digest = hashlib.sha256()
    size = 0
    header = dict(kind="header", version=4, id=SESSION_ID, createdAt=0, cwd=str(workspace))
    if shape == "modern":
        header["progressFormat"] = FORMAT
    with path.open("xb", buffering=1024 * 1024) as stream:
        def write(value):
            nonlocal size
            raw = (json.dumps(value, ensure_ascii=False, separators=(",", ":")) + "\n").encode("utf-8")
            stream.write(raw)
            digest.update(raw)
            size += len(raw)
        write(header)
        for number in range(1, args.frames + 1):
            seq += 1
            run = "synthetic-run-" + str((number - 1) // 4000)
            write(dict(kind="record", type="assistant_frame", id="frame-" + str(number),
                       seq=seq, timestamp=0, lane="main", runId=run, streamIndex=0,
                       op="append", frame={"type": "text_delta", "contentIndex": 0,
                                           "delta": "x" * args.frame_bytes}))
            target = number * args.entries // args.frames
            while emitted < target:
                emitted += 1
                seq += 1
                write(dict(kind="entry", type="message", id="entry-" + str(emitted),
                           seq=seq, timestamp=0, lane="main",
                           parentId="entry-" + str(emitted - 1) if emitted > 1 else None,
                           message={"kind": "user", "role": "user", "timestamp": 0,
                                    "content": "synthetic-entry-" + str(emitted)}))
            if number % 4000 == 0 or number == args.frames:
                seq += 1
                commits += 1
                write(dict(kind="record", type="assistant_frame", id="commit-" + str(commits),
                           seq=seq, timestamp=0, lane="main", runId=run, streamIndex=0,
                           op="commit_stream", frame={"entryId": "entry-" + str(max(1, emitted))}))
    companion = Path(str(path) + ".progress.jsonl")
    if shape == "modern":
        # Exactly one newline-terminated checkpoint line, with no unknown fields.
        with companion.open("x", encoding="utf-8", newline="\n") as stream:
            stream.write(json.dumps({"format": FORMAT, "session_id": SESSION_ID,
                                     "state": {"sequence": 0, "frames": []}}, separators=(",", ":")) + "\n")
    return {"historySha256": digest.hexdigest(), "historyBytes": size,
            "companionSha256": sha256(companion) if shape == "modern" else None,
            "appendFrames": args.frames, "entries": emitted, "commitStreams": commits,
            "lastSeq": seq, "shape": shape, "existingIndex": False}


class WindowsJob:
    """Kill-on-close job: only this Popen and descendants, never name-wide kills."""
    def __init__(self):
        from ctypes import wintypes as w
        class Basic(ctypes.Structure):
            _fields_ = [("ProcessTime", ctypes.c_longlong), ("JobTime", ctypes.c_longlong),
                        ("Flags", w.DWORD), ("MinWorkingSet", ctypes.c_size_t),
                        ("MaxWorkingSet", ctypes.c_size_t), ("ActiveProcesses", w.DWORD),
                        ("Affinity", ctypes.c_size_t), ("Priority", w.DWORD), ("Scheduling", w.DWORD)]
        class IO(ctypes.Structure):
            _fields_ = [(name, ctypes.c_ulonglong) for name in
                        ("ReadOps", "WriteOps", "OtherOps", "ReadBytes", "WriteBytes", "OtherBytes")]
        class Extended(ctypes.Structure):
            _fields_ = [("Basic", Basic), ("IO", IO), ("ProcessMemory", ctypes.c_size_t),
                        ("JobMemory", ctypes.c_size_t), ("PeakProcess", ctypes.c_size_t),
                        ("PeakJob", ctypes.c_size_t)]
        self.api = ctypes.WinDLL("kernel32", use_last_error=True)
        self.api.CreateJobObjectW.argtypes = [ctypes.c_void_p, w.LPCWSTR]
        self.api.CreateJobObjectW.restype = w.HANDLE
        self.api.SetInformationJobObject.argtypes = [w.HANDLE, ctypes.c_int, ctypes.c_void_p, w.DWORD]
        self.api.SetInformationJobObject.restype = w.BOOL
        self.api.AssignProcessToJobObject.argtypes = [w.HANDLE, w.HANDLE]
        self.api.AssignProcessToJobObject.restype = w.BOOL
        self.api.CloseHandle.argtypes = [w.HANDLE]
        self.api.CloseHandle.restype = w.BOOL
        self.handle = self.api.CreateJobObjectW(None, None)
        if not self.handle:
            raise ctypes.WinError(ctypes.get_last_error())
        limits = Extended()
        limits.Basic.Flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if not self.api.SetInformationJobObject(self.handle, 9, ctypes.byref(limits), ctypes.sizeof(limits)):
            error = ctypes.WinError(ctypes.get_last_error())
            self.close()
            raise error

    def assign(self, proc):
        if not self.api.AssignProcessToJobObject(self.handle, int(proc._handle)):
            raise ctypes.WinError(ctypes.get_last_error())

    def close(self):
        if self.handle:
            if not self.api.CloseHandle(self.handle):
                raise ctypes.WinError(ctypes.get_last_error())
            self.handle = None


class ColdHost(comparison.CompatibleHost):
    def __init__(self, command, root, env, timeout):
        # Same binary-pipe/UTF-8 protocol as CompatibleHost; own a process tree.
        self.timeout, self.secret, self.raw = timeout, "", False
        self.queue = queue.Queue(maxsize=32)
        self.error, self.counter, self.closed = None, 0, False
        self.log_lock, self.log_bytes = threading.Lock(), 0
        self.ready_ns = None
        self.tree_cleanup = []
        self.threads = []
        self.proc = None
        self.job = None
        self.log = (root / "process.jsonl").open("x", encoding="utf-8", newline="\n")
        try:
            if os.name == "nt":
                self.job = WindowsJob()
            self.start_ns = time.perf_counter_ns()
            self.proc = subprocess.Popen(command, cwd=root / "workspace", env=env,
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                start_new_session=os.name != "nt")
            if self.job:
                self.job.assign(self.proc)
            self.threads = [threading.Thread(target=self.pump, args=(pipe, name), daemon=True)
                            for pipe, name in ((self.proc.stdout, "stdout"), (self.proc.stderr, "stderr"))]
            for thread in self.threads:
                thread.start()
        except BaseException as error:
            # Preserve startup AND cleanup failures, even when no host is returned.
            try:
                self.close()
            except Exception as cleanup_error:
                raise smoke.Problem("startup_cleanup", str(error) + "; cleanup: " +
                                    str(cleanup_error)) from error
            raise

    def record(self, name, line):
        if name == "stdout" and self.ready_ns is None:
            observed_ns = time.perf_counter_ns()
            try:
                if json.loads(line).get("type") == "ready":
                    self.ready_ns = observed_ns
            except (ValueError, AttributeError):
                pass  # CompatibleHost records the line and reports malformed JSON.
        super().record(name, line)

    def pump(self, pipe, name):
        # CompatibleHost's protocol, with explicit evidence for malformed UTF-8,
        # oversize lines and exceptions (never silently lose the offending row).
        try:
            while True:
                raw = pipe.readline(smoke.MAX_LINE + 1)
                if not raw:
                    break
                if len(raw) > smoke.MAX_LINE:
                    self.record("invalid-line", name + ": oversize bytes=" + str(len(raw)) +
                                " prefixHex=" + raw[:256].hex())
                    raise smoke.Problem("protocol", "oversize process line")
                try:
                    line = raw.decode("utf-8", errors="strict").rstrip("\r\n")
                except UnicodeDecodeError:
                    self.record("invalid-line", name + ": invalid UTF-8 prefixHex=" + raw[:4096].hex())
                    raise
                self.record(name, line)
                if name != "stdout" or not line:
                    continue
                frame = json.loads(line)
                require(isinstance(frame, dict), "non-object frame")
                if frame.get("type") == "ready":
                    self.ready_frame = frame
                frame = comparison.normalize_control(frame, getattr(self, "ready_frame", {}))
                if frame and frame.get("type") in {"ready", "response", "agent_settled", "prepared",
                        "round_complete", "reset_complete", "stopped"}:
                    try:
                        self.queue.put_nowait(frame)
                    except queue.Full:
                        raise smoke.Problem("protocol", "control event retention limit exceeded")
        except Exception as error:
            if self.error is None:
                self.error = error
            try:
                self.record("reader-error", name + ": " + type(error).__name__ + ": " + str(error))
            except Exception as log_error:
                self.error = smoke.Problem("protocol_log", str(self.error) + "; log: " + str(log_error))

    def close(self):
        if self.closed:
            return
        errors = []
        try:
            if self.job:
                self.job.close()
                self.tree_cleanup.append({"method": "owned-windows-job-close", "ok": True})
            elif self.proc:
                try:
                    os.killpg(self.proc.pid, signal.SIGKILL)
                    self.tree_cleanup.append({"method": "owned-posix-process-group", "ok": True})
                except ProcessLookupError:
                    self.tree_cleanup.append({"method": "owned-posix-process-group", "ok": True, "alreadyExited": True})
        except Exception as error:
            errors.append(str(error))
            self.tree_cleanup.append({"method": "owned-tree", "ok": False, "error": str(error)})
        try:
            if self.proc:
                super().close()
            else:
                self.log.close()
                self.closed = True
        except Exception as error:
            errors.append(str(error))
        if errors:
            raise smoke.Problem("cleanup", "; ".join(errors))


def run_once(binary, root, args, shape, variant, repeat):
    root.mkdir()
    result = {"shape": shape, "variant": variant, "repeat": repeat,
              "status": "RUNNING", "errors": [], "cleanup": [], "metrics": {}}
    host = sampler = None
    process_cleanup_ok = True
    history = root / "sessions" / "fixture" / "history.jsonl"
    owned = [history, Path(str(history) + ".progress.jsonl"), Path(str(history) + ".lock")]
    phase = "environment"
    def failure(error, where):
        result["errors"].append({"phase": where, "category": getattr(error, "category", "runtime"),
                                 "type": type(error).__name__, "message": str(error)})
    try:
        env = smoke.environment(root, comparison.fixture_args(), "http://127.0.0.1:1/v1")
        env.update(PYTHONUTF8="1", PYTHONIOENCODING="utf-8")
        history.parent.mkdir()
        phase = "generate"
        result["fixture"] = generate(history, root / "workspace", shape, args)
        dump(root / "fixture.json", result["fixture"])
        command = [str(binary), "--mode", "rpc", "--session", SESSION_ID,
                   "--session-dir", str(root / "sessions"), "--provider", "resource-fixture",
                   "--model", "synthetic", "--rpc-message-updates", "snapshot", "--offline",
                   "--local-only", "--no-extensions", "--no-skills", "--no-context-files",
                   "--no-prompt-templates", "--no-themes", "--no-tools", "--no-builtin-tools",
                   "--thinking", "off", "--timeout", "30"]
        result["command"] = command
        phase = "ready"
        host = ColdHost(command, root, env, 30)
        sampler = smoke.Sampler(host.proc.pid, .01)
        host.wait(lambda frame: frame.get("type") == "ready")
        require(host.ready_ns is not None, "ready event missing clock")
        result["metrics"]["readyMs"] = (host.ready_ns - host.start_ns) / 1e6
        sampler.take()
        phase = "get_messages"
        started = time.perf_counter_ns()
        messages = host.call("get_messages", limit=40)
        result["metrics"]["getMessages40Ms"] = (time.perf_counter_ns() - started) / 1e6
        require(isinstance(messages, dict) and isinstance(messages.get("messages"), list), "invalid messages")
        rows = messages["messages"]
        count = min(40, args.entries)
        require(len(rows) == count, "wrong synthetic message count", "semantic")
        require(messages.get("pendingResume") is False, "missing or pending resume state", "semantic")
        for row, number in zip(rows, range(args.entries - count + 1, args.entries + 1)):
            require(isinstance(row, dict) and row.get("role", row.get("kind")) == "user" and
                    smoke.text(row.get("content")) == "synthetic-entry-" + str(number),
                    "synthetic message order/content mismatch", "semantic")
        result["messageCount"] = count
        sampler.take()
        phase = "get_state"
        started = time.perf_counter_ns()
        state = host.call("get_state")
        result["metrics"]["getStateMs"] = (time.perf_counter_ns() - started) / 1e6
        require(isinstance(state, dict), "invalid state")
        # Validate the actual get_state schema, not optional flags that the
        # current RPC does not return. Missing fields must not imply idle.
        require("activeOperation" in state and state["activeOperation"] is None,
                "missing or active operation", "semantic")
        queued = state.get("queue")
        require(isinstance(queued, dict), "missing queue state", "semantic")
        for key in ("steeringCount", "followUpCount"):
            require(type(queued.get(key)) is int and queued[key] == 0,
                    "missing or nonempty queue count: " + key, "semantic")
        for key in ("steering", "followUp"):
            require(queued.get(key) == [], "missing or nonempty queue: " + key, "semantic")
        result["stateFlags"] = {"activeOperation": state["activeOperation"],
                                "steeringCount": queued["steeringCount"],
                                "followUpCount": queued["followUpCount"],
                                "pendingResume": messages["pendingResume"]}
        final_sample = sampler.take()
        result["primaryMetric"] = final_sample["primary_metric"]
        result["metrics"]["peakPrimaryBytes"] = sampler.peak
        result["metrics"]["peakPrivateBytes"] = sampler.peak if os.name == "nt" else None
        result["metrics"]["osPeakCommitBytes"] = final_sample.get("peak_commit_bytes")
        phase = "shutdown"
        sampler.close()
        sampler = None
        host.call("stop")
        host.exit_ok()
    except (Exception, KeyboardInterrupt) as error:
        failure(error, phase)
        if "cleanup" in getattr(error, "category", ""):
            process_cleanup_ok = False
        if isinstance(error, KeyboardInterrupt):
            raise
    finally:
        if sampler:
            result["metrics"]["peakPrimaryBytes"] = sampler.peak
            try:
                sampler.close()
            except Exception as error:
                failure(error, "sampler-cleanup")
        if host:
            try:
                host.close()
            except Exception as error:
                process_cleanup_ok = False
                failure(error, "process-cleanup")
            result["cleanup"].extend(host.tree_cleanup)
            result["processExitCode"] = host.proc.returncode if host.proc else None
        # Exact allowlist only. Never recurse or delete indexes/unknown artifacts.
        for path in owned:
            try:
                if not process_cleanup_ok:
                    result["cleanup"].append({"path": str(path), "retained": True,
                                              "reason": "process cleanup incomplete"})
                    continue
                if path.is_symlink():
                    raise smoke.Problem("cleanup", "refusing symlink: " + str(path))
                if path.exists():
                    size = path.stat().st_size
                    path.unlink()
                    result["cleanup"].append({"path": str(path), "deleted": True, "bytes": size})
                else:
                    result["cleanup"].append({"path": str(path), "absent": True})
            except Exception as error:
                failure(error, "history-cleanup")
                result["cleanup"].append({"path": str(path), "deleted": False, "error": str(error)})
        result["status"] = "FAIL" if result["errors"] else "PASS"
        dump(root / "report.json", result)
    return result


def variants(args):
    return ("baseline", "candidate") if args.baseline is not None else ("candidate",)


def summarize(runs, args):
    summaries = []
    for shape in args.shapes:
        for variant in variants(args):
            group = [run for run in runs if run["shape"] == shape and run["variant"] == variant]
            complete = len(group) == args.repeats and all(run["status"] == "PASS" for run in group)
            metrics = {}
            for key in ("readyMs", "getMessages40Ms", "getStateMs", "peakPrimaryBytes",
                        "peakPrivateBytes", "osPeakCommitBytes"):
                values = [run["metrics"].get(key) for run in group]
                valid = len(values) == args.repeats and all(
                    type(value) in (int, float) and math.isfinite(value) and value >= 0 for value in values)
                metrics[key] = comparison.median_range(values) if valid else None
            ready = metrics["readyMs"]
            median_ok = bool(ready and ready["median"] <= args.max_ready_ms)
            max_ok = bool(ready and ready["max"] <= args.max_single_ready_ms)
            messages = metrics["getMessages40Ms"]
            state = metrics["getStateMs"]
            messages_ok = bool(messages and messages["max"] <= args.max_get_messages_ms)
            state_ok = bool(state and state["max"] <= args.max_get_state_ms)
            summaries.append({"shape": shape, "variant": variant, "metrics": metrics,
                "gate": {"complete": complete, "medianReadyPass": median_ok, "maxReadyPass": max_ok,
                         "getMessagesPass": messages_ok, "getStatePass": state_ok,
                         "maxMedianReadyMs": args.max_ready_ms, "maxSingleReadyMs": args.max_single_ready_ms,
                         "maxGetMessagesMs": args.max_get_messages_ms, "maxGetStateMs": args.max_get_state_ms,
                         "gatesExit": variant == "candidate",
                         "status": "PASS" if complete and median_ok and max_ok and messages_ok and state_ok else "FAIL"}})
    return summaries


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--baseline", type=Path, help="optional old executable for paired AB/BA comparison")
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--shapes", nargs="+", choices=("legacy", "modern"), default=["legacy", "modern"])
    for name, default in (("frames", 701000), ("entries", 21733), ("repeats", 5), ("frame-bytes", 256)):
        parser.add_argument("--" + name, type=int, default=default)
    parser.add_argument("--max-ready-ms", type=float, default=2000)
    parser.add_argument("--max-single-ready-ms", type=float, default=3000)
    parser.add_argument("--max-get-messages-ms", type=float, default=100)
    parser.add_argument("--max-get-state-ms", type=float, default=1500)
    args = parser.parse_args(argv)
    for key in ("frames", "entries", "repeats", "frame_bytes", "max_ready_ms", "max_single_ready_ms",
                "max_get_messages_ms", "max_get_state_ms"):
        value = getattr(args, key)
        if value <= 0 or not math.isfinite(value):
            parser.error("--" + key.replace("_", "-") + " must be finite and positive")
    if len(set(args.shapes)) != len(args.shapes):
        parser.error("--shapes must not contain duplicates")
    for key in variants(args):
        path = getattr(args, key).resolve()
        if not path.is_file():
            parser.error("--" + key + " must be an existing executable file")
        setattr(args, key, path)
    repository = Path(__file__).resolve().parent.parent
    if args.output.exists() or args.output.is_symlink():
        parser.error("--output already exists; refusing overwrite")
    args.output = args.output.resolve()
    if args.output == repository or repository in args.output.parents:
        parser.error("--output must be outside the repository")
    return args


def main(argv=None):
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="backslashreplace")
    argv = list(sys.argv[1:] if argv is None else argv)
    args = parse_args(argv)
    if not (sys.platform == "win32" or sys.platform.startswith("linux")):
        print("SKIP: resource sampling supports Windows and Linux only")
        return 77
    args.output.mkdir(parents=True, exist_ok=False)
    parameters = {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()}
    data = {"status": "RUNNING", "scenario": "historical-frame-heavy-v1",
            "parameters": parameters, "platform": sys.platform,
            "python": sys.version, "diskCacheFlushed": False, "runs": [],
            "notes": ["Process cold start only; OS disk cache is NOT flushed (generation warms it).",
                      "No provider is started and no prompt/model/tool call is sent.",
                      "All settings/model registry files are synthetic isolated fixtures.",
                      "Legacy ready includes progress-format upgrade and possible history rewrite; modern does not.",
                      "Generation and SHA256 hashing excluded from ready latency; clock starts immediately before Popen.",
                      "10 ms sampled peak private commit on Windows; Linux reports RSS, never labels it private.",
                      "Baseline latency gates informational; candidate latency and every run/cleanup gate exit.",
                      "Each history is generated independently and deleted before the next observation."]}
    invocation = [sys.executable, "-B", str(Path(__file__).resolve()), *argv]
    dump(args.output / "reproduce.json", {"argv": invocation, "parameters": parameters,
         "generator": str(Path(__file__).resolve()), "generatorSha256": sha256(Path(__file__)),
         "instruction": "Use these parameters with a NEW output directory. Generator is this script's generate()."})
    dump(args.output / "results.json", data)
    try:
        data["executables"] = {variant: {"path": str(getattr(args, variant)),
                                        "sha256": sha256(getattr(args, variant))}
                               for variant in variants(args)}
        for shape in args.shapes:
            for repeat in range(1, args.repeats + 1):
                order = variants(args) if repeat % 2 else tuple(reversed(variants(args)))
                for variant in order:
                    root = args.output / (shape + "-" + str(repeat).zfill(2) + "-" + variant)
                    run = run_once(getattr(args, variant), root, args, shape, variant, repeat)
                    data["runs"].append(run)
                    dump(args.output / "results.json", data)
                    print(shape, variant, "repeat=" + str(repeat), run["status"],
                          "readyMs=" + str(run["metrics"].get("readyMs")))
                    # Never accumulate histories or more processes after cleanup failure.
                    if any("cleanup" in error["phase"] or "cleanup" in error["category"]
                           for error in run["errors"]):
                        raise smoke.Problem("cleanup", "cleanup failed; stopping further generation")
        data["summaries"] = summarize(data["runs"], args)
        data["status"] = "PASS" if (all(run["status"] == "PASS" for run in data["runs"]) and
            all(item["gate"]["status"] == "PASS" for item in data["summaries"]
                if item["gate"]["gatesExit"])) else "FAIL"
        # This difference is observational, not a causal measure of upgrade cost.
        data["legacyMinusModernReadyMedianMs"] = {}
        for variant in variants(args):
            medians = {item["shape"]: item["metrics"]["readyMs"]["median"] for item in data["summaries"]
                       if item["variant"] == variant and item["metrics"]["readyMs"]}
            if "legacy" in medians and "modern" in medians:
                data["legacyMinusModernReadyMedianMs"][variant] = medians["legacy"] - medians["modern"]
    except (Exception, KeyboardInterrupt) as error:
        data["status"] = "FAIL"
        data["fatalError"] = {"type": type(error).__name__, "message": str(error)}
        data["summaries"] = summarize(data["runs"], args)
    finally:
        dump(args.output / "results.json", data)
    print(data["status"] + ": " + str(args.output / "results.json"))
    return 0 if data["status"] == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
