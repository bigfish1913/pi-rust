#!/usr/bin/env python3
"""Real native JSONL external-tool smoke; Python 3 standard library only.

Run with an explicitly built Stage3 binary (never searches PATH or builds):
  python -B scripts/external-tools-smoke.py --rpi <target/debug/rpi.exe> \
      --diagnostics-dir <Cargo-target/acp-stage3-01/smoke-worker-unique>

All homes, configuration, sessions, temporary files and wire/provider logs are
retained under the required diagnostics directory. No real credentials, external
network, ACP bridge, installed extensions, or cross-repository helpers are used.
The caller must choose a new diagnostics directory underneath its Cargo target.
"""
import argparse
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import threading
import time
import traceback
import uuid


TOOL = "external_fixture"
ARGS = {"query": "preserve / spaces / 雪", "nested": {"items": [1, True, None, "x"]}}
TEXT = "EXTERNAL_TEXT_CANARY_7b301"
STRUCTURED = {"answer": 42, "nested": {"preserved": True}, "unicode": "雪"}
PRIVATE = "PRIVATE_METADATA_MUST_NOT_REACH_PROVIDER_90f"
RAW_RESULT = {
    "content": [{"type": "text", "text": TEXT,
                 "annotations": {"private": PRIVATE}, "textSignature": PRIVATE}],
    "structuredContent": STRUCTURED,
    "privateMetadata": {"secret": PRIVATE, "retained": [1, 2, 3]},
    "isError": False,
}
DEFINITION = {
    "name": TOOL, "description": "Loopback smoke external lookup",
    "inputSchema": {"type": "object", "properties": {
        "query": {"type": "string"}, "nested": {"type": "object"}},
        "required": ["query", "nested"], "additionalProperties": False},
}
TIMEOUT = 25


def dump(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2), encoding="utf-8")


def content_text(value):
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        return "\n".join(part.get("text", "") for part in value if isinstance(part, dict))
    return ""


def objects(value):
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from objects(child)
    elif isinstance(value, list):
        for child in value:
            yield from objects(child)


class Model:
    """Single owned serving thread; blocked requests are explicitly releasable."""
    def __init__(self, root):
        self.root = root
        self.calls, self.errors = [], []
        self.entered, self.release = threading.Event(), threading.Event()
        self.stopping = threading.Event()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def setup(self):
                super().setup()
                self.connection.settimeout(10)

            def log_message(self, *_args):
                pass

            def do_GET(self):
                owner.errors.append("unexpected GET " + self.path)
                self.send_error(404)

            def do_POST(self):
                try:
                    assert self.path == "/v1/chat/completions", self.path
                    size = int(self.headers["Content-Length"])
                    assert 0 < size < 4 * 1024 * 1024, size
                    body = json.loads(self.rfile.read(size))
                    owner.calls.append(body)
                    dump(root / "provider-requests.json", owner.calls)
                    if owner.stopping.is_set():
                        self.close_connection = True
                        return
                    assert body["model"] == "synthetic", body.get("model")
                    assert PRIVATE not in json.dumps(body), "private metadata leaked to provider"
                    messages = body["messages"]
                    last_user = next(m for m in reversed(messages) if m["role"] == "user")
                    prompt = content_text(last_user.get("content"))
                    tool = prompt.startswith("TOOL_") and messages[-1]["role"] != "tool"
                    if tool:
                        functions = {t["function"]["name"]: t["function"] for t in body["tools"]}
                        assert TOOL in functions, functions
                        assert functions[TOOL]["parameters"] == DEFINITION["inputSchema"]
                        assert functions[TOOL]["description"] == DEFINITION["description"]
                        assert "atomic_should_not_install" not in functions, functions
                        delta = {"role": "assistant", "tool_calls": [{
                            "index": 0, "id": "fixture-" + uuid.uuid4().hex,
                            "type": "function", "function": {"name": TOOL,
                            "arguments": json.dumps(ARGS, ensure_ascii=False)}}]}
                        reason = "tool_calls"
                    else:
                        if prompt == "TOOL_ROUNDTRIP":
                            results = [m for m in messages if m["role"] == "tool"]
                            assert results, messages
                            text = content_text(results[-1].get("content"))
                            assert TEXT in text, text
                            # Check an explicit JSON text projection, not mere key substrings.
                            decoder = json.JSONDecoder()
                            values = []
                            for index, char in enumerate(text):
                                if char == "{":
                                    try:
                                        values.append(decoder.raw_decode(text[index:])[0])
                                    except ValueError:
                                        pass
                            assert STRUCTURED in values, text
                        if prompt == "MODEL_ONLY_SLOW":
                            owner.entered.set()
                            assert owner.release.wait(TIMEOUT), "slow model not released"
                        delta = {"role": "assistant", "content": "FIXTURE_COMPLETED:" + prompt}
                        reason = "stop"
                    events = [
                        {"choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                        {"choices": [{"index": 0, "delta": {}, "finish_reason": reason}],
                         "usage": {"prompt_tokens": 50, "completion_tokens": 10, "total_tokens": 60}},
                    ]
                    data = ("".join("data: " + json.dumps(e) + "\n\n" for e in events)
                            + "data: [DONE]\n\n").encode("utf-8")
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                    pass  # Native cancellation may close a loopback request.
                except Exception:
                    owner.errors.append(traceback.format_exc())
                    dump(root / "provider-errors.json", owner.errors)
                    self.close_connection = True

        self.server = HTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=lambda: self.server.serve_forever(poll_interval=.05),
                                       name="external-smoke-provider")
        self.thread.start()

    def close(self):
        self.stopping.set()
        self.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=15)
        assert not self.thread.is_alive(), "provider thread did not join"
        count = len(self.calls)
        time.sleep(.1)
        assert len(self.calls) == count, "provider was not quiescent after shutdown"
        assert not self.errors, self.errors


