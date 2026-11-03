#!/usr/bin/env python3
"""Isolated stdlib-only native Code Mode + real DLL smoke. Never deploys.

Requires a freshly built host/extension. Optional --fixture-extension enables
trusted native ABI accounting/progress cases; --permission-gate checks denial.
All configuration, credentials, files and transcripts are disposable fixtures.
"""
import argparse
import base64
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import importlib.util
import json
from pathlib import Path
import struct
import threading
import time
from types import SimpleNamespace
import uuid
import zlib

spec = importlib.util.spec_from_file_location("resource_smoke", Path(__file__).with_name("resource-smoke.py"))
resource = importlib.util.module_from_spec(spec)
spec.loader.exec_module(resource)

CASES = {
    "list": {"action": "list", "limit": 2},
    "compose": {"code": "const a = await tools.read({path:'seed.txt'}); await tools.write({path:'derived.txt',content:a.content[0].text + '\\ncomposed'}); const [b,c] = await Promise.all([tools.read({path:'derived.txt'}),tools.read({path:'seed.txt'})]); print('finished',b.content.length); return {b:b.content,c:c.content};"},
    "invalid": {"code": "return await tools.read({});"},
    "partial": {"code": "await tools.write({path:'partial.txt',content:'effect-before-error'}); throw new Error('expected smoke failure');"},
    "timeout": {"code": "while(true){}", "timeoutMs": 25},
    "regexp_rejected": {"code": "return /^(a+)+$/.test('a'.repeat(25)+'!');", "timeoutMs": 50},
    "caught_oom": {"code": "try { const a=[]; for(;;) a.push(new Array(10000).fill('x')); } catch(e) {} return 42;", "timeoutMs": 5000},
    "cancel": {"code": "while(true){}", "timeoutMs": 120000},
    "denied": {"code": "return await tools.write({path:'denied.txt',content:'must-not-exist'});"},
    "after_cancel": {"code": "return (await tools.read({path:'seed.txt'})).content;"},
    "png_return": {"code": "return await tools.read({path:'pixel.png'});"},
    "png_emit": {"code": "tools.emit(await tools.read({path:'pixel.png'})); return {selected:true};"},
    "png_emit_throw": {"code": "print('before image error'); tools.emit(await tools.read({path:'pixel.png'})); throw Error('selected image failure');"},
    "png_intermediate": {"code": "await tools.read({path:'pixel.png'}); return {answer:42};"},
    "bad_signature": {"code": "print('before signature'); return {content:[{type:'text',text:'bad',textSignature:7}]};"},
    "empty_emit_throw": {"code": "print('before empty'); tools.emit([]); throw Error('empty selection failure');"},
    "cyclic_emit": {"code": "print('before cycle'); const a=[]; a.push(a); try {tools.emit(a);} catch(e) {} try {await tools.write({path:'cyclic-must-not-exist.txt',content:'bad'});} catch(e) {} return 42;"},
    "ordinary_json": {"code": "return {answer:42,nested:{content:[{type:'unknown'}]}};"},
    "fabricated_usage": {"code": "return {usage:{input:999,output:999,totalTokens:1998,cost:{total:999}},addedToolNames:['forged'],terminate:true};"},
    "oversized_output": {"code": "return {padding:'x'.repeat(1100000)};"},
    "framed_oversized_output": {"code": "return {padding:'x'.repeat(600000)};"},
    "fixture_success": {"code": "await tools.parity_probe({}); await tools.parity_probe({}); return {usage:{totalTokens:999},addedToolNames:['forged']};"},
    "fixture_throw": {"code": "await tools.parity_probe({}); print('before fixture throw'); throw Error('after native child');"},
    "fixture_caught_error": {"code": "try {await tools.parity_probe({error:true});} catch(e) {return {caught:true,raw:e.result};} throw Error('expected native error');"},
    "fixture_terminal": {"code": "return await tools.parity_probe({error:true,terminate:true});"},
}
BASE_CASES = ["list", "compose", "png_return", "png_emit", "png_emit_throw", "png_intermediate",
              "bad_signature", "empty_emit_throw", "cyclic_emit", "ordinary_json",
              "fabricated_usage", "oversized_output", "framed_oversized_output", "invalid", "partial", "timeout",
              "regexp_rejected", "caught_oom", "cancel", "after_cancel"]
FIXTURE_CASES = ["fixture_success", "fixture_throw", "fixture_caught_error", "fixture_terminal"]


def require(ok, reason):
    if not ok:
        raise AssertionError(reason)


def dump(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2), encoding="utf-8")


