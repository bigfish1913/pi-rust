"""Offline tests for release authorization, retries and channel readiness."""
import copy
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch
import os
import sys
import tempfile
import tomllib

sys.dont_write_bytecode = True


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


automation = load("automation", "release-automation.py")
channels = load("channels", "verify-channel-release.py")


class ReleaseGateTests(unittest.TestCase):
    def setUp(self):
        self.commit = "a" * 40
        self.pr = {"merged_at": "2026-10-15T00:00:00Z", "merge_commit_sha": self.commit,
                   "head": {"ref": "release-plz-123", "repo": {"full_name": automation.REPOSITORY}},
                   "base": {"ref": "main", "repo": {"full_name": automation.REPOSITORY}}}

    def test_only_merged_upstream_release_pr_authorizes_publishing(self):
        self.assertTrue(automation.release_pr(self.pr, self.commit))
        for field, value in (("merged_at", None), ("merge_commit_sha", "b" * 40)):
            pr = copy.deepcopy(self.pr)
            pr[field] = value
            self.assertFalse(automation.release_pr(pr, self.commit))
        for section, field, value in (("head", "ref", "feature-fix"), ("base", "ref", "dev")):
            pr = copy.deepcopy(self.pr)
            pr[section][field] = value
            self.assertFalse(automation.release_pr(pr, self.commit))

    def test_fork_cannot_spoof_a_release_branch_name(self):
        self.pr["head"]["repo"]["full_name"] = "contributor/pi-rust"
        self.assertFalse(automation.release_pr(self.pr, self.commit))
        self.pr["head"]["repo"] = None
        self.assertFalse(automation.release_pr(self.pr, self.commit))

    def test_new_version_or_identical_commit_retry_but_no_retag_or_downgrade(self):
        automation.check_version("0.3.25", ["v0.3.24"], self.commit)
        automation.check_version("0.3.25", ["v0.3.25"], self.commit, self.commit)
        for version in ("0.3.23", "0.3.24", "0.03.25", "0.3.25-rc1"):
            with self.assertRaises(ValueError):
                automation.check_version(version, ["v0.3.24"], self.commit)
        with self.assertRaisesRegex(ValueError, "another commit"):
            automation.check_version("0.3.25", [], self.commit, "b" * 40)

    def test_fork_and_pull_request_events_are_denied_before_api_access(self):
        for repository, event, ref in (("fork/pi-rust", "push", "refs/heads/main"),
                                       (automation.REPOSITORY, "pull_request", "refs/pull/1/merge"),
                                       (automation.REPOSITORY, "push", "refs/heads/dev")):
            with patch.dict(os.environ, {"GITHUB_REPOSITORY": repository,
                                          "GITHUB_EVENT_NAME": event, "GITHUB_REF": ref}), \
                    patch.object(sys, "argv", ["script", "detect"]), \
                    patch.object(automation, "api") as api:
                with self.assertRaises(ValueError):
                    automation.main()
                api.assert_not_called()

    def test_ordinary_merge_skips_release_even_when_a_version_was_edited(self):
        with patch.dict(os.environ, {"GITHUB_REPOSITORY": automation.REPOSITORY,
                                      "GITHUB_EVENT_NAME": "push", "GITHUB_REF": "refs/heads/main",
                                      "GITHUB_SHA": self.commit}), \
                patch.object(sys, "argv", ["script", "detect"]), \
                patch.object(automation, "run", return_value=self.commit), \
                patch.object(automation, "api", side_effect=[[{"number": 1}],
                             {**self.pr, "head": {"ref": "feature", "repo": {"full_name": automation.REPOSITORY}}}]), \
                patch.object(automation, "output") as output:
            automation.main()
            output.assert_called_once_with(release="false")

    def test_all_nine_core_crates_share_one_release_group(self):
        root = Path(__file__).resolve().parent.parent
        config = tomllib.loads((root / "release-plz.toml").read_text(encoding="utf-8"))
        self.assertFalse(config["workspace"]["release"])
        packages = config["package"]
        self.assertEqual(len(packages), 9)
        self.assertTrue(all(p["release"] and p["version_group"] == "rpi" for p in packages))
        self.assertEqual(sum(p.get("changelog_update", False) for p in packages), 1)

    def test_release_plz_linked_changelog_heading_allows_the_release(self):
        for command in ("detect", "tag"):
            with patch.dict(os.environ, {"GITHUB_REPOSITORY": automation.REPOSITORY,
                                          "GITHUB_EVENT_NAME": "push", "GITHUB_REF": "refs/heads/main",
                                          "GITHUB_SHA": self.commit}), \
                    patch.object(sys, "argv", ["script", command]), \
                    patch.object(automation, "run", side_effect=[self.commit, "v0.3.24"]), \
                    patch.object(automation.Path, "read_text", side_effect=[
                        '[workspace.package]\nversion = "0.3.25"\n',
                        '## [0.3.25](https://github.com/compare/v0.3.24...v0.3.25) - 2026-10-15\n']), \
                    patch.object(automation, "api", side_effect=[[{"number": 1}], self.pr, {}]) as api, \
                    patch.object(automation, "output") as output:
                automation.main()
                output.assert_called_once_with(release="true", version="0.3.25", tag="v0.3.25")
                if command == "tag":
                    self.assertIn("sha=" + self.commit, api.call_args.args)

    def test_checkout_that_differs_from_tested_commit_is_rejected(self):
        with patch.dict(os.environ, {"GITHUB_REPOSITORY": automation.REPOSITORY,
                                      "GITHUB_EVENT_NAME": "push", "GITHUB_REF": "refs/heads/main",
                                      "GITHUB_SHA": "b" * 40}), \
                patch.object(sys, "argv", ["script", "tag"]), \
                patch.object(automation, "run", return_value=self.commit), \
                patch.object(automation, "api") as api:
            with self.assertRaisesRegex(ValueError, "tested main commit"):
                automation.main()
            api.assert_not_called()


