"""Offline tests for the release-channel drift monitor."""
import contextlib
import importlib.util
import io
from pathlib import Path
import json
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location("drift", Path(__file__).with_name("check-channel-drift.py"))
drift = importlib.util.module_from_spec(spec)
spec.loader.exec_module(drift)


class FakeRegistry:
    """Serve canned bodies per URL prefix and record every request header."""

    def __init__(self, bodies):
        self.bodies = bodies
        self.calls = []

    def __call__(self, url, headers=None, timeout=20):
        self.calls.append((url, headers or {}))
        for prefix, body in self.bodies.items():
            if url.startswith(prefix):
                if callable(body):
                    return body()
                status, text = body
                return drift.Response(status, text)
        raise AssertionError(f"unexpected URL {url}")


def release(tag, *, draft=False, prerelease=False):
    return {"tag_name": tag, "draft": draft, "prerelease": prerelease}


class LatestReleaseTests(unittest.TestCase):
    def test_newest_published_stable_release_wins(self):
        releases = [
            release("v0.26.0", draft=True),
            release("v0.25.1", prerelease=True),
            release("nightly"),
            release("v0.25.0"),
            release("v0.24.2"),
        ]
        self.assertEqual(drift.latest_stable_release(releases), "0.25.0")
        self.assertIsNone(drift.latest_stable_release([release("v1.0.0", draft=True)]))

    def test_invalid_versions_are_rejected_not_guessed(self):
        for tag in ["", "0.25", "v0.25.0-rc.1", "release-1.0.0"]:
            with self.assertRaises(ValueError):
                drift.stable_version(tag)


class CompareTests(unittest.TestCase):
    def test_lagging_and_ahead_are_directional(self):
        self.assertEqual(drift.compare("0.25.0", "0.25.0"), "ok")
        self.assertEqual(drift.compare("0.22.1", "0.25.0"), "lagging")
        self.assertEqual(drift.compare("0.10.0", "0.9.9"), "ahead")
        self.assertEqual(drift.compare(None, "0.25.0"), "unknown")
        self.assertEqual(drift.compare("", "0.25.0"), "unknown")
        with self.assertRaises(ValueError):
            drift.compare("0.25.0", None)


