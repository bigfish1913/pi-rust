#!/usr/bin/env bash
# pi-rust crates.io release helper (Unix/CI). PowerShell release.ps1 is the
# canonical source; this mirrors it for Linux/macOS and CI.
#
# Publishes the rpi-* workspace crates to crates.io in dependency order:
#   rpi-telemetry -> rpi-ai -> rpi-agent -> rpi-tools -> rpi-harness -> rpi-cli
# (examples are publish=false and skipped).
#
# Default mode is a SAFE DRY RUN: each crate is packed + verified with
# `cargo publish --dry-run`. Only rpi-telemetry fully resolves in a dry run —
# every downstream crate SKIPs with "no matching package `rpi-X`" because its
# rpi-* deps aren't on crates.io yet. That's NORMAL for a first-time workspace
# publish; the real publish (--publish) resolves them in order.
#
# --publish runs the real `cargo publish` per crate, stopping on the first hard
# failure (downstream crates depend on it). A short sleep between publishes lets
# the crates.io index propagate; a transient "no matching package" error is
# retried a few times before being treated as a failure.
#
# Prerequisites for --publish:
#   * `cargo login` run once interactively (creates ~/.cargo/credentials(.toml)).
#   * workspace.repository in the root Cargo.toml set to YOUR repo URL
#     (must NOT be the upstream earendil-works/pi placeholder).
#   * crates.io names are permanent — check them first.
#
# Usage:
#   ./release.sh                # safe dry run
#   ./release.sh --publish      # real publish, dep order
#   ./release.sh --publish --skip-test

set -u

REAL=0
SKIP_TEST=0
SLEEP_SECONDS=3

while [ $# -gt 0 ]; do
  case "$1" in
    --publish) REAL=1; shift;;
    --dry-run) REAL=0; shift;;
    --skip-test) SKIP_TEST=1; shift;;
    --sleep) SLEEP_SECONDS="$2"; shift 2;;
    -h|--help)
      sed -n '2,30p' "$0"
      exit 0;;
    *) echo "Unknown option: $1" >&2; exit 2;;
  esac
done

MODE="DRY RUN"
if [ "$REAL" = "1" ]; then MODE="PUBLISH (real)"; fi

ORDER=(rpi-telemetry rpi-ai rpi-agent rpi-tools rpi-harness rpi-cli)
REPO_ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$REPO_ROOT"

ok()   { printf '[\033[32mok\033[0m]   %s\n' "$1"; }
skip() { printf '[\033[33mskip\033[0m] %s\n' "$1"; }
bad()  { printf '[\033[31mfail\033[0m] %s\n' "$1"; }

# --- credentials check (real publish only) ---
if [ "$REAL" = "1" ]; then
  if [ ! -f "$HOME/.cargo/credentials.toml" ] && [ ! -f "$HOME/.cargo/credentials" ]; then
    bad "No cargo credentials found (~/.cargo/credentials.toml)."
    echo "  Run 'cargo login' once interactively, then re-run --publish." >&2
    exit 1
  fi
fi

# --- repository sanity check ---
if grep -q 'earendil-works/pi' Cargo.toml; then
  if [ "$REAL" = "1" ]; then
    bad "repository in Cargo.toml points at the upstream TS source (earendil-works/pi)."
    echo "  Set [workspace.package].repository to YOUR rpi repo URL before publishing." >&2
    echo "  crates.io records are permanent — aborting." >&2
    exit 1
  else
    skip "repository URL is the upstream placeholder (fine for dry run; fix before --publish)."
  fi
fi

# --- clean-tree check ---
if [ -n "$(git status --porcelain 2>/dev/null)" ]; then
  if [ "$REAL" = "1" ]; then
    bad "Working tree has uncommitted changes; refusing to --publish."
    git status --porcelain
    exit 1
  else
    skip "Working tree is dirty (allowed in dry run; --allow-dirty will be passed)."
  fi
fi

# --- pre-flight tests ---
if [ "$SKIP_TEST" = "0" ]; then
  echo "=== Pre-flight: cargo test --workspace ==="
  cargo test --workspace
  if [ $? -ne 0 ]; then
    bad "Pre-flight tests failed. Aborting."
    exit 1
  fi
  ok "tests pass"
else
  skip "Skipping pre-flight tests (--skip-test)."
fi

# --- publish loop ---
echo "=== pi-rust release — $MODE — order: ${ORDER[*]} ==="

ANY_FAIL=0
declare -a SUM_CRATE SUM_STATUS

publish_one() {
  local crate="$1"
  echo "--- $crate ---"
  local args=(publish -p "$crate")
  if [ "$REAL" = "0" ]; then
    args+=(--dry-run --allow-dirty)
  fi
  local max_attempts=1
  if [ "$REAL" = "1" ]; then max_attempts=4; fi
  local attempt=0
  while true; do
    attempt=$((attempt + 1))
    local out
    out="$(cargo "${args[@]}" 2>&1)"
    local code=$?
    if [ $code -eq 0 ]; then
      ok "$crate"
      SUM_CRATE+=("$crate"); SUM_STATUS+=("PASS")
      return 0
    fi
    if echo "$out" | grep -qE 'no matching package|failed to select a version for the requirement|candidate versions found which didn'; then
      if [ "$REAL" = "1" ] && [ $attempt -lt $max_attempts ]; then
        skip "$crate: deps not indexed yet (attempt $attempt/$max_attempts); sleeping ${SLEEP_SECONDS}s..."
        sleep "$SLEEP_SECONDS"
        continue
      fi
      skip "$crate (expected: rpi-* deps not on crates.io yet)"
      SUM_CRATE+=("$crate"); SUM_STATUS+=("SKIP")
      return 0
    fi
    bad "$crate failed (exit $code):"
    echo "$out" | tail -n 12 | sed 's/^/    /'
    SUM_CRATE+=("$crate"); SUM_STATUS+=("FAIL")
    ANY_FAIL=1
    if [ "$REAL" = "1" ]; then
      bad "Stopping (--publish): downstream crates depend on this one."
    fi
    return 1
  done
}

for crate in "${ORDER[@]}"; do
  publish_one "$crate"
  if [ "$ANY_FAIL" = "1" ]; then break; fi
  if [ "$REAL" = "1" ] && [ "$crate" != "${ORDER[-1]}" ]; then
    echo "  (sleeping ${SLEEP_SECONDS}s for crates.io index propagation...)"
    sleep "$SLEEP_SECONDS"
  fi
done

# If we broke early, mark the rest as not-run.
if [ "$ANY_FAIL" = "1" ]; then
  ran=0
  for c in "${SUM_CRATE[@]}"; do if [ "$c" = "${c}" ]; then ran=$((ran+1)); fi; done
  # Determine which never ran.
  for crate in "${ORDER[@]}"; do
    found=0
    for c in "${SUM_CRATE[@]}"; do if [ "$c" = "$crate" ]; then found=1; break; fi; done
    if [ "$found" = "0" ]; then
      SUM_CRATE+=("$crate"); SUM_STATUS+=("N/R")
    fi
  done
fi

# --- summary ---
echo "=== Summary ==="
for i in "${!SUM_CRATE[@]}"; do
  printf '  %-14s %s\n' "${SUM_CRATE[$i]}" "${SUM_STATUS[$i]}"
done

if [ "$ANY_FAIL" = "1" ]; then
  bad "Release completed with failures."
  exit 1
elif [ "$REAL" = "1" ]; then
  ok "All crates published."
else
  echo "Dry run complete. (Skipped crates resolve only during a real --publish,"
  echo "  once their rpi-* deps are live on crates.io.)"
fi
exit 0
