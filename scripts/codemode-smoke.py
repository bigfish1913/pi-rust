#!/usr/bin/env python3
"""Isolated native Code Mode + real DLL smoke (stdlib only, loopback provider).

Requires locally built companion host and extension. Never installs/deploys; all
credentials, files and transcripts are disposable fixtures under a NEW evidence
directory. Optional permission-gate DLL checks headless approval denial.
"""
import argparse
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import importlib.util
import json
from pathlib import Path
import threading
import time
from types import SimpleNamespace
import uuid

spec = importlib.util.spec_from_file_location("resource_smoke", Path(__file__).with_name("resource-smoke.py"))
resource = importlib.util.module_from_spec(spec)
spec.loader.exec_module(resource)

CASES = {
    "list": {"action": "list", "limit": 2},
    "compose": {"code": "const a = await tools.read({path:'seed.txt'}); await tools.write({path:'derived.txt',content:a.content[0].text + '\\ncomposed'}); const [b,c] = await Promise.all([tools.read({path:'derived.txt'}),tools.read({path:'seed.txt'})]); print('finished',b.content.length); return {b:b.content,c:c.content};"},
    "invalid": {"code": "return await tools.read({});"},
    "partial": {"code": "await tools.write({path:'partial.txt',content:'effect-before-error'}); throw new Error('expected smoke failure');"},
    "timeout": {"code": "while(true){}", "timeoutMs": 25},
    "cancel": {"code": "while(true){}", "timeoutMs": 120000},
    "denied": {"code": "return await tools.write({path:'denied.txt',content:'must-not-exist'});"},
    "after_cancel": {"code": "return (await tools.read({path:'seed.txt'})).content;"},
}


def require(ok, reason):
    if not ok:
        raise AssertionError(reason)


def dump(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2), encoding="utf-8")


class Provider:
    def __init__(self, root):
        self.calls, self.errors = [], []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                try:
                    require(self.path == "/v1/chat/completions", self.path)
                    size = int(self.headers["Content-Length"])
                    require(0 < size <= 2 * 1024 * 1024, "provider input budget")
                    body = json.loads(self.rfile.read(size))
                    require(len(owner.calls) < 32, "provider request budget")
                    owner.calls.append(body)
                    dump(root / "provider-requests.json", owner.calls)
                    messages = body["messages"]
                    last = max(i for i, m in enumerate(messages) if m["role"] == "user")
                    case = resource.text(messages[last]["content"])
                    require(case in CASES, "unknown case: " + case)
                    results = [m for m in messages[last + 1:] if m["role"] == "tool"]
                    # An internal call must not appear as an orphan provider tool result.
                    assistants = {c["id"] for m in messages if m["role"] == "assistant"
                                  for c in m.get("tool_calls", [])}
                    require(all(m.get("tool_call_id") in assistants for m in messages if m["role"] == "tool"),
                            "orphan tool result in provider context")
                    require(all(not m.get("tool_call_id", "").startswith("tp-")
                                for m in messages if m["role"] == "tool"), "nested result leaked into provider history")
                    if not results:
                        names = {t["function"]["name"] for t in body.get("tools", [])}
                        require({"codemode", "read", "write"} <= names, "missing tools: " + str(names))
                        delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": "smoke-" + case,
                            "type": "function", "function": {"name": "codemode",
                            "arguments": json.dumps(CASES[case])}}]}
                        finish = "tool_calls"
                    else:
                        require(len(results) == 1, "expected only outer codemode result")
                        delta, finish = {"role": "assistant", "content": "DONE:" + case}, "stop"
                    frames = [
                        {"choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                        {"choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
                         "usage": {"prompt_tokens": 30, "completion_tokens": 10, "total_tokens": 40}},
                    ]
                    data = ("".join("data: " + json.dumps(f) + "\n\n" for f in frames)
                            + "data: [DONE]\n\n").encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except Exception as error:
                    owner.errors.append(str(error))
                    self.close_connection = True

        self.server = HTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=lambda: self.server.serve_forever(poll_interval=.05), daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
        require(not self.thread.is_alive(), "provider leaked")
        require(not self.errors, self.errors)


class Host(resource.Host):
    """The fixture only rejects approvals; it never grants authority."""
    def __init__(self, *args):
        self.denials = []
        super().__init__(*args)

    def record(self, name, line):
        super().record(name, line)
        if name == "stdout":
            frame = json.loads(line)
            if frame.get("type") == "ui_dialog_request":
                require(frame.get("toolCallId", "").startswith("tp-"), "approval is not a nested call")
                self.denials.append(frame)
                self.send({"type": "ui_dialog_response", "requestId": frame["requestId"], "answer": False})


