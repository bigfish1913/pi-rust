#!/usr/bin/env python3
"""Isolated shared-Harness HTTP/tool concurrency baseline (not a production host).

Build --release -p rpi-harness --example shared_host_probe first. The asyncio
HTTP fixture is outside the measured Rust process group. Only fresh output
roots are accepted; no provider credentials, user sessions or extensions load.
"""
import argparse
import asyncio
import contextlib
import ctypes
import functools
import json
import os
from pathlib import Path
import platform
import resource
import signal
import subprocess
import sys
import time

sys.dont_write_bytecode = True


def positive(value):
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def nonnegative(value):
    number = int(value)
    if number < 0:
        raise argparse.ArgumentTypeError("must be non-negative")
    return number


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output-root", required=True, type=Path)
    parser.add_argument("--agents", type=positive, default=8)
    parser.add_argument("--mode", choices=("empty", "idle", "model", "raw", "tool"), default="tool")
    parser.add_argument("--tool-kind", choices=("async", "bash"), default="async")
    parser.add_argument("--context-bytes", type=nonnegative, default=1024)
    parser.add_argument("--history-entries", type=nonnegative, default=0)
    parser.add_argument("--workers", type=positive, default=4)
    parser.add_argument("--max-blocking-threads", type=positive,
                        help="Tokio blocking-I/O pool budget (default: probe/Tokio 512); not Agent slots")
    parser.add_argument("--stagger-ms", type=nonnegative, default=0,
                        help="spread raw-mode connection starts; complete-request barrier unchanged")
    parser.add_argument("--hold-seconds", type=positive, default=2)
    parser.add_argument("--timeout", type=positive, default=120)
    parser.add_argument("--cgroup", type=Path,
                        help="fresh, empty, delegated Linux cgroup v2; only probe/children enroll")
    args = parser.parse_args(argv)
    if args.agents > 10_000:
        parser.error("--agents must not exceed 10000")
    if os.name != "posix" or platform.system() not in ("Linux", "Darwin"):
        parser.error("measurement/cleanup currently supports Linux and macOS only")
    try:
        args.binary = args.binary.resolve(strict=True)
    except OSError as error:
        parser.error(str(error))
    if not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("--binary must be executable")
    args.output_root = args.output_root.absolute()
    if args.output_root.exists() or args.output_root.is_symlink():
        parser.error("--output-root must not exist (no reuse or overwrite)")
    if not args.output_root.parent.is_dir():
        parser.error("--output-root parent must already exist")
    if args.cgroup:
        if platform.system() != "Linux":
            parser.error("--cgroup requires Linux cgroup v2")
        try:
            args.cgroup = args.cgroup.resolve(strict=True)
            if (args.cgroup / "cgroup.procs").read_text().strip():
                parser.error("--cgroup must be empty; controller/service must remain outside it")
            events = dict(line.split() for line in (args.cgroup / "cgroup.events").read_text().splitlines())
            if events.get("populated") != "0" or int((args.cgroup / "memory.peak").read_text()) != 0:
                parser.error("--cgroup must be fresh, unpopulated (including descendants), peak=0")
            for name in ("memory.current", "memory.peak", "memory.stat", "memory.events"):
                (args.cgroup / name).read_text()
        except OSError as error:
            parser.error(str(error))
    return args


@functools.lru_cache(maxsize=1)
def darwin_proc_pidinfo():
    function = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True).proc_pidinfo
    function.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
    function.restype = ctypes.c_int
    return function


@functools.lru_cache(maxsize=1)
def darwin_proc_pid_rusage():
    function = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True).proc_pid_rusage
    function.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
    function.restype = ctypes.c_int
    return function


def darwin_footprint(pid):
    """Physical footprint from libproc rusage.

    macOS RSS and physical footprint account for memory differently, notably
    compressed pages. Retain both rather than inferring live allocations from
    RSS alone. RUSAGE_INFO_V0 is the stable leading layout: uuid[16], then ten
    uint64 fields; resident_size and phys_footprint precede two abstime fields.
    """
    class RusageInfoV0(ctypes.Structure):
        _fields_ = [("uuid", ctypes.c_uint8 * 16),
                    ("user_time", ctypes.c_uint64),
                    ("system_time", ctypes.c_uint64),
                    ("pkg_idle_wkups", ctypes.c_uint64),
                    ("interrupt_wkups", ctypes.c_uint64),
                    ("pageins", ctypes.c_uint64),
                    ("wired_size", ctypes.c_uint64),
                    ("resident_size", ctypes.c_uint64),
                    ("phys_footprint", ctypes.c_uint64),
                    # v0 ends with two abstime fields; omitting them lets the
                    # kernel write past the buffer and corrupts the heap.
                    ("proc_start_abstime", ctypes.c_uint64),
                    ("proc_exit_abstime", ctypes.c_uint64)]

    info = RusageInfoV0()
    if darwin_proc_pid_rusage()(pid, 0, ctypes.byref(info)) != 0:
        raise OSError(ctypes.get_errno(), "proc_pid_rusage failed")
    return {"hostFootprintBytes": info.phys_footprint,
            "hostResidentBytes": info.resident_size}


