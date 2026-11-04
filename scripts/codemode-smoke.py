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
import re
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

# Local overrides ONLY: resource-smoke's shared small-output limits stay intact.
# Real native results legitimately exceed 1 MiB; every RPC line/log remains bounded.
MIB = 1024 * 1024
resource.MAX_LINE = 16 * MIB
resource.MAX_LOG = 64 * MIB
HTTP_BYTES, HTTP_REQUESTS = 16 * MIB, 128
LARGE_TEXT = "native-large:中文😀:" + "x" * (2 * MIB)


# Legitimate business properties/strings must survive every boundary unchanged.
BUSINESS = {
    "runtimeKey": "customer-key", "runtimeHandle": {"id": 7},
    "__rpi": {"owner": "customer", "runtimeKey": "business-deep-key"},
    "__rpiRuntimeKey": "customer-runtime",
    "items": [{"runtimeKey": "nested-key", "runtimeHandle": "nested-handle",
               "__rpi": {"runtimeKey": "deep-key"}, "__rpiRuntimeKey": "nested-runtime"}],
    "literal": "runtimeKey runtimeHandle __rpi __rpiRuntimeKey are business strings",
}
BUSINESS_JS = json.dumps(BUSINESS, ensure_ascii=False)
DIRECT_CASES = {
    "fixture_direct_success": {"business": BUSINESS},
    "fixture_direct_error": {"business": BUSINESS, "error": True},
}
EXACT_CASES = {"fixture_result_success": "fixture_direct_success",
               "fixture_result_error": "fixture_direct_error",
               "fixture_result_deleted_details": "fixture_direct_success",
               "fixture_result_undefined_details": "fixture_direct_error"}
MUTATED_DETAILS_CASES = {"fixture_result_deleted_details", "fixture_result_undefined_details"}
BINDING_CASES = ("fixture_binding_success", "fixture_binding_error")

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
CASES.update({
    **DIRECT_CASES,
    "fixture_result_success": {"code": "return tools.result(await tools.parity_probe({business:" + BUSINESS_JS + "}));"},
    # A caught native error completes JS normally, but exact selection retains
    # outer isError and must not synthesize error text or a CodeMode summary.
    "fixture_result_error": {"code": "try {await tools.parity_probe({error:true,business:" + BUSINESS_JS + "});} catch(e) {return tools.result(e.result);} throw Error('missing native error');"},
    "fixture_result_deleted_details": {"code": "const r=await tools.parity_probe({business:" + BUSINESS_JS + "}); delete r.details; return tools.result(r);"},
    "fixture_result_undefined_details": {"code": "try {await tools.parity_probe({error:true,business:" + BUSINESS_JS + "});} catch(e) {e.result.details=undefined; return tools.result(e.result);} throw Error('missing native error');"},
    "fixture_business": {"code": "const catalog=tools.list(); const raw=await tools.parity_probe({business:" + BUSINESS_JS + "}); return {schema:catalog.find(t=>t.name==='parity_probe'),raw:raw,guest:" + BUSINESS_JS + "};"},
    "fixture_aliases": {"code": "const out=[]; for(const name of ['call','list','emit','result','release']) {out.push(await tools.call(name,{business:" + BUSINESS_JS + "}));} return out;"},
})
for binding_case in BINDING_CASES:
    CASES[binding_case] = {"code": "let raw, caught=false; try {raw=await tools.parity_probe({echoBinding:true,error:"
        + ("true" if binding_case.endswith("error") else "false") + ",business:" + BUSINESS_JS
        + "});} catch(e) {caught=true; raw=e.result;} const echo=raw.details.bindingEcho; "
          "if(echo.metadata!==null || echo.key!=='[redacted invocation credential]') throw Error('binding reached guest'); "
          "return {raw:raw,caught:caught,guest:JSON.parse(JSON.stringify(raw)),text:JSON.parse(raw.content[0].text)};"}

