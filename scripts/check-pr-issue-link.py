#!/usr/bin/env python3
"""Require every pull request to reference an existing issue.

A change should be traceable to a reported bug or an agreed feature, so the PR
description has to point at an issue in this repository before review starts.
The repository owner, maintainers and the repository's own automation remain
exempt: they already own the decision, and release PRs are generated, not
written.

Exit codes: 0 when a referenced issue exists or the PR is exempt, 1 otherwise.
"""
import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.request

USER_AGENT = "rpi-pr-issue-link (https://github.com/bigfish1913/pi-rust)"

# Maintainers (and the repository owner) do not need a filed issue.
EXEMPT_ASSOCIATIONS = ("OWNER", "MEMBER")
# Branches this repository's automation opens by itself.
AUTOMATION_BRANCH_PREFIXES = ("release-plz-", "automation/", "dependabot/", "renovate/")

# HTML comments carry the pull request template's example ("Closes #123"), so
# they must be stripped before scanning or every PR would pass for free.
HTML_COMMENT = re.compile(r"<!--.*?-->", re.DOTALL)
FENCED_CODE = re.compile(r"^```.*?^```", re.DOTALL | re.MULTILINE)
ISSUE_URL = re.compile(r"https?://github\.com/([\w.-]+/[\w.-]+)/issues/(\d+)")
ISSUE_REFERENCE = re.compile(r"(?<![\w/&#])#(\d+)\b")

MISSING_HINT = """\
This pull request does not reference an issue in {repo}.

Every pull request needs a filed issue so the change is reviewable in context:
  1. Search {repo}/issues for an existing report, or open one.
  2. Replace the "Related issues" comment in the description with the number,
     for example `Closes #123`.

Maintainer-owned pull requests and repository automation are exempt. If that
applies to you and this still fails, the repository owner can re-run the
"Require linked issue" job after checking the branch association.
"""


class VerificationError(RuntimeError):
    """The GitHub API could not be asked whether an issue exists."""


def default_fetch(url, headers=None, timeout=20):
    """Return (status, body text) for one GitHub API request."""
    request = urllib.request.Request(url, headers=headers or {})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.status, response.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode("utf-8", "replace")
    except urllib.error.URLError as error:
        raise VerificationError(str(error.reason)) from error


def exemption_reason(author, author_association, head_ref, repo):
    """Return why this pull request needs no linked issue, or None."""
    owner = repo.split("/")[0]
    if author and (author == owner or author.endswith("[bot]")):
        return f"author {author} is the repository owner or a bot"
    if author_association.upper() in EXEMPT_ASSOCIATIONS:
        return f"author association is {author_association.upper()}"
    for prefix in AUTOMATION_BRANCH_PREFIXES:
        if head_ref.startswith(prefix):
            return f"branch {head_ref} is repository automation"
    return None


def referenced_issues(body, repo):
    """Return the issue numbers a PR description points at, in order."""
    text = FENCED_CODE.sub("", HTML_COMMENT.sub("", body))
    numbers = []
    for match in ISSUE_URL.finditer(text):
        if match.group(1).lower() == repo.lower():
            numbers.append(int(match.group(2)))
    for match in ISSUE_REFERENCE.finditer(text):
        numbers.append(int(match.group(1)))
    seen = set()
    unique = []
    for number in numbers:
        if number not in seen:
            seen.add(number)
            unique.append(number)
    return unique


def fetch_issue(repo, number, token, fetch=default_fetch):
    """Return the issue payload, or None when the number is not an issue."""
    url = f"https://api.github.com/repos/{repo}/issues/{number}"
    headers = {
        "Accept": "application/vnd.github+json",
        "User-Agent": USER_AGENT,
    }
    if token:
        headers["Authorization"] = f"Bearer {token}"
    status, body = fetch(url, headers)
    if status == 404:
        return None
    if status >= 300:
        raise VerificationError(f"GitHub API returned HTTP {status} for {url}")
    payload = json.loads(body)
    if "pull_request" in payload:
        # A pull request number is not an issue number.
        return None
    return payload


def read_body(path):
    with open(path, "r", encoding="utf-8") as handle:
        return handle.read()


def main(argv=None, fetch=default_fetch, stream=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", required=True, help="owner/name of the repository")
    parser.add_argument("--body-file", required=True, help="file holding the PR description")
    parser.add_argument("--author", default="", help="pull request author login")
    parser.add_argument("--author-association", default="", help="author_association")
    parser.add_argument("--head-ref", default="", help="pull request head branch")
    parser.add_argument("--token", default=os.environ.get("GITHUB_TOKEN", ""))
    args = parser.parse_args(argv)
    out = stream if stream is not None else sys.stdout

    reason = exemption_reason(args.author, args.author_association, args.head_ref, args.repo)
    if reason:
        print(f"Exempt from the linked-issue requirement: {reason}.", file=out)
        return 0

    numbers = referenced_issues(read_body(args.body_file), args.repo)
    if not numbers:
        print(MISSING_HINT.format(repo=args.repo), file=sys.stderr)
        return 1

    rejected = []
    for number in numbers:
        try:
            issue = fetch_issue(args.repo, number, args.token, fetch)
        except VerificationError as error:
            print(
                f"Could not verify #{number}: {error}. Re-run this check once the "
                "GitHub API is reachable.",
                file=sys.stderr,
            )
            return 1
        if issue is not None:
            print(
                f"Linked issue #{number} exists: {issue.get('title', '(no title)')}",
                file=out,
            )
            return 0
        rejected.append(number)

    print(
        "None of the referenced numbers are issues in "
        f"{args.repo}: "
        + ", ".join(f"#{number}" for number in rejected)
        + ". Reference an existing issue, for example `Closes #123`.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
