#!/usr/bin/env python3
"""Offline real-process session recovery smoke (Python standard library only).

python -B scripts/session-smoke.py --rpi /absolute/path/to/rpi \
    --diagnostics-dir target/session-smoke-unique [--extension /path/to/plugin]

Never uses user history, installed configuration, real API keys or PATH discovery.
An explicit extension is trusted native code, not sandboxed by this harness.
"""
import argparse
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import os
from pathlib import Path
import queue
import re
import subprocess
import sys
import threading
import time
import traceback
import uuid

TIMEOUT = 25
FILE_TEXT = "session smoke / 中文 / 雪\n"
TODO_TEXT = "SESSION_TODO_CANARY_8391"
WORKFLOW = "SESSION_DURABLE_CANARY_8391"


def dump(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2), encoding="utf-8")


def text(content):
    if isinstance(content, str):
        return content
    return "\n".join(p.get("text", "") for p in content or [] if isinstance(p, dict))


class Model:
    def __init__(self, root):
        self.root, self.calls, self.errors = root, [], []
        self.entered, self.release = threading.Event(), threading.Event()
        self.blocked_once = False
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def setup(self):
                super().setup()
                self.connection.settimeout(10)

            def log_message(self, *_args):
                pass

            def do_POST(self):
                try:
                    assert self.path == "/v1/chat/completions", self.path
                    size = int(self.headers["Content-Length"])
                    assert 0 < size < 4 * 1024 * 1024, size
                    body = json.loads(self.rfile.read(size))
                    assert body["model"] == "synthetic", body
                    owner.calls.append(body)
                    dump(root / "provider-requests.json", owner.calls)
                    messages = body["messages"]
                    last_user = max(i for i, m in enumerate(messages) if m["role"] == "user")
                    prompt = text(messages[last_user].get("content"))
                    # runResume is a custom transcript notice projected as a
                    # provider user message, not a duplicated user prompt.
                    if prompt == "Continuing the run that was interrupted (resume 1 of 3).":
                        assert text(messages[last_user - 1].get("content")) == "CRASH_PENDING", messages
                        prompt = "CRASH_PENDING"
                    results = [m for m in messages[last_user + 1:] if m["role"] == "tool"]
                    name, args = None, None
                    if prompt == "ROUNDTRIP 中文":
                        if not results:
                            name, args = "write", {"path": "fixture.txt", "content": FILE_TEXT}
                        elif len(results) == 1:
                            name, args = "read", {"path": "fixture.txt"}
                        else:
                            assert len(results) == 2, results
                            assert FILE_TEXT.strip() in text(results[-1].get("content")), results
                    elif prompt in ("TODO_ADD", "TODO_LIST") and not results:
                        name = "todo"
                        args = ({"action": "add", "text": TODO_TEXT} if prompt == "TODO_ADD"
                                else {"action": "list", "includeDone": True})
                    elif prompt.startswith("DURABLE_") and not results:
                        name = "durable"
                        args = {"DURABLE_START": {"action": "start", "name": WORKFLOW,
                                    "steps": [{"id": "verify", "title": "Verify restart"}]},
                                "DURABLE_CHECKPOINT": {"action": "checkpoint", "step": "verify",
                                                       "status": "completed"},
                                "DURABLE_STATUS": {"action": "status"}}[prompt]
                    if prompt == "CRASH_PENDING" and not owner.blocked_once:
                        owner.blocked_once = True
                        owner.entered.set()
                        assert owner.release.wait(TIMEOUT), "crash fixture was not released"
                    if name:
                        available = {t["function"]["name"] for t in body.get("tools", [])}
                        assert name in available, (name, available)
                        delta = {"role": "assistant", "tool_calls": [{"index": 0,
                            "id": "smoke-" + uuid.uuid4().hex, "type": "function",
                            "function": {"name": name, "arguments": json.dumps(args, ensure_ascii=False)}}]}
                        reason = "tool_calls"
                    else:
                        delta = {"role": "assistant", "content": "COMPLETED:" + prompt}
                        reason = "stop"
                    frames = [
                        {"choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                        {"choices": [{"index": 0, "delta": {}, "finish_reason": reason}],
                         "usage": {"prompt_tokens": 30, "completion_tokens": 10, "total_tokens": 40}},
                    ]
                    data = ("".join("data: " + json.dumps(f) + "\n\n" for f in frames)
                            + "data: [DONE]\n\n").encode("utf-8")
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                    pass  # The crash scenario deliberately closes a live request.
                except Exception:
                    owner.errors.append(traceback.format_exc())
                    dump(root / "provider-errors.json", owner.errors)
                    self.close_connection = True

        self.server = HTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=lambda: self.server.serve_forever(poll_interval=.05),
                                       name="session-smoke-provider")
        self.thread.start()

    def close(self):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=10)
        assert not self.thread.is_alive(), "provider thread leaked"
        assert not self.errors, self.errors


