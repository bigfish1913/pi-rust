#!/usr/bin/env bash
# One-command release for the rpi-* crate family.
#
#   task release RELEASE_VERSION=0.3.4
#   bash scripts/release.sh 0.3.4 --dry-run
#
# The pipeline: bump -> changelog -> docs -> tag -> push -> wait for the
# release binaries -> refresh the install channels (brew/scoop/winget + site)
# -> push the taps -> open the winget PR -> publish to crates.io -> deploy site.
#
# Every step is printed before it runs. Mutating network steps still run under
# --dry-run's shadow (nothing is executed), so you can review the exact plan
# first. Phases are individually selectable (--only / --from / --skip).
#
# Credentials this expects (all optional until the phase that needs them):
#   * git push access to bigfish1913/pi-rust, bigfish1913/homebrew-tap,
#     bigfish1913/scoop-bucket
#   * `gh` authenticated (release assets, taps, fork + PR to winget-pkgs)
#   * `cargo login` for the crates.io publish phase
#   * SSH to the rpi web host for the site phase
#
# The winget phase only *opens* a PR: a winget-team moderator merges it, which
# no command here can do.

set -euo pipefail

REPO="bigfish1913/pi-rust"
# `<target>:<archive extension>` — must mirror the `archive:` column of the
# release-binaries matrix. Each target publishes exactly ONE archive (plus its
# `.sha256` sidecar), so requiring both formats here waits forever.
ASSET_TARGETS=(
  "x86_64-unknown-linux-gnu:tar.gz"
  "aarch64-unknown-linux-gnu:tar.gz"
  "aarch64-apple-darwin:tar.gz"
  "x86_64-apple-darwin:tar.gz"
  "x86_64-pc-windows-msvc:zip"
)

DRY_RUN=0
ASSUME_YES=0
ONLY=""
FROM=""
SKIP=""
RELEASE_DATE=""

die() { echo "error: $*" >&2; exit 1; }
note() { echo "» $*"; }

# Run a command, echoing it first. Under --dry-run it is only echoed.
step() {
  echo "  \$ $*"
  if [[ "$DRY_RUN" == "1" ]]; then return 0; fi
  "$@"
}

usage() {
  sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'
  cat <<'EOF'

Options:
  --only  PHASE[,PHASE...]  run just these phases
  --from  PHASE             start at this phase
  --skip  PHASE[,PHASE...]  skip these phases
  --date  YYYY-MM-DD        changelog date (default: today, UTC)
  --dry-run                 print the plan, execute nothing
  --yes                     do not stop at the confirmation checkpoints

Phases (in order):
  preflight bump changelog docs validate commit tag push wait
  channels commit-channels taps winget crates site
EOF
}

PHASES=(preflight bump changelog docs validate commit tag push wait channels commit-channels taps winget crates site)

phase_selected() {
  local phase="$1"
  if [[ -n "$ONLY" ]]; then
    [[ ",$ONLY," == *",$phase,"* ]] || return 1
  fi
  if [[ -n "$SKIP" && ",$SKIP," == *",$phase,"* ]]; then return 1; fi
  if [[ -n "$FROM" ]]; then
    local idx_from idx_phase
    idx_from=$(phase_index "$FROM")
    idx_phase=$(phase_index "$phase")
    [[ "$idx_phase" -ge "$idx_from" ]] || return 1
  fi
  return 0
}

phase_index() {
  local i
  for i in "${!PHASES[@]}"; do
    [[ "${PHASES[$i]}" == "$1" ]] && { echo "$i"; return; }
  done
  die "unknown phase: $1"
}

confirm() {
  [[ "$ASSUME_YES" == "1" ]] && return 0
  [[ "$DRY_RUN" == "1" ]] && return 0
  read -r -p "$1 [y/N] " reply
  [[ "$reply" == "y" || "$reply" == "Y" ]]
}

# --------------------------------------------------------------------------
# Phases
# --------------------------------------------------------------------------

release_version=""
release_tag=""

phase_preflight() {
  note "preflight"
  [[ "$(git rev-parse --is-inside-work-tree 2>/dev/null)" == "true" ]] || die "not a git repo"
  [[ -z "$(git status --porcelain --untracked-files=no)" ]] \
    || die "working tree has tracked changes; commit or stash first"
  [[ -z "$(git tag --list "$release_tag")" ]] || die "tag $release_tag already exists"
  local current
  current=$(grep -m1 -oE '^version = "[0-9.]+"' Cargo.toml | grep -oE '[0-9.]+') \
    || die "could not read the workspace version from Cargo.toml"
  [[ "$current" != "$release_version" ]] || die "Cargo.toml is already at $release_version"
  note "  branch: $(git rev-parse --abbrev-ref HEAD) · tag: $release_tag · bump $current -> $release_version"
}

phase_bump() {
  note "bump Cargo.toml pins to $release_version"
  local current
  current=$(grep -m1 -oE '^version = "[0-9.]+"' Cargo.toml | grep -oE '[0-9.]+') \
    || die "could not read the workspace version"
  if [[ "$current" == "$release_version" ]]; then
    note "  already at $release_version"
  else
    # The workspace version + every internal pin use the same literal, so a
    # single substitution covers both.
    step perl -pi -e "s/version = \"$current\"/version = \"$release_version\"/g" Cargo.toml
  fi
  step cargo update --workspace
}

