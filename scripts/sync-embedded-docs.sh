#!/bin/sh
# Refresh the documentation snapshot that is compiled into the `rpi` binary.
#
# crates/pi-cli/src/docs_tool.rs pulls these files in with include_str!, so they
# must be byte-copies of the repository documents -- otherwise `rpi` ships a
# `docs` tool whose output disagrees with the repository, and the published
# crate tarball cannot be regenerated from the source tree.
#
# CI runs this script and fails on any resulting diff, so drift is caught at the
# point it is introduced rather than at release time.

set -eu

DEST="crates/pi-cli/embedded-docs"

# source -> destination file name
FILES="README.md:README.md \
docs/agent-project.md:agent-project.md \
docs/architecture.md:architecture.md \
docs/extension-authoring.md:extension-authoring.md \
docs/rust-debugging.md:rust-debugging.md \
docs/user-guide.md:user-guide.md"

for entry in $FILES; do
  src="${entry%%:*}"
  name="${entry##*:}"
  [ -f "$src" ] || { echo "missing source document: $src" >&2; exit 1; }
  cp "$src" "${DEST}/${name}"
done

echo "synced ${DEST} from the repository documents"