def darwin_resources(pid):
    """libproc counters: no lsof child or process-wide environment mutations."""
    class TaskInfo(ctypes.Structure):
        # sys/proc_info.h: proc_taskinfo (6 uint64 + 12 int32), Darwin ARM/x86.
        _fields_ = [("times", ctypes.c_uint64 * 6), ("counters", ctypes.c_int32 * 12)]

    function = darwin_proc_pidinfo()
    result = {}
    info = TaskInfo()
    if function(pid, 4, 0, ctypes.byref(info), ctypes.sizeof(info)) == ctypes.sizeof(info):
        result["hostThreads"] = info.counters[9]  # pti_threadnum
    result.update(darwin_footprint(pid))
    fd_size = 8  # sizeof(proc_fdinfo): uint32 fd, uint32 type
    size = function(pid, 1, 0, None, 0)
    for _ in range(3):
        if size <= 0:
            break
        buffer = ctypes.create_string_buffer(size + 32 * fd_size)
        count = function(pid, 1, 0, buffer, len(buffer))
        if 0 < count < len(buffer):
            result["hostFds"] = count // fd_size
            break
        size = len(buffer) * 2
    return result


def sample_process(pid):
    """Sum RSS is NOT physical memory: shared pages can be double-counted."""
    table = subprocess.run(
        ["ps", "-axo", "pid=,pgid=,rss="], capture_output=True, text=True, check=True)
    rows = [tuple(map(int, line.split())) for line in table.stdout.splitlines() if line.strip()]
    group = [(p, rss) for p, pgid, rss in rows if pgid == pid]
    result = {
        "monotonicNs": time.monotonic_ns(),
        "hostRssBytes": next((rss * 1024 for p, rss in group if p == pid), None),
        "processGroupRssSumBytes": sum(rss * 1024 for _, rss in group),
        "processGroupProcesses": len(group),
        "method": "ps RSS; group sum includes tools, may double-count shared pages",
    }
    if platform.system() == "Darwin":
        try:
            result.update(darwin_resources(pid))
        except (OSError, AttributeError) as error:
            result["resourceSamplingError"] = str(error)
    if platform.system() == "Linux":
        try:
            fields = {}
            for line in Path(f"/proc/{pid}/status").read_text().splitlines():
                if ":" in line:
                    key, value = line.split(":", 1)
                    fields[key] = value.strip()
            result["hostPeakRssBytes"] = int(fields["VmHWM"].split()[0]) * 1024
            result["hostThreads"] = int(fields["Threads"])
            result["hostFds"] = len(list(Path(f"/proc/{pid}/fd").iterdir()))
        except (OSError, KeyError):
            pass  # Child may have exited between ps and /proc; report that, not zero.
    return result


def sample_cgroup(path):
    result = {"currentBytes": int((path / "memory.current").read_text()),
              "peakBytes": int((path / "memory.peak").read_text())}
    for name in ("memory.stat", "memory.events"):
        result[name] = {key: int(value) for key, value in
                        (line.split() for line in (path / name).read_text().splitlines())}
    return result


