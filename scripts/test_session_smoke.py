#!/usr/bin/env python3
"""Regression tests for the smoke controller itself; no native binary needed."""
import copy
import importlib.util
from pathlib import Path
import queue
import unittest
from unittest.mock import Mock

spec = importlib.util.spec_from_file_location("session_smoke", Path(__file__).with_name("session-smoke.py"))
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


class MessageViewTests(unittest.TestCase):
    def test_only_optional_tool_details_null_is_normalized(self):
        tool = {"role": "toolResult", "content": [], "entryId": "r1", "details": None}
        saved = {k: v for k, v in tool.items() if k != "details"}
        self.assertEqual(smoke.message_view([tool]), smoke.message_view([saved]))
        for field, value in (("entryId", "different"), ("content", ["lost"]),
                             ("details", {"preserve": True})):
            changed = dict(tool, **{field: value})
            self.assertNotEqual(smoke.message_view([tool]), smoke.message_view([changed]))
        assistant = dict(tool, role="assistant")
        self.assertIn("details", smoke.message_view([assistant])[0])

    def test_message_order_is_preserved(self):
        messages = [{"entryId": "one"}, {"entryId": "two"}]
        self.assertNotEqual(smoke.message_view(messages), smoke.message_view(messages[::-1]))


class ToolCorrelationTests(unittest.TestCase):
    def setUp(self):
        self.old = {"entryId": "old", "role": "toolResult", "toolName": "durable",
                    "toolCallId": "old-call", "content": [], "isError": False}
        self.new = [
            {"entryId": "a1", "role": "assistant", "content": [{"type": "toolCall",
             "id": "call-1", "name": "durable", "arguments": {"action": "status"}}]},
            dict(self.old, entryId="r1", toolCallId="call-1"),
        ]

    def invoke(self, added):
        host = Mock()
        host.snapshot.side_effect = [{"messages": [self.old]}, {"messages": [self.old] + added}]
        return smoke.Host.tool(host, "DURABLE_STATUS")

    def test_new_correlated_result_is_accepted(self):
        self.assertEqual(self.invoke(self.new)["toolCallId"], "call-1")

    def test_historical_result_cannot_satisfy_new_prompt(self):
        with self.assertRaises(AssertionError):
            self.invoke([])

    def test_wrong_tool_or_call_id_is_rejected(self):
        for field, value in (("toolName", "todo"), ("toolCallId", "wrong"), ("isError", True),
                             ("details", {"error": "restore failed"})):
            added = copy.deepcopy(self.new)
            added[-1][field] = value
            with self.subTest(field=field), self.assertRaises(AssertionError):
                self.invoke(added)


class RpcCorrelationTests(unittest.TestCase):
    def test_unmatched_events_are_retained_and_response_id_matched(self):
        host = smoke.Host.__new__(smoke.Host)
        host.root = Path("not-created")
        host.pending, host.queue = [], queue.Queue()
        event = {"type": "agent_settled", "outcome": "completed"}
        response = {"type": "response", "id": "two"}
        host.queue.put(event)
        host.queue.put({"type": "response", "id": "one"})
        host.queue.put(response)
        self.assertEqual(host.wait(lambda f: f.get("id") == "two"), response)
        self.assertEqual(host.wait(lambda f: f.get("type") == "agent_settled"), event)
        self.assertEqual(host.pending, [{"type": "response", "id": "one"}])

    def test_bad_json_and_eof_are_not_ignored(self):
        host = smoke.Host.__new__(smoke.Host)
        host.root = Path("not-created")
        host.pending, host.queue = [], queue.Queue()
        host.queue.put(ValueError("invalid JSON"))
        with self.assertRaisesRegex(ValueError, "invalid JSON"):
            host.wait(lambda _: False)
        host.queue.put(None)
        with self.assertRaisesRegex(AssertionError, "stdout EOF"):
            host.wait(lambda _: False)


if __name__ == "__main__":
    unittest.main()