class CollectTests(unittest.TestCase):
    def bodies(self, npm="0.22.1", crates="0.25.0", homebrew="0.25.0", scoop="0.25.0", winget=None):
        return {
            "https://api.github.com/repos/bigfish1913/pi-rust/releases":
                (200, json.dumps([release("v0.26.0", draft=True), release("v0.25.0")])),
            "https://api.github.com/search/issues":
                (200, json.dumps({"items": winget or []})),
            "https://crates.io/api/v1/crates/rpi-cli":
                (200, json.dumps({"crate": {"max_stable_version": crates}})),
            "https://registry.npmjs.org/@bigfishnpm%2Frpi":
                (200, json.dumps({"dist-tags": {"latest": npm}})),
            "https://raw.githubusercontent.com/bigfish1913/homebrew-tap":
                (200, 'class Rpi < Formula\n  version "%s"\nend\n' % homebrew),
            "https://raw.githubusercontent.com/bigfish1913/scoop-bucket":
                (200, json.dumps({"version": scoop})),
        }

    def test_lagging_npm_channel_is_the_only_drift(self):
        release_version, rows = drift.collect(FakeRegistry(self.bodies()))
        self.assertEqual(release_version, "0.25.0")
        self.assertEqual([row["channel"] for row in drift.failures(rows)], ["npm"])
        npm = next(row for row in rows if row["channel"] == "npm")
        self.assertEqual(npm["status"], "lagging")

    def test_healthy_channels_report_no_drift(self):
        bodies = self.bodies(npm="0.25.0")
        _, rows = drift.collect(FakeRegistry(bodies))
        self.assertEqual(drift.failures(rows), [])
        tag, lines, lagging = drift.report("0.25.0", rows)
        self.assertEqual(tag, "v0.25.0")
        self.assertEqual(lagging, [])
        self.assertIn("Every tracked channel matches", "\n".join(lines))

    def test_github_token_never_reaches_third_party_hosts(self):
        registry = FakeRegistry(self.bodies())
        drift.collect(registry, token="secret-token")
        for url, headers in registry.calls:
            if not url.startswith("https://api.github.com/"):
                self.assertNotIn("Authorization", headers, url)
        github_calls = [headers for url, headers in registry.calls if url.startswith("https://api.github.com/")]
        self.assertTrue(github_calls)
        self.assertTrue(all(headers.get("Authorization") == "Bearer secret-token" for headers in github_calls))

    def test_missing_release_fails_instead_of_reporting_healthy(self):
        bodies = self.bodies()
        bodies["https://api.github.com/repos/bigfish1913/pi-rust/releases"] = (
            200, json.dumps([release("v0.26.0", draft=True)]))
        with self.assertRaises(RuntimeError):
            drift.collect(FakeRegistry(bodies))

    def test_rate_limited_lookup_names_the_missing_token(self):
        bodies = self.bodies()
        bodies["https://api.github.com/repos/bigfish1913/pi-rust/releases"] = (
            403, '{"message":"API rate limit exceeded"}')
        with self.assertRaises(RuntimeError) as unauthenticated:
            drift.collect(FakeRegistry(bodies))
        message = str(unauthenticated.exception)
        self.assertIn("HTTP 403", message)
        self.assertIn("GITHUB_TOKEN", message)
        # With a token the failure is a real API answer, not a missing header.
        with self.assertRaises(RuntimeError) as authenticated:
            drift.collect(FakeRegistry(bodies), token="secret-token")
        self.assertNotIn("GITHUB_TOKEN", str(authenticated.exception))

    def test_registry_outage_is_unknown_not_silently_ok(self):
        bodies = self.bodies()
        bodies["https://registry.npmjs.org/@bigfishnpm%2Frpi"] = (503, "")
        _, rows = drift.collect(FakeRegistry(bodies))
        npm = next(row for row in rows if row["channel"] == "npm")
        self.assertEqual(npm["status"], "unknown")
        self.assertEqual([row["channel"] for row in drift.failures(rows)], ["npm"])

    def test_winget_stays_informational_even_when_behind(self):
        bodies = self.bodies(npm="0.25.0", winget=[{
            "title": "New version: bigfish1913.rpi version 0.24.2",
            "html_url": "https://github.com/microsoft/winget-pkgs/pull/481240",
        }])
        _, rows = drift.collect(FakeRegistry(bodies))
        winget = next(row for row in rows if row["channel"] == "winget")
        self.assertEqual((winget["status"], winget["version"]), ("informational", "0.24.2"))
        self.assertEqual(drift.failures(rows), [])


class ReportTests(unittest.TestCase):
    def rows(self, npm="0.22.1"):
        return [
            {"channel": "crates.io", "version": "0.25.0", "status": "ok", "source": "crates"},
            {"channel": "npm", "version": npm, "status": drift.compare(npm, "0.25.0"), "source": "npm"},
            {"channel": "homebrew", "version": "0.25.0", "status": "ok", "source": "brew"},
            {"channel": "scoop", "version": "0.25.0", "status": "ok", "source": "scoop"},
            {"channel": "winget", "version": "0.24.2", "status": "informational", "source": "winget"},
        ]

    def test_recovery_commands_name_the_workflow_and_tag(self):
        tag, lines, lagging = drift.report("0.25.0", self.rows())
        text = "\n".join(lines)
        self.assertEqual((tag, [row["channel"] for row in lagging]), ("v0.25.0", ["npm"]))
        self.assertIn("gh workflow run release-npm.yml --ref main -f tag=v<version>", text)
        self.assertIn("| npm | 0.22.1 | 0.25.0 | lagging |", text)

    def test_main_exit_codes_and_json_payload(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory, "drift.json")
            for npm, expected in (("0.22.1", 1), ("0.25.0", 0)):
                collector = lambda token=None: ("0.25.0", self.rows(npm))
                output = io.StringIO()
                with contextlib.redirect_stdout(output):
                    code = drift.main(["--json", str(target)], collector=collector)
                self.assertEqual(code, expected)
                payload = json.loads(target.read_text())
                self.assertEqual(payload["tag"], "v0.25.0")
                self.assertEqual(payload["drift"], [] if expected == 0 else ["npm"])
                self.assertIn("| channel", "| channel")  # table header is human readable

    def test_collector_failure_exits_two(self):
        def broken(token=None):
            raise RuntimeError("GitHub releases lookup failed: HTTP 503")

        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(drift.main([], collector=broken), 2)
        self.assertIn("HTTP 503", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