def environment(root, model):
    # Positive allowlist: no inherited auth, providers, proxies, config or RPI_*.
    allowed = {"SYSTEMROOT", "WINDIR", "COMSPEC", "PATH", "PATHEXT", "SYSTEMDRIVE",
               "PROCESSOR_ARCHITECTURE", "NUMBER_OF_PROCESSORS"}
    env = {k: v for k, v in os.environ.items() if k.upper() in allowed}
    for name in ("home", "agent", "tmp", "sessions", "workspace", "appdata", "localappdata"):
        (root / name).mkdir()
    home = root / "home"
    env.update(HOME=str(home), USERPROFILE=str(home), APPDATA=str(root / "appdata"),
               LOCALAPPDATA=str(root / "localappdata"), XDG_CONFIG_HOME=str(home / ".config"),
               XDG_CACHE_HOME=str(home / ".cache"), XDG_DATA_HOME=str(home / ".local/share"),
               TEMP=str(root / "tmp"), TMP=str(root / "tmp"), TMPDIR=str(root / "tmp"),
               RPI_CODING_AGENT_DIR=str(root / "agent"), RPI_OFFLINE="1", NO_COLOR="1")
    dump(root / "agent" / "auth.json", {})
    dump(root / "agent" / "settings.json", {
        "defaultProvider": "fixture", "defaultModel": "synthetic", "defaultThinkingLevel": "off"})
    dump(root / "agent" / "models.json", {"providers": {"fixture": {
        "api": "openai-completions", "baseUrl": f"http://127.0.0.1:{model.server.server_port}/v1",
        "apiKey": "local-dummy-key-not-a-credential", "models": [{
            "id": "synthetic", "name": "synthetic", "reasoning": False, "input": ["text"],
            "contextWindow": 100000, "maxTokens": 1024}]}}})
    return env


