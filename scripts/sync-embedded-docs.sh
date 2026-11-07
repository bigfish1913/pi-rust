#!/bin/sh
# Refresh the documentation snapshot that is compiled into the `rpi` binary.
#
# crates/rpi-cli/src/docs_tool.rs pulls these files in with include_str!, so they
# must be byte-copies of the dedicated docs/rpi-tool/ sources. Keep these
# sources separate from docs/ ordinary user/developer documentation: the latter
# may contain release notes, website-oriented pages, or implementation notes
# that should not become part of the model-facing docs tool.
# The changelog is bundled separately for TUI startup notices and /changelog;
# it is not registered as a model-facing docs topic.
#
# CI runs this script and fails on any resulting diff, so drift is caught at the
# point it is introduced rather than at release time.

set -eu

DEST="crates/rpi-cli/embedded-docs"

# source -> destination file name
FILES="docs/rpi-tool/README.md:README.md \
docs/rpi-tool/agent-project.md:agent-project.md \
docs/rpi-tool/architecture.md:architecture.md \
docs/rpi-tool/extension-authoring.md:extension-authoring.md \
docs/rpi-tool/rust-debugging.md:rust-debugging.md \
docs/rpi-tool/user-guide.md:user-guide.md \
CHANGELOG.md:changelog.md"

for entry in $FILES; do
  src="${entry%%:*}"
  name="${entry##*:}"
  [ -f "$src" ] || { echo "missing source document: $src" >&2; exit 1; }
  cp "$src" "${DEST}/${name}"
done

echo "synced ${DEST} from the repository documents"