class FixtureServer:
    """HTTP/1.1 fixture; counts complete request bodies, never just connections."""
    def __init__(self, agents, mode, max_body_bytes):
        self.agents = agents
        self.mode = mode
        self.max_body_bytes = max_body_bytes
        self.first = set()
        self.final = set()
        self.active = 0
        self.peak = 0
        self.requests = 0
        self.tool_results = set()
        self.ready = asyncio.Event()
        self.release = asyncio.Event()
        self.errors = []
        self.tasks = set()
        self.writers = set()
        self.closing = False
        self.server = None

    async def start(self):
        self.server = await asyncio.start_server(self.accept, "127.0.0.1", 0, limit=65536,
                                                 backlog=max(128, self.agents))
        port = self.server.sockets[0].getsockname()[1]
        return f"http://127.0.0.1:{port}/v1"

    async def close(self):
        self.closing = True
        self.release.set()
        if self.server:
            self.server.close()
            await self.server.wait_closed()
        for writer in list(self.writers):
            writer.close()
        for task in list(self.tasks):
            task.cancel()
        await asyncio.gather(*list(self.tasks), return_exceptions=True)

    async def accept(self, reader, writer):
        task = asyncio.current_task()
        self.tasks.add(task)
        self.writers.add(writer)
        counted = False
        try:
            header = (await reader.readuntil(b"\r\n\r\n")).decode("ascii")
            lines = header.split("\r\n")
            if lines[0] != "POST /v1/chat/completions HTTP/1.1":
                raise ValueError("unexpected fixture endpoint")
            headers = {}
            for line in lines[1:]:
                if line:
                    key, value = line.split(":", 1)
                    key = key.lower()
                    if key in headers:
                        raise ValueError("duplicate HTTP header")
                    headers[key] = value.strip()
            length = int(headers.get("content-length", "0"))
            if "transfer-encoding" in headers or not 0 < length <= self.max_body_bytes:
                raise ValueError("missing/oversized Content-Length or unsupported transfer encoding")
            body = json.loads(await reader.readexactly(length))
            agent, second = self.validate_request(body)
            self.requests += 1
            self.active += 1
            self.peak = max(self.peak, self.active)
            counted = True
            if second:
                if self.mode != "tool" or agent not in self.first or agent in self.final:
                    raise ValueError("unexpected/duplicate follow-up")
                self.final.add(agent)
                self.tool_results.add(agent)
            else:
                if agent in self.first:
                    raise ValueError("duplicate first request/retry")
                self.first.add(agent)
                if len(self.first) == self.agents:
                    self.ready.set()
                await self.release.wait()
                if self.mode in ("model", "raw"):
                    self.final.add(agent)
            response = self.response(agent, tool=self.mode == "tool" and not second)
            writer.write(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n"
                + f"Content-Length: {len(response)}\r\n\r\n".encode() + response)
            await writer.drain()
            # Half-close and drain before closing: writing an RST while the peer
            # is still sending (a pooled connection being reused) turns a normal
            # close into ECONNRESET and makes the fixture itself flaky.
            if not writer.is_closing():
                writer.write_eof()
                with contextlib.suppress(Exception):
                    await asyncio.wait_for(reader.read(), 2)
        except (Exception,) as error:
            if not self.closing:
                self.errors.append(f"{type(error).__name__}: {error}")
                with contextlib.suppress(Exception):
                    writer.write(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    await writer.drain()
        finally:
            if counted:
                self.active -= 1
            writer.close()
            with contextlib.suppress(Exception):
                await writer.wait_closed()
            self.writers.discard(writer)
            self.tasks.discard(task)

    def validate_request(self, body):
        if body.get("model") != "probe" or body.get("stream") is not True:
            raise ValueError("unexpected model or non-streaming request")
        users = [m for m in body.get("messages", []) if m.get("role") == "user"]
        if not users:
            raise ValueError("missing user prompt")
        content = users[-1]["content"]
        if isinstance(content, list):
            content = "".join(block.get("text", "") for block in content)
        marker = content.split("\n", 1)[0]
        if not marker.startswith("probe-agent:"):
            raise ValueError("missing agent marker")
        agent = int(marker.removeprefix("probe-agent:"))
        if not 0 <= agent < self.agents:
            raise ValueError("agent outside configured range")
        results = [m for m in body["messages"] if m.get("role") == "tool"]
        if results and (len(results) != 1 or results[0].get("tool_call_id") != f"call-{agent}"
                        or results[0].get("content") != f"tool completed {agent}"):
            raise ValueError("wrong/missing/cross-agent tool result")
        return agent, bool(results)

    @staticmethod
    def response(agent, tool=False):
        if tool:
            delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": f"call-{agent}",
                     "type": "function", "function": {"name": "probe_barrier",
                     "arguments": json.dumps({"agent": agent})}}]}
        else:
            delta = {"role": "assistant", "content": f"completed {agent}"}
        chunks = [
            {"id": f"response-{agent}", "model": "probe", "choices": [
                {"index": 0, "delta": delta, "finish_reason": None}]},
            {"id": f"response-{agent}", "model": "probe", "choices": [
                {"index": 0, "delta": {}, "finish_reason": "tool_calls" if tool else "stop"}]},
        ]
        return ("".join("data: " + json.dumps(chunk) + "\n\n" for chunk in chunks)
                + "data: [DONE]\n\n").encode()

    def summary(self):
        return {"requests": self.requests, "firstRequests": len(self.first),
                "finalResponses": len(self.final), "toolResults": len(self.tool_results),
                "peakInFlight": self.peak, "errors": list(self.errors),
                "protocol": "HTTP/1.1, loopback, no TLS, Connection: close",
                "measurementBoundary": "fixture server excluded; separate controller process"}