class Native:
    def __init__(self, rpi, root, encoding, model):
        self.root, self.frames, self.pending = root, [], []
        self.queue = queue.Queue()
        self.counter = 0
        self.stderr = (root / "native-stderr.log").open("w", encoding="utf-8")
        command = [str(rpi), "--mode", "rpc", "--provider", "fixture", "--model", "synthetic",
                   "--session-dir", str(root / "sessions"), "--session-id", str(uuid.uuid4()),
                   "--rpc-message-updates", encoding, "--no-extensions", "--no-skills",
                   "--no-prompt-templates", "--no-context-files", "--local-only"]
        dump(root / "native-command.json", command)
        try:
            self.process = subprocess.Popen(command, cwd=root / "workspace", env=environment(root, model),
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr,
                text=True, encoding="utf-8", errors="strict", bufsize=1)
        except BaseException:
            self.stderr.close()
            raise
        self.reader = threading.Thread(target=self.pump, name="external-smoke-stdout")
        self.reader.start()

    def pump(self):
        try:
            with (self.root / "native-stdout.jsonl").open("w", encoding="utf-8") as log:
                for line in self.process.stdout:
                    log.write(line)
                    log.flush()
                    if line.strip():
                        frame = json.loads(line)
                        assert isinstance(frame, dict), frame
                        self.frames.append(frame)
                        self.queue.put(frame)
        except BaseException as error:
            self.queue.put(error)
        finally:
            self.queue.put(None)

    def wait(self, predicate, timeout=TIMEOUT):
        deadline = time.monotonic() + timeout
        while True:
            for index, frame in enumerate(self.pending):
                if predicate(frame):
                    return self.pending.pop(index)
            remaining = deadline - time.monotonic()
            assert remaining > 0, "native frame deadline exceeded; see " + str(self.root)
            try:
                frame = self.queue.get(timeout=remaining)
            except queue.Empty:
                raise AssertionError("native frame timeout; see " + str(self.root)) from None
            assert frame is not None, "unexpected native stdout EOF; see " + str(self.root)
            if isinstance(frame, BaseException):
                raise frame
            self.pending.append(frame)

    def send(self, kind, **fields):
        self.counter += 1
        frame = dict(type=kind, id=f"controller-{self.counter}", **fields)
        with (self.root / "native-stdin.jsonl").open("a", encoding="utf-8") as log:
            log.write(json.dumps(frame, ensure_ascii=False) + "\n")
        self.process.stdin.write(json.dumps(frame, ensure_ascii=False) + "\n")
        self.process.stdin.flush()
        return frame["id"]

    def call(self, kind, ok=True, expected_error=None, **fields):
        request_id = self.send(kind, **fields)
        frame = self.wait(lambda f: f.get("type") == "response" and f.get("id") == request_id)
        assert frame.get("command") == kind and frame.get("success") is ok, frame
        if not ok:
            assert frame.get("error"), frame
            if expected_error:
                assert expected_error in frame["error"], frame
        return frame.get("data") if ok else frame

    def ready(self, encoding):
        ready = self.wait(lambda f: f.get("type") == "ready")
        assert ready["semantics"]["messageUpdates"] == encoding, ready
        assert {"configure_external_tools", "external_tool_response"} <= set(ready["supportedCommands"])

    def prompt(self, message):
        assert self.call("prompt", message=message) == {"accepted": True}

    def settled(self, outcome=None):
        frame = self.wait(lambda f: f.get("type") == "agent_settled")
        if outcome:
            assert frame.get("outcome") == outcome, frame
        return frame

    def eof(self):
        if not self.process.stdin.closed:
            self.process.stdin.close()

    def joined(self, timeout=TIMEOUT):
        self.eof()
        assert self.process.wait(timeout=timeout) == 0, "native exited nonzero; see " + str(self.root)
        self.reader.join(timeout=5)
        assert not self.reader.is_alive(), "native stdout thread did not join"
        while not self.queue.empty():
            frame = self.queue.get_nowait()
            if isinstance(frame, BaseException):
                raise frame

    def close(self):
        # Cleanup never relies on an unbounded stdin read, detached reader, or
        # the old two-second native shutdown watchdog. A failed test kills/reaps.
        try:
            self.eof()
            if self.process.poll() is None:
                try:
                    self.process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=10)
        finally:
            self.reader.join(timeout=10)
            self.process.stdout.close()
            self.stderr.close()
            assert not self.reader.is_alive(), "leaked native reader thread"
            assert self.process.poll() is not None, "leaked native process"


def external_request(host):
    frame = host.wait(lambda f: f.get("type") == "external_tool_request")
    assert frame["toolName"] == TOOL and frame["arguments"] == ARGS, frame
    assert isinstance(frame["requestId"], str) and frame["requestId"], frame
    assert not frame["requestId"].startswith("controller-"), frame
    return frame["requestId"]


