#!/usr/bin/env python3
"""Mirror one verified rpi CLI release to an optional S3-compatible RustFS bucket."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
from urllib.parse import urlparse

REQUIRED = (
    "RPI_RUSTFS_ENDPOINT",
    "RPI_RUSTFS_BUCKET",
    "RPI_RUSTFS_ACCESS_KEY",
    "RPI_RUSTFS_SECRET_KEY",
)


def digest(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def config(environ=None):
    env = os.environ if environ is None else environ
    values = {key: env.get(key, "").strip() for key in REQUIRED}
    public = env.get("RPI_CDN_BASE_URL", "").strip().rstrip("/")
    if not any(values.values()):
        return None
    missing = [key for key, value in values.items() if not value]
    if missing:
        raise ValueError("RustFS mirror is partially configured; missing " + ", ".join(missing))
    endpoint = urlparse(values["RPI_RUSTFS_ENDPOINT"])
    if (endpoint.scheme not in ("http", "https") or not endpoint.netloc
            or endpoint.username or endpoint.password or endpoint.query or endpoint.fragment):
        raise ValueError("RPI_RUSTFS_ENDPOINT must be an HTTP(S) URL without credentials or query")
    public_url = urlparse(public) if public else None
    if public_url and (public_url.scheme != "https" or not public_url.netloc
                       or public_url.username or public_url.password
                       or public_url.query or public_url.fragment):
        raise ValueError("RPI_CDN_BASE_URL must be an HTTPS URL without credentials or query")
    values["RPI_CDN_BASE_URL"] = public
    return values


def files(directory: Path, manifest: str):
    selected = [directory / manifest]
    selected += sorted(directory.glob("rpi-*.tar.gz"))
    selected += sorted(directory.glob("rpi-*.zip"))
    selected += sorted(directory.glob("rpi-*.tar.gz.sha256"))
    selected += sorted(directory.glob("rpi-*.zip.sha256"))
    selected = [path for path in selected if path.is_file()]
    if not (directory / manifest).is_file():
        raise ValueError(f"missing release manifest: {directory / manifest}")
    if not any(path.name.endswith((".tar.gz", ".zip")) for path in selected):
        raise ValueError("no release archives found")
    if any(path.name != Path(path.name).name for path in selected):
        raise ValueError("release asset name contains a path")
    return selected


def head_command(aws, cfg, key):
    return [aws, "s3api", "head-object", "--bucket", cfg["RPI_RUSTFS_BUCKET"],
            "--key", key, "--endpoint-url", cfg["RPI_RUSTFS_ENDPOINT"], "--no-cli-pager"]


def copy_command(aws, cfg, key, path):
    if path.suffix == ".json":
        content_type = "application/json"
        cache_control = "public,max-age=60,s-maxage=60,must-revalidate"
    elif path.name.endswith(".sha256"):
        content_type = "text/plain"
        cache_control = "public,max-age=31536000,immutable"
    else:
        content_type = "application/octet-stream"
        cache_control = "public,max-age=31536000,immutable"
    return [aws, "s3", "cp", str(path),
            f"s3://{cfg['RPI_RUSTFS_BUCKET']}/{key}",
            "--endpoint-url", cfg["RPI_RUSTFS_ENDPOINT"], "--only-show-errors",
            "--content-type", content_type, "--cache-control", cache_control,
            "--metadata", f"sha256={digest(path)}"]


def missing(result):
    text = ((result.stdout or "") + "\n" + (result.stderr or "")).lower()
    return "nosuchkey" in text or "status code: 404" in text or "(404)" in text


def mirror(args):
    cfg = config()
    if cfg is None:
        print("RustFS mirror not configured; skipping optional CDN mirror.")
        return 0
    aws = shutil.which("aws")
    if not aws:
        raise RuntimeError("AWS CLI is required when RustFS mirror settings are configured")
    root = Path(args.directory).resolve()
    selected = files(root, args.manifest)
    env = os.environ.copy()
    env.update({"AWS_ACCESS_KEY_ID": cfg["RPI_RUSTFS_ACCESS_KEY"],
                "AWS_SECRET_ACCESS_KEY": cfg["RPI_RUSTFS_SECRET_KEY"],
                "AWS_EC2_METADATA_DISABLED": "true"})
    base = cfg["RPI_CDN_BASE_URL"]
    if base:
        print(f"Mirroring {len(selected)} rpi release files below {base}/rpi/releases/{args.tag}/")
    else:
        print(f"Mirroring {len(selected)} rpi release files to configured RustFS")
    for path in selected:
        key = f"rpi/releases/{args.tag}/{path.name}"
        existing = subprocess.run(head_command(aws, cfg, key), env=env,
                                  capture_output=True, text=True)
        if existing.returncode == 0:
            try:
                metadata = json.loads(existing.stdout)
            except json.JSONDecodeError as error:
                raise RuntimeError(f"cannot verify existing immutable object: {key}") from error
            if metadata.get("ContentLength") != path.stat().st_size:
                raise RuntimeError(f"existing immutable object has a different size: {key}")
            remote = (metadata.get("Metadata") or {}).get("sha256")
            if remote and remote != digest(path):
                raise RuntimeError(f"existing immutable object has a different hash: {key}")
            print(f"Preserving existing immutable object: {key}")
            continue
        if not missing(existing):
            raise RuntimeError(f"cannot inspect RustFS object before upload: {key}")
        subprocess.run(copy_command(aws, cfg, key, path), env=env, check=True)
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", required=True)
    parser.add_argument("--manifest", default="rpi-release.json")
    parser.add_argument("--tag", required=True)
    args = parser.parse_args()
    try:
        return mirror(args)
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
