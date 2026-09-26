#!/usr/bin/env bash
# scripts/metrics.sh — one screen of the numbers that tell you whether exposure
# is working, from sources that already exist (no analytics account required).
#
#   task metrics          # or: bash scripts/metrics.sh
#
# Why this instead of a web-analytics snippet: the site has no traffic yet, so
# the measurable signal right now is upstream — crates.io downloads, release
# asset downloads and repo traffic. Those are the endpoints a channel actually
# moves, and they are readable today. Add site analytics later, when there is a
# site traffic number worth splitting by referrer.
set -uo pipefail

REPO="bigfish1913/pi-rust"
CRATES=(
  rpi-telemetry rpi-ai rpi-agent rpi-plugin-sdk rpi-extensions
  rpi-tools rpi-harness rpi-tui rpi-cli
)
# crates.io returns 403 without a User-Agent, so every request carries one.
# `set --` splits the "<all> <recent>" pair; the defaults keep a row honest
# (0, not empty) when the API is unreachable.
UA="rpi-metrics (+https://github.com/${REPO})"

hr() { printf '%s\n' "────────────────────────────────────────────────────────────"; }

hr
echo "repo  https://github.com/${REPO}"
if command -v gh >/dev/null 2>&1; then
  gh api "repos/${REPO}" --jq '"  stars \(.stargazers_count)   forks \(.forks_count)   watchers \(.subscribers_count)   open issues \(.open_issues_count)"' 2>/dev/null \
    || echo "  (gh api failed — is gh authenticated?)"
else
  echo "  (gh not installed; skipping repo + traffic + release counts)"
fi

hr
echo "crates.io downloads (all = since publish, recent = last 90 days)"
total_all=0
total_recent=0
for c in "${CRATES[@]}"; do
  all=0
  recent=0
  out=$(curl -fsS --max-time 20 -A "${UA}" "https://crates.io/api/v1/crates/${c}" 2>/dev/null \
    | python -c 'import sys,json;d=json.load(sys.stdin)["crate"];print(d.get("downloads",0), d.get("recent_downloads") or 0)' 2>/dev/null)
  if [ -n "${out}" ]; then
    # shellcheck disable=SC2086
    set -- ${out}
    all=$1
    recent=${2:-0}
  fi
  printf '  %-18s %7s  %7s\n' "$c" "${all}" "${recent}"
  total_all=$((total_all + all))
  total_recent=$((total_recent + recent))
done
printf '  %-18s %7s  %7s\n' "TOTAL" "${total_all}" "${total_recent}"

hr
echo "GitHub release asset downloads (per platform)"
if command -v gh >/dev/null 2>&1; then
  gh api "repos/${REPO}/releases?per_page=5" \
    --jq '.[] | "  \(.tag_name)", (.assets[] | "    \(.download_count)\t\(.name)")' 2>/dev/null \
    || echo "  (gh api failed)"
fi

hr
echo "repo traffic, last 14 days (unique views / uniques, clones / uniques)"
if command -v gh >/dev/null 2>&1; then
  gh api "repos/${REPO}/traffic/views" \
    --jq '"  views \(.count) / \(.uniques)"' 2>/dev/null || echo "  views: (needs push access)"
  gh api "repos/${REPO}/traffic/clones" \
    --jq '"  clones \(.count) / \(.uniques)"' 2>/dev/null || echo "  clones: (needs push access)"
fi
hr

# Baseline for comparison. Fill a new row when you publish or submit something,
# so a channel's effect is attributable instead of remembered.
#
#   2026-09-26  site compare.html shipped           views __ / __   crates __
#   2026-09-26  awesome-cli-coding-agents PR        views __ / __   crates __
echo "Record a row per channel (see the comment at the end of this file)."
