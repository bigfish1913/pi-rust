#!/usr/bin/env bash
# scripts/demo-transcript.sh — regenerate demo/transcript.txt.
#
#   task demo-transcript        # or: bash scripts/demo-transcript.sh
#
# demo/transcript.txt is the README's offline demo. Every line under a `$` line
# is the **actual stdout** of that command, captured here — nothing is written by
# hand. That is the only thing that makes it worth quoting in the README: the
# claim is "this works with no API key and no network", and the transcript is the
# evidence rather than a restatement of it.
#
# Both commands use the `faux` provider, so the output is byte-identical on every
# run and the transcript can be regenerated and diffed in CI.
set -euo pipefail

cd "$(dirname "$0")/.."

out=demo/transcript.txt

{
  echo '$ cargo run -q -p minimal'
  cargo run -q -p minimal 2>/dev/null
  echo
  echo '$ cargo run -q -p tools-example'
  cargo run -q -p tools-example 2>/dev/null
} > "$out"

echo "wrote ${out} ($(wc -l < "$out") lines)"