BASE_CASES = ["list", "compose", "png_return", "png_emit", "png_emit_throw", "png_intermediate",
              "bad_signature", "empty_emit_throw", "cyclic_emit", "ordinary_json",
              "fabricated_usage", "oversized_output", "framed_oversized_output", "invalid", "partial", "timeout",
              "regexp_rejected", "caught_oom", "cancel", "after_cancel"]
FIXTURE_CASES = [*DIRECT_CASES, *EXACT_CASES, "fixture_business", *BINDING_CASES,
                 "fixture_aliases", "fixture_success", "fixture_throw",
                 "fixture_caught_error", "fixture_terminal"]


# Isolated native sessions avoid growing multi-megabyte provider contexts. Actual
# direct end-event baselines are retained as canonical payload hashes only.
LARGE_DIRECT = {
    "large_direct_text": {"kind":"text", "business":BUSINESS},
    "large_direct_image": {"kind":"image", "business":BUSINESS},
    "large_direct_error": {"kind":"error"},
    "large_direct_terminal": {"kind":"text", "business":BUSINESS, "terminate":True},
}
LARGE_EXACT = {"large_result_" + k: "large_direct_" + k for k in ("text", "image", "error")}
LARGE_EXACT["large_terminal"] = "large_direct_terminal"
LARGE_CASES = [*LARGE_DIRECT, *LARGE_EXACT, "large_inspect", "large_release", "large_select_throw"]
CASES.update(LARGE_DIRECT)
for kind in ("text", "image"):
    CASES["large_result_" + kind] = {"code": "return tools.result(await tools.parity_large({kind:'"
        + kind + "',business:" + BUSINESS_JS + "}));"}
CASES["large_result_error"] = {"code": "try {await tools.parity_large({kind:'error'});} catch(e) {return tools.result(e.result);} throw Error('missing native error');"}
CASES["large_terminal"] = {"code": "await tools.parity_large({kind:'text',business:" + BUSINESS_JS
    + ",terminate:true}); await tools.write({path:'terminal-must-not-exist.txt',content:'bad'}); throw Error('JS resumed after terminal child');"}
CASES["large_inspect"] = {"code": "const r=await tools.parity_large({kind:'text',business:" + BUSINESS_JS + "}); "
    "if(!Object.isFrozen(r)||!Object.isFrozen(r.content)||!Object.isFrozen(r.content[0])||!Object.isFrozen(r.details)) throw Error('receipt not frozen'); "
    "let rejected=0; for(const mutate of [()=>{'use strict';r.details={bad:true}},()=>{'use strict';r.content[0].text='bad'},()=>{'use strict';r.details.business.runtimeKey='bad'}]) "
    "{try {mutate();} catch(e) {rejected++;}} if(rejected!==3) throw Error('mutation silently ignored'); "
    "return {length:r.content[0].text.length,blocks:r.content.length,fixture:r.details.fixture,business:r.details.business,rejected};"}
CASES["large_release"] = {"code": "for(let i=0;i<20;i++){const r=await tools.parity_large({kind:'text',business:"
    + BUSINESS_JS + "}); if(!Object.isFrozen(r)||r.content.length!==1||r.details.business.runtimeKey!=='customer-key') throw Error('bad receipt'); "
    "await tools.release(r);} return {released:20};", "timeoutMs":120000}
CASES["large_select_throw"] = {"code": "tools.result(await tools.parity_large({kind:'text',business:"
    + BUSINESS_JS + "})); throw Error('discard selected native receipt');"}
TERMINAL_CASES = {"fixture_terminal", "large_direct_terminal", "large_terminal"}
IMAGE_CASES = {"png_return", "png_emit", "png_emit_throw", "large_direct_image", "large_result_image"}


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


