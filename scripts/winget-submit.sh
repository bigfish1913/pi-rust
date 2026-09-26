#!/usr/bin/env bash
# Open the winget-pkgs PR for a released version.
#
#   bash scripts/winget-submit.sh 0.3.4
#
# Server-side only (no 866 MB monorepo clone): fork, branch off upstream
# master, PUT the three manifests, open the PR. The winget-team moderator merge
# is external — this command cannot complete it.
#
# After it opens the PR, sign the CLA by commenting at column 0 with nothing
# else on the line:
#   @microsoft-github-policy-service agree
# (a leading space silently fails the match).

set -euo pipefail

VERSION="${1:?usage: winget-submit.sh X.Y.Z}"

die() { echo "error: $*" >&2; exit 1; }
note() { echo "» $*"; }

cd "$(git rev-parse --show-toplevel)"

me=$(gh api user --jq .login) || die "gh is not authenticated"
branch="rpi-$VERSION"
dir="manifests/b/bigfish1913/rpi/$VERSION"
up=$(gh api repos/microsoft/winget-pkgs/commits/master --jq .sha) \
  || die "could not read upstream master"

note "fork $me/winget-pkgs (server-side)"
gh repo fork microsoft/winget-pkgs --clone=false >/dev/null 2>&1 || true

note "branch $branch off $up"
gh api --method POST "repos/$me/winget-pkgs/git/refs" \
  -f ref="refs/heads/$branch" -f sha="$up" >/dev/null 2>&1 \
  || note "  (branch may already exist; continuing)"

for f in \
  packaging/winget/bigfish1913.rpi.yaml \
  packaging/winget/bigfish1913.rpi.installer.yaml \
  packaging/winget/bigfish1913.rpi.locale.en-US.yaml; do
  name=$(basename "$f")
  [[ -f "$f" ]] || die "missing $f"
  content=$(base64 <"$f" | tr -d '\n')
  note "put $dir/$name"
  gh api --method PUT "repos/$me/winget-pkgs/contents/$dir/$name" \
    -f message="New package: bigfish1913.rpi version $VERSION" \
    -f content="$content" -f branch="$branch" >/dev/null 2>&1 \
    || die "failed to upload $name (a file may already exist at that path)"
done

note "open PR"
gh pr create --repo microsoft/winget-pkgs --base master --head "$me:$branch" \
  --title "New package: bigfish1913.rpi version $VERSION" \
  --body "Adds bigfish1913.rpi $VERSION.

Manifests are generated from the release assets of
bigfish1913/pi-rust (tag v$VERSION); the installer is the portable Windows zip."

note "PR opened — sign the CLA if prompted (comment at column 0):"
note "  @microsoft-github-policy-service agree"