def validate_result(args, phases, server):
    names = [p["phase"] for p in phases]
    expected = ["ready"]
    if args.mode == "tool":
        expected += ["tools_held"]
    expected += ["settled", "closed", "complete"]
    if names != expected or phases[-1].get("success") is not True:
        raise ValueError(f"incomplete/invalid probe phases: {names}")
    settled = next(p for p in phases if p.get("phase") == "settled")
    if any(p.get("agents") != args.agents for p in (phases[0], settled)):
        raise ValueError("wrong agent count")
    active = args.mode in ("model", "raw", "tool")
    if settled["completed"] != (args.agents if active else 0):
        raise ValueError("not all agents completed")
    if server.errors:
        raise ValueError(f"fixture errors: {server.errors[:3]}")
    n = args.agents if active else 0
    requests = n * (2 if args.mode == "tool" else 1)
    if (len(server.first), len(server.final), server.requests) != (n, n, requests):
        raise ValueError("request count/final response mismatch")
    if active and server.peak < args.agents:
        raise ValueError("requested model concurrency not reached")
    tools = args.agents if args.mode == "tool" else 0
    if (settled["toolEntered"], settled["toolPeak"], len(server.tool_results)) != (tools, tools, tools):
        raise ValueError("requested tool concurrency/result count not reached")
    if args.mode == "tool" and (phases[1].get("entered"), phases[1].get("live")) != (tools, tools):
        raise ValueError("tool barrier readiness count mismatch")


async def stop_process_group(process):
    """Kill descendants even if the primary process has already exited."""
    if process is None:
        return
    async def signal_group(sig):
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            pass
        except PermissionError:
            # Some macOS sandboxes return EPERM rather than ESRCH for a group
            # that has already disappeared. Never suppress a live-group denial.
            sample = await asyncio.to_thread(sample_process, process.pid)
            if sample["processGroupProcesses"]:
                raise

    await signal_group(signal.SIGTERM)
    try:
        await asyncio.wait_for(process.wait(), 3)
    except asyncio.TimeoutError:
        pass
    await signal_group(signal.SIGKILL)
    await process.wait()