def large_fixtures(workspace):
    """Deterministic incompressible 768x768 RGB, below native resize dimension cap."""
    def chunk(kind, data):
        return (struct.pack(">I", len(data)) + kind + data
                + struct.pack(">I", zlib.crc32(kind + data) & 0xffffffff))
    noise = hashlib.shake_256(b"rpi-native-large-PNG-v1").digest(768 * 768 * 3)
    scanlines = b"".join(bytes([0]) + noise[i:i + 768 * 3] for i in range(0, len(noise), 768 * 3))
    png = (bytes([137,80,78,71,13,10,26,10]) + chunk(b"IHDR", struct.pack(">IIBBBBB", 768, 768, 8, 2, 0, 0, 0))
           + chunk(b"IDAT", zlib.compress(scanlines)) + chunk(b"IEND", b""))
    data = base64.b64encode(png).decode()
    require(len(data) > MIB, "PNG did not exercise large native result")
    common = {"details":{"fixture":"native-large", "business":BUSINESS}, "terminate":False,
              "usage":{"input":1,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":3,
                       "cost":{"input":.1,"output":.2,"cacheRead":0,"cacheWrite":0,"total":.3}},
              "addedToolNames":["fixture-added","fixture-added"]}
    dump(workspace / "large-native-text-result.json", {**common,"content":[{"type":"text","text":LARGE_TEXT}]})
    # Keep the native receipt large even if the host reencodes/resizes image bytes.
    image_common = {**common,"details":{**common["details"],"padding":"i" * (MIB + 32)}}
    dump(workspace / "large-native-image-result.json", {**image_common,"content":[
        {"type":"text","text":"native large image"}, {"type":"image","data":data,"mimeType":"image/png"}]})


def bounded_value(value):
    """Retain exact small fields and size/SHA256 for big strings, never big copies."""
    if isinstance(value, str) and len(value.encode()) > 4096:
        raw = value.encode()
        return {"$largeString":{"bytes":len(raw),"sha256":hashlib.sha256(raw).hexdigest()}}
    if isinstance(value, list):
        return [bounded_value(v) for v in value]
    if isinstance(value, dict):
        return {k:bounded_value(v) for k,v in value.items()}
    return value


def payload_digest(value):
    raw = json.dumps(exact_payload(value), sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode()
    return {"bytes":len(raw), "sha256":hashlib.sha256(raw).hexdigest()}


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
        self.calls, self.errors, self.retained_bytes = [], [], 0
        self.image_evidence, self.result_evidence = {}, {}
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                try:
                    require(self.path == "/v1/chat/completions", self.path)
                    size = int(self.headers["Content-Length"])
                    require(0 < size <= HTTP_BYTES, "provider input budget")
                    body = json.loads(self.rfile.read(size))
                    require(len(owner.calls) < HTTP_REQUESTS, "provider request budget")
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
                        if case in LARGE_CASES:
                            require("parity_large" in names, "missing large legacy fixture tool")
                        delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": "smoke-" + case,
                            "type": "function", "function": {"name": ("parity_probe" if case in DIRECT_CASES else "parity_large" if case in LARGE_DIRECT else "codemode"),
                            "arguments": json.dumps(CASES[case])}}]}
                        finish = "tool_calls"
                    else:
                        require(len(results) == 1, "expected one direct or outer result")
                        require(case not in TERMINAL_CASES, "terminal child triggered a second model turn")
                        require(results[0]["tool_call_id"] == "smoke-" + case, "wrong current tool result identity")
                        require(image_count(messages[last + 1:]) == (1 if case in IMAGE_CASES else 0),
                                "wrong image count in latest case segment: " + case)
                        delta, finish = {"role": "assistant", "content": "DONE:" + case}, "stop"
                    # Validate real request BEFORE replacing large strings.
                    assert_no_binding(body)
                    if results and case in ("large_direct_image", "large_result_image"):
                        images = [b for m in messages[last + 1:] for b in
                                  (m.get("content") if isinstance(m.get("content"), list) else [])
                                  if b.get("type") in ("image_url", "input_image", "image")]
                        require(len(images) == 1, "large provider image missing")
                        image = images[0]
                        url = image.get("image_url", {})
                        url = url.get("url", "") if isinstance(url, dict) else url
                        require(isinstance(url, str) and url.startswith("data:image/"), "provider image not inline")
                        decoded = base64.b64decode(url.split(",", 1)[1], validate=True)
                        require(decoded.startswith((bytes([137,80,78,71,13,10,26,10]), bytes([255,216,255]))), "invalid provider image")
                        owner.image_evidence[case] = bounded_value(image)
                    if results and case in LARGE_CASES:
                        owner.result_evidence[case] = bounded_value(results[0].get("content"))
                    retained = bounded_value(body)
                    retained_bytes = len(json.dumps(retained).encode())
                    require(owner.retained_bytes + retained_bytes <= 8 * MIB, "provider evidence retention budget")
                    owner.calls.append(retained)
                    owner.retained_bytes += retained_bytes
                    dump(root / "provider-requests.json", owner.calls)
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
        self.last_end = None
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
        elif kind == "tool_execution_end":
            require(len(self.ends) < 64, "end metadata retention budget")
            result = frame.get("result", {})
            self.ends[frame["toolCallId"]] = {"terminate": result.get("terminate"), "isError": frame.get("isError")}
            # One bounded native payload only, including direct baselines; no backlog.
            require(len(line.encode()) <= resource.MAX_LINE, "native end retention budget")
            self.last_end = {"toolCallId":frame["toolCallId"], "toolName":frame["toolName"],
                             "result":{**result,"isError":frame.get("isError", False)}}
        elif kind == "ui_dialog_request":
            require(frame.get("toolCallId", "").startswith("tp-"), "approval is not a nested call")
            require(len(self.denials) < 8, "denial retention budget")
            self.denials.append(frame)
            self.send({"type": "ui_dialog_response", "requestId": frame["requestId"], "answer": False})


