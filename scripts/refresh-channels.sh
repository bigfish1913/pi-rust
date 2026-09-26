#!/usr/bin/env bash
# Rewrite the install-channel manifests + site data for a released version.
#
#   bash scripts/refresh-channels.sh 0.3.4
#
# Requires the release assets to exist (run after the binaries workflow
# finishes). It downloads only the small `.sha256` sidecars via `gh` and does
# targeted substitutions — no templates — so a format tweak upstream does not
# silently drop. Idempotent: re-running against the same version is a no-op.

set -euo pipefail

VERSION="${1:?usage: refresh-channels.sh X.Y.Z}"
REPO="bigfish1913/pi-rust"
TAG="v$VERSION"

die() { echo "error: $*" >&2; exit 1; }
note() { echo "» $*"; }

cd "$(git rev-parse --show-toplevel)"

OLD=$(grep -m1 -oE '"version": "[0-9.]+"' packaging/scoop/rpi.json | grep -oE '[0-9.]+')
[[ -n "$OLD" ]] || die "could not read the current version from packaging/scoop/rpi.json"
[[ "$OLD" != "$VERSION" ]] || { note "manifests already at $VERSION"; exit 0; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
note "fetching .sha256 sidecars for $TAG"
gh release download "$TAG" -R "$REPO" -p "*.sha256" -D "$tmp" >/dev/null

# sha ASSET -> lowercase hex digest from its .sha256 sidecar.
sha() {
  local file="$tmp/$1.sha256"
  [[ -f "$file" ]] || die "missing $1.sha256 in the $TAG release"
  awk '{print $1}' "$file" | tr 'A-F' 'a-f'
}

LINUX_X64="rpi-$TAG-x86_64-unknown-linux-gnu.tar.gz"
LINUX_ARM="rpi-$TAG-aarch64-unknown-linux-gnu.tar.gz"
MAC_ARM="rpi-$TAG-aarch64-apple-darwin.tar.gz"
WIN_X64="rpi-$TAG-x86_64-pc-windows-msvc.zip"

SHA_LINUX_X64=$(sha "$LINUX_X64")
SHA_LINUX_ARM=$(sha "$LINUX_ARM")
SHA_MAC_ARM=$(sha "$MAC_ARM")
SHA_WIN_X64=$(sha "$WIN_X64")

# --------------------------------------------------------------------------
# Homebrew: version + URLs + the three sha256 lines (order in file:
# apple-darwin, linux aarch64, linux x86_64).
# --------------------------------------------------------------------------
note "packaging/homebrew/rpi.rb"
mapfile -t OLD_BREW < <(grep -oE 'sha256 "[0-9a-f]+"' packaging/homebrew/rpi.rb | grep -oE '[0-9a-f]{64}')
[[ "${#OLD_BREW[@]}" -eq 3 ]] || die "expected 3 sha256 lines in the Homebrew formula"
perl -pi -e "s/version \"$OLD\"/version \"$VERSION\"/; s/v$OLD/v$VERSION/g" packaging/homebrew/rpi.rb
perl -pi -e "s/${OLD_BREW[0]}/$SHA_MAC_ARM/" packaging/homebrew/rpi.rb
perl -pi -e "s/${OLD_BREW[1]}/$SHA_LINUX_ARM/" packaging/homebrew/rpi.rb
perl -pi -e "s/${OLD_BREW[2]}/$SHA_LINUX_X64/" packaging/homebrew/rpi.rb

# --------------------------------------------------------------------------
# Scoop
# --------------------------------------------------------------------------
note "packaging/scoop/rpi.json"
OLD_SCOOP=$(grep -m1 -oE '"hash": "[0-9a-fA-F]+"' packaging/scoop/rpi.json | grep -oE '[0-9a-fA-F]{64}')
[[ -n "$OLD_SCOOP" ]] || die "could not read the Scoop hash"
perl -pi -e "s/\"version\": \"$OLD\"/\"version\": \"$VERSION\"/; s/v$OLD/v$VERSION/g; s/$OLD_SCOOP/$SHA_WIN_X64/" packaging/scoop/rpi.json

# --------------------------------------------------------------------------
# winget
# --------------------------------------------------------------------------
note "packaging/winget/*.yaml"
DATE=$(date -u +%Y-%m-%d)
OLD_WINGET=$(grep -m1 -oE 'InstallerSha256: [0-9A-Fa-f]+' packaging/winget/bigfish1913.rpi.installer.yaml | grep -oE '[0-9A-Fa-f]{64}')
[[ -n "$OLD_WINGET" ]] || die "could not read the winget InstallerSha256"
WIN_UPPER=$(printf '%s' "$SHA_WIN_X64" | tr 'a-f' 'A-F')
perl -pi -e "s/PackageVersion: $OLD/PackageVersion: $VERSION/; s/v$OLD/v$VERSION/g; s/$OLD_WINGET/$WIN_UPPER/; s/ReleaseDate: [0-9-]+/ReleaseDate: $DATE/" \
  packaging/winget/bigfish1913.rpi.installer.yaml
perl -pi -e "s/PackageVersion: $OLD/PackageVersion: $VERSION/; s/v$OLD/v$VERSION/g" \
  packaging/winget/bigfish1913.rpi.locale.en-US.yaml
perl -pi -e "s/PackageVersion: $OLD/PackageVersion: $VERSION/" \
  packaging/winget/bigfish1913.rpi.yaml

# --------------------------------------------------------------------------
# Site data: the "current release" card + the version string.
# --------------------------------------------------------------------------
note "website/data/{packages,locales}.json"
perl -pi -e "s/v$OLD · current/v$VERSION · current/g" website/data/packages.json
perl -pi -e "s/\"version\": \"$OLD\"/\"version\": \"$VERSION\"/" website/data/locales.json

# --------------------------------------------------------------------------
# packaging/README.md: the sha table label/rows + the example tag.
# --------------------------------------------------------------------------
note "packaging/README.md"
perl -pi -e "s/sha256 \(v$OLD\)/sha256 (v$VERSION)/; s/TAG=v$OLD/TAG=v$VERSION/; s/v$OLD/v$VERSION/g" packaging/README.md
mapfile -t OLD_TABLE < <(grep -oE '\| `[0-9a-f]{64}` \|' packaging/README.md | grep -oE '[0-9a-f]{64}')
if [[ "${#OLD_TABLE[@]}" -eq 4 ]]; then
  # Table order: linux x86_64, linux aarch64, apple aarch64, windows.
  perl -pi -e "s/${OLD_TABLE[0]}/$SHA_LINUX_X64/" packaging/README.md
  perl -pi -e "s/${OLD_TABLE[1]}/$SHA_LINUX_ARM/" packaging/README.md
  perl -pi -e "s/${OLD_TABLE[2]}/$SHA_MAC_ARM/" packaging/README.md
  perl -pi -e "s/${OLD_TABLE[3]}/$SHA_WIN_X64/" packaging/README.md
fi

note "done — review the diff, then commit the channel refresh"
