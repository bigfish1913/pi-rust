#!/usr/bin/env python3
"""Smoke-test the native JSONL RPC host, without the web bridge.

    cargo build -p rpi-cli --bin rpi
    RPI_BIN=$PWD/target/debug/rpi python3 scripts/rpc-smoke.py

Covers the parts of the protocol a UI depends on but cannot fake: compaction
verdicts, the compaction marker inside `get_messages`, and the crash/resume
handshake. Run it somewhere a `kill -9` is harmless (WSL or a container) — a
killed host is exactly the state `resume_pending` exists to repair, and faking
that state from a fixture is how you end up testing the fixture.
"""
import json
import os
import queue
import shutil
import signal
import subprocess
import sys
import threading
import time
import uuid

BIN = os.environ.get("RPI_BIN", os.path.join(os.getcwd(), "target/debug/rpi"))
HOME = os.environ.get("RPI_HOME", os.path.expanduser("~"))
ROOT = os.environ.get("RPI_SMOKE_ROOT", "/tmp/rpi-rpc-smoke")
PROJECT = os.path.join(ROOT, "project")
SESSIONS = os.path.join(ROOT, "sessions")


def fresh(session_id=None):
    session_id = session_id or str(uuid.uuid4())
    os.makedirs(PROJECT, exist_ok=True)
    os.makedirs(SESSIONS, exist_ok=True)
    return session_id


class Host:
    def __init__(self, session_id):
        env = os.environ.copy()
        env["HOME"] = HOME
        self.log = open(os.path.join(ROOT, f"{session_id[:8]}.log"), "ab")
        self.proc = subprocess.Popen(
            [BIN, "--mode", "rpc", "--session-id", session_id, "--session-dir", SESSIONS],
            cwd=PROJECT,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.log,
            env=env,
            text=True,
            bufsize=1,
        )
        # A reader thread, not select(): the text stream buffers, so polling the
        # fd drops frames that are already decoded but not yet consumed.
        self.frames = queue.Queue()
        threading.Thread(target=self._pump, daemon=True).start()
        self.ready = self.read(timeout=60)
        assert self.ready and self.ready.get("type") == "ready", self.ready

    def _pump(self):
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                self.frames.put(json.loads(line))
            except json.JSONDecodeError:
                continue
        self.frames.put(None)

    def send(self, **payload):
        self.proc.stdin.write(json.dumps(payload) + "\n")
        self.proc.stdin.flush()

    def read(self, timeout):
        try:
            return self.frames.get(timeout=max(0.05, timeout))
        except queue.Empty:
            return None

    def wait(self, predicate, timeout):
        deadline = time.time() + timeout
        seen = []
        while time.time() < deadline:
            frame = self.read(timeout=max(0.5, deadline - time.time()))
            if frame is None:
                break
            seen.append(frame)
            if predicate(frame):
                return frame, seen
        return None, seen

    def call(self, kind, timeout=30, **extra):
        self.send(type=kind, id=str(uuid.uuid4()), **extra)
        frame, _ = self.wait(
            lambda f: f.get("type") == "response" and f.get("command") == kind, timeout
        )
        assert frame, f"no response for {kind}"
        return frame

    def kill9(self):
        os.kill(self.proc.pid, signal.SIGKILL)
        self.proc.wait(timeout=10)

    def stop(self):
        try:
            self.send(type="stop", id=str(uuid.uuid4()))
            self.proc.wait(timeout=20)
        except Exception:
            self.kill9()


def settled(host, timeout=180):
    """Wait for the settlement of the run just accepted.

    A retryable provider refusal is reported as the outcome rather than waited
    out: the retry ladder alone can outlast any sensible test budget.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        frame = host.read(timeout=max(1.0, deadline - time.time()))
        if frame is None:
            return None
        kind = frame.get("type")
        if kind == "retry_scheduled" and "429" in (frame.get("error") or ""):
            return {"type": "agent_settled", "outcome": "failed", "error": frame.get("error")}
        if kind == "agent_settled":
            return frame
    return None


def short(value):
    text = value or ""
    return text.splitlines()[0][:120] if text else ""


def check(name, ok, detail=""):
    print(f"{'PASS' if ok else 'FAIL'}  {name}{('  ' + detail) if detail else ''}")
    return ok


def main():
    shutil.rmtree(ROOT, ignore_errors=True)
    results = []

    # ---- T1: compact, and the compaction marker in the snapshot -------------
    sid = fresh()
    host = Host(sid)
    accepted = host.call("prompt", message="Reply with exactly: alpha")
    run = settled(host)
    # A provider quota wall is not a protocol failure; say so and stop rather
    # than reporting six misleading FAILs.
    if run and run.get("outcome") == "failed" and "429" in (run.get("error") or ""):
        print(f"SKIP  provider refused the run: {short(run.get('error'))}")
        host.stop()
        return 0
    results.append(check("T1 prompt settles", bool(run) and run.get("outcome") == "completed",
                         f'outcome={run and run.get("outcome")}'))

    accepted = host.call("compact")
    results.append(check("T1 compact accepted", accepted["success"] is True))
    done = settled(host)
    results.append(check("T1 compact settles as compacted",
                         bool(done) and done.get("outcome") == "compacted",
                         f'outcome={done and done.get("outcome")} summary={done and done.get("summary")!r}'))

    snap = host.call("get_messages")["data"]
    markers = [m for m in snap["messages"] if m.get("role") == "compaction"]
    results.append(check("T1 snapshot carries a compaction marker", len(markers) == 1,
                         f'markers={len(markers)} tokensBefore={markers and markers[0]["tokensBefore"]}'))
    results.append(check("T1 compact clears pendingResume", snap["pendingResume"] is False))

    again = host.call("compact")
    declined = settled(host)
    results.append(check("T1 second compact declines",
                         again["success"] is True and (declined or {}).get("outcome") == "declined",
                         f'summary={(declined or {}).get("summary")!r}'))
    host.stop()

    # ---- T2: real crash while the provider is still thinking ----------------
    sid = fresh()
    host = Host(sid)
    host.call("prompt", message="Write a 400 word essay about rivers.")
    time.sleep(0.4)
    host.kill9()

    host = Host(sid)
    snap = host.call("get_messages")["data"]
    results.append(check("T2 a crashed run leaves pendingResume",
                         snap["pendingResume"] is True,
                         f'pendingResume={snap["pendingResume"]} messages={len(snap["messages"])}'))
    resumed = host.call("resume_pending")
    results.append(check("T2 resume_pending accepted",
                         resumed["success"] is True and resumed["data"].get("resumed") is not False,
                         f'data={resumed["data"]}'))
    run = settled(host, timeout=240)
    results.append(check("T2 resumed run settles", bool(run) and run.get("outcome") == "completed",
                         f'outcome={run and run.get("outcome")}'))
    snap = host.call("get_messages")["data"]
    results.append(check("T2 pendingResume cleared after resume",
                         snap["pendingResume"] is False,
                         f'messages={len(snap["messages"])}'))
    host.stop()

    # ---- T3: nothing pending is refused, not faked --------------------------
    sid = fresh()
    host = Host(sid)
    empty = host.call("resume_pending")
    results.append(check("T3 resume_pending refuses an empty session",
                         empty["success"] is True and empty["data"].get("resumed") is False,
                         f'data={empty["data"]}'))
    host.stop()

    print()
    print(f"{sum(1 for r in results if r)}/{len(results)} checks passed")
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