def outer_result(host, expected_tool="codemode"):
    data = host.call("get_messages", limit=64)
    results = [m for m in data["messages"] if m.get("role") in ("toolResult", "tool")]
    require(results, data)
    result = results[-1]
    require(result.get("toolName", result.get("tool_name")) == expected_tool, result)
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


def exact_payload(result):
    # Compare ALL payload fields; only identity/time and added-name set
    # semantics differ at the outer native accounting boundary.
    ignored = {"role", "toolCallId", "tool_call_id", "toolName", "tool_name", "timestamp", "entryId"}
    value = {k: v for k, v in result.items() if k not in ignored}
    if "addedToolNames" in value:
        value["addedToolNames"] = sorted(set(value["addedToolNames"]))
    return value


def check_binding(raw):
    details = raw["details"]
    require(details["business"] == BUSINESS, details)
    echo = details["bindingEcho"]
    require(echo["metadata"] is None, "actual metadata was not null")
    require(echo["key"] == "[redacted invocation credential]", "private key escaped")
    require(echo["embedded"] == "before:[redacted invocation credential]:after", echo)
    require(isinstance(echo["handle"], int) and echo["handle"] > 0, "bare handle was redacted")
    require(echo["business"] == BUSINESS, echo)
    require(json.loads(echo["serialized"]) == {
        "metadata": None, "key": "[redacted invocation credential]", "business": BUSINESS}, echo)
    require(json.loads(raw["content"][0]["text"]) == details, "text echo differs from object echo")
    require(raw["content"][0]["textSignature"] == "native-parity-signature", raw)
    # Private invocation keys are UUID v4; numeric handles alone aren't secrets.
    require(not re.search(r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}",
                          json.dumps(raw), re.I), "private invocation UUID escaped")


