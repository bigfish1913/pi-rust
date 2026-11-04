#!/usr/bin/env python3
"""Fail-closed controller regressions; no native binary or real user data needed."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "startup_smoke", Path(__file__).with_name("resource-lease-startup-smoke.py"))
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


def result(frames, code=0, stderr=""):
    return subprocess.CompletedProcess(["synthetic-rpi"], code,
                                       "\n".join(json.dumps(f) for f in frames), stderr)


class ProtocolTests(unittest.TestCase):
    def setUp(self):
        self.ready = {"type": "ready", "supportedCommands": ["get_state"]}
        self.reply = {"type": "response", "id": "startup-state",
                      "command": "get_state", "success": True, "data": {}}
        self.frames = [self.ready, self.reply]

    def test_startup_and_correlated_command_are_required(self):
        smoke.validate(result(self.frames))
        for frames in ([], [self.ready], [self.reply], self.frames * 2,
                       [{"type": "ready", "supportedCommands": []}, self.reply]):
            with self.subTest(frames=frames), self.assertRaises(AssertionError):
                smoke.validate(result(frames))

    def test_response_id_command_and_success_cannot_be_faked(self):
        for field, value in (("id", "wrong"), ("command", "prompt"), ("success", False)):
            with self.subTest(field=field), self.assertRaises(AssertionError):
                smoke.validate(result([self.ready, dict(self.reply, **{field: value})]))

    def test_nonzero_exit_after_ready_is_not_success(self):
        with self.assertRaises(AssertionError):
            smoke.validate(result(self.frames, code=1))

    def test_malformed_stdout_is_not_ignored(self):
        with self.assertRaises(json.JSONDecodeError):
            smoke.validate(subprocess.CompletedProcess([], 0, "invalid JSON", ""))

    def test_link_rejection_requires_the_specific_error_before_ready(self):
        smoke.validate(result([], code=1, stderr=smoke.LINK_ERROR + " owned-fixture"), True)
        for frames, code, stderr in (([], 0, smoke.LINK_ERROR), ([], 1, "no API key"),
                                     ([], 1, ""), (self.frames, 1, smoke.LINK_ERROR)):
            with self.subTest(frames=frames, code=code, stderr=stderr):
                with self.assertRaises(AssertionError):
                    smoke.validate(result(frames, code, stderr), True)


class IsolationTests(unittest.TestCase):
    def test_inherited_settings_and_credentials_are_not_forwarded(self):
        with tempfile.TemporaryDirectory() as directory:
            case = Path(directory)
            home, agent = case / "home", case / "home/.rpi/agent"
            with patch.dict(os.environ, {
                "HOME": "not-owned", "USERPROFILE": "not-owned",
                "RPI_CODING_AGENT_DIR": "not-owned", "OPENAI_API_KEY": "do-not-forward",
                "ANTHROPIC_AUTH_TOKEN": "do-not-forward", "RPI_IM_CONFIG": "not-owned",
                "RPI_SESSION_DIR": "not-owned", "HTTP_PROXY": "http://not-owned",
            }, clear=True):
                env = smoke.environment(case, home, agent)
            self.assertEqual(env["RPI_CODING_AGENT_DIR"], str(agent))
            self.assertEqual(env["HOME"], str(home))
            self.assertEqual(env["USERPROFILE"], str(home))
            for name in ("OPENAI_API_KEY", "ANTHROPIC_AUTH_TOKEN", "RPI_IM_CONFIG",
                         "RPI_SESSION_DIR", "HTTP_PROXY"):
                self.assertNotIn(name, env)

    def test_windows_known_folder_fallback_is_forbidden(self):
        with patch.object(smoke.os, "name", "nt"):
            with self.assertRaisesRegex(ValueError, "explicit owned"):
                smoke.environment(None, None, None, default_agent=True)

    def test_child_timeout_and_clean_eof_contract(self):
        calls = []
        frames = [{"type": "ready", "supportedCommands": ["get_state"]},
                  {"type": "response", "id": "startup-state", "command": "get_state",
                   "success": True}]

        def child(command, **kwargs):
            calls.append(kwargs)
            for scope in (Path(kwargs["env"]["RPI_CODING_AGENT_DIR"]),
                          kwargs["cwd"] / ".rpi"):
                scope.mkdir(parents=True, exist_ok=True)
                (scope / ".resource-lease").write_bytes(b"")
            return result(frames)

        with tempfile.TemporaryDirectory() as directory:
            with patch.object(smoke, "link_directory"), patch.object(
                    smoke.subprocess, "run", side_effect=child):
                smoke.run_case(Path("synthetic-rpi"), Path(directory), "owned")
        self.assertEqual(len(calls), 2)  # cold start and restart
        for call in calls:
            self.assertEqual(call["timeout"], smoke.TIMEOUT)
            self.assertTrue(call["capture_output"])
            self.assertEqual(json.loads(call["input"]), smoke.STATE_REQUEST)
            self.assertNotIn("prompt", call["input"])

    def test_target_snapshot_detects_added_modified_and_removed_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            sentinel = root / "sentinel"
            sentinel.write_bytes(b"unchanged")
            original = smoke.tree_state(root)
            sentinel.write_bytes(b"changed")
            self.assertNotEqual(smoke.tree_state(root), original)
            sentinel.write_bytes(b"unchanged")
            added = root / ".resource-lease"
            added.write_bytes(b"")
            self.assertNotEqual(smoke.tree_state(root), original)
            added.unlink()
            self.assertEqual(smoke.tree_state(root), original)
            sentinel.unlink()
            self.assertNotEqual(smoke.tree_state(root), original)

    def test_timeout_is_not_retried_or_treated_as_success(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(smoke, "link_directory"), patch.object(
                    smoke.subprocess, "run", side_effect=subprocess.TimeoutExpired([], 1)) as run:
                with self.assertRaises(subprocess.TimeoutExpired):
                    smoke.run_case(Path("synthetic-rpi"), Path(directory), "timeout")
                self.assertEqual(run.call_count, 1)


if __name__ == "__main__":
    unittest.main()
