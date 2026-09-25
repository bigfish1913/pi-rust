#!/bin/sh
# Reproduce the numbers quoted in README.md "Performance".
#
#   sh scripts/bench.sh
#
# Reports the release binary size and the median wall-clock time of a short
# no-network command (`rpi --version`). Timing uses python3 when available,
# because `date` has no sub-second precision that is portable across Linux,
# macOS and MSYS.

set -eu

RUNS="${RUNS:-7}"
BIN_NAME="rpi"

log() { printf '%s\n' "$*"; }

log "== environment =="
log "os:      $(uname -s) $(uname -m)"
if command -v rustc >/dev/null 2>&1; then
  log "rustc:   $(rustc --version)"
fi
log "profile: release (lto=thin, codegen-units=16, strip=symbols)"
log ""

log "== build =="
cargo build -p rpi-cli --release --locked

target_dir="$(cargo metadata --no-deps --format-version 1 --locked \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | head -n 1)"
[ -n "${target_dir}" ] || { log "could not resolve target_directory"; exit 1; }

bin="${target_dir}/release/${BIN_NAME}"
[ -f "${bin}" ] || bin="${bin}.exe"
[ -f "${bin}" ] || { log "release binary not found under ${target_dir}/release"; exit 1; }

log "== binary size =="
bytes="$(wc -c < "${bin}" | tr -d ' ')"
log "path: ${bin}"
log "size: ${bytes} bytes ($(awk "BEGIN{printf \"%.1f\", ${bytes}/1048576}") MiB)"
log ""

log "== startup (${RUNS} runs of '${BIN_NAME} --version') =="
if command -v python3 >/dev/null 2>&1; then
  python3 - "${bin}" "${RUNS}" <<'PY'
import statistics, subprocess, sys, time

binary, runs = sys.argv[1], int(sys.argv[2])
samples = []
for _ in range(runs):
    start = time.perf_counter()
    subprocess.run([binary, "--version"], stdout=subprocess.DEVNULL, check=True)
    samples.append((time.perf_counter() - start) * 1000)

print("runs (ms): " + ", ".join(f"{s:.1f}" for s in samples))
print(f"median:    {statistics.median(samples):.1f} ms")
print(f"min:       {min(samples):.1f} ms")
print(f"max:       {max(samples):.1f} ms")
PY
else
  log "python3 not found; skipping the timing measurement"
fi
