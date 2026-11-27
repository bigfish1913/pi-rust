#!/usr/bin/env python3
"""Offline regressions for the daily README image refresh and support layout."""

from datetime import datetime, timezone
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().with_name("refresh-readme-images.py")
spec = importlib.util.spec_from_file_location("refresh_readme_images", SCRIPT)
images = importlib.util.module_from_spec(spec)
spec.loader.exec_module(images)


class RefreshTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.readme = Path(temporary.name) / "README.md"
        url = f"{images.WECHAT_URL}?date=2026-10-31"
        self.original = (
            f'交流群\r\n<a href="{url}"><img src="{url}"></a>\r\n'
            'https://assets.bigfisharc.cn/images/support/wechat.png\r\n'
        ).encode("utf-8")
        self.readme.write_bytes(self.original)

    def test_refreshes_both_urls_and_preserves_other_bytes(self):
        self.assertTrue(images.refresh(self.readme, "2026-11-01"))
        self.assertEqual(
            self.readme.read_bytes(),
            self.original.replace(b"2026-10-31", b"2026-11-01"),
        )

    def test_same_day_is_noop(self):
        self.assertFalse(images.refresh(self.readme, "2026-10-31"))
        self.assertEqual(self.readme.read_bytes(), self.original)

    def test_missing_or_extra_urls_fail_without_writing(self):
        for content in (b"no dated image", self.original + self.original):
            with self.subTest(content=content):
                self.readme.write_bytes(content)
                with self.assertRaises(ValueError):
                    images.refresh(self.readme, "2026-11-01")
                self.assertEqual(self.readme.read_bytes(), content)

    def test_invalid_date_fails_without_writing(self):
        with self.assertRaises(ValueError):
            images.refresh(self.readme, "2026-02-30")
        self.assertEqual(self.readme.read_bytes(), self.original)

    def test_daily_boundary_is_utc_plus_eight(self):
        before = datetime(2026, 10, 31, 15, 59, tzinfo=timezone.utc)
        after = datetime(2026, 10, 31, 16, 0, tzinfo=timezone.utc)
        self.assertEqual(before.astimezone(images.CHINA_TIME).date().isoformat(), "2026-10-31")
        self.assertEqual(after.astimezone(images.CHINA_TIME).date().isoformat(), "2026-11-01")

    def test_main_refreshes_both_readmes(self):
        chinese_readme = self.readme.with_name("README.zh-CN.md")
        chinese_readme.write_bytes(self.original)
        now = datetime(2026, 11, 1, tzinfo=images.CHINA_TIME)
        with mock.patch.object(images, "READMES", (self.readme, chinese_readme)), \
                mock.patch.object(images, "datetime", wraps=datetime) as clock, \
                mock.patch("builtins.print"):
            clock.now.return_value = now
            images.main()
        for path in (self.readme, chinese_readme):
            self.assertEqual(path.read_bytes(), self.original.replace(b"2026-10-31", b"2026-11-01"))

    def test_repository_readme_layout_and_image_sources(self):
        sections = {
            "README.md": ["Support", "Community", "Star history", "Contributors"],
            "README.zh-CN.md": ["赞赏支持", "交流群", "Star 趋势", "贡献者"],
        }
        self.assertEqual({path.name for path in images.READMES}, set(sections))
        for path in images.READMES:
            with self.subTest(readme=path.name):
                text = path.read_text(encoding="utf-8")
                headings = [line[3:] for line in text.splitlines() if line.startswith("## ")]
                self.assertEqual(headings[-4:], sections[path.name])
                for name in ("alipay.jpg", "wechat.png", "binance.png"):
                    self.assertEqual(text.count(f"https://assets.bigfisharc.cn/images/support/{name}"), 2)
                self.assertEqual(len(images.DATED_URL.findall(text)), 2)
                self.assertNotIn("{date}", text)
                self.assertIn('src="https://assets.bigfisharc.cn/images/support/tg.png"', text)
                self.assertNotIn('src="docs/images/tg.png"', text)
                self.assertIn("https://contrib.rocks/image?repo=bigfish1913/pi-rust", text)
                self.assertIn("https://github.com/bigfish1913/pi-rust/graphs/contributors", text)