def closed(host, request_id):
    host.wait(lambda f: f.get("type") == "external_tool_request_closed"
              and f.get("requestId") == request_id)


def roundtrip(host, model):
    baseline = host.call("get_available_tools")
    installed = host.call("configure_external_tools", tools=[DEFINITION])
    assert installed["tools"] == [TOOL], installed
    assert set(installed["activeTools"]) == set(baseline["activeTools"]) | {TOOL}, installed
    before = host.call("get_available_tools")
    candidate = dict(DEFINITION, name="atomic_should_not_install")
    for batch, error in [
        ([candidate, dict(DEFINITION, name="read")], "reserved external tool name: read"),
        ([candidate, candidate], "duplicate external tool name: atomic_should_not_install"),
        ([candidate, dict(DEFINITION, inputSchema={"type": "array"})], "inputSchema"),
        ([candidate, dict(DEFINITION, inputSchema={"type": "object", "$ref": "https://invalid.test/schema"})],
         "only root-local fragment $ref"),
        ([candidate, dict(DEFINITION, inputSchema={"type": "object", "$ref": "#"})],
         "reference cycles are not supported"),
    ]:
        host.call("configure_external_tools", ok=False, expected_error=error, tools=batch)
        assert host.call("get_available_tools") == before, "invalid batch mutated registry/active set"

    start, call_count = len(host.frames), len(model.calls)
    host.prompt("TOOL_ROUNDTRIP")
    request_id = external_request(host)
    # Admission remains busy while a real external request is pending.
    host.call("configure_external_tools", ok=False,
              expected_error="cannot configure external tools while a run is active", tools=[])
    host.call("prompt", ok=False, expected_error="a run is already active", message="MUST_NOT_RUN")
    for fields, error in [
        ({"result": {}}, "external result requires a content array"),
        ({"result": {"content": [{"type": "resource_link", "uri": "https://invalid.test/"}]}}, None),
        ({"result": {"content": [{"type": "text", "text": 42}]}}, None),
        ({"result": {"content": [], "structuredContent": []}}, "structuredContent must be an object"),
        ({"cancelled": False}, "cancelled must be true when present"),
        ({"result": RAW_RESULT, "error": "ambiguous"}, "requires exactly one"),
    ]:
        host.call("external_tool_response", ok=False, expected_error=error, requestId=request_id, **fields)
        assert len(model.calls) == call_count + 1, "malformed reply retired waiter / continued model"
        assert not any(f.get("type") == "external_tool_request_closed"
                       and f.get("requestId") == request_id for f in host.frames)
    assert host.call("external_tool_response", requestId=request_id, result=RAW_RESULT) == {"accepted": True}
    assert host.call("external_tool_response", requestId=request_id, result=RAW_RESULT) == {"accepted": False}
    assert host.call("external_tool_response", requestId="unknown-fixture-id", cancelled=True) == {"accepted": False}
    closed(host, request_id)
    host.settled("completed")
    assert len(model.calls) == call_count + 2, model.calls
    events = host.frames[start:]
    starts = [f for f in events if f.get("type") == "tool_execution_start" and f.get("toolName") == TOOL]
    ends = [f for f in events if f.get("type") == "tool_execution_end" and f.get("toolName") == TOOL]
    assert len(starts) == len(ends) == 1, events
    assert starts[0]["toolCallId"] == ends[0]["toolCallId"] and ends[0]["isError"] is False, ends
    snapshot = host.call("get_messages", limit=50)
    assert snapshot["hasMore"] is False, snapshot
    raw = [obj["details"]["externalToolResult"] for obj in objects(snapshot["messages"])
           if isinstance(obj.get("details"), dict) and "externalToolResult" in obj["details"]]
    assert RAW_RESULT in raw, "get_messages lost unmodified raw external result"

    # Abort an accepted prompt with a pending real request; late reply is inert.
    host.prompt("TOOL_ABORT")
    aborted_id = external_request(host)
    assert aborted_id != request_id
    host.call("abort")
    aborted_settlement = host.settled()
    closed(host, aborted_id)
    assert host.call("external_tool_response", requestId=aborted_id, result=RAW_RESULT) == {"accepted": False}
    host.prompt("AFTER_ABORT")
    host.settled("completed")
    assert content_text(model.calls[-1]["messages"][-1].get("content")) == "AFTER_ABORT", model.calls[-1]
    cleared = host.call("configure_external_tools", tools=[])
    assert cleared["tools"] == [] and set(cleared["activeTools"]) == set(baseline["activeTools"]), cleared
    assert host.call("get_available_tools") == baseline
    host.call("stop")
    host.joined()
    # Check last so even a misclassified native abort still exercises late reply,
    # recovery, registry clearing, normal stop and process/thread ownership.
    assert aborted_settlement.get("outcome") == "aborted", aborted_settlement


