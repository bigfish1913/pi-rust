"""Offline tests for the pull-request linked-issue check."""
import contextlib
import importlib.util
import io
from pathlib import Path
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location("issue_link", Path(__file__).with_name("check-pr-issue-link.py"))
issue_link = importlib.util.module_from_spec(spec)
spec.loader.exec_module(issue_link)

REPO = "bigfish1913/pi-rust"
TEMPLATE_BODY = """\
## Summary

Something changed.

## Related issues

<!-- "Closes #123", "Refs #456". -->
"""


class FakeApi:
    """Serve canned issue payloads and record every request."""

    def __init__(self, responses):
        self.responses = responses
        self.calls = []

    def __call__(self, url, headers=None, timeout=20):
        self.calls.append((url, headers or {}))
        for suffix, response in self.responses.items():
            if url.endswith(suffix):
                if isinstance(response, Exception):
                    raise response
                return response
        return 404, "{}"


def issue_body(number, title="a real issue"):
    return 200, '{{"number": {0}, "title": "{1}"}}'.format(number, title)


def pull_request_body(number):
    return 200, '{{"number": {0}, "pull_request": {{"url": "x"}}}}'.format(number)


def verify(test, body, responses=None, author="someone", association="NONE", head_ref="fix/thing", token="s3cret"):
    """Run main() over a temporary description file and capture the output."""
    fetched = FakeApi(responses or {})
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "body.md"
        path.write_text(body, encoding="utf-8")
        out, err = io.StringIO(), io.StringIO()
        argv = [
            "--repo", REPO,
            "--body-file", str(path),
            "--author", author,
            "--author-association", association,
            "--head-ref", head_ref,
            "--token", token,
        ]
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = issue_link.main(argv, fetch=fetched)
    return code, out.getvalue(), err.getvalue(), fetched


class ExemptionTests(unittest.TestCase):
    def test_owner_is_exempt(self):
        code, out, _, fetched = verify(self, TEMPLATE_BODY, author="bigfish1913")
        self.assertEqual(code, 0)
        self.assertIn("Exempt", out)
        self.assertEqual(fetched.calls, [])

    def test_maintainer_association_is_exempt(self):
        for association in ("OWNER", "MEMBER", "member"):
            code, out, _, _ = verify(self, TEMPLATE_BODY, association=association)
            self.assertEqual(code, 0, association)
            self.assertIn("Exempt", out)

    def test_bot_author_is_exempt(self):
        code, out, _, _ = verify(self, TEMPLATE_BODY, author="github-actions[bot]")
        self.assertEqual(code, 0)
        self.assertIn("bot", out)

    def test_automation_branch_is_exempt(self):
        for head_ref in ("release-plz-0.3.0", "automation/readme-images",
                         "dependabot/cargo/serde-1.0", "renovate/all"):
            code, out, _, _ = verify(self, TEMPLATE_BODY, head_ref=head_ref)
            self.assertEqual(code, 0, head_ref)
            self.assertIn("automation", out)


class ReferenceTests(unittest.TestCase):
    def test_template_comments_do_not_count(self):
        code, _, err, fetched = verify(self, TEMPLATE_BODY)
        self.assertEqual(code, 1)
        self.assertIn("does not reference an issue", err)
        self.assertEqual(fetched.calls, [])

    def test_closing_keyword_reference_passes(self):
        responses = {"/issues/123": issue_body(123, "tray icon vanishes")}
        code, out, _, fetched = verify(self, "Closes #123", responses)
        self.assertEqual(code, 0)
        self.assertIn("Linked issue #123 exists", out)
        url, headers = fetched.calls[0]
        self.assertEqual(url, f"https://api.github.com/repos/{REPO}/issues/123")
        self.assertEqual(headers["Authorization"], "Bearer s3cret")
        self.assertIn("rpi-pr-issue-link", headers["User-Agent"])

    def test_issue_url_reference_passes(self):
        responses = {"/issues/5": issue_body(5)}
        code, out, _, _ = verify(self, f"See https://github.com/{REPO}/issues/5 for details.", responses)
        self.assertEqual(code, 0)
        self.assertIn("#5", out)

    def test_foreign_repository_url_is_ignored(self):
        responses = {"/issues/5": issue_body(5)}
        code, _, err, fetched = verify(self, "https://github.com/other/repo/issues/5", responses)
        self.assertEqual(code, 1)
        self.assertIn("does not reference an issue", err)
        self.assertEqual(fetched.calls, [])

    def test_pull_request_number_is_not_an_issue(self):
        code, _, err, _ = verify(self, "Closes #7", {"/issues/7": pull_request_body(7)})
        self.assertEqual(code, 1)
        self.assertIn("None of the referenced numbers", err)

    def test_missing_issue_is_rejected(self):
        code, _, err, _ = verify(self, "Fixes #999")
        self.assertEqual(code, 1)
        self.assertIn("#999", err)

    def test_one_missing_number_does_not_hide_a_valid_issue(self):
        responses = {"/issues/8": issue_body(8)}
        code, out, _, _ = verify(self, "Refs #404 and Closes #8", responses)
        self.assertEqual(code, 0)
        self.assertIn("#8", out)

    def test_api_failure_is_reported(self):
        code, _, err, _ = verify(self, "Closes #9", {"/issues/9": (500, "boom")})
        self.assertEqual(code, 1)
        self.assertIn("Could not verify #9", err)

    def test_duplicates_are_checked_once(self):
        code, _, _, fetched = verify(self, "Closes #12\n\nFollow-up to #12", {"/issues/12": issue_body(12)})
        self.assertEqual(code, 0)
        self.assertEqual(len(fetched.calls), 1)


class RegexTests(unittest.TestCase):
    def test_references_inside_fenced_code_are_ignored(self):
        body = "## Related issues\n\n```bash\ncurl ... #123\n```\n"
        self.assertEqual(issue_link.referenced_issues(body, REPO), [])

    def test_headings_and_colors_are_not_references(self):
        self.assertEqual(issue_link.referenced_issues("## 2 changes\ncolor #fff", REPO), [])


if __name__ == "__main__":
    unittest.main()
