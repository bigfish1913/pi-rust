#!/usr/bin/env python3
"""Publish the refreshed README to a stable branch and an automation PR.

The workflow that invokes this script immediately administrator-merges the
restricted README image PR without waiting for normal CI. GH_TOKEN must be
RELEASE_AUTOMATION_TOKEN, not GITHUB_TOKEN (whose PR events cannot trigger CI).
"""

import json
import os
from pathlib import Path
import subprocess


REPOSITORY = "bigfish1913/pi-rust"
BRANCH = "automation/readme-images"
REF = f"refs/heads/{BRANCH}"
ROOT = Path(__file__).resolve().parent.parent
READMES = {"README.md", "README.zh-CN.md"}


def run(*args, cwd):
    return subprocess.run(args, cwd=cwd, check=True, text=True, capture_output=True).stdout.strip()


def prepare(root=ROOT):
    if (
        os.environ.get("GITHUB_REPOSITORY") != REPOSITORY
        or os.environ.get("GITHUB_REF") != "refs/heads/main"
        or os.environ.get("GITHUB_EVENT_NAME") not in ("schedule", "workflow_dispatch")
    ):
        raise ValueError("README PR writes require an upstream main schedule/dispatch run")
    if not os.environ.get("GH_TOKEN", "").strip():
        raise ValueError("Set RELEASE_AUTOMATION_TOKEN with contents and pull requests write access")

    def git(*args):
        return run("git", *args, cwd=root)

    def remote(*args):
        # Use the environment token without persisting it in git config or URLs.
        return git("-c", "credential.helper=", "-c", "credential.helper=!gh auth git-credential", *args)

    def gh(*args):
        return run("gh", "pr", *args, "--repo", REPOSITORY, cwd=root)

    if git("rev-parse", "HEAD") != git("rev-parse", "origin/main"):
        raise ValueError("Expected a fresh checkout of origin/main")
    if git("diff", "--cached", "--name-only"):
        raise ValueError("Expected an empty staging area")
    changed = git("diff", "--name-only").splitlines()
    if not changed:
        print("README image dates are already current; no PR needed.")
        return
    if not READMES.issuperset(changed):
        raise ValueError("Only README.md and README.zh-CN.md may be changed")

    prs = json.loads(gh("list", "--state", "open", "--base", "main", "--head", BRANCH,
                        "--json", "number,headRepositoryOwner,isCrossRepository"))
    prs = [pr for pr in prs if not pr["isCrossRepository"]
           and pr["headRepositoryOwner"]["login"] == REPOSITORY.split("/")[0]]
    if len(prs) > 1:
        raise ValueError("Multiple upstream README image PRs are open")

    refs = remote("ls-remote", "--heads", "origin", REF).splitlines()
    previous = refs[0].split()[0] if refs else ""
    if previous:
        remote("fetch", "--no-tags", "origin", REF)
        if git("rev-parse", "FETCH_HEAD") != previous:
            raise ValueError("README branch changed concurrently; rerun the workflow")

    git("add", "--", *changed)
    tree = git("write-tree")
    if not previous or git("rev-parse", f"{previous}^{{tree}}") != tree:
        git("-c", "user.name=github-actions[bot]", "-c",
            "user.email=41898282+github-actions[bot]@users.noreply.github.com",
            "commit", "-m", "docs: refresh WeChat group image cache key")
        # Rebuild from main rather than accumulating daily commits. An explicit
        # lease protects concurrent changes, including first-creation races.
        # This is NEVER a force push to main, nor a branch-protection bypass.
        remote("push", f"--force-with-lease={REF}:{previous}", "origin", f"HEAD:{REF}")
        head = git("rev-parse", "HEAD")
    else:
        print("Stable README branch already has this tree; leaving its SHA unchanged.")
        head = previous

    if prs:
        print(f"Reusing README image PR #{prs[0]['number']}.")
    else:
        gh("create", "--base", "main", "--head", BRANCH,
           "--title", "docs: refresh WeChat group image cache key",
           "--body", "Automated daily README image cache-key refresh.\n\n"
           "The trusted README automation workflow verifies this README-only "
           "branch and merges it without waiting for normal CI.")
        print("Created the README image refresh PR.")
    return head


def main(root=ROOT):
    head = prepare(root)
    output = os.environ.get("GITHUB_OUTPUT")
    if head and output:
        with Path(output).open("a", encoding="utf-8") as stream:
            stream.write(f"head={head}\n")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, subprocess.CalledProcessError) as error:
        # Do not echo command output or credentials on authentication/API errors.
        message = str(error) if isinstance(error, ValueError) else "Git/GitHub operation failed; inspect token permissions and branch rules"
        raise SystemExit(f"::error::{message}") from None
