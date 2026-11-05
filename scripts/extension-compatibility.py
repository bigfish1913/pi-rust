#!/usr/bin/env python3
"""Run the published ACP/Web/native protocol compatibility gate.

The caller supplies binaries built from the current checkout and from the
official rpi-package checkout. Every process gets an isolated agent directory,
model catalog, session directory and project. No provider request is made.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import queue
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen
import uuid


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rpi", type=Path, required=True)
    parser.add_argument("--extensions-dir", type=Path, required=True)
    parser.add_argument("--acp", type=Path, required=True)
    parser.add_argument("--web", type=Path, required=True)
    parser.add_argument("--diagnostics-dir", type=Path, required=True)
    return parser.parse_args()


def executable(path: Path) -> Path:
    if path.is_file():
        return path
    candidate = path.with_suffix(path.suffix + ".exe")
    if candidate.is_file():
        return candidate
    raise AssertionError(f"executable not found: {path}")


def clean_environment(root: Path, extensions: Path) -> dict[str, str]:
    env = dict(os.environ)
    for key in list(env):
        if key.startswith("RPI_") or key.endswith(("_API_KEY", "_AUTH_TOKEN")):
            env.pop(key, None)
    agent = root / "agent"
    agent.mkdir(parents=True, exist_ok=True)
    (root / "sessions").mkdir()
    (agent / "settings.json").write_text(
        json.dumps(
            {
                "defaultProvider": "compat-fixture",
                "defaultModel": "synthetic",
                "defaultThinkingLevel": "off",
            }
        ),
        encoding="utf-8",
    )
    (agent / "models.json").write_text(
        json.dumps(
            {
                "providers": {
                    "compat-fixture": {
                        "api": "openai-completions",
                        "baseUrl": "http://127.0.0.1:1/v1",
                        "models": [
                            {
                                "id": "synthetic",
                                "name": "synthetic",
                                "reasoning": False,
                                "input": ["text"],
                                "contextWindow": 8192,
                                "maxTokens": 256,
                            }
                        ],
                    }
                }
            }
        ),
        encoding="utf-8",
    )
    env.update(
        {
            "RPI_OFFLINE": "1",
            "RPI_CODING_AGENT_DIR": str(agent),
            "RPI_ACP_SESSION_DIR": str(root / "sessions"),
            "RPI_ACP_NO_AUTO_REGISTER_ZED": "1",
            "RPI_ACP_NO_PERMISSION_GATE": "1",
            "RPI_EXTENSIONS_DIR": str(extensions),
            "HOME": str(root / "home"),
            "USERPROFILE": str(root / "home"),
        }
    )
    (root / "home").mkdir()
    return env


def read_json_lines(process: subprocess.Popen[str], output: queue.Queue, errors: list[str]) -> None:
    def stdout_reader() -> None:
        assert process.stdout is not None
        for line in process.stdout:
            try:
                output.put(json.loads(line))
            except json.JSONDecodeError:
                errors.append(f"non-JSON stdout: {line[:200]!r}")

    def stderr_reader() -> None:
        assert process.stderr is not None
        for line in process.stderr:
            errors.append(line.rstrip())

    threading.Thread(target=stdout_reader, daemon=True).start()
    threading.Thread(target=stderr_reader, daemon=True).start()


def request(
    process: subprocess.Popen[str], output: queue.Queue, request_id: str, method: str, params: dict
) -> dict:
    assert process.stdin is not None
    process.stdin.write(
        json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        + "\n"
    )
    process.stdin.flush()
    deadline = time.monotonic() + 35
    while time.monotonic() < deadline:
        try:
            value = output.get(timeout=1)
        except queue.Empty:
            if process.poll() is not None:
                raise AssertionError(f"process exited with {process.returncode} during {method}")
            continue
        if value.get("id") == request_id:
            return value
    raise AssertionError(f"timed out waiting for {method}")


def stop(process: subprocess.Popen[str]) -> None:
    if process.stdin:
        process.stdin.close()
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=10)


def native_rpc(rpi: Path, extensions: Path, root: Path, env: dict[str, str]) -> dict:
    process = subprocess.Popen(
        [
            str(rpi),
            "--mode",
            "rpc",
            "--offline",
            "--no-session",
            "--local-only",
            "--no-skills",
            "--no-prompt-templates",
            "--no-context-files",
            "--extensions-dir",
            str(extensions),
            "--provider",
            "compat-fixture",
            "--model",
            "synthetic",
        ],
        cwd=root,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        bufsize=1,
    )
    output: queue.Queue = queue.Queue()
    errors: list[str] = []
    read_json_lines(process, output, errors)
    try:
        ready = output.get(timeout=35)
        assert ready.get("type") == "ready", ready
        supported = set(ready.get("supportedCommands", []))
        assert {"get_commands", "get_session_branch"} <= supported, ready
        commands = request(process, output, "commands", "get_commands", {})
        assert commands.get("success") is True, commands
        assert isinstance(commands.get("data", {}).get("commands"), list), commands
        branch = request(
            process,
            output,
            "branch",
            "get_session_branch",
            {"customTypes": ["durable-state"]},
        )
        assert branch.get("success") is True, branch
        data = branch.get("data", {})
        assert data.get("filtered") is True, branch
        assert isinstance(data.get("entries"), list), branch
        return {"ready": ready, "commands": commands["data"], "branch": data, "stderr": errors}
    finally:
        stop(process)


def acp_smoke(acp: Path, rpi: Path, root: Path, env: dict[str, str]) -> dict:
    acp_env = dict(env)
    acp_env["RPI_BIN"] = str(rpi)
    process = subprocess.Popen(
        [str(acp)],
        cwd=root,
        env=acp_env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        bufsize=1,
    )
    output: queue.Queue = queue.Queue()
    errors: list[str] = []
    read_json_lines(process, output, errors)
    try:
        initialized = request(process, output, "initialize", "initialize", {"protocolVersion": 1})
        assert "result" in initialized, initialized
        created = request(
            process,
            output,
            "new",
            "session/new",
            {"cwd": str(root), "mcpServers": []},
        )
        assert "result" in created, created
        session_id = created["result"]["sessionId"]
        closed = request(
            process, output, "close", "session/close", {"sessionId": session_id}
        )
        assert "result" in closed, closed
        loaded = request(
            process,
            output,
            "load",
            "session/load",
            {"sessionId": session_id, "cwd": str(root), "mcpServers": []},
        )
        assert "result" in loaded, loaded
        return {
            "initialize": initialized["result"],
            "new": created["result"],
            "close": closed["result"],
            "load": loaded["result"],
            "stderr": errors,
        }
    finally:
        stop(process)


def http_json(url: str, method: str = "GET", payload: dict | None = None) -> tuple[int, dict]:
    request_data = None if payload is None else json.dumps(payload).encode()
    request_obj = Request(
        url,
        data=request_data,
        method=method,
        headers={"Content-Type": "application/json", "X-Rpi-Web": "1"},
    )
    try:
        with urlopen(request_obj, timeout=35) as response:
            body = response.read()
            return response.status, json.loads(body) if body else {}
    except HTTPError as error:
        body = error.read()
        return error.code, json.loads(body) if body else {}


def web_smoke(web: Path, rpi: Path, root: Path, env: dict[str, str]) -> dict:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    config = root / "web.json"
    config.write_text(
        json.dumps(
            {
                "project": str(root),
                "rpiBin": str(rpi),
                "bind": "127.0.0.1",
                "port": port,
                "registry": str(root / "registry.json"),
                "sessionDir": str(root / "web-sessions"),
                "allowedHosts": [f"127.0.0.1:{port}"],
            }
        ),
        encoding="utf-8",
    )
    log = (root / "web.log").open("w", encoding="utf-8")
    process = subprocess.Popen(
        [str(web), "--config", str(config)], cwd=root, env=env, stdout=log, stderr=log
    )
    base = f"http://127.0.0.1:{port}"
    session_id = None
    try:
        deadline = time.monotonic() + 35
        while True:
            try:
                status, health = http_json(base + "/api/v1/health")
                break
            except (OSError, URLError):
                if process.poll() is not None:
                    raise AssertionError(f"web exited with {process.returncode}")
                if time.monotonic() >= deadline:
                    raise AssertionError("web health check timed out")
                time.sleep(0.1)
        assert status == 200 and health.get("ok") is True, (status, health)
        status, created = http_json(base + "/api/v1/sessions", "POST", {})
        assert status == 201 and created.get("sessionId"), (status, created)
        session_id = created["sessionId"]
        status, durable = http_json(base + f"/api/v1/sessions/{session_id}/durable")
        assert status == 200 and "state" in durable, (status, durable)
        status, snapshot = http_json(base + f"/api/v1/sessions/{session_id}/snapshot")
        assert status == 200 and snapshot.get("sessionId") == session_id, (status, snapshot)
        return {"health": health, "created": created, "durable": durable, "snapshot": {"sessionId": snapshot.get("sessionId"), "status": snapshot.get("status")}}
    finally:
        if session_id:
            try:
                http_json(base + f"/api/v1/sessions/{session_id}", "DELETE")
            except (OSError, URLError):
                pass
        process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=10)
        log.close()


def main() -> int:
    args = parse_args()
    rpi = executable(args.rpi)
    acp = executable(args.acp)
    web = executable(args.web)
    extensions = args.extensions_dir.resolve()
    assert extensions.is_dir(), extensions
    args.diagnostics_dir.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="extension-compat-", dir=args.diagnostics_dir))
    env = clean_environment(root, extensions)
    results: dict = {"rpi": str(rpi), "extensions": str(extensions)}
    try:
        results["native"] = native_rpc(rpi, extensions, root, env)
        results["acp"] = acp_smoke(acp, rpi, root, env)
        results["web"] = web_smoke(web, rpi, root, env)
        results["status"] = "passed"
        print(json.dumps(results, indent=2, ensure_ascii=False))
        return 0
    except Exception as error:
        results["status"] = "failed"
        results["error"] = repr(error)
        print(json.dumps(results, indent=2, ensure_ascii=False), file=sys.stderr)
        raise
    finally:
        (args.diagnostics_dir / "results.json").write_text(
            json.dumps(results, indent=2, ensure_ascii=False), encoding="utf-8"
        )
        if os.environ.get("RPI_KEEP_COMPAT_DIAGNOSTICS") != "1":
            shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