pr_spec = importlib.util.spec_from_file_location(
    "prepare_readme_image_pr", SCRIPT.with_name("prepare-readme-image-pr.py")
)
image_pr = importlib.util.module_from_spec(pr_spec)
pr_spec.loader.exec_module(image_pr)


class ImagePRTests(unittest.TestCase):
    """Real git operations use disposable local repositories; gh is always mocked."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.origin = self.root / "origin.git"
        self.work = self.root / "work"
        self.real_run = image_pr.run
        self.real_run("git", "init", "--bare", str(self.origin), cwd=self.root)
        self.real_run("git", "init", "-b", "main", str(self.work), cwd=self.root)
        self.git("config", "user.name", "Offline Test")
        self.git("config", "user.email", "test@example.invalid")
        self.git("config", "core.autocrlf", "false")
        self.readmes = ("README.md", "README.zh-CN.md")
        for name in self.readmes:
            (self.work / name).write_text("old date\n", encoding="utf-8")
        self.git("add", *self.readmes)
        self.git("commit", "-m", "initial")
        self.git("remote", "add", "origin", str(self.origin))
        self.git("push", "-u", "origin", "main")
        self.main = self.git("rev-parse", "HEAD")
        self.prs = []
        self.calls = []
        self.before_push = None
        self.fail_create = False
        self.addCleanup(mock.patch.stopall)
        mock.patch.dict(os.environ, {
            "GITHUB_REPOSITORY": image_pr.REPOSITORY,
            "GITHUB_REF": "refs/heads/main",
            "GITHUB_EVENT_NAME": "schedule",
            "GH_TOKEN": "offline-placeholder",
        }).start()
        mock.patch.object(image_pr, "run", side_effect=self.command).start()

    def git(self, *args):
        return self.real_run("git", *args, cwd=self.work)

    def command(self, *args, cwd):
        self.calls.append(args)
        if args[:3] == ("gh", "pr", "list"):
            return json.dumps(self.prs)
        if args[:3] == ("gh", "pr", "create"):
            if self.fail_create:
                raise subprocess.CalledProcessError(1, args)
            self.prs = [{"number": 42, "headRepositoryOwner": {"login": "bigfish1913"},
                         "isCrossRepository": False}]
            return "https://example.invalid/pull/42"
        if args[0] == "gh":
            self.fail(f"Unexpected GitHub command: {args}")
        if "push" in args and self.before_push:
            self.before_push()
        return self.real_run(*args, cwd=cwd)

    def refresh(self, text="new date\n"):
        self.git("reset", "--hard", "origin/main")
        for name in self.readmes:
            (self.work / name).write_text(text, encoding="utf-8")

    def branch_head(self):
        return self.real_run("git", "--git-dir", str(self.origin), "rev-parse",
                             image_pr.REF, cwd=self.root)

    def assert_main_untouched(self):
        self.assertEqual(self.real_run("git", "--git-dir", str(self.origin),
                                       "rev-parse", "refs/heads/main", cwd=self.root), self.main)
        for command in self.calls:
            if "push" in command:
                self.assertEqual(command[-1], f"HEAD:{image_pr.REF}")
                self.assertTrue(any(arg.startswith("--force-with-lease=") for arg in command))

    def test_first_run_creates_branch_and_pr_without_skip_directive(self):
        self.refresh()
        image_pr.prepare(self.work)
        self.assert_main_untouched()
        self.assertEqual(len(self.prs), 1)
        pushes = [command for command in self.calls if "push" in command]
        self.assertEqual(len(pushes), 1)
        self.assertIn(f"--force-with-lease={image_pr.REF}:", pushes[0])
        self.assertNotIn("[skip ci]", self.git("log", "-1", "--format=%B"))
        self.assertEqual(set(self.git("diff", "--name-only", "origin/main", "HEAD").splitlines()),
                         set(self.readmes))
        for command in self.calls:
            self.assertNotIn("offline-placeholder", " ".join(command))

    def test_only_the_readme_with_a_stale_date_is_committed(self):
        chinese_readme = "README.zh-CN.md"
        (self.work / chinese_readme).write_text("new date\n", encoding="utf-8")
        image_pr.prepare(self.work)
        self.assertEqual(self.git("diff", "--name-only", "origin/main", "HEAD"), chinese_readme)
        self.assert_main_untouched()

    def test_same_day_reuses_branch_sha_and_pr(self):
        output = self.root / "github-output"
        with mock.patch.dict(os.environ, {"GITHUB_OUTPUT": str(output)}):
            self.refresh()
            image_pr.main(self.work)
            previous = self.branch_head()
            self.assertEqual(output.read_text(encoding="utf-8"), f"head={previous}\n")
            output.unlink()
            self.calls.clear()
            self.refresh()
            image_pr.main(self.work)
        self.assertEqual(output.read_text(encoding="utf-8"), f"head={previous}\n")
        self.assertEqual(self.branch_head(), previous)
        self.assertFalse(any("push" in command for command in self.calls))
        self.assertFalse(any(command[:3] == ("gh", "pr", "create") for command in self.calls))
        self.assert_main_untouched()

    def test_new_day_rebuilds_from_main_and_reuses_pr(self):
        self.refresh()
        image_pr.prepare(self.work)
        previous = self.branch_head()
        self.calls.clear()
        self.refresh("next day\n")
        image_pr.prepare(self.work)
        self.assertNotEqual(self.branch_head(), previous)
        self.assertEqual(self.git("rev-parse", "HEAD^"), self.main)
        self.assertFalse(any(command[:3] == ("gh", "pr", "create") for command in self.calls))
        self.assert_main_untouched()

    def test_no_changes_has_no_remote_operations_or_merge_output(self):
        output = self.root / "github-output"
        with mock.patch.dict(os.environ, {"GITHUB_OUTPUT": str(output)}):
            image_pr.main(self.work)
        self.assertFalse(output.exists())
        self.assertFalse(any(command[0] == "gh" or "push" in command for command in self.calls))

    def test_missing_token_fails_even_without_changes(self):
        with mock.patch.dict(os.environ, {"GH_TOKEN": ""}):
            with self.assertRaisesRegex(ValueError, "RELEASE_AUTOMATION_TOKEN"):
                image_pr.prepare(self.work)
        self.assertEqual(self.calls, [])

    def test_untrusted_events_repositories_and_refs_cannot_write(self):
        for key, value in (("GITHUB_EVENT_NAME", "pull_request"),
                           ("GITHUB_EVENT_NAME", "pull_request_target"),
                           ("GITHUB_EVENT_NAME", "push"),
                           ("GITHUB_REPOSITORY", "fork/pi-rust"),
                           ("GITHUB_REF", "refs/heads/preview")):
            with self.subTest(key=key, value=value), mock.patch.dict(os.environ, {key: value}):
                with self.assertRaisesRegex(ValueError, "upstream main"):
                    image_pr.prepare(self.work)
        self.assertEqual(self.calls, [])

    def test_failed_pr_creation_is_retryable_without_another_push(self):
        self.refresh()
        self.fail_create = True
        with self.assertRaises(subprocess.CalledProcessError):
            image_pr.prepare(self.work)
        previous = self.branch_head()
        self.fail_create = False
        self.calls.clear()
        self.refresh()
        image_pr.prepare(self.work)
        self.assertEqual(self.branch_head(), previous)
        self.assertFalse(any("push" in command for command in self.calls))
        self.assertEqual(len(self.prs), 1)

    def test_lease_does_not_overwrite_concurrent_branch_change(self):
        self.refresh()
        image_pr.prepare(self.work)
        original = self.branch_head()
        tree = self.git("rev-parse", f"{original}^{{tree}}")
        competitor = self.git("commit-tree", tree, "-p", original, "-m", "concurrent update")
        # Make the competing object available to the bare remote before moving its ref.
        self.git("push", "origin", f"{competitor}:refs/heads/competitor")
        self.before_push = lambda: self.real_run(
            "git", "--git-dir", str(self.origin), "update-ref", image_pr.REF, competitor,
            cwd=self.root)
        self.refresh("next day\n")
        with self.assertRaises(subprocess.CalledProcessError):
            image_pr.prepare(self.work)
        self.assertEqual(self.branch_head(), competitor)
        self.assert_main_untouched()

    def test_new_main_content_is_preserved_when_updating_existing_branch(self):
        self.refresh()
        image_pr.prepare(self.work)
        self.git("reset", "--hard", "origin/main")
        (self.work / "new-main.txt").write_text("upstream change", encoding="utf-8")
        self.git("add", "new-main.txt")
        self.git("commit", "-m", "advance main")
        self.git("push", "origin", "HEAD:main")
        self.main = self.git("rev-parse", "HEAD")
        self.refresh("next day\n")
        image_pr.prepare(self.work)
        self.assertEqual(self.git("rev-parse", "HEAD^"), self.main)
        self.assertEqual(self.git("show", "HEAD:new-main.txt"), "upstream change")
        self.assertEqual(set(self.git("diff", "--name-only", "origin/main", "HEAD").splitlines()),
                         set(self.readmes))
        self.assert_main_untouched()

    def test_api_failure_is_not_silent_success(self):
        self.refresh()

        def fail_api(*args, cwd):
            if args[:3] == ("gh", "pr", "list"):
                raise subprocess.CalledProcessError(1, args)
            return self.command(*args, cwd=cwd)

        with mock.patch.object(image_pr, "run", side_effect=fail_api):
            with self.assertRaises(subprocess.CalledProcessError):
                image_pr.prepare(self.work)
        self.assertFalse(any("push" in command for command in self.calls))
        self.assert_main_untouched()

    def test_multiple_upstream_prs_fail_before_pushing(self):
        self.refresh()
        self.prs = [{"number": number, "headRepositoryOwner": {"login": "bigfish1913"},
                     "isCrossRepository": False} for number in (1, 2)]
        with self.assertRaisesRegex(ValueError, "Multiple upstream"):
            image_pr.prepare(self.work)
        self.assertFalse(any("push" in command for command in self.calls))

    def test_fork_pr_is_not_reused(self):
        self.refresh()
        self.prs = [{"number": 1, "headRepositoryOwner": {"login": "other"},
                     "isCrossRepository": True}]
        image_pr.prepare(self.work)
        self.assertTrue(any(command[:3] == ("gh", "pr", "create") for command in self.calls))
        self.assert_main_untouched()

    def test_other_dirty_files_are_rejected(self):
        other = self.work / "other.txt"
        other.write_text("original\n", encoding="utf-8")
        self.git("add", "other.txt")
        self.git("commit", "-m", "add unrelated file")
        self.git("push", "origin", "HEAD:main")
        self.main = self.git("rev-parse", "HEAD")
        self.refresh()
        other.write_text("changed\n", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "Only README.md and README.zh-CN.md"):
            image_pr.prepare(self.work)
        self.git("add", "other.txt")
        with self.assertRaisesRegex(ValueError, "staging"):
            image_pr.prepare(self.work)
        self.assertFalse(any(command[0] == "gh" for command in self.calls))
        self.assert_main_untouched()


class WorkflowPolicyTests(unittest.TestCase):
    def test_refresh_uses_existing_automation_token_with_read_only_job_token(self):
        workflow = SCRIPT.parent.parent / ".github/workflows/readme-images.yml"
        text = workflow.read_text(encoding="utf-8")
        self.assertIn("secrets.RELEASE_AUTOMATION_TOKEN", text)
        self.assertIn("Require the automation token", text)
        self.assertNotIn("contents: write", text)
        self.assertNotIn("pull-requests: write", text)
        self.assertEqual(text.count("persist-credentials: false"), 2)
        self.assertIn("github.event_name == 'schedule' || github.event_name == 'workflow_dispatch'", text)
        self.assertNotIn("[skip ci]", text)
        self.assertNotIn("HEAD:main", text)
        self.assertIn("scripts/prepare-readme-image-pr.py", text)
        self.assertIn("if: steps.prepare.outputs.head != ''", text)
        self.assertIn("EXPECTED_HEAD: ${{ steps.prepare.outputs.head }}", text)
        self.assertIn('gh pr merge "$number"', text)
        self.assertIn('--match-head-commit "$EXPECTED_HEAD"', text)
        self.assertIn("--admin", text)
        self.assertIn("--squash", text)
        self.assertIn("--delete-branch", text)
        self.assertIn("without waiting for CI", text)
        self.assertIn("automation/readme-images", text)
        self.assertNotIn("--auto", text)


if __name__ == "__main__":
    unittest.main()
