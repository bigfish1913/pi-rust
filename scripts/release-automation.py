#!/usr/bin/env python3
"""Gate CI publishing on a merged, same-repository Release PR, and tag safely."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib

REPOSITORY = "bigfish1913/pi-rust"
VERSION = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
SERIES = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")


def run(*args):
    return subprocess.check_output(args, text=True, encoding="utf-8").strip()


def api(path, *args):
    return json.loads(run("gh", "api", path, *args))


def release_pr(pr, commit):
    """A fork branch named release-plz-* cannot authorize publication."""
    return (pr.get("merged_at") is not None
            and pr.get("merge_commit_sha") == commit
            and pr.get("base", {}).get("ref") == "main"
            and (pr.get("base", {}).get("repo") or {}).get("full_name") == REPOSITORY
            and (pr.get("head", {}).get("repo") or {}).get("full_name") == REPOSITORY
            and pr.get("head", {}).get("ref", "").startswith("release-plz-"))


def check_version(version, tags, commit, existing_commit=None):
    if not VERSION.fullmatch(version):
        raise ValueError("release version must be a stable X.Y.Z")
    if existing_commit is not None:
        if existing_commit != commit:
            raise ValueError("release tag already belongs to another commit; never move it")
        return  # An interrupted release can resume at the identical commit.
    versions = [tuple(map(int, tag[1:].split("."))) for tag in tags
                if tag.startswith("v") and VERSION.fullmatch(tag[1:])]
    if versions and tuple(map(int, version.split("."))) <= max(versions):
        raise ValueError("release version must be newer than all existing stable tags")


def check_series(version, root):
    series = root["workspace"].get("metadata", {}).get("release", {}).get("series")
    if series is not None:
        if not SERIES.fullmatch(series):
            raise ValueError("release series must be a stable X.Y")
        if not VERSION.fullmatch(version) or version.rsplit(".", 1)[0] != series:
            raise ValueError(f"release version {version} is outside the configured {series}.x series")
    return series


def series_version(proposed, series, tags):
    if not SERIES.fullmatch(series):
        raise ValueError("release series must be a stable X.Y")
    versions = [tuple(map(int, tag[1:].split("."))) for tag in tags
                if tag.startswith(f"v{series}.") and VERSION.fullmatch(tag[1:])]
    patch = max((v[2] for v in versions), default=-1) + 1
    if VERSION.fullmatch(proposed) and proposed.rsplit(".", 1)[0] == series:
        patch = max(patch, int(proposed.rsplit(".", 1)[1]))
    return f"{series}.{patch}"


def normalize_release(root, tags):
    manifest_path = root / "Cargo.toml"
    manifest = manifest_path.read_text(encoding="utf-8")
    config = tomllib.loads(manifest)
    proposed = config["workspace"]["package"]["version"]
    series = config["workspace"].get("metadata", {}).get("release", {}).get("series")
    if series is None:
        return proposed
    if f"v{proposed}" in tags:
        raise ValueError("cannot normalize an already published release")
    version = series_version(proposed, series, tags)
    if version == proposed:
        return version
    changelog_path = root / "CHANGELOG.md"
    changelog = changelog_path.read_text(encoding="utf-8")
    # Only retarget the proposed release section, preserving historical notes.
    section = re.search(rf"^## \[{re.escape(proposed)}\][^\n]*\n(?:(?!^## ).)*",
                        changelog, re.MULTILINE | re.DOTALL)
    if section is None:
        raise ValueError("proposed release version is missing from CHANGELOG.md")
    changelog = (changelog[:section.start()] + section[0].replace(proposed, version)
                 + changelog[section.end():])
    # The shared workspace version and internal dependency pins use this literal.
    manifest = re.sub(rf'(\bversion\s*=\s*"){re.escape(proposed)}(")',
                      lambda m: m[1] + version + m[2], manifest)
    manifest_path.write_text(manifest, encoding="utf-8", newline="\n")
    changelog_path.write_text(changelog, encoding="utf-8", newline="\n")
    return version


def output(**values):
    for key, value in values.items():
        line = f"{key}={value}\n"
        print(line, end="")
        if os.environ.get("GITHUB_OUTPUT"):
            with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as file:
                file.write(line)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("detect", "tag", "normalize"))
    args = parser.parse_args()
    if args.command == "normalize":
        print(normalize_release(Path("."), run("git", "tag", "--list").splitlines()))
        return
    if (os.environ.get("GITHUB_REPOSITORY") != REPOSITORY
            or os.environ.get("GITHUB_REF") != "refs/heads/main"
            or os.environ.get("GITHUB_EVENT_NAME") not in ("push", "workflow_dispatch")):
        raise ValueError("publishing is only allowed from the upstream main branch")
    commit = run("git", "rev-parse", "HEAD")
    if commit != os.environ["GITHUB_SHA"]:
        raise ValueError("checkout must match the tested main commit")
    # Use full PR metadata to check its origin, merge state and merge commit.
    prs = api(f"repos/{REPOSITORY}/commits/{commit}/pulls")
    if not any(release_pr(api(f"repos/{REPOSITORY}/pulls/{pr['number']}"), commit) for pr in prs):
        if args.command == "tag":
            raise ValueError("only a merged Release PR may create a release tag")
        output(release="false")
        return
    root = tomllib.loads(Path("Cargo.toml").read_text(encoding="utf-8"))
    version = root["workspace"]["package"]["version"]
    check_series(version, root)
    tag = f"v{version}"
    tags = run("git", "tag", "--list").splitlines()
    existing = run("git", "rev-parse", f"{tag}^{{commit}}") if tag in tags else None
    check_version(version, tags, commit, existing)
    if not re.search(rf"^## \[?{re.escape(version)}\]?(?:\(|\s|$)",
                     Path("CHANGELOG.md").read_text(encoding="utf-8"), re.MULTILINE):
        raise ValueError("Release PR must include this version in CHANGELOG.md")
    if args.command == "tag" and existing is None:
        api(f"repos/{REPOSITORY}/git/refs", "--method", "POST",
            "-f", f"ref=refs/tags/{tag}", "-f", f"sha={commit}")
    output(release="true", version=version, tag=tag)


if __name__ == "__main__":
    main()