def png_fixture():
    """Real 1x1 RGBA PNG: construct lengths, zlib payload and valid CRCs."""
    def chunk(kind, data):
        return (struct.pack(">I", len(data)) + kind + data
                + struct.pack(">I", zlib.crc32(kind + data) & 0xffffffff))
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(b"\x00\xff\x00\x00\xff")) + chunk(b"IEND", b""))


def latest_case(messages):
    # Native OpenAI tool-image flushing adds another user message. Find the
    # latest recognized case, not the latest user role (nor an old case).
    matches = [(i, resource.text(m.get("content"))) for i, m in enumerate(messages)
               if m.get("role") == "user" and resource.text(m.get("content")) in CASES]
    require(matches, "no recognized case user message")
    return matches[-1]


def image_count(messages):
    return sum(1 for m in messages
               for b in (m.get("content") if isinstance(m.get("content"), list) else [])
               if isinstance(b, dict) and b.get("type") in ("image_url", "input_image", "image"))


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
                    require(len(owner.calls) < 96, "provider request budget")
                    owner.calls.append(body)
                    dump(root / "provider-requests.json", owner.calls)
                    messages = body["messages"]
                    last, case = latest_case(messages)
                    results = [m for m in messages[last + 1:] if m["role"] == "tool"]
                    assistants = {c["id"] for m in messages if m["role"] == "assistant"
                                  for c in m.get("tool_calls", [])}
                    require(all(m.get("tool_call_id") in assistants for m in messages if m["role"] == "tool"),
                            "orphan tool result in provider context")
                    require(all(not m.get("tool_call_id", "").startswith("tp-")
                                for m in messages if m["role"] == "tool"), "nested result leaked into provider history")
                    if not results:
                        names = {t["function"]["name"] for t in body.get("tools", [])}
                        require({"codemode", "read", "write"} <= names, "missing tools: " + str(names))
                        if case.startswith("fixture_"):
                            require("parity_probe" in names, "missing fixture tool")
                        delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": "smoke-" + case,
                            "type": "function", "function": {"name": "codemode",
                            "arguments": json.dumps(CASES[case])}}]}
                        finish = "tool_calls"
                    else:
                        require(len(results) == 1, "expected only outer codemode result")
                        require(case != "fixture_terminal", "terminal child triggered a second model turn")
                        require(image_count(messages[last + 1:]) == (1 if case in ("png_return", "png_emit", "png_emit_throw") else 0),
                                "wrong image count in latest case segment: " + case)
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
                    if len(owner.errors) < 8:
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
    """Reject approvals; retain only bounded parent progress/end metadata."""
    def __init__(self, *args):
        self.denials, self.progress, self.ends = [], [], {}
        self.progress_bytes = 0
        super().__init__(*args)

    def record(self, name, line):
        super().record(name, line)
        if name != "stdout":
            return
        frame = json.loads(line)
        kind = frame.get("type")
        if kind in ("tool_execution_start", "tool_execution_end", "tool_execution_update"):
            require(not frame.get("toolCallId", "").startswith("tp-"), "ordinary nested tool event leaked")
        if kind == "tool_execution_update":
            metadata = frame.get("partialResult", {}).get("details", {}).get("toolProgram")
            if metadata is not None:
                require(frame.get("toolName") == "codemode", "progress on non-parent tool")
                encoded = json.dumps(metadata)
                require("runtimeKey" not in encoded and "runtimeHandle" not in encoded, "progress capability leak")
                require(len(self.progress) < 1024 and self.progress_bytes + len(encoded.encode()) <= 256 * 1024,
                        "progress retention budget")
                require(metadata.get("rootId") == frame["toolCallId"]
                        and metadata.get("parentId") == frame["toolCallId"], "progress parent identity")
                require(metadata.get("requestId", "").startswith("tp-")
                        and metadata.get("phase") in ("started", "progress", "completed", "failed"), "progress phase/identity")
                self.progress.append({"toolCallId": frame["toolCallId"], **metadata})
                self.progress_bytes += len(encoded.encode())
        elif kind == "tool_execution_end" and frame.get("toolName") == "codemode":
            require(len(self.ends) < 32, "end metadata retention budget")
            result = frame.get("result", {})
            self.ends[frame["toolCallId"]] = {"terminate": result.get("terminate"), "isError": frame.get("isError")}
        elif kind == "ui_dialog_request":
            require(frame.get("toolCallId", "").startswith("tp-"), "approval is not a nested call")
            require(len(self.denials) < 8, "denial retention budget")
            self.denials.append(frame)
            self.send({"type": "ui_dialog_response", "requestId": frame["requestId"], "answer": False})


