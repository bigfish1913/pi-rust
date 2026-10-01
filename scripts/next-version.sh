#!/usr/bin/env bash
# Compute the next release version for the rpi-* crate family.
#
#   bash scripts/next-version.sh [patch|minor|major|none]
#
# `patch` (the default), `minor` and `major` increment the highest released
# `vX.Y.Z` git tag. The workspace version in Cargo.toml wins whenever it is
# already ahead of every tag: that means a bump was prepared (or a release was
# interrupted before its tag shipped) and must not be incremented twice.
#
# `none` never increments — it prints that already-prepared workspace version.
# Use it to resume an interrupted release:
#
#   bash scripts/release.sh "$(bash scripts/next-version.sh none)" --from push
#
# The result is a bare X.Y.Z on stdout so callers can capture it:
#
#   VERSION="$(bash scripts/next-version.sh minor)"
set -euo pipefail

kind="${1:-patch}"
case "$kind" in
  patch|minor|major|none) ;;
  *) echo "error: unknown bump '$kind' (expected patch|minor|major|none)" >&2; exit 1 ;;
esac

workspace="$(grep -m1 -oE '^version = "[0-9]+\.[0-9]+\.[0-9]+"' Cargo.toml \
  | grep -oE '[0-9]+\.[0-9]+\.[0-9]+')" \
  || { echo "error: could not read the workspace version from Cargo.toml" >&2; exit 1; }
[[ -n "$workspace" ]] \
  || { echo "error: could not read the workspace version from Cargo.toml" >&2; exit 1; }

tagged="$(git tag --list --sort=-v:refname 'v*' \
  | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' \
  | head -n1 \
  | sed 's/^v//')" || true
tagged="${tagged:-0.0.0}"

# True when $1 sorts strictly after $2 as a numeric triple.
is_newer() {
  [[ "$1" != "$2" ]] || return 1
  [[ "$(printf '%s\n%s\n' "$1" "$2" | sort -t. -k1,1n -k2,2n -k3,3n | tail -n1)" == "$1" ]]
}

# A prepared-but-unreleased workspace version is reused verbatim: the tree may
# already be bumped, committed and even tagged locally by an interrupted run.
if is_newer "$workspace" "$tagged" || [[ "$kind" == "none" ]]; then
  echo "$workspace"
  exit 0
fi

IFS=. read -r major minor patch <<<"$tagged"
major="${major:-0}"; minor="${minor:-0}"; patch="${patch:-0}"
case "$kind" in
  patch) patch=$((patch + 1)) ;;
  minor) minor=$((minor + 1)); patch=0 ;;
  major) major=$((major + 1)); minor=0; patch=0 ;;
esac
echo "$major.$minor.$patch"