def environment(root, model):
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
               RPI_CODING_AGENT_DIR=str(root / "agent"), RPI_OFFLINE="1", NO_COLOR="1",
               RPI_VOICE_AUTO_TTS="off")
    dump(root / "agent/auth.json", {})
    dump(root / "agent/settings.json", {
        "defaultProvider": "fixture", "defaultModel": "synthetic", "defaultThinkingLevel": "off"})
    dump(root / "agent/models.json", {"providers": {"fixture": {
        "api": "openai-completions", "baseUrl": f"http://127.0.0.1:{model.server.server_port}/v1",
        "apiKey": "local-dummy-not-a-credential", "models": [{"id": "synthetic",
        "name": "synthetic", "reasoning": False, "input": ["text"],
        "contextWindow": 100000, "maxTokens": 1024}]}}})
    return env


class Host:
    def __init__(self, binary, root, env, sid, encoding, extensions, label):
        self.root = root / label
        self.root.mkdir()
        self.queue, self.pending = queue.Queue(), []
        self.counter = 0
        self.stderr = (self.root / "stderr.log").open("w", encoding="utf-8")
        command = [str(binary), "--mode", "rpc", "--provider", "fixture", "--model", "synthetic",
                   "--session-dir", str(root / "sessions"), "--session-id", sid,
                   "--rpc-message-updates", encoding, "--local-only", "--no-skills",
                   "--no-prompt-templates", "--no-context-files", "--verbose", "--timeout", "15"]
        if extensions:
            for path in extensions:
                command.extend(["--extension", str(path)])
        else:
            command.append("--no-extensions")
        dump(self.root / "command.json", command)
        try:
            self.proc = subprocess.Popen(command, cwd=root / "workspace", env=env,
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr,
                text=True, encoding="utf-8", errors="strict", bufsize=1)
        except BaseException:
            self.stderr.close()
            raise
        self.reader = threading.Thread(target=self.pump, name="session-smoke-stdout")
        self.reader.start()

    def pump(self):
        try:
            with (self.root / "stdout.jsonl").open("w", encoding="utf-8") as log:
                for line in self.proc.stdout:
                    log.write(line)
                    log.flush()
                    if line.strip():
                        frame = json.loads(line)
                        assert isinstance(frame, dict), frame
                        self.queue.put(frame)
        except BaseException as error:
            self.queue.put(error)
        finally:
            self.queue.put(None)

    def wait(self, predicate):
        deadline = time.monotonic() + TIMEOUT
        while True:
            for index, frame in enumerate(self.pending):
                if predicate(frame):
                    return self.pending.pop(index)
            remaining = deadline - time.monotonic()
            assert remaining > 0, "frame deadline exceeded: " + str(self.root)
            try:
                frame = self.queue.get(timeout=remaining)
            except queue.Empty:
                raise AssertionError("frame timeout: " + str(self.root)) from None
            assert frame is not None, "unexpected stdout EOF: " + str(self.root)
            if isinstance(frame, BaseException):
                raise frame
            self.pending.append(frame)

    def call(self, kind, **fields):
        self.counter += 1
        request = dict(type=kind, id=f"smoke-{self.counter}", **fields)
        line = json.dumps(request, ensure_ascii=False) + "\n"
        with (self.root / "stdin.jsonl").open("a", encoding="utf-8") as log:
            log.write(line)
        self.proc.stdin.write(line)
        self.proc.stdin.flush()
        frame = self.wait(lambda f: f.get("type") == "response" and f.get("id") == request["id"])
        assert frame.get("command") == kind and frame.get("success") is True, frame
        return frame.get("data")

    def ready(self, encoding, count):
        ready = self.wait(lambda f: f.get("type") == "ready")
        assert ready["semantics"]["messageUpdates"] == encoding, ready
        assert {"get_messages", "resume_pending"} <= set(ready["supportedCommands"]), ready
        if count:
            # Explicit libraries can be silently skipped by the native loader;
            # --verbose's actual loaded count must match, not just ready=true.
            stderr = (self.root / "stderr.log").read_text(encoding="utf-8")
            match = re.search(r"extensions: loaded (\d+) plugin\(s\)", stderr)
            assert match and int(match[1]) == count, "explicit extension load failed: " + stderr
        dump(self.root / "registry.json", {"tools": self.call("get_available_tools"),
                                           "commands": self.call("get_commands")})

    def snapshot(self):
        data = self.call("get_messages", limit=100)
        assert data["hasMore"] is False, "fixture history unexpectedly truncated"
        return data

    def prompt(self, message):
        assert self.call("prompt", message=message) == {"accepted": True}
        frame = self.wait(lambda f: f.get("type") == "agent_settled")
        assert frame.get("outcome") == "completed", frame

    def tool(self, message):
        previous = {m["entryId"] for m in self.snapshot()["messages"]}
        self.prompt(message)
        added = [m for m in self.snapshot()["messages"] if m["entryId"] not in previous]
        results = tool_results(added)
        calls = [c for m in added if m.get("role") == "assistant"
                 for c in m.get("content", []) if c.get("type") == "toolCall"]
        assert len(calls) == len(results) == 1, (message, added)
        result, call = results[0], calls[0]
        expected_tool = "todo" if message.startswith("TODO_") else "durable"
        assert call["name"] == result["toolName"] == expected_tool, (call, result)
        assert call["id"] == result["toolCallId"], (call, result)
        assert not result.get("isError") and not (result.get("details") or {}).get("error"), result
        return result

    def stop(self):
        self.call("stop")
        self.proc.stdin.close()
        assert self.proc.wait(timeout=TIMEOUT) == 0, "nonzero stop: " + str(self.root)

    def kill(self):
        self.proc.kill()
        self.proc.wait(timeout=10)

    def close(self):
        try:
            if not self.proc.stdin.closed:
                self.proc.stdin.close()
            if self.proc.poll() is None:
                try:
                    self.proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self.kill()
        finally:
            self.reader.join(timeout=10)
            self.proc.stdout.close()
            self.stderr.close()
            assert not self.reader.is_alive(), "stdout reader leaked"
            assert self.proc.poll() is not None, "native process leaked"


