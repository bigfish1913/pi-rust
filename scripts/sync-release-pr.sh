#!/usr/bin/env bash
# Finish the bot's draft PR before marking it ready for review/CI.
set -euo pipefail
# On a no-op retry release-plz can omit its PR output. Recover the existing PR
# returned by the draft step so it cannot be stranded in draft after success.
release_pr=$(jq -c --argjson existing "${EXISTING_RELEASE_PR_JSON:-null}" \
  'if .head_branch? and .number? then . else $existing end' <<<"${RELEASE_PR_JSON:-null}")
branch=$(jq -r '.head_branch // empty' <<<"$release_pr")
number=$(jq -r '.number // empty' <<<"$release_pr")
[[ -n "$branch" && -n "$number" ]] || exit 0
[[ "$branch" == release-plz-* ]] || { echo 'unexpected release branch' >&2; exit 1; }
git fetch origin "$branch"
git switch --detach FETCH_HEAD
cargo update --workspace
python3 - <<'PY'
from pathlib import Path
import re
import tomllib
version = tomllib.loads(Path('Cargo.toml').read_text(encoding='utf-8'))['workspace']['package']['version']
for name in ('README.md', 'docs/rpi-tool/README.md'):
    path = Path(name)
    text = re.sub(r'(Current release[^\n]*?)\b\d+\.\d+\.\d+',
                  lambda match: match[1] + version, path.read_text(encoding='utf-8'))
    path.write_text(text, encoding='utf-8', newline='\n')
PY
sh scripts/sync-embedded-docs.sh
rustc --edition=2021 release-helper.rs -o .git/rpi-release-helper
.git/rpi-release-helper validate "$(python3 -c "import tomllib; print(tomllib.load(open('Cargo.toml', 'rb'))['workspace']['package']['version'])")" --allow-dirty
git config user.name 'github-actions[bot]'
git config user.email '41898282+github-actions[bot]@users.noreply.github.com'
git add Cargo.toml Cargo.lock CHANGELOG.md README.md docs/rpi-tool/README.md crates/rpi-cli/embedded-docs
if ! git diff --cached --quiet; then
  git commit -m 'chore(release): sync bundled release documentation'
  gh auth setup-git
  git push origin "HEAD:refs/heads/$branch"
fi
is_draft=$(gh pr view "$number" --json isDraft --jq .isDraft)
if [[ "$is_draft" == true ]]; then
  gh pr ready "$number"
fi