def eof_pending(host, _model):
    host.call("configure_external_tools", tools=[DEFINITION])
    host.prompt("TOOL_EOF_PENDING")
    request_id = external_request(host)
    host.eof()
    closed(host, request_id)
    host.settled()  # EOF disables external backend; do not prescribe a model stop reason.
    host.joined(timeout=15)  # Must not wait for the 60-second external-tool deadline.
    assert sum(f.get("type") == "agent_settled" for f in host.frames) == 1


def eof_model_only(host, model):
    host.prompt("MODEL_ONLY_SLOW")
    assert model.entered.wait(TIMEOUT), "ordinary prompt did not reach loopback provider"
    host.eof()
    # A deliberate >2.2-second post-EOF block detects the former watchdog.
    time.sleep(2.4)
    assert host.process.poll() is None, "native exited while accepted healthy prompt was blocked"
    assert not any(f.get("type") == "agent_settled" for f in host.frames), host.frames
    model.release.set()
    host.settled("completed")
    host.joined()
    assert len(model.calls) == 1, model.calls
    assert any("FIXTURE_COMPLETED:MODEL_ONLY_SLOW" in json.dumps(f) for f in host.frames)
    assert sum(f.get("type") == "agent_settled" for f in host.frames) == 1


def run(rpi, diagnostics):
    passed, failed = [], []
    for encoding in ("compact-deltas", "snapshot"):
        for name, scenario in (("roundtrip-abort-atomicity", roundtrip),
                               ("eof-pending", eof_pending), ("eof-model-only", eof_model_only)):
            root = diagnostics / (encoding + "-" + name)
            root.mkdir()
            model, host = Model(root), None
            label = encoding + ": " + name
            try:
                try:
                    host = Native(rpi, root, encoding, model)
                    host.ready(encoding)
                    scenario(host, model)
                finally:
                    model.stopping.set()
                    model.release.set()
                    try:
                        if host is not None:
                            host.close()
                    finally:
                        model.close()
            except Exception:
                failure = traceback.format_exc()
                (root / "failure.txt").write_text(failure, encoding="utf-8")
                failed.append({"scenario": label, "error": failure})
                print("FAIL " + label + "; see " + str(root / "failure.txt"), flush=True)
            else:
                passed.append(label)
                print("PASS " + label, flush=True)
            dump(diagnostics / "passed.json", passed)
            dump(diagnostics / "results.json", {"passed": passed, "failed": failed})
    assert not failed, f"{len(failed)} native scenario(s) failed; see {diagnostics / 'results.json'}"
    return passed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rpi", required=True, type=Path)
    parser.add_argument("--diagnostics-dir", required=True, type=Path)
    args = parser.parse_args()
    rpi = args.rpi.resolve(strict=True)
    assert rpi.is_file(), "--rpi must name a native executable, not a directory"
    diagnostics = args.diagnostics_dir.resolve()
    # Refuse reuse: every run has independent native homes and session state.
    diagnostics.mkdir(parents=True, exist_ok=False)
    try:
        passed = run(rpi, diagnostics)
    except BaseException:
        (diagnostics / "failure.txt").write_text(traceback.format_exc(), encoding="utf-8")
        raise
    print(f"PASS {len(passed)} real-native scenarios; diagnostics: {diagnostics}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