phase_changelog() {
  note "cut CHANGELOG $release_version ($RELEASE_DATE)"
  if grep -q "^## \[$release_version\]" CHANGELOG.md; then
    note "  already present"
    return
  fi
  step perl -0pi -e \
    "s/(## \[Unreleased\])(\r?\n)/\$1\$2\$2## [$release_version] - $RELEASE_DATE\$2/" CHANGELOG.md
}

phase_docs() {
  note "sync embedded docs"
  step bash scripts/sync-embedded-docs.sh
}

phase_validate() {
  note "release-helper validate (allowing the uncommitted bump)"
  # The version bump is not committed until `phase_commit`, so the helper must
  # tolerate a dirty tree here. `task publish` re-runs the strict clean-tree
  # check after the commit.
  step task release:validate-dirty RELEASE_VERSION="$release_version"
}

phase_commit() {
  note "commit the version bump"
  step git add -A
  # Re-running a release whose bump already landed leaves an empty index;
  # `git commit` would exit 1 there and abort the pipeline under `set -e`.
  if [[ "$DRY_RUN" != "1" ]] && git diff --cached --quiet; then
    note "  nothing to commit"
    return
  fi
  step git commit -m "chore(release): $release_version"
}

phase_tag() {
  note "tag $release_tag"
  step git tag "$release_tag"
}

phase_push() {
  note "push branch + tag"
  step git push origin HEAD
  step git push origin "$release_tag"
}

phase_wait() {
  note "wait for the release binaries (release-binaries.yml)"
  local needed=()
  local entry target ext
  for entry in "${ASSET_TARGETS[@]}"; do
    target="${entry%%:*}"
    ext="${entry##*:}"
    needed+=("rpi-$release_tag-$target.$ext" "rpi-$release_tag-$target.$ext.sha256")
  done
  if [[ "$DRY_RUN" == "1" ]]; then
    echo "  \$ poll: gh release view $release_tag --json assets until all archives + .sha256 exist"
    return
  fi
  local attempt
  for attempt in $(seq 1 120); do
    local present
    present=$(gh release view "$release_tag" -R "$REPO" --json assets --jq '.assets[].name' 2>/dev/null || true)
    local ok=1 want
    for want in "${needed[@]}"; do
      grep -qxF "$want" <<<"$present" || { ok=0; break; }
    done
    if [[ "$ok" == "1" ]]; then
      note "  assets ready"
      return
    fi
    sleep 15
  done
  die "timed out waiting for the release assets; re-run --from channels once CI finishes"
}

phase_channels() {
  note "refresh install channels + site data for $release_version"
  step bash scripts/refresh-channels.sh "$release_version"
}

phase_commit_channels() {
  note "commit the channel refresh"
  step git add -A
  if [[ "$DRY_RUN" != "1" ]] && git diff --cached --quiet; then
    note "  nothing to commit"
    return
  fi
  step git commit -m "chore(release): refresh the install channels and the site for $release_version"
}

phase_taps() {
  note "push Homebrew tap + Scoop bucket"
  step bash scripts/push-taps.sh "$release_version"
}

phase_winget() {
  note "open the winget-pkgs PR"
  step bash scripts/winget-submit.sh "$release_version"
}

phase_crates() {
  note "publish nine crates to crates.io"
  confirm "Publish $release_version to crates.io (irreversible)?" || { note "  skipped"; return; }
  step task publish:crates RELEASE_VERSION="$release_version"
}

phase_site() {
  note "deploy the site"
  confirm "Deploy the site (SSH to the rpi host)?" || { note "  skipped"; return; }
  step task rpi-deploy
}

# --------------------------------------------------------------------------
# Entry
# --------------------------------------------------------------------------

main() {
  [[ $# -gt 0 ]] || { usage; exit 1; }
  release_version=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --dry-run) DRY_RUN=1 ;;
      --yes|-y) ASSUME_YES=1 ;;
      --only) ONLY="${2:?}"; shift ;;
      --from) FROM="${2:?}"; shift ;;
      --skip) SKIP="${2:?}"; shift ;;
      --date) RELEASE_DATE="${2:?}"; shift ;;
      -h|--help) usage; exit 0 ;;
      --*) die "unknown flag: $1" ;;
      *) [[ -z "$release_version" ]] || die "version already set ($release_version)"; release_version="$1" ;;
    esac
    shift
  done

  [[ -n "$release_version" ]] || { usage; exit 1; }
  [[ "$release_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "version must be X.Y.Z"
  release_tag="v$release_version"
  [[ -n "$RELEASE_DATE" ]] || RELEASE_DATE="$(date -u +%Y-%m-%d)"

  echo "release $release_version  (tag $release_tag, date $RELEASE_DATE)"
  [[ "$DRY_RUN" == "1" ]] && echo "MODE: dry-run (nothing executes)"
  echo

  local phase
  for phase in "${PHASES[@]}"; do
    phase_selected "$phase" || continue
    "phase_${phase//-/_}"
    echo
  done

  note "done"
}

main "$@"
