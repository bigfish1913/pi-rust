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


def run(*args):
    return subprocess.check_output(args, text=True, encoding="utf-8").strip()


def api(path, *args):
    return json.loads(run("gh", "api", path, *args))


def upstream_release_branch(pr):
    """A fork branch named release-plz-* is not an upstream Release PR."""
    return (pr.get("base", {}).get("ref") == "main"
            and (pr.get("base", {}).get("repo") or {}).get("full_name") == REPOSITORY
            and (pr.get("head", {}).get("repo") or {}).get("full_name") == REPOSITORY
            and pr.get("head", {}).get("ref", "").startswith("release-plz-"))


def release_pr(pr, commit):
    return (pr.get("merged_at") is not None
            and pr.get("merge_commit_sha") == commit
            and upstream_release_branch(pr))


def draft_release_prs():
    # Pagination matters: an existing Release PR need not be on the first page.
    pages = api(f"repos/{REPOSITORY}/pulls?state=open&base=main&per_page=100",
                "--paginate", "--slurp")
    prs = [pr for page in pages for pr in page
           if pr.get("state") == "open" and upstream_release_branch(pr)]
    if len(prs) > 1:
        raise ValueError("multiple open Release PRs; close obsolete ones before preparing a release")
    if not prs:
        return
    pr = prs[0]
    if not pr["draft"]:
        run("gh", "pr", "ready", str(pr["number"]), "--undo", "--repo", REPOSITORY)
    # release-plz may return no PR on a no-op retry. Still sync/validate this PR
    # and mark it ready again, but only after preparation has succeeded.
    output(pr=json.dumps({"number": pr["number"], "head_branch": pr["head"]["ref"]}))


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


def output(**values):
    for key, value in values.items():
        line = f"{key}={value}\n"
        print(line, end="")
        if os.environ.get("GITHUB_OUTPUT"):
            with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as file:
                file.write(line)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("detect", "tag", "draft-release-prs"))
    args = parser.parse_args()
    if (os.environ.get("GITHUB_REPOSITORY") != REPOSITORY
            or os.environ.get("GITHUB_REF") != "refs/heads/main"
            or os.environ.get("GITHUB_EVENT_NAME") not in ("push", "workflow_dispatch")):
        raise ValueError("publishing is only allowed from the upstream main branch")
    commit = run("git", "rev-parse", "HEAD")
    if commit != os.environ["GITHUB_SHA"]:
        raise ValueError("checkout must match the tested main commit")
    if args.command == "draft-release-prs":
        draft_release_prs()
        return
    # Use full PR metadata to check its origin, merge state and merge commit.
    prs = api(f"repos/{REPOSITORY}/commits/{commit}/pulls")
    if not any(release_pr(api(f"repos/{REPOSITORY}/pulls/{pr['number']}"), commit) for pr in prs):
        if args.command == "tag":
            raise ValueError("only a merged Release PR may create a release tag")
        output(release="false")
        return
    root = tomllib.loads(Path("Cargo.toml").read_text(encoding="utf-8"))
    version = root["workspace"]["package"]["version"]
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
