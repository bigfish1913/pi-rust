"""Offline regressions for draft -> sync -> ready Release PR CI scheduling."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("automation", ROOT / "scripts/release-automation.py")
automation = importlib.util.module_from_spec(spec)
spec.loader.exec_module(automation)

# Keep this independent of YAML dependencies; compare every job's actual expression.
CONDITION = " ".join("""${{ !(github.event_name == 'pull_request' &&
    github.repository == 'bigfish1913/pi-rust' &&
    github.event.pull_request.base.ref == 'main' &&
    github.event.pull_request.head.repo.full_name == github.repository &&
    startsWith(github.event.pull_request.head.ref, 'release-plz-') &&
    github.event.pull_request.draft == true) }}""".split())


class ReadinessWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.ci = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.release = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")

    def test_ready_event_runs_ci_even_without_a_new_commit(self):
        events = self.ci.split("  pull_request:\n", 1)[1].split("  workflow_dispatch:", 1)[0]
        self.assertIn("types: [opened, synchronize, reopened, ready_for_review]", events)
        config = (ROOT / "release-plz.toml").read_text(encoding="utf-8")
        self.assertIn("pr_draft = true", config)

    def test_only_upstream_release_drafts_defer_all_validation_jobs(self):
        for job in ("msrv", "windows-installer", "check", "test", "windows-plugin-lifecycle"):
            with self.subTest(job=job):
                block = re.search(rf"^  {job}:\n(.*?)(?=^  [\w-]+:|\Z)", self.ci, re.M | re.S)[1]
                expression = re.search(r"^    if: >-\n((?:^      .*\n)+)", block, re.M)
                self.assertIsNotNone(expression, job)
                self.assertEqual(" ".join(expression[1].split()), CONDITION)

    def test_gate_scenarios(self):
        # Simulate just the pinned Actions expression, not arbitrary workflow code.
        values = {"github.event_name": "pull_request", "github.repository": automation.REPOSITORY,
                  "github.event.pull_request.base.ref": "main",
                  "github.event.pull_request.head.repo.full_name": automation.REPOSITORY,
                  "github.event.pull_request.head.ref": "release-plz-123",
                  "github.event.pull_request.draft": True}
        scenarios = [({}, False), ({"github.event.pull_request.draft": False}, True),
                     ({"github.event.pull_request.head.ref": "feature"}, True),
                     ({"github.event.pull_request.head.repo.full_name": "fork/pi-rust"}, True),
                     ({"github.event.pull_request.head.repo.full_name": None}, True),
                     ({"github.event.pull_request.base.ref": "preview"}, True),
                     ({"github.repository": "fork/pi-rust"}, True),
                     ({"github.event_name": "push"}, True),
                     ({"github.event_name": "workflow_dispatch"}, True)]
        for overrides, expected in scenarios:
            with self.subTest(overrides=overrides):
                context = values | overrides
                expression = CONDITION.removeprefix("${{ ").removesuffix(" }}")
                expression = re.sub(r"github\.[\w.]+", lambda m: repr(context[m[0]]), expression)
                expression = expression.replace("&&", " and ").replace("!(", "not (")
                expression = expression.replace("== true", "== True")
                self.assertEqual(eval(expression, {"__builtins__": {},
                                                  "startsWith": lambda s, p: s.startswith(p)}), expected)

    def test_skipped_drafts_cannot_satisfy_required_check_names(self):
        predicate = CONDITION.removeprefix("${{ !(").removesuffix(") }}")
        suffix = "${{ " + predicate + " && ' (preparing release)' || '' }}"
        for job, required_name in (("check", "Cargo check (${{ matrix.os }})"), ("test", "Cargo test")):
            with self.subTest(job=job):
                block = re.search(rf"^  {job}:\n(.*?)(?=^  [\w-]+:|\Z)", self.ci, re.M | re.S)[1]
                name = re.search(r"^    name: >-\n((?:^      .*\n)+)", block, re.M)
                self.assertIsNotNone(name)
                self.assertEqual(" ".join(name[1].split()), required_name + suffix)

    def test_prepare_orders_draft_before_updates_and_ready_after_sync(self):
        draft = self.release.index("python3 scripts/release-automation.py draft-release-prs")
        update = self.release.index("uses: release-plz/action@")
        sync = self.release.index("run: bash scripts/sync-release-pr.sh")
        self.assertLess(draft, update)
        self.assertLess(update, sync)
        self.assertIn("EXISTING_RELEASE_PR_JSON: ${{ steps.draft-release-pr.outputs.pr }}", self.release)
        self.assertIn("python3 scripts/test_release_pr_readiness.py", self.ci)
        # Do not weaken the actual consistency check or workspace tests.
        self.assertIn("git diff --exit-code -- crates/rpi-cli/embedded-docs", self.ci)
        self.assertIn("run: cargo test --workspace --locked", self.ci)


class DraftReleasePrTests(unittest.TestCase):
    def setUp(self):
        self.pr = {"number": 123, "state": "open", "draft": False,
                   "head": {"ref": "release-plz-123", "repo": {"full_name": automation.REPOSITORY}},
                   "base": {"ref": "main", "repo": {"full_name": automation.REPOSITORY}}}
        self.env = {"GITHUB_REPOSITORY": automation.REPOSITORY, "GITHUB_REF": "refs/heads/main",
                    "GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_SHA": "a" * 40}

    def invoke(self, pages):
        with patch.dict(os.environ, self.env), \
                patch.object(sys, "argv", ["script", "draft-release-prs"]), \
                patch.object(automation, "api", return_value=pages) as api, \
                patch.object(automation, "run", return_value="a" * 40) as run, \
                patch.object(automation, "output") as output:
            automation.main()
            return api, run, output

    def test_existing_ready_pr_is_drafted_and_returned_for_noop_retries(self):
        api, run, output = self.invoke([[], [self.pr]])
        api.assert_called_once_with(f"repos/{automation.REPOSITORY}/pulls?state=open&base=main&per_page=100",
                                    "--paginate", "--slurp")
        run.assert_any_call("gh", "pr", "ready", "123", "--undo", "--repo", automation.REPOSITORY)
        self.assertEqual(json.loads(output.call_args.kwargs["pr"]),
                         {"number": 123, "head_branch": "release-plz-123"})

    def test_already_draft_pr_is_idempotent_and_still_returned(self):
        self.pr["draft"] = True
        _, run, output = self.invoke([[self.pr]])
        run.assert_called_once_with("git", "rev-parse", "HEAD")
        self.assertEqual(json.loads(output.call_args.kwargs["pr"])["number"], 123)

    def test_unrelated_fork_closed_and_other_base_prs_are_not_modified(self):
        variants = []
        for section, field, value in (("head", "ref", "feature"), ("base", "ref", "preview")):
            pr = copy.deepcopy(self.pr)
            pr[section][field] = value
            variants.append(pr)
        for section in ("head", "base"):
            for repo in ({"full_name": "fork/pi-rust"}, None):
                pr = copy.deepcopy(self.pr)
                pr[section]["repo"] = repo
                variants.append(pr)
        variants.append(self.pr | {"state": "closed"})
        _, run, output = self.invoke([variants])
        run.assert_called_once_with("git", "rev-parse", "HEAD")
        output.assert_not_called()

    def test_multiple_release_prs_fail_before_mutation(self):
        with self.assertRaisesRegex(ValueError, "multiple"):
            self.invoke([[self.pr, self.pr | {"number": 124}]])

    def test_unauthorized_events_and_untested_checkout_fail_before_api(self):
        for overrides in ({"GITHUB_REPOSITORY": "fork/pi-rust"}, {"GITHUB_REF": "refs/heads/preview"},
                          {"GITHUB_EVENT_NAME": "pull_request"}, {"GITHUB_SHA": "b" * 40}):
            with self.subTest(overrides=overrides), \
                    patch.dict(os.environ, self.env | overrides), \
                    patch.object(sys, "argv", ["script", "draft-release-prs"]), \
                    patch.object(automation, "run", return_value="a" * 40), \
                    patch.object(automation, "api") as api:
                with self.assertRaises(ValueError):
                    automation.main()
                api.assert_not_called()

    def test_draft_failure_is_not_silenced(self):
        with patch.dict(os.environ, self.env), \
                patch.object(sys, "argv", ["script", "draft-release-prs"]), \
                patch.object(automation, "api", return_value=[[self.pr]]), \
                patch.object(automation, "run", side_effect=["a" * 40, RuntimeError("API failed")]), \
                patch.object(automation, "output") as output:
            with self.assertRaisesRegex(RuntimeError, "API failed"):
                automation.main()
            output.assert_not_called()


class SyncReleasePrTests(unittest.TestCase):
    def test_empty_action_output_reuses_existing_pr_and_only_readies_after_validation(self):
        for action_output in ("", "null", "{}", '{"number":456,"head_branch":"release-plz-new"}'):
            with self.subTest(action_output=action_output), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / ".git").mkdir()
                helper = root / ".git/rpi-release-helper"
                helper.write_text('#!/bin/sh\necho "validate $*" >> commands.log\nexit "${FAIL_VALIDATE:-0}"\n')
                helper.chmod(0o755)
                # Mock external commands; run the real orchestration script, offline.
                shell = r'''
                git() {
                    echo "git $*" >> commands.log
                    if [[ "$1" == diff ]]; then return "${HAS_CHANGES:-1}"; fi
                    [[ "$1" != "${FAIL_GIT:-}" ]]
                }
                cargo() { echo "cargo $*" >> commands.log; }
                python3() { echo '0.14.0'; }
                sh() { echo 'sync docs' >> commands.log; return "${FAIL_SYNC:-0}"; }
                rustc() { :; }
                gh() {
                    echo "gh $*" >> commands.log
                    if [[ "$*" == *'--json isDraft'* ]]; then
                        [[ "${FAIL_VIEW:-0}" == 0 ]] || return 1
                        echo true
                    fi
                }
                export -f git cargo python3 sh rustc gh
                bash "$SYNC_SCRIPT"
                '''
                env = os.environ | {"SYNC_SCRIPT": (ROOT / "scripts/sync-release-pr.sh").as_posix(),
                                    "RELEASE_PR_JSON": action_output,
                                    "EXISTING_RELEASE_PR_JSON": '{"number":123,"head_branch":"release-plz-old"}'}
                result = subprocess.run(["bash", "-c", shell], cwd=root, env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                log = (root / "commands.log").read_text()
                number = 456 if "456" in action_output else 123
                branch = "release-plz-new" if number == 456 else "release-plz-old"
                self.assertIn(f"git fetch origin {branch}", log)
                self.assertIn(f"gh pr ready {number}", log)
                self.assertLess(log.index("sync docs"), log.index("validate"))
                self.assertLess(log.index("validate"), log.index("git commit"))
                self.assertLess(log.index("git push"), log.index("gh pr ready"))
                for failure in ({"FAIL_VALIDATE": "1"}, {"FAIL_SYNC": "1"}, {"FAIL_GIT": "commit"},
                                {"FAIL_GIT": "push"}, {"FAIL_VIEW": "1"}):
                    with self.subTest(failure=failure):
                        (root / "commands.log").unlink()
                        result = subprocess.run(["bash", "-c", shell], cwd=root, env=env | failure,
                                                capture_output=True, text=True)
                        self.assertNotEqual(result.returncode, 0)
                        self.assertNotIn("gh pr ready", (root / "commands.log").read_text())
                (root / "commands.log").unlink()
                result = subprocess.run(["bash", "-c", shell], cwd=root, env=env | {"HAS_CHANGES": "0"},
                                        capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                log = (root / "commands.log").read_text()
                self.assertNotIn("git push", log)
                self.assertIn(f"gh pr ready {number}", log)


if __name__ == "__main__":
    unittest.main()
