#!/usr/bin/env python3
"""Build/assemble CLI release manifests using the prebuilt package protocol."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import tarfile
import zipfile

TARGETS = {
    "x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl", "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin", "aarch64-apple-darwin",
}
REPOSITORY = "https://github.com/bigfish1913/pi-rust"


def sha(data):
    return hashlib.sha256(data).hexdigest()


def describe(archive, target, version, commit, repository=REPOSITORY):
    if target not in TARGETS or not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("invalid target/source commit")
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", version):
        raise ValueError("invalid package version")
    if repository.rstrip("/") != REPOSITORY:
        raise ValueError("unexpected CLI publisher")
    archive = Path(archive)
    name = "rpi.exe" if "windows" in target else "rpi"
    archive_format = "zip" if archive.name.endswith(".zip") else "tar.gz"
    if archive_format == "zip":
        with zipfile.ZipFile(archive) as package:
            matches = [f for f in package.infolist() if f.filename.removeprefix("./") == name]
            if len(matches) != 1:
                raise ValueError("archive must contain the exact binary once")
            binary = package.read(matches[0])
    else:
        with tarfile.open(archive, "r:gz") as package:
            matches = [f for f in package.getmembers() if f.name.removeprefix("./") == name]
            if len(matches) != 1 or not matches[0].isfile():
                raise ValueError("archive must contain one regular executable")
            binary = package.extractfile(matches[0]).read()
    data = archive.read_bytes()
    if not binary or len(binary) > 128 * 1024 * 1024 or len(data) > 256 * 1024 * 1024:
        raise ValueError("artifact exceeds installer limits")
    return {"version": version, "repository": REPOSITORY, "source_commit": commit, "target": target,
            "distribution": {"url": f"{REPOSITORY}/releases/download/v{version}/{archive.name}",
                             "format": archive_format, "size": len(data), "sha256": sha(data),
                             "binary": {"path": name, "sha256": sha(binary)}}}


def assemble(directory, tag, commit):
    directory = Path(directory)
    rows = [json.loads(file.read_text(encoding="utf-8")) for file in directory.glob("*.prebuilt.json")]
    if len(rows) != 6 or {row["target"] for row in rows} != TARGETS:
        raise ValueError("all six target descriptors are required, without duplicates")
    version = tag.removeprefix("v")
    for row in rows:
        if row["version"] != version or row["repository"] != REPOSITORY or row["source_commit"] != commit:
            raise ValueError("target builds disagree on version, publisher or source commit")
        asset = row["distribution"]["url"].rsplit("/", 1)[-1]
        if Path(asset).name != asset:
            raise ValueError("unsafe artifact filename")
        actual = describe(directory / asset, row["target"], version, commit)
        if row != actual:
            raise ValueError("artifact contents differ from build descriptor")
        checksum = (directory / (asset + ".sha256")).read_text().strip().split()
        if len(checksum) != 2 or checksum[0] != actual["distribution"]["sha256"] or checksum[1].lstrip("*") != asset:
            raise ValueError("legacy installer checksum differs from manifest")
    if tag != f"v{version}":
        raise ValueError("invalid CLI release tag")
    return {"schema_version": 1, "name": "rpi-cli", "version": version, "repository": REPOSITORY,
            "tag": tag, "source_commit": commit,
            "targets": {row["target"]: row["distribution"] for row in sorted(rows, key=lambda r: r["target"])}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    describe_args = commands.add_parser("describe")
    for name in ("archive", "target", "version", "commit", "output"):
        describe_args.add_argument(f"--{name}", required=True)
    assembly = commands.add_parser("assemble")
    for name in ("directory", "tag", "commit", "output"):
        assembly.add_argument(f"--{name}", required=True)
    args = parser.parse_args()
    result = (describe(args.archive, args.target, args.version, args.commit) if args.command == "describe"
              else assemble(args.directory, args.tag, args.commit))
    Path(args.output).write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