def user_texts(messages):
    return [text(m.get("content")) for m in messages if m.get("role") == "user"]


def tool_results(messages):
    return [m for m in messages if m.get("role") in ("toolResult", "tool")]


def message_view(messages):
    # The live tool projection includes details:null, while the persisted
    # projection omits this optional field. All IDs, content and order still
    # have to match; do not discard arbitrary metadata or timestamps.
    return [{k: v for k, v in m.items()
             if not (k == "details" and v is None and m.get("role") == "toolResult")}
            for m in messages]


def scenario(binary, root, encoding, extensions):
    model, hosts = Model(root), []
    try:
        env = environment(root, model)
        sid = str(uuid.uuid4())

        def start(label, session=sid):
            host = Host(binary, root, env, session, encoding, extensions, label)
            hosts.append(host)
            host.ready(encoding, len(extensions))
            return host

        host = start("01-roundtrip")
        empty = host.snapshot()
        assert empty["messages"] == [] and empty["pendingResume"] is False, empty
        assert host.call("resume_pending").get("resumed") is False
        host.prompt("ROUNDTRIP 中文")
        assert (root / "workspace/fixture.txt").read_text(encoding="utf-8") == FILE_TEXT
        saved = host.snapshot()
        dump(root / "snapshot-before-restart.json", saved)
        assert user_texts(saved["messages"]) == ["ROUNDTRIP 中文"], saved
        results = tool_results(saved["messages"])
        assert len(results) == 2 and not any(m.get("isError") for m in results), results
        calls = [c for m in saved["messages"] for c in m.get("content", [])
                 if isinstance(c, dict) and c.get("type") == "toolCall"]
        assert len(calls) == 2, calls
        assert {c["id"] for c in calls} == {r["toolCallId"] for r in results}, (calls, results)
        assert "COMPLETED:ROUNDTRIP 中文" in json.dumps(saved, ensure_ascii=False), saved
        assert saved["pendingResume"] is False
        active = host.call("get_available_tools")["activeTools"]
        has_todo, has_durable = "todo" in active, "durable" in active
        if has_todo:
            assert TODO_TEXT in json.dumps(host.tool("TODO_ADD")), "todo add failed"
        if has_durable:
            assert host.tool("DURABLE_START")["details"]["workflow"]["name"] == WORKFLOW
            workflow = host.tool("DURABLE_CHECKPOINT")["details"]["workflow"]
            assert workflow["steps"][0]["status"] == "completed", workflow
        saved = host.snapshot()
        dump(root / "snapshot-before-restart.json", saved)
        host.stop()

        host = start("02-restarted")
        restored = host.snapshot()
        dump(root / "snapshot-after-restart.json", restored)
        assert message_view(restored["messages"]) == message_view(saved["messages"]), "normal restart changed messages"
        assert restored["pendingResume"] is False, restored
        host.prompt("AFTER_RESTART")
        request = model.calls[-1]
        assert user_texts(request["messages"]) == user_texts(saved["messages"]) + ["AFTER_RESTART"], request
        replayed = [m for m in request["messages"] if m["role"] == "tool"]
        assert len(replayed) >= 2 and FILE_TEXT.strip() in text(replayed[1].get("content")), replayed
        if has_todo:
            assert TODO_TEXT in json.dumps(host.tool("TODO_LIST")), "todo restart lost state"
        if has_durable:
            workflow = host.tool("DURABLE_STATUS")["details"]["workflow"]
            assert workflow["name"] == WORKFLOW and workflow["steps"][0]["status"] == "completed", workflow
        host.stop()

        host = start("03-empty-session", str(uuid.uuid4()))
        empty = host.snapshot()
        assert empty["messages"] == [] and empty["pendingResume"] is False, empty
        assert host.call("resume_pending").get("resumed") is False
        if has_todo:
            assert TODO_TEXT not in json.dumps(host.tool("TODO_LIST")), "todo leaked across sessions"
        if has_durable:
            assert host.tool("DURABLE_STATUS")["details"].get("workflow") is None, "workflow leaked across sessions"
        host.stop()

        crash_sid = str(uuid.uuid4())
        host = start("04-before-crash", crash_sid)
        assert host.call("prompt", message="CRASH_PENDING") == {"accepted": True}
        assert model.entered.wait(TIMEOUT), "provider never received crash prompt"
        host.kill()
        model.release.set()
        host = start("05-resume-crash", crash_sid)
        crashed = host.snapshot()
        dump(root / "snapshot-crashed.json", crashed)
        assert crashed["pendingResume"] is True, crashed
        assert user_texts(crashed["messages"]) == ["CRASH_PENDING"], crashed
        resumed = host.call("resume_pending")
        assert resumed == {"accepted": True}, resumed
        settled = host.wait(lambda f: f.get("type") == "agent_settled")
        assert settled.get("outcome") == "completed", settled
        recovered = host.snapshot()
        assert recovered["pendingResume"] is False, recovered
        assert user_texts(recovered["messages"]) == ["CRASH_PENDING"], recovered
        replies = [m for m in recovered["messages"] if m.get("role") == "assistant"
                   and "COMPLETED:CRASH_PENDING" in text(m.get("content"))]
        assert len(replies) == 1, recovered
        assert user_texts(model.calls[-1]["messages"]) == [
            "CRASH_PENDING", "Continuing the run that was interrupted (resume 1 of 3)."
        ], model.calls[-1]
        assert host.call("resume_pending").get("resumed") is False
        host.stop()

        host = start("06-recovered-restart", crash_sid)
        final = host.snapshot()
        assert message_view(final["messages"]) == message_view(recovered["messages"]), final
        assert final["pendingResume"] is False, final
        host.stop()
        return {"encoding": encoding, "status": "PASS", "todoState": "PASS" if has_todo else "SKIP",
                "durableState": "PASS" if has_durable else "SKIP",
                "checks": ["file-tool-roundtrip", "message-restart", "provider-history-replay",
                           "empty-session-isolation", "real-kill-resume", "recovered-restart"]}
    finally:
        model.release.set()
        try:
            for host in reversed(hosts):
                host.close()
        finally:
            model.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rpi", required=True, type=Path)
    parser.add_argument("--diagnostics-dir", required=True, type=Path)
    parser.add_argument("--extension", action="append", default=[], type=Path)
    parser.add_argument("--encoding", choices=("compact-deltas", "snapshot", "both"), default="both")
    args = parser.parse_args()
    binary = args.rpi.resolve()
    extensions = [p.resolve() for p in args.extension]
    for path in [binary] + extensions:
        if not path.is_file():
            parser.error(f"explicit binary/library does not exist: {path}")
    if len(set(extensions)) != len(extensions):
        parser.error("duplicate --extension path")
    diagnostics = args.diagnostics_dir.resolve()
    try:
        diagnostics.mkdir(parents=True, exist_ok=False)
    except FileExistsError:
        parser.error(f"refusing to overwrite diagnostics directory: {diagnostics}")
    results = []
    dump(diagnostics / "inputs.json", {"rpi": str(binary), "extensions": list(map(str, extensions))})
    encodings = ("compact-deltas", "snapshot") if args.encoding == "both" else (args.encoding,)
    for encoding in encodings:
        root = diagnostics / encoding
        root.mkdir()
        try:
            result = scenario(binary, root, encoding, extensions)
        except Exception:
            error = traceback.format_exc()
            (root / "failure.log").write_text(error, encoding="utf-8")
            result = {"encoding": encoding, "status": "FAIL", "error": error}
        results.append(result)
        print(result["status"], encoding, "diagnostics=" + str(root), flush=True)
        if result["status"] == "FAIL":
            print(result["error"], file=sys.stderr)
    dump(diagnostics / "results.json", results)
    if not extensions:
        print("SKIP independent extensions: no --extension libraries supplied")
    return 0 if all(r["status"] == "PASS" for r in results) else 1


if __name__ == "__main__":
    sys.exit(main())