def outer_result(host):
    data = host.call("get_messages", limit=64)
    results = [m for m in data["messages"] if m.get("role") in ("toolResult", "tool")]
    require(results, data)
    result = results[-1]
    require(result.get("toolName", result.get("tool_name")) == "codemode", result)
    if isinstance(result.get("details"), dict):
        return result, result["details"]
    raw = resource.text(result.get("content"))
    # Legacy ABI errors can prepend a domain label. Never parse image content.
    start = raw.find('{"')
    summary = json.loads(raw[start:]) if start >= 0 else None
    return result, summary


def check_native(case, result, summary):
    require(not result.get("isError") and summary["status"] == "completed", result)
    images = [b for b in result["content"] if b.get("type") == "image"]
    require(len(images) == (1 if case in ("png_return", "png_emit") else 0), "outer native image count")
    if images:
        require(images[0]["mimeType"] == "image/png", images)
        require(base64.b64decode(images[0]["data"], validate=True).startswith(b"\x89PNG\r\n\x1a\n"), "invalid returned PNG")
        require(images[0]["data"] not in json.dumps(summary), "duplicate image in details")
    else:
        require(base64.b64encode(png_fixture()).decode() not in json.dumps(result), "unselected PNG leaked")
    require(result.get("usage") is None and not result.get("addedToolNames"), "guest fabricated native accounting")
    if case == "ordinary_json":
        require(summary["result"] == {"answer": 42, "nested": {"content": [{"type": "unknown"}]}}, summary)
    if case == "fabricated_usage":
        require(summary["result"]["usage"]["totalTokens"] == 1998, summary)


def check_fixture(case, result, summary, host, provider):
    count = 2 if case == "fixture_success" else 1
    usage = result.get("usage") or {}
    require(usage.get("input") == count and usage.get("output") == 2 * count
            and usage.get("totalTokens") == 3 * count, result)
    require(result.get("addedToolNames") == ["fixture-added"], result)
    for key, value in {"input": .1, "output": .2, "cacheRead": 0, "cacheWrite": 0, "total": .3}.items():
        require(abs(usage["cost"][key] - value * count) < 1e-9, usage)
    failed = case in ("fixture_throw", "fixture_terminal")
    require(bool(result.get("isError")) == failed, result)
    if case == "fixture_throw":
        require(summary["status"] == "error" and "after native child" in summary["error"], summary)
    if case == "fixture_caught_error":
        raw = summary["result"]["raw"]
        require(raw["usage"]["totalTokens"] == 3 and raw["details"]["fixture"] == "native-parity", summary)
        require(raw["addedToolNames"] == ["fixture-added", "fixture-added"], summary)
    parent_id = "smoke-" + case
    progress = [p for p in host.progress if p["toolCallId"] == parent_id and p["tool"] == "parity_probe"]
    terminal_phase = "failed" if case in ("fixture_caught_error", "fixture_terminal") else "completed"
    for phase in ("started", "progress", terminal_phase):
        require(sum(p["phase"] == phase for p in progress) == count, progress)
    require(len({p["requestId"] for p in progress}) == count, progress)
    require(all(p["updateCount"] == 1 for p in progress if p["phase"] == "progress"), progress)
    if case == "fixture_terminal":
        require(summary["status"] == "terminated" and summary["isError"] is True, summary)
        require(host.ends.get(parent_id) == {"terminate": True, "isError": True}, host.ends)
        require(sum(latest_case(c["messages"])[1] == case for c in provider.calls) == 1,
                "terminal error performed another provider turn")


