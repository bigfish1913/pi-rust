#!/usr/bin/env bash
# Run from the repository root; used by tui.tape.
set -euo pipefail
demo_dir=$(mktemp -d)
trap 'rm -f -- "$demo_dir/settings.json"; rmdir -- "$demo_dir" 2>/dev/null || true' EXIT
# Native Windows binaries need a Windows path rather than Git Bash's /tmp path.
if command -v cygpath >/dev/null 2>&1; then
  export RPI_CODING_AGENT_DIR="$(cygpath -m "$demo_dir")"
else
  export RPI_CODING_AGENT_DIR="$demo_dir"
fi
export RPI_OFFLINE=1 RPI_VOICE_AUTO_TTS=off
clear
./target/debug/rpi --provider openai --api-key local-demo-only \
  --no-session --no-extensions --no-context-files --no-skills --no-prompt-templates
