#!/usr/bin/env python3
"""Offline real-binary startup regression for every release platform.

Use only an owned temporary HOME/workspace and synthetic provider configuration.
No prompts are sent. --version is insufficient: it bypasses harness/resource leases.
Windows uses junctions (no developer-mode/admin symlink privilege required).
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

TIMEOUT = 30
LINK_ERROR = "links/reparse points are not owned resource paths:"
STATE_REQUEST = {"type": "get_state", "id": "startup-state"}


def link_directory(target, alias):
    if os.name == "nt":
        result = subprocess.run(
            ["cmd", "/D", "/C", "mklink", "/J", str(alias), str(target)],
            capture_output=True, timeout=10,
        )
        if result.returncode:
            raise RuntimeError(f"mklink /J failed: {result.stdout!r}{result.stderr!r}")
    else:
        alias.symlink_to(target, target_is_directory=True)


def environment(case, home, agent, default_agent=False):
    # dirs::home_dir uses a Windows Known Folder, not HOME/USERPROFILE.
    # Never let a synthetic run fall back to that real user profile.
    if default_agent and os.name == "nt":
        raise ValueError("Windows startup smoke requires an explicit owned agent directory")
    allowed = {"SYSTEMROOT", "WINDIR", "COMSPEC", "PATH", "PATHEXT", "SYSTEMDRIVE",
               "PROCESSOR_ARCHITECTURE", "NUMBER_OF_PROCESSORS"}
    env = {k: v for k, v in os.environ.items() if k.upper() in allowed}
    for name in ("tmp", "appdata", "localappdata", "sessions"):
        (case / name).mkdir()
    env.update(
        HOME=str(home), USERPROFILE=str(home),
        APPDATA=str(case / "appdata"), LOCALAPPDATA=str(case / "localappdata"),
        XDG_CONFIG_HOME=str(home / ".config"), XDG_CACHE_HOME=str(home / ".cache"),
        XDG_DATA_HOME=str(home / ".local/share"), XDG_STATE_HOME=str(home / ".local/state"),
        TEMP=str(case / "tmp"), TMP=str(case / "tmp"), TMPDIR=str(case / "tmp"),
        RPI_OFFLINE="1", NO_COLOR="1", RPI_VOICE_AUTO_TTS="off",
    )
    if not default_agent:
        env["RPI_CODING_AGENT_DIR"] = str(agent)
    return env


def configure(agent):
    agent.mkdir(parents=True)
    (agent / "auth.json").write_text("{}\n", encoding="utf-8")
    (agent / "settings.json").write_text(json.dumps({
        "defaultProvider": "fixture", "defaultModel": "synthetic",
        "defaultThinkingLevel": "off",
    }), encoding="utf-8")
    (agent / "models.json").write_text(json.dumps({"providers": {"fixture": {
        "api": "openai-completions", "baseUrl": "http://127.0.0.1:9/v1",
        "apiKey": "local-dummy-not-a-credential", "models": [{
            "id": "synthetic", "name": "synthetic", "reasoning": False,
            "input": ["text"], "contextWindow": 100000, "maxTokens": 64,
        }],
    }}}), encoding="utf-8")


def tree_state(root):
    return {str(path.relative_to(root)): path.read_bytes() if path.is_file() else None
            for path in root.rglob("*")}


def validate(result, rejected=False):
    frames = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
    if rejected:
        if (result.returncode == 0 or LINK_ERROR not in result.stderr
                or any(f.get("type") == "ready" for f in frames)):
            raise AssertionError(f"unsafe resource link was not rejected: {result}")
        return
    if result.returncode != 0:
        raise AssertionError(f"native startup failed: {result}")
    ready = [f for f in frames if f.get("type") == "ready"]
    if len(ready) != 1 or "get_state" not in ready[0].get("supportedCommands", []):
        raise AssertionError(f"missing/invalid ready frame: {result}")
    replies = [f for f in frames if f.get("type") == "response"
               and f.get("id") == STATE_REQUEST["id"]]
    if (len(replies) != 1 or replies[0].get("command") != "get_state"
            or replies[0].get("success") is not True):
        raise AssertionError(f"missing/failed correlated get_state response: {result}")


def run_case(binary, root, label, linked_home=False, linked_cwd=False,
             default_agent=False, reject_scope=None, reject_child=None):
    case = root / label
    case.mkdir()
    physical = case / "physical"
    physical.mkdir()
    alias = case / "linked-prefix"
    link_directory(physical, alias)
    home = (alias if linked_home else physical) / "home"
    workspace = (alias if linked_cwd else physical) / "project" / "workspace"
    workspace.mkdir(parents=True)
    agent = home / ".rpi" / "agent"
    configure(agent)
    env = environment(case, home, agent, default_agent)
    if reject_scope:
        scope = agent if reject_scope == "global" else workspace / ".rpi"
        # Directory links suffice even for file-shaped lease/journal paths:
        # the link check must reject them before opening/reading the target.
        target = case / "outside-owned-scope"
        target.mkdir()
        if reject_child:
            scope.mkdir(exist_ok=True)
            link_directory(target, scope / reject_child)
        else:
            if scope == agent:
                agent.rename(target / "config")
                target = target / "config"
            link_directory(target, scope)
        (target / "sentinel").write_bytes(b"must remain unchanged")
        before = tree_state(target)
    command = [
        str(binary), "--mode", "rpc", "--provider", "fixture", "--model", "synthetic",
        "--local-only", "--no-skills", "--no-extensions", "--no-prompt-templates",
        "--no-context-files", "--session-dir", str(case / "sessions"),
        "--session-id", "lease-smoke-" + label, "--rpc-message-updates", "snapshot",
        "--timeout", "10",
    ]
    # communicate via run drains both pipes, bounds the whole child lifetime,
    # kills/reaps on timeout, and tests clean shutdown on stdin EOF (exit 0).
    for _ in range(1 if reject_scope else 2):
        result = subprocess.run(
            command, cwd=workspace, env=env,
            input=json.dumps(STATE_REQUEST) + "\n", capture_output=True,
            text=True, encoding="utf-8", errors="strict", timeout=TIMEOUT,
        )
        validate(result, rejected=bool(reject_scope))
    if reject_scope:
        if tree_state(target) != before:
            raise AssertionError("rejected link target was modified")
    else:
        for scope in (agent, workspace / ".rpi"):
            if not (scope / ".resource-lease").is_file():
                raise AssertionError(f"startup skipped resource lease: {scope}")
    print(f"PASS {label}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rpi", required=True, type=Path)
    args = parser.parse_args()
    binary = args.rpi.resolve(strict=True)
    if not binary.is_file():
        raise SystemExit(f"candidate binary does not exist: {binary}")
    with tempfile.TemporaryDirectory(prefix="rpi-resource-lease-smoke-") as directory:
        root = Path(directory)
        run_case(binary, root, "physical")
        run_case(binary, root, "linked-home", linked_home=True)
        run_case(binary, root, "linked-cwd", linked_cwd=True)
        run_case(binary, root, "linked-both", linked_home=True, linked_cwd=True)
        if os.name != "nt":
            run_case(binary, root, "default-home", linked_home=True, linked_cwd=True,
                     default_agent=True)
        else:
            print("NOTE Windows always uses an explicit owned agent directory (Known Folder API)")
        for scope in ("global", "project"):
            for child in (None, ".resource-lease", "resource-transaction.json"):
                label = "reject-" + scope + "-" + (child or "root").strip(".")
                run_case(binary, root, label, linked_home=True, linked_cwd=True,
                         reject_scope=scope, reject_child=child)
    print(f"resource lease startup PASS ({sys.platform})")


if __name__ == "__main__":
    main()
