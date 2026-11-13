"""Offline checks for the trusted CI cancellation recovery workflow."""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parent.parent
RECOVERY = ROOT / ".github/workflows/ci-transient-recovery.yml"


class CiTransientRecoveryTests(unittest.TestCase):
    def setUp(self):
        self.workflow = RECOVERY.read_text(encoding="utf-8")

    def test_is_workflow_run_only_and_has_no_untrusted_checkout(self):
        self.assertIn("workflow_run:", self.workflow)
        self.assertIn("types: [completed]", self.workflow)
        self.assertIn("actions: write", self.workflow)
        self.assertIn("contents: read", self.workflow)
        self.assertNotIn("actions/checkout", self.workflow)
        self.assertNotIn("secrets.", self.workflow)

    def test_only_retries_first_trusted_main_run(self):
        for expression in (
            "github.event.workflow_run.conclusion == 'failure'",
            "github.event.workflow_run.run_attempt == 1",
            "github.event.workflow_run.head_branch == 'main'",
            "github.event.workflow_run.head_repository.full_name == github.repository",
            "github.event.workflow_run.event == 'schedule'",
            "github.event.workflow_run.event == 'push'",
            "github.event.workflow_run.event == 'workflow_dispatch'",
        ):
            self.assertIn(expression, self.workflow)

    def test_only_cancellation_only_validation_failures_retry_the_whole_run(self):
        validation_families = (
            "Rust 1\\\\.88",
            "Cargo check",
            "Cargo test",
            "Windows installer",
            "Platform startup",
            "Offline resource regression",
            "Official extension compatibility",
        )
        for family in validation_families:
            self.assertIn(family, self.workflow)
        self.assertIn('conclusion == "cancelled"', self.workflow)
        self.assertIn('conclusion == "failure"', self.workflow)
        self.assertIn('gh run rerun "${RUN_ID}"', self.workflow)
        self.assertIn("run_attempt becomes 2", self.workflow)
        self.assertNotIn("gh run rerun --failed", self.workflow)


if __name__ == "__main__":
    unittest.main()