async def run(args):
    # Atomic exclusive creation also protects against races after parse_args.
    args.output_root.mkdir(parents=False, exist_ok=False)
    server = FixtureServer(args.agents, args.mode,
                           max(1024 * 1024, 6 * args.context_bytes * (args.history_entries + 1) + 65536))
    process = None
    monitors = []
    phases, samples = [], []
    result = {"success": False, "configuration": {
        key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
        "environment": platform.platform(),
        "resourceLimits": {"nofile": resource.getrlimit(resource.RLIMIT_NOFILE),
                           "nproc": resource.getrlimit(resource.RLIMIT_NPROC)},
        "phases": phases, "memorySamples": samples,
        "notes": ["synthetic fixture, not arbitrary coding workloads", "no extensions or TLS",
                  "no 1 GB acceptance claim; RSS sum is not cgroup physical memory",
                  "model and tool concurrency are proven in separate causal phases"]}
    log = args.output_root / "phases.jsonl"
    stderr = (args.output_root / "probe.stderr").open("wb")

    async def record(value):
        with log.open("a", encoding="utf8") as output:
            output.write(json.dumps(value) + "\n")

    async def memory(phase):
        sample = await asyncio.to_thread(sample_process, process.pid)
        sample["phase"] = phase
        sample["fixtureInFlight"] = server.active
        sample["fixtureFirstRequests"] = len(server.first)
        sample["fixtureRequests"] = server.requests
        if args.cgroup:
            sample["cgroup"] = sample_cgroup(args.cgroup)
        samples.append(sample)
        # Persist incrementally so a controller crash cannot erase all samples.
        with (args.output_root / "memory.jsonl").open("a", encoding="utf8") as output:
            output.write(json.dumps(sample) + "\n")
        return sample

    async def send(command):
        process.stdin.write((command + "\n").encode())
        await process.stdin.drain()

    async def observe_requests():
        await server.ready.wait()
        if server.active != args.agents:
            raise ValueError("not all request handlers remain in flight")
        await record({"phase": "requests_held", "requests": len(server.first), "live": server.active})
        await memory("requests_held")
        await asyncio.sleep(args.hold_seconds)
        server.release.set()

    async def sample_periodically():
        while process.returncode is None:
            await memory("periodic")
            await asyncio.sleep(0.2)

    async def exercise():
        if args.cgroup:
            result["cgroupBefore"] = sample_cgroup(args.cgroup)
        base_url = await server.start()
        command = [str(args.binary), "--output-root", str(args.output_root / "fixture"),
                   "--base-url", base_url]
        for key in ("agents", "mode", "tool_kind", "context_bytes", "history_entries", "workers",
                    "stagger_ms", "timeout"):
            command += ["--" + key.replace("_", "-"), str(getattr(args, key))]
        if args.max_blocking_threads is not None:
            command += ["--max-blocking-threads", str(args.max_blocking_threads)]
        if args.cgroup:
            command += ["--cgroup", str(args.cgroup)]
        nonlocal process
        process = await asyncio.create_subprocess_exec(
            *command, stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
            stderr=stderr, start_new_session=True)
        monitors.append(asyncio.create_task(sample_periodically()))
        while line := await process.stdout.readline():
            value = json.loads(line)
            await record(value)
            phase = value.get("phase")
            if phase == "setup_progress":
                continue
            phases.append(value)
            if phase == "ready":
                await memory("ready")
                await asyncio.sleep(args.hold_seconds)
                if args.mode in ("model", "raw", "tool"):
                    monitors.append(asyncio.create_task(observe_requests()))
                await send("run")
            elif phase == "tools_held":
                if value.get("entered") != args.agents or value.get("live") != args.agents:
                    raise ValueError("not all tools reached internal barrier")
                sample = await memory("tools_held")
                if args.tool_kind == "bash" and sample["processGroupProcesses"] < args.agents + 1:
                    raise ValueError("bash tools reported ready but live process group count is too small")
                await asyncio.sleep(args.hold_seconds)
                await send("release_tools")
            elif phase == "closed":
                await memory("closed")
                await send("closed_sampled")
            elif phase == "settled":
                await memory("settled")
                await asyncio.sleep(args.hold_seconds)
                await send("finish")
            elif phase == "error":
                raise ValueError(value.get("error", "probe failed"))
            elif phase != "complete":
                raise ValueError(f"unexpected probe phase {phase}")
            for task in monitors:
                if task.done():
                    task.result()  # Surface sampler/barrier errors, never silently ignore them.
        code = await process.wait()
        if code:
            raise ValueError(f"probe exit {code}; see probe.stderr")
        validate_result(args, phases, server)
        result["success"] = True

    try:
        await asyncio.wait_for(exercise(), args.timeout + 5)
    except Exception as error:
        result["error"] = f"{type(error).__name__}: {error}"
    finally:
        for task in monitors:
            task.cancel()
        await asyncio.gather(*monitors, return_exceptions=True)
        cleanup_errors = []
        try:
            await stop_process_group(process)
        except Exception as error:
            cleanup_errors.append(f"process group: {type(error).__name__}: {error}")
        try:
            await server.close()
        except Exception as error:
            cleanup_errors.append(f"fixture server: {type(error).__name__}: {error}")
        stderr.close()
        result["fixtureServer"] = server.summary()
        if args.cgroup:
            try:
                result["cgroupAfter"] = sample_cgroup(args.cgroup)
                before = result["cgroupBefore"]["memory.events"]
                after = result["cgroupAfter"]["memory.events"]
                if any(after.get(key, 0) > before.get(key, 0) for key in ("oom", "oom_kill")):
                    result["success"] = False
                    result["error"] = "measurement cgroup observed OOM"
            except Exception as error:
                cleanup_errors.append(f"cgroup metrics: {type(error).__name__}: {error}")
        if cleanup_errors:
            result["success"] = False
            result["cleanupErrors"] = cleanup_errors
        host = [s["hostRssBytes"] for s in samples if s["hostRssBytes"] is not None]
        result["sampledPeakHostRssBytes"] = max(host, default=None)
        result["sampledPeakGroupRssSumBytes"] = max(
            (s["processGroupRssSumBytes"] for s in samples), default=None)
        result["sampledPeakGroupProcesses"] = max(
            (s["processGroupProcesses"] for s in samples), default=None)
        result["sampledPeakHostFootprintBytes"] = max(
            (s.get("hostFootprintBytes") or 0 for s in samples), default=None)
        for key in ("hostThreads", "hostFds"):
            result["sampledPeak" + key[0].upper() + key[1:]] = max(
                (s[key] for s in samples if key in s), default=None)
        (args.output_root / "results.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf8")
    return result


def main():
    args = parse_args()
    result = asyncio.run(run(args))
    print(json.dumps({key: value for key, value in result.items() if key not in ("memorySamples", "phases")}))
    return 0 if result["success"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
