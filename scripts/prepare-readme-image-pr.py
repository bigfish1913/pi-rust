#!/usr/bin/env python3
"""Publish only the refreshed README to a stable branch and an ordinary CI-gated PR.

Run after refresh-readme-images.py on a clean checkout of main. GH_TOKEN must be
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
        print("README image date is already current; no PR needed.")
        return
    if changed != ["README.md"]:
        raise ValueError("Only README.md may be changed")

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

    git("add", "--", "README.md")
    tree = git("write-tree")
    if not previous or git("rev-parse", f"{previous}^{{tree}}") != tree:
        git("-c", "user.name=github-actions[bot]", "-c",
            "user.email=41898282+github-actions[bot]@users.noreply.github.com",
            "commit", "-m", "docs: refresh WeChat group image cache key")
        # Rebuild from main rather than accumulating daily commits. An explicit
        # lease protects concurrent changes, including first-creation races.
        # This is NEVER a force push to main, nor a branch-protection bypass.
        remote("push", f"--force-with-lease={REF}:{previous}", "origin", f"HEAD:{REF}")
    else:
        print("Stable README branch already has this tree; leaving its SHA unchanged.")

    if prs:
        print(f"Reusing README image PR #{prs[0]['number']}.")
    else:
        gh("create", "--base", "main", "--head", BRANCH,
           "--title", "docs: refresh WeChat group image cache key",
           "--body", "Automated daily README image cache-key refresh.\n\n"
           "Normal required CI and branch protection apply; merge after checks pass.")
        print("Created the README image refresh PR.")


if __name__ == "__main__":
    try:
        prepare()
    except (ValueError, subprocess.CalledProcessError) as error:
        # Do not echo command output or credentials on authentication/API errors.
        message = str(error) if isinstance(error, ValueError) else "Git/GitHub operation failed; inspect token permissions and branch rules"
        raise SystemExit(f"::error::{message}") from None