def check_fixture(case, result, summary, host, provider, baselines):
    count = 5 if case == "fixture_aliases" else 2 if case == "fixture_success" else 1
    usage = result.get("usage") or {}
    require(usage.get("input") == count and usage.get("output") == 2 * count
            and usage.get("totalTokens") == 3 * count, result)
    require(set(result.get("addedToolNames", [])) == {"fixture-added"}, result)
    if case not in DIRECT_CASES:
        require(result["addedToolNames"] == ["fixture-added"], "outer names not deduped")
    for key, value in {"input": .1, "output": .2, "cacheRead": 0, "cacheWrite": 0, "total": .3}.items():
        require(abs(usage["cost"][key] - value * count) < 1e-9, usage)
    failed = case in ("fixture_throw", "fixture_terminal", "fixture_direct_error", "fixture_result_error", "fixture_result_undefined_details")
    require(bool(result.get("isError")) == failed, result)
    if case in DIRECT_CASES:
        require(summary == {"fixture": "native-parity", "business": BUSINESS}, summary)
        require(result["content"] == [{"type": "text", "text": "native parity probe",
                "textSignature": "native-parity-signature"}], result)
        baselines[case] = result
        require(not any(p["toolCallId"] == "smoke-" + case for p in host.progress),
                "direct baseline went through CodeMode")
        return
    if case in EXACT_CASES:
        baseline = exact_payload(baselines[EXACT_CASES[case]])
        if case in MUTATED_DETAILS_CASES:
            baseline["details"] = None
        require(exact_payload(result) == baseline,
                {"case": case, "actual": exact_payload(result), "baseline": baseline})
        require(summary == (None if case in MUTATED_DETAILS_CASES else
                            {"fixture": "native-parity", "business": BUSINESS}),
                "exact details contain a CodeMode summary or lack native null")
    elif case in ("fixture_business", *BINDING_CASES, "fixture_aliases"):
        require(summary["status"] == "completed", summary)
        value = summary["result"]
        if case == "fixture_business":
            require(value["guest"] == BUSINESS and value["raw"]["details"]["business"] == BUSINESS, value)
            schema = value["schema"]["parameters"]
            require(schema["properties"]["business"] == {
                "type": "object", "additionalProperties": True, "properties": {
                    "runtimeKey": {"type": "string"}, "runtimeHandle": {},
                    "__rpi": {"type": "object", "additionalProperties": True,
                              "properties": {"runtimeKey": {"type": "string"}}},
                    "__rpiRuntimeKey": {"type": "string"}}}, schema)
            require(schema["properties"]["echoBinding"] == {"type": "boolean"}, schema)
            require(value["raw"]["content"][0]["textSignature"] == "native-parity-signature", value)
        elif case in BINDING_CASES:
            check_binding(value["raw"])
            require(value["guest"] == value["raw"] and value["text"] == value["raw"]["details"], value)
            require(value["caught"] == case.endswith("error"), value)
        else:
            require(len(value) == 5 and all(v["details"]["business"] == BUSINESS for v in value), value)
    if case == "fixture_throw":
        require(summary["status"] == "error" and "after native child" in summary["error"], summary)
    if case == "fixture_caught_error":
        raw = summary["result"]["raw"]
        require(raw["usage"]["totalTokens"] == 3 and raw["details"]["fixture"] == "native-parity", summary)
        require(raw["addedToolNames"] == ["fixture-added", "fixture-added"], summary)
    parent_id = "smoke-" + case
    progress = [p for p in host.progress if p["toolCallId"] == parent_id
                and p["tool"] in (["call", "list", "emit", "result", "release"] if case == "fixture_aliases" else ["parity_probe"])]
    terminal_phase = "failed" if case in ("fixture_caught_error", "fixture_terminal", "fixture_result_error", "fixture_result_undefined_details", "fixture_binding_error") else "completed"
    for phase in ("started", "progress", terminal_phase):
        require(sum(p["phase"] == phase for p in progress) == count, progress)
    require(len({p["requestId"] for p in progress}) == count, progress)
    require(all(p["updateCount"] == 1 for p in progress if p["phase"] == "progress"), progress)
    if case == "fixture_terminal":
        # Terminal results now attach the original native receipt, not an
        # extension diagnostic wrapper. Outcome remains host-authoritative.
        require(summary == {"fixture": "native-parity"}, summary)
        require(result["content"] == [{"type": "text", "text": "native parity probe",
                "textSignature": "native-parity-signature"}], result)
        require(host.ends.get(parent_id) == {"terminate": True, "isError": True}, host.ends)
        require(sum(latest_case(c["messages"])[1] == case for c in provider.calls) == 1,
                "terminal error performed another provider turn")


