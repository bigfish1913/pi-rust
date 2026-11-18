#!/usr/bin/env python3
"""Report install channels that lag the latest stable GitHub Release.

A release run can fail after crates.io and the GitHub Release succeeded, leaving
one channel behind while every later release keeps moving on. That happened to
`@bigfishnpm/rpi`, which stayed at 0.22.1 through three releases because the only
evidence was one red npm job in a green-looking pipeline. This check compares the
published channels with the newest stable release and fails loudly.
"""
import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.request

REPOSITORY = "bigfish1913/pi-rust"
CRATE = "rpi-cli"
NPM_PACKAGE = "@bigfishnpm/rpi"
HOMEBREW_FORMULA = "https://raw.githubusercontent.com/bigfish1913/homebrew-tap/HEAD/Formula/rpi.rb"
SCOOP_MANIFEST = "https://raw.githubusercontent.com/bigfish1913/scoop-bucket/HEAD/bucket/rpi.json"
WINGET_SEARCH = ("https://api.github.com/search/issues?per_page=5&q="
                 "repo:microsoft/winget-pkgs+is:pr+is:open+in:title+bigfish1913.rpi")
STABLE = re.compile(r"(?:v)?((?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))")
USER_AGENT = "rpi-channel-drift (https://github.com/bigfish1913/pi-rust)"

# Tracked channels are the ones this repository publishes itself; winget is only
# informational because it waits on an upstream review nobody here controls.
TRACKED = ("crates.io", "npm", "homebrew", "scoop")
INFORMATIONAL = ("winget",)


class Response:
    def __init__(self, status, body):
        self.status = status
        self.body = body

    def json(self):
        return json.loads(self.body)


def fetch(url, headers=None, timeout=20):
    """Read one URL; callers pass an Authorization header for GitHub only."""
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT, **(headers or {})})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return Response(response.status, response.read().decode("utf-8", "replace"))
    except urllib.error.HTTPError as error:
        return Response(error.code, error.read().decode("utf-8", "replace"))
    except (urllib.error.URLError, TimeoutError) as error:
        # A transport failure is an unknown channel, never a healthy one.
        return Response(0, str(error))


def version_of(text, pattern):
    match = re.search(pattern, text or "", re.MULTILINE)
    return match.group(1) if match else None


def stable_version(tag):
    match = STABLE.fullmatch((tag or "").strip())
    if not match:
        raise ValueError(f"not a stable X.Y.Z tag: {tag!r}")
    return match.group(1)


def parse_version(value):
    """Return the numeric parts of a channel version, or None when unknown."""
    match = STABLE.fullmatch((value or "").strip())
    return tuple(int(part) for part in match.group(1).split(".")) if match else None


def latest_stable_release(releases):
    for release in releases:
        if release.get("draft") or release.get("prerelease"):
            continue
        try:
            version = stable_version(release.get("tag_name"))
        except ValueError:
            continue
        return version
    return None


def compare(channel_version, release_version):
    channel = parse_version(channel_version)
    release = parse_version(release_version)
    if channel is None:
        return "unknown"
    if release is None:
        raise ValueError(f"invalid release version: {release_version!r}")
    if channel == release:
        return "ok"
    return "lagging" if channel < release else "ahead"