class ReleaseSeriesTests(unittest.TestCase):
    def test_versions_stay_on_the_selected_series_and_increment_numerically(self):
        tags = ["v0.8.0", "v0.8.9", "v0.7.99", "v0.8.10-rc1", "v0.8.invalid"]
        self.assertEqual(automation.series_version("0.9.0", "0.8", tags), "0.8.10")
        self.assertEqual(automation.series_version("1.0.0", "0.8", tags), "0.8.10")
        self.assertEqual(automation.series_version("0.8.12", "0.8", tags), "0.8.12")
        self.assertEqual(automation.series_version("0.9.0", "0.8", []), "0.8.0")
        for series in ("0.08", "0.8.0", "invalid"):
            with self.assertRaises(ValueError):
                automation.series_version("0.9.0", series, tags)

    def test_publish_gate_rejects_a_version_outside_the_series(self):
        root = {"workspace": {"metadata": {"release": {"series": "0.8"}}}}
        automation.check_series("0.8.1", root)
        for version in ("0.9.0", "1.0.0", "0.8.1-rc1"):
            with self.assertRaises(ValueError):
                automation.check_series(version, root)

    def test_normalize_updates_pins_and_new_notes_without_rewriting_history(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = root / "Cargo.toml"
            manifest.write_text('''[workspace.package]
version = "0.9.0"
[workspace.metadata.release]
series = "0.8"
[workspace.dependencies]
rpi-tui = { path = "crates/rpi-tui", version = "0.9.0" }
serde = "1"
''', encoding="utf-8")
            changelog = root / "CHANGELOG.md"
            historical = "## [0.8.0] - 2026-10-22\n\nPrevious release, targeting 0.9.0 later.\n"
            changelog.write_text("## [Unreleased]\n\n"
                                 "## [0.9.0](https://example.test/v0.8.0...v0.9.0) - 2026-10-23\n"
                                 "\n### Added\n\n- New feature\n\n" + historical,
                                 encoding="utf-8")
            self.assertEqual(automation.normalize_release(root, ["v0.8.0"]), "0.8.1")
            result = tomllib.loads(manifest.read_text(encoding="utf-8"))
            self.assertEqual(result["workspace"]["package"]["version"], "0.8.1")
            self.assertEqual(result["workspace"]["dependencies"]["rpi-tui"]["version"], "0.8.1")
            self.assertEqual(result["workspace"]["dependencies"]["serde"], "1")
            notes = changelog.read_text(encoding="utf-8")
            self.assertIn("## [0.8.1](https://example.test/v0.8.0...v0.8.1)", notes)
            self.assertTrue(notes.endswith(historical))
            self.assertEqual(automation.normalize_release(root, ["v0.8.0"]), "0.8.1")
            self.assertEqual(changelog.read_text(encoding="utf-8"), notes)

    def test_missing_proposed_notes_fail_before_modifying_the_manifest(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = root / "Cargo.toml"
            original = ('[workspace.package]\nversion = "0.9.0"\n'
                        '[workspace.metadata.release]\nseries = "0.8"\n')
            manifest.write_text(original, encoding="utf-8")
            (root / "CHANGELOG.md").write_text("## [0.8.0]\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "missing from CHANGELOG"):
                automation.normalize_release(root, ["v0.8.0"])
            self.assertEqual(manifest.read_text(encoding="utf-8"), original)

    def test_published_notes_cannot_be_retargeted(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = root / "Cargo.toml"
            manifest.write_text('[workspace.package]\nversion = "0.8.0"\n'
                                '[workspace.metadata.release]\nseries = "0.8"\n', encoding="utf-8")
            changelog = root / "CHANGELOG.md"
            original = "## [0.8.0]\n\nPublished notes\n"
            changelog.write_text(original, encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "already published"):
                automation.normalize_release(root, ["v0.8.0"])
            self.assertEqual(changelog.read_text(encoding="utf-8"), original)


class ChannelReadinessTests(unittest.TestCase):
    def setUp(self):
        self.tag = "v0.3.25"
        self.manifest = {"schema_version": 1, "name": "rpi-cli", "version": "0.3.25",
                         "tag": self.tag, "repository": channels.REPOSITORY,
                         "source_commit": "a" * 40, "targets": {}}
        self.release = {"draft": False, "prerelease": False, "tag_name": self.tag,
                        "assets": [{"name": "rpi-release.json"}]}
        for target in channels.TARGETS:
            extension = "zip" if "windows" in target else "tar.gz"
            name = f"rpi-{self.tag}-{target}.{extension}"
            self.manifest["targets"][target] = {
                "url": f"{channels.REPOSITORY}/releases/download/{self.tag}/{name}",
                "format": extension, "size": 10, "sha256": "b" * 64}
            self.release["assets"] += [{"name": name, "size": 10, "digest": "sha256:" + "b" * 64},
                                       {"name": name + ".sha256"}]

    def test_complete_release_is_ready(self):
        channels.verify(self.tag, self.manifest, self.release)

    def test_partial_release_and_changed_asset_are_rejected(self):
        for asset in self.release["assets"]:
            release = copy.deepcopy(self.release)
            release["assets"].remove(asset)
            with self.assertRaises(ValueError):
                channels.verify(self.tag, self.manifest, release)
        self.release["assets"][1]["digest"] = "sha256:" + "c" * 64
        with self.assertRaises(ValueError):
            channels.verify(self.tag, self.manifest, self.release)

    def test_wrong_publisher_missing_target_and_prerelease_are_rejected(self):
        for field, value in (("repository", "https://github.com/fork/pi-rust"),
                             ("version", "0.3.24"), ("source_commit", "invalid")):
            manifest = copy.deepcopy(self.manifest)
            manifest[field] = value
            with self.assertRaises(ValueError):
                channels.verify(self.tag, manifest, self.release)
        self.manifest["targets"].pop(next(iter(channels.TARGETS)))
        with self.assertRaises(ValueError):
            channels.verify(self.tag, self.manifest, self.release)


if __name__ == "__main__":
    unittest.main()