def outer_result(host):
    data = host.call("get_messages", limit=64)
    results = [m for m in data["messages"] if m.get("role") in ("toolResult", "tool")]
    require(results, data)
    result = results[-1]
    require(result.get("toolName", result.get("tool_name")) == "codemode", result)
    raw = resource.text(result.get("content"))
    # AgentError can prepend its domain label to ABI error text.
    start = raw.find('{"')
    summary = json.loads(raw[start:]) if start >= 0 else None
    return result, summary


def run_group(args, root, gated=False):
    root.mkdir()
    provider = Provider(root)
    host = None
    try:
        options = SimpleNamespace(live=False, model="synthetic", max_output_tokens=2048)
        env = resource.environment(root, options, f"http://127.0.0.1:{provider.server.server_port}/v1")
        (root / "workspace/seed.txt").write_text("native-code-mode-中文", encoding="utf-8")
        command = [str(args.rpi), "--mode", "rpc", "--provider", "resource-fixture", "--model", "synthetic",
                   "--session-dir", str(root / "sessions"), "--session-id", str(uuid.uuid4()),
                   "--rpc-message-updates", "compact-deltas", "--local-only", "--no-skills", "--no-context-files",
                   "--no-prompt-templates", "--tools", "read,write,codemode", "--thinking", "off", "--timeout", "15",
                   "--extension", str(args.extension)]
        if gated:
            env["RPI_PERMISSION_GATE"] = "1"
            command += ["--extension", str(args.permission_gate)]
        dump(root / "command.json", command)
        host = Host(command, root, env, 45)
        ready = host.wait(lambda f: f.get("type") == "ready")
        dump(root / "ready.json", ready)
        cases = ["denied"] if gated else ["list", "compose", "invalid", "partial", "timeout", "cancel", "after_cancel"]
        findings = []
        for case in cases:
            require(host.call("prompt", message=case) == {"accepted": True}, "prompt acceptance")
            if case == "cancel":
                time.sleep(.3)
                require(host.call("abort").get("aborted") is True, "abort did not reach running program")
                host.wait(lambda f: f.get("type") == "agent_settled")
                findings.append({"case": case, "passed": True})
                continue
            host.settlement()
            result, summary = outer_result(host)
            if case == "list":
                require(not result.get("isError"), result)
                require(summary["protocolVersion"] == 1 and len(summary["tools"]) <= 2, summary)
                require(all(t["name"] != "codemode" for t in summary["tools"]), summary)
            elif case in ("compose", "after_cancel"):
                require(not result.get("isError") and summary["status"] == "completed", result)
                if case == "compose":
                    require((root / "workspace/derived.txt").read_text(encoding="utf-8").endswith("composed"), summary)
                    require(len(summary["calls"]) == 4 and summary["logs"], summary)
            else:
                require(result.get("isError") is True, result)
                require(summary and summary["status"] == ("timed_out" if case == "timeout" else "error"), result)
                if case == "partial":
                    require((root / "workspace/partial.txt").read_text() == "effect-before-error", summary)
                if case == "denied":
                    require(not (root / "workspace/denied.txt").exists(), "approval bypass")
                    require(len(host.denials) == 1, "expected one rejected nested approval")
            findings.append({"case": case, "passed": True, "summary": summary})
        data = host.call("get_messages", limit=64)
        dump(root / "messages.json", data)
        require("runtimeKey" not in json.dumps(data) and "runtimeHandle" not in json.dumps(data), "capability leak")
        host.exit_ok()
        sessions = list((root / "sessions").rglob("*.jsonl"))
        require(sessions, "no native JSONL transcript")
        raw = "\n".join(p.read_text(encoding="utf-8") for p in sessions)
        if not gated:
            require("rpi.tool_program.intent.v1" in raw and "rpi.tool_program.finished.v1" in raw, "no durable nested markers")
        require("runtimeKey" not in raw and "runtimeHandle" not in raw, "capability persisted")
        return findings
    finally:
        if host:
            host.close()
        provider.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rpi", required=True, type=Path)
    parser.add_argument("--extension", required=True, type=Path)
    parser.add_argument("--permission-gate", type=Path)
    parser.add_argument("--evidence-dir", required=True, type=Path)
    args = parser.parse_args()
    args.rpi, args.extension = args.rpi.resolve(strict=True), args.extension.resolve(strict=True)
    if args.permission_gate:
        args.permission_gate = args.permission_gate.resolve(strict=True)
    root = args.evidence_dir.resolve()
    root.mkdir(parents=True, exist_ok=False)
    provenance = {name: {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                  for name, path in [("host", args.rpi), ("extension", args.extension)]}
    dump(root / "provenance.json", provenance)
    result = {"status": "FAILED", "cases": []}
    try:
        result["cases"] += run_group(args, root / "normal")
        if args.permission_gate:
            result["cases"] += run_group(args, root / "gated", True)
        else:
            result["approval"] = "SKIP: no --permission-gate DLL"
        result["status"] = "PASSED"
    except Exception as error:
        result["error"] = str(error)
        raise
    finally:
        dump(root / "results.json", result)
    print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