def collect(fetcher=fetch, token=None):
    """Read the newest stable release and every channel version.

    The GitHub token is only ever sent to api.github.com; the registries and the
    tap repositories are public and must never receive it.
    """
    github = {"Authorization": f"Bearer {token}"} if token else {}
    releases = fetcher(f"https://api.github.com/repos/{REPOSITORY}/releases?per_page=20", github)
    if releases.status != 200:
        raise RuntimeError(f"GitHub releases lookup failed: HTTP {releases.status}")
    release_version = latest_stable_release(releases.json())
    if not release_version:
        raise RuntimeError("no published stable release found")

    rows = []

    def add(name, version, source):
        rows.append({"channel": name, "version": version,
                     "status": compare(version, release_version), "source": source})

    crates = fetcher(f"https://crates.io/api/v1/crates/{CRATE}")
    add("crates.io", crates.json()["crate"]["max_stable_version"] if crates.status == 200 else None,
        f"https://crates.io/crates/{CRATE}")
    npm = fetcher(f"https://registry.npmjs.org/{NPM_PACKAGE.replace('/', '%2F')}")
    add("npm", npm.json().get("dist-tags", {}).get("latest") if npm.status == 200 else None,
        f"https://www.npmjs.com/package/{NPM_PACKAGE}")
    homebrew = fetcher(HOMEBREW_FORMULA)
    add("homebrew", version_of(homebrew.body, r'^\s*version\s+"([0-9.]+)"') if homebrew.status == 200 else None,
        "https://github.com/bigfish1913/homebrew-tap")
    scoop = fetcher(SCOOP_MANIFEST)
    add("scoop", (scoop.json().get("version") if scoop.status == 200 else None),
        "https://github.com/bigfish1913/scoop-bucket")

    winget = fetcher(WINGET_SEARCH, github)
    open_prs = []
    if winget.status == 200:
        open_prs = [item.get("title", "") for item in winget.json().get("items", [])]
    pending = next((title.split("version")[-1].strip() for title in open_prs
                    if "bigfish1913.rpi" in title), None)
    rows.append({"channel": "winget", "version": pending,
                 "status": "informational", "source": "https://github.com/microsoft/winget-pkgs",
                 "detail": f"open upstream PR: {open_prs[0]}" if open_prs else "no open upstream PR"})
    return release_version, rows


def recovery_command(row):
    tag = "v<version>"
    return {
        "npm": f"gh workflow run release-npm.yml --ref main -f tag={tag}",
        "homebrew": f"gh workflow run release-channels.yml --ref main -f tag={tag}  # homebrew tap",
        "scoop": f"gh workflow run release-channels.yml --ref main -f tag={tag}  # scoop bucket",
        "winget": f"gh workflow run release-channels.yml --ref main -f tag={tag}  # winget PR",
        "crates.io": "rerun `Release automation` for the release commit; published crates are skipped",
    }.get(row["channel"], "")


def report(release_version, rows, tag_prefix="v"):
    tag = f"{tag_prefix}{release_version}"
    lines = [f"Latest stable release: {tag}", "",
             "| Channel | Version | Expected | Status | Source |",
             "| --- | --- | --- | --- | --- |"]
    for row in rows:
        lines.append(f"| {row['channel']} | {row['version'] or 'unknown'} | {release_version} "
                     f"| {row['status']} | {row['source']} |")
    drift = [row for row in rows if row["status"] in ("lagging", "unknown") and row["channel"] in TRACKED]
    if drift:
        lines += ["", "Channels to recover:", ""]
        for row in drift:
            command = recovery_command(row)
            lines.append(f"- **{row['channel']}** is on {row['version'] or 'an unknown version'}:"
                         + (f" `{command}`" if command else ""))
        lines += ["", "Homebrew and Scoop share one workflow; npm has its own. Recover the",
                  "channel before cutting the next release so the gap stays one version wide."]
    else:
        lines += ["", "Every tracked channel matches the latest stable release."]
    return tag, lines, drift


def failures(rows):
    return [row for row in rows if row["status"] in ("lagging", "unknown") and row["channel"] in TRACKED]


def main(argv=None, collector=collect):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", help="write the machine-readable result to this path")
    parser.add_argument("--release", help="compare against this stable tag instead of the newest release")
    parser.add_argument("--token-env", default="GITHUB_TOKEN",
                        help="environment variable holding an optional GitHub token")
    args = parser.parse_args(argv)
    token = os.environ.get(args.token_env) or None
    try:
        release_version, rows = collector(token=token)
        if args.release:
            release_version = stable_version(args.release)
            rows = [{**row, "status": ("informational" if row["channel"] in INFORMATIONAL
                                       else compare(row["version"], release_version))} for row in rows]
    except (RuntimeError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    tag, lines, drift = report(release_version, rows)
    print("\n".join(lines))
    if args.json:
        with open(args.json, "w", encoding="utf-8") as handle:
            json.dump({"tag": tag, "release": release_version, "channels": rows,
                       "drift": [row["channel"] for row in drift]}, handle, indent=2)
            handle.write("\n")
    return 1 if drift else 0


if __name__ == "__main__":
    sys.exit(main())