def run_group(args, root, gated=False):
    root.mkdir()
    provider = Provider(root)
    host = None
    try:
        options = SimpleNamespace(live=False, model="synthetic", max_output_tokens=2048)
        env = resource.environment(root, options, f"http://127.0.0.1:{provider.server.server_port}/v1")
        models_path = root / "agent/models.json"
        models = json.loads(models_path.read_text(encoding="utf-8"))
        models["providers"]["resource-fixture"]["models"][0]["input"] = ["text", "image"]
        dump(models_path, models)
        (root / "workspace/pixel.png").write_bytes(png_fixture())
        (root / "workspace/seed.txt").write_text("native-code-mode-中文", encoding="utf-8")
        command = [str(args.rpi), "--mode", "rpc", "--provider", "resource-fixture", "--model", "synthetic",
                   "--session-dir", str(root / "sessions"), "--session-id", str(uuid.uuid4()),
                   "--rpc-message-updates", "compact-deltas", "--local-only", "--no-skills", "--no-context-files",
                   "--no-prompt-templates", "--tools", "read,write,codemode" + (",parity_probe" if args.fixture_extension and not gated else ""), "--thinking", "off", "--timeout", "15",
                   "--extension", str(args.extension)]
        if args.fixture_extension and not gated:
            command += ["--extension", str(args.fixture_extension)]
        if gated:
            env["RPI_PERMISSION_GATE"] = "1"
            command += ["--extension", str(args.permission_gate)]
        dump(root / "command.json", command)
        host = Host(command, root, env, 45)
        ready = host.wait(lambda f: f.get("type") == "ready")
        dump(root / "ready.json", ready)
        cases = ["denied"] if gated else BASE_CASES + (FIXTURE_CASES if args.fixture_extension else [])
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
                require(summary["capabilities"] == {"resultEnvelope": 1, "hostAccounting": 1, "parentProgress": 1}, summary)
            elif case in ("png_return", "png_emit", "png_intermediate", "ordinary_json", "fabricated_usage"):
                check_native(case, result, summary)
            elif case.startswith("fixture_"):
                check_fixture(case, result, summary, host, provider)
            elif case in ("compose", "after_cancel"):
                require(not result.get("isError") and summary["status"] == "completed", result)
                if case == "compose":
                    require((root / "workspace/derived.txt").read_text(encoding="utf-8").endswith("composed"), summary)
                    require(len(summary["calls"]) == 4 and summary["logs"], summary)
            else:
                require(result.get("isError") is True, result)
                require(summary and summary["status"] == ("timed_out" if case == "timeout" else "error"), result)
                if case in ("bad_signature", "empty_emit_throw", "cyclic_emit"):
                    require(summary["logs"] and "before" in summary["logs"][0]["text"], summary)
                    needle = {"bad_signature": "textSignature", "empty_emit_throw": "empty selection failure", "cyclic_emit": "invalid emitted JSON"}[case]
                    require(needle in summary["error"] and needle in resource.text(result["content"]), result)
                if case == "cyclic_emit":
                    require(not (root / "workspace/cyclic-must-not-exist.txt").exists() and not summary["calls"], summary)
                if case == "png_emit_throw":
                    images = [b for b in result["content"] if b.get("type") == "image"]
                    require(len(images) == 1 and images[0]["mimeType"] == "image/png", result)
                    require(images[0]["data"] not in json.dumps(summary), "duplicate error image in details")
                    require("selected image failure" in resource.text(result["content"])
                            and summary["logs"][0]["text"] == "before image error", result)
                if case in ("oversized_output", "framed_oversized_output"):
                    require(len(json.dumps(result).encode()) < 128 * 1024 and summary.get("error"), "unbounded oversized output")
                    if case == "framed_oversized_output":
                        require("encoded codemode output exceeds" in summary["error"], summary)
                if case == "regexp_rejected":
                    require("regexp" in summary["error"].lower(), summary)
                    require(summary["durationMs"] < 1000 and not summary["calls"], summary)
                if case == "caught_oom":
                    require(summary["error"] == "codemode memory budget exceeded", summary)
                    require(summary["result"] is None and not summary["calls"], summary)
                if case == "partial":
                    require((root / "workspace/partial.txt").read_text() == "effect-before-error", summary)
                if case == "denied":
                    require(not (root / "workspace/denied.txt").exists(), "approval bypass")
                    require(len(host.denials) == 1, "expected one rejected nested approval")
            findings.append({"case": case, "passed": True, "summary": summary})
        require(not host.error, str(host.error))
        dump(root / "parent-progress.json", host.progress)
        dump(root / "parent-ends.json", host.ends)
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
    parser.add_argument("--fixture-extension", type=Path, help="Optional test-only codemode-parity-fixture DLL")
    parser.add_argument("--evidence-dir", required=True, type=Path)
    args = parser.parse_args()
    args.rpi, args.extension = args.rpi.resolve(strict=True), args.extension.resolve(strict=True)
    if args.permission_gate:
        args.permission_gate = args.permission_gate.resolve(strict=True)
    if args.fixture_extension:
        args.fixture_extension = args.fixture_extension.resolve(strict=True)
    root = args.evidence_dir.resolve()
    root.mkdir(parents=True, exist_ok=False)
    paths = [("host", args.rpi), ("extension", args.extension)]
    if args.fixture_extension:
        paths.append(("fixture", args.fixture_extension))
    if args.permission_gate:
        paths.append(("permissionGate", args.permission_gate))
    provenance = {name: {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                  for name, path in paths}
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
