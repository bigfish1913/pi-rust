#!/usr/bin/env bash
# Push the refreshed Homebrew formula + Scoop manifest into their tap repos.
#
#   bash scripts/push-taps.sh 0.3.4
#
# The files in packaging/ are the source of truth; the tap repos are copies.
# Uses the GitHub contents API (needs write access to both repos).

set -euo pipefail

VERSION="${1:?usage: push-taps.sh X.Y.Z}"
MSG="rpi $VERSION"

die() { echo "error: $*" >&2; exit 1; }
note() { echo "» $*"; }

cd "$(git rev-parse --show-toplevel)"

# put REPO PATH LOCAL_FILE — create or update a file via the contents API.
put() {
  local repo="$1" path="$2" file="$3"
  [[ -f "$file" ]] || die "missing $file"
  local content
  content=$(base64 <"$file" | tr -d '\n')
  local sha
  sha=$(gh api "repos/$repo/contents/$path" --jq .sha 2>/dev/null || true)
  if [[ -n "$sha" ]]; then
    note "update $repo/$path"
    gh api --method PUT "repos/$repo/contents/$path" \
      -f message="$MSG" -f content="$content" -f sha="$sha" >/dev/null
  else
    note "create $repo/$path"
    gh api --method PUT "repos/$repo/contents/$path" \
      -f message="$MSG" -f content="$content" >/dev/null
  fi
}

put "bigfish1913/homebrew-tap" "Formula/rpi.rb" "packaging/homebrew/rpi.rb"
put "bigfish1913/scoop-bucket" "bucket/rpi.json" "packaging/scoop/rpi.json"

note "taps pushed"
