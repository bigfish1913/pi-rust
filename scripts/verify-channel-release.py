#!/usr/bin/env python3
"""Require all six verified archives before advertising a package-manager update."""
import json
from pathlib import Path
import re
import subprocess
import sys

REPOSITORY = "https://github.com/bigfish1913/pi-rust"
TARGETS = {"x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu",
           "x86_64-unknown-linux-musl", "aarch64-unknown-linux-gnu",
           "x86_64-apple-darwin", "aarch64-apple-darwin"}


def verify(tag, manifest, release):
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", tag):
        raise ValueError("invalid release tag")
    if (manifest.get("schema_version") != 1 or manifest.get("name") != "rpi-cli"
            or manifest.get("repository") != REPOSITORY or manifest.get("tag") != tag
            or manifest.get("version") != tag[1:]
            or not re.fullmatch(r"[0-9a-f]{40}", manifest.get("source_commit", ""))
            or set(manifest.get("targets", {})) != TARGETS):
        raise ValueError("manifest must describe this upstream release and all six targets")
    if release.get("draft") or release.get("prerelease") or release.get("tag_name") != tag:
        raise ValueError("install channels require a published stable release")
    assets = {asset["name"]: asset for asset in release["assets"]}
    if "rpi-release.json" not in assets:
        raise ValueError("release is missing its readiness manifest")
    for target, row in manifest["targets"].items():
        extension = "zip" if "windows" in target else "tar.gz"
        name = f"rpi-{tag}-{target}.{extension}"
        asset = assets.get(name, {})
        if (row.get("url") != f"{REPOSITORY}/releases/download/{tag}/{name}"
                or row.get("format") != extension
                or not re.fullmatch(r"[0-9a-f]{64}", row.get("sha256", ""))
                or asset.get("digest") != "sha256:" + row["sha256"]
                or not isinstance(row.get("size"), int) or row["size"] <= 0
                or asset.get("size") != row["size"] or name + ".sha256" not in assets):
            raise ValueError(f"missing or inconsistent release archive: {target}")


if __name__ == "__main__":
    tag, filename = sys.argv[1:]
    release = json.loads(subprocess.check_output([
        "gh", "api", f"repos/bigfish1913/pi-rust/releases/tags/{tag}"], text=True, encoding="utf-8"))
    manifest = json.loads(Path(filename).read_text(encoding="utf-8"))
    commit = json.loads(subprocess.check_output([
        "gh", "api", f"repos/bigfish1913/pi-rust/commits/{tag}"], text=True, encoding="utf-8"))
    if manifest.get("source_commit") != commit["sha"]:
        raise ValueError("release manifest source does not match the tag")
    verify(tag, manifest, release)
    print(f"Verified all six platforms for {tag}")