def check_large(case, host, provider, baselines, root):
    end = host.last_end
    require(end and end["toolCallId"] == "smoke-" + case, "missing exact native end event: " + case)
    require(end["toolName"] == ("parity_large" if case in LARGE_DIRECT else "codemode"), "wrong native tool")
    result = end["result"]
    assert_no_binding(result)
    error = case in ("large_direct_error", "large_result_error", "large_select_throw")
    require(bool(result.get("isError")) == error, "wrong large error flag")
    require(bool(result.get("terminate")) == (case in TERMINAL_CASES), "wrong large terminal flag")
    count = 20 if case == "large_release" else 1
    if case in ("large_direct_error", "large_result_error"):
        require(result.get("usage") is None and not result.get("addedToolNames"), "AgentError invented accounting")
    else:
        usage = result.get("usage") or {}
        require(all(usage.get(k) == n * count for k,n in
                    {"input":1,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":3}.items()), "large accounting not counted once")
        for key,n in {"input":.1,"output":.2,"cacheRead":0,"cacheWrite":0,"total":.3}.items():
            require(abs(usage["cost"][key] - n * count) < 1e-9, "large cost not counted once")
        require(set(result.get("addedToolNames", [])) == {"fixture-added"}, "large names missing/forged")
        if case not in LARGE_DIRECT:
            require(result["addedToolNames"] == ["fixture-added"], "large names not deduped")
    payload = payload_digest(result)
    if case in LARGE_DIRECT or case in LARGE_EXACT:
        require(payload["bytes"] > MIB, "not a real large native result")
    if case in LARGE_DIRECT:
        baselines[case] = payload
        if case not in TERMINAL_CASES:
            baselines[case + "_provider"] = provider.result_evidence[case]
        if case == "large_direct_image":
            baselines["provider_image"] = provider.image_evidence[case]
    elif case in LARGE_EXACT:
        require(payload == baselines[LARGE_EXACT[case]],
                {"case":case,"actual":payload,"direct":baselines[LARGE_EXACT[case]]})
        if case not in TERMINAL_CASES:
            require(provider.result_evidence[case] == baselines[LARGE_EXACT[case] + "_provider"],
                    "direct/composed provider tool content differs")
        if case == "large_result_image":
            require(provider.image_evidence[case] == baselines["provider_image"], "direct/composed provider image differs")
    else:
        summary = result["details"]
        if case == "large_inspect":
            require(summary["status"] == "completed", bounded_value(summary))
            # JS string length is UTF-16 code units, not Python Unicode scalars.
            require(summary["result"] == {"length":len(LARGE_TEXT.encode("utf-16-le")) // 2,
                    "blocks":1,"fixture":"native-large","business":BUSINESS,"rejected":3}, "lazy inspection/mutation mismatch")
        elif case == "large_release":
            require(summary["status"] == "completed" and summary["result"] == {"released":20}, "release loop failed")
            # Twenty 2 MiB results exceed both 16 references and 64 MiB charged
            # native+encoded storage if release fails to recover either quota.
        else:
            require(summary["status"] == "error" and "discard selected native receipt" in summary["error"], "selection failure lost")
            require(payload["bytes"] < 128 * 1024 and not any(b.get("type") == "image" for b in result["content"]), "failed selection forwarded native payload")
    if case in TERMINAL_CASES:
        require(len(provider.calls) == 1, "terminal child resumed provider")
        require(not (root / "workspace/terminal-must-not-exist.txt").exists(), "terminal child resumed JS")
    else:
        require(len(provider.calls) == 2, "large result not delivered to provider")
    count = 0 if case in LARGE_DIRECT else count
    progress = [p for p in host.progress if p["toolCallId"] == "smoke-" + case and p["tool"] == "parity_large"]
    for phase in ("started", "progress", "failed" if case == "large_result_error" else "completed"):
        require(sum(p["phase"] == phase for p in progress) == count, "large progress lifecycle: " + case)
    require(len({p["requestId"] for p in progress}) == count, "large progress identity")
    require(all(p["updateCount"] == 1 for p in progress if p["phase"] == "progress"), "large partial not exactly once")
    dump(root / "large-payload.json", {"case":case,"payload":payload,"fields":bounded_value(result)})


def assert_no_binding(value):
    if isinstance(value, dict):
        # Field-name bans reject legitimate data. Actual injected metadata has
        # both a numeric handle and a UUID credential, including text echoes.
        key = value.get("runtimeKey")
        require(not (isinstance(key, str) and isinstance(value.get("runtimeHandle"), int)
                     and re.fullmatch(r"[0-9a-f-]{36}", key, re.I)), "injected binding escaped")
        for item in value.values():
            assert_no_binding(item)
    elif isinstance(value, list):
        for item in value:
            assert_no_binding(item)
    elif isinstance(value, str):
        try:
            decoded = json.loads(value)
        except (ValueError, RecursionError):
            return
        if isinstance(decoded, (dict, list)):
            assert_no_binding(decoded)


def run_group(args, root, gated=False, selected_cases=None, large_baselines=None):
    root.mkdir()
    provider = Provider(root)
    host = None
    try:
        options = SimpleNamespace(live=False, model="synthetic", max_output_tokens=2048)
        env = resource.environment(root, options, f"http://127.0.0.1:{provider.server.server_port}/v1")
        models_path = root / "agent/models.json"
        models = json.loads(models_path.read_text(encoding="utf-8"))
        model = models["providers"]["resource-fixture"]["models"][0]
        model["input"] = ["text", "image"]
        model["contextWindow"] = 8_000_000  # Valid model config; avoid synthetic compaction.
        dump(models_path, models)
        if selected_cases:
            large_fixtures(root / "workspace")
        (root / "workspace/pixel.png").write_bytes(png_fixture())
        (root / "workspace/seed.txt").write_text("native-code-mode-中文", encoding="utf-8")
        command = [str(args.rpi), "--mode", "rpc", "--provider", "resource-fixture", "--model", "synthetic",
                   "--session-dir", str(root / "sessions"), "--session-id", str(uuid.uuid4()),
                   "--rpc-message-updates", "compact-deltas", "--local-only", "--no-skills", "--no-context-files",
                   "--no-prompt-templates", "--tools", "read,write,codemode" + (",parity_probe,parity_large,call,list,emit,result,release" if args.fixture_extension and not gated else ""), "--thinking", "off", "--timeout", "15",
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
        cases = selected_cases or (["denied"] if gated else BASE_CASES + (FIXTURE_CASES if args.fixture_extension else []))
        findings, baselines = [], {}
        for case in cases:
            require(host.call("prompt", message=case) == {"accepted": True}, "prompt acceptance")
            if case == "cancel":
                time.sleep(.3)
                require(host.call("abort").get("aborted") is True, "abort did not reach running program")
                host.wait(lambda f: f.get("type") == "agent_settled")
                findings.append({"case": case, "passed": True})
                continue
            host.settlement()
            if case in LARGE_CASES:
                check_large(case, host, provider, large_baselines, root)
                findings.append({"case":case,"passed":True,"validation":"native end event + provider delivery"})
                continue
            result, summary = outer_result(host, "parity_probe" if case in DIRECT_CASES else "codemode")
            if case == "list":
                require(not result.get("isError"), result)
                require(summary["protocolVersion"] == 1 and len(summary["tools"]) <= 2, summary)
                require(all(t["name"] != "codemode" for t in summary["tools"]), summary)
                require(summary["capabilities"] == {"resultEnvelope": 1, "hostAccounting": 1, "parentProgress": 1, "losslessValues": 1, "resultReferences": 1}, summary)
            elif case in ("png_return", "png_emit", "png_intermediate", "ordinary_json", "fabricated_usage"):
                check_native(case, result, summary)
            elif case.startswith("fixture_"):
                check_fixture(case, result, summary, host, provider, baselines)
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
        # Not a large transcript-equality test: bounded last-message snapshot,
        # or user summary when terminal completion leaves a huge tool message.
        data = (host.call("get_message_summaries", limit=1) if selected_cases and cases[-1] in TERMINAL_CASES
                else host.call("get_messages", limit=1 if selected_cases else 64))
        dump(root / "messages.json", bounded_value(data))
        assert_no_binding(data)
        host.exit_ok()
        sessions = list((root / "sessions").rglob("*.jsonl"))
        require(sessions, "no native JSONL transcript")
        intents, finished = [], []
        for path in sessions:
            with path.open(encoding="utf-8") as stream:
                while True:
                    line = stream.readline(resource.MAX_LINE + 1)
                    if not line:
                        break
                    require(len(line.encode()) <= resource.MAX_LINE, "transcript line budget")
                    if not line.strip():
                        continue
                    entry = json.loads(line)
                    assert_no_binding(entry)
                    kind = entry.get("customType")
                    if kind in ("rpi.tool_program.intent.v1", "rpi.tool_program.finished.v1"):
                        dest = intents if kind.endswith("intent.v1") else finished
                        require(len(dest) < 1024, "lifecycle retention budget")
                        dest.append(entry["data"])
        if not gated:
            if selected_cases and cases[0] in LARGE_DIRECT:
                require(not intents and not finished, "direct baseline nested")
            else:
                require(intents and finished, "no durable native markers")
        assert_no_binding(provider.calls)
        with (root / "process.jsonl").open(encoding="utf-8") as stream:
            for line in stream:
                assert_no_binding(json.loads(line))
        require(all(e["replay"] == "never" for e in intents), "native child is replayable")
        for case in FIXTURE_CASES:
            if case not in cases:
                continue
            expected = 0 if case in DIRECT_CASES else 5 if case == "fixture_aliases" else 2 if case == "fixture_success" else 1
            starts = [e for e in intents if e["rootToolCallId"] == "smoke-" + case]
            ends = [e for e in finished if e["rootToolCallId"] == "smoke-" + case]
            require(len(starts) == expected and len(ends) == expected,
                    "native child replayed/lost, or direct baseline nested: " + case)
            require({e["requestId"] for e in starts} == {e["requestId"] for e in ends},
                    "unmatched native lifecycle marker: " + case)
            child_error = case in ("fixture_caught_error", "fixture_terminal",
                                   "fixture_result_error", "fixture_result_undefined_details",
                                   "fixture_binding_error")
            require(all(e["isError"] == child_error for e in ends),
                    "native child lost its error: " + case)
        for case in (c for c in cases if c in LARGE_CASES):
            expected = 0 if case in LARGE_DIRECT else 20 if case == "large_release" else 1
            starts = [e for e in intents if e["rootToolCallId"] == "smoke-" + case]
            ends = [e for e in finished if e["rootToolCallId"] == "smoke-" + case]
            require(len(starts) == len(ends) == expected, "large native replay/lost lifecycle: " + case)
            require({e["requestId"] for e in starts} == {e["requestId"] for e in ends}, "large unmatched lifecycle")
            require(all(e["isError"] == (case == "large_result_error") for e in ends), "large child error lost")
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
        if args.fixture_extension:
            baselines = {}
            for case in LARGE_CASES:
                result["cases"] += run_group(args, root / case, selected_cases=[case], large_baselines=baselines)
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
