# Performance vs. native TypeScript `pi`

How `rpi` is benchmarked against the upstream TypeScript coding agent, what the
numbers are, and — just as important — what is **not** measured.

Last run: **2026-09-30**, `rpi 0.3.6` vs `pi 0.87.1`. 3 warmup runs discarded,
11 measured runs, medians reported.

## What is measured

Three things, all from the same harness (`scripts/bench-vs-pi.mjs`):

1. **`--version` wall time.** Process spawn → exit. The floor cost of starting
   the tool at all.
2. **Cold start.** From *before* `spawn()` → the parent parsing the answer to a
   real `get_state` request on the JSONL command channel. It includes process creation,
   arg parsing, config, model catalog, tool registry, extension discovery/loading,
   and harness/session construction; it excludes process teardown (the child is
   killed right after) and any LLM or tool work. This is **native pi's own
   definition** of startup completion (see its
   `scripts/profile-coding-agent-node.mjs`), not a metric chosen to flatter rpi.
3. **RSS at ready.** Resident set size of the live process once it has answered
   `get_state`, read in a separate run.

## Fairness rules

Both tools get identical treatment:

| Rule | Implementation | Why |
| --- | --- | --- |
| Same endpoint | Both answer `get_state` over the same JSONL command channel | Compare "agent usable", not "args parsed" |
| Cold and isolated | Fresh empty config dir per run: pi via `PI_CODING_AGENT_DIR`, **rpi via `RPI_CODING_AGENT_DIR`** | No existing sessions, extensions, or provider config. The two tools use different variable names — see trap 3 |
| Offline | pi via `PI_OFFLINE=1`, rpi via `RPI_OFFLINE=1` | Measure runtime cost, not network |
| Placeholder credential | A dummy `ANTHROPIC_API_KEY` for both | rpi refuses to build a session in an empty dir with no provider credential; offline, it is never sent |
| Neutral cwd | A fresh empty temp directory for both | Neither scans this repository on startup |
| No sampling in the timing window | RSS is measured in its own phase | See trap 1 |
| No shell in the path | Resolve npm's `.bin/pi` shim to the JS entrypoint and run under `node` | See trap 2 |

**rpi does not read the `PI_*` variables.** It uses `RPI_OFFLINE` and
`RPI_CODING_AGENT_DIR`. Setting only `PI_OFFLINE` / `PI_CODING_AGENT_DIR` leaves
rpi pointed at the real `~/.rpi/agent`, which silently loads the machine's global
plugin set and destroys the comparison. This was the single largest source of
error in an earlier version of this document.

## Results

Environment: Windows 11 (build 26200) / x86_64, rustc 1.97.1 `--release`,
Node v25.9.0.

| Metric | rpi (Rust, 0.3.6) | pi (TypeScript, 0.87.1) | Difference |
| --- | --: | --: | --: |
| `--version` | **17.5 ms** | 169.5 ms | **9.7× faster** |
| Cold start | **17.7 ms** | 189.5 ms | **10.7× faster** |
| RSS at ready | **12.0 MiB** | 91.5 MiB | **7.6× smaller** |
| Install footprint | **21.8 MiB** (one binary) | ~385 MiB (npm tree) | **~18×** |
| Install footprint incl. runtime | **21.8 MiB** | ~476 MiB (+ ~91 MiB Node) | **~22×** |

### Raw per-run numbers

Timing, 11 runs, in milliseconds:

```text
rpi  --version  17.8 19.4 17.4 16.9 19.8 16.8 17.5 18.9 17.2 17.0 19.5   median 17.5
rpi  cold start 18.3 22.2 16.6 17.7 17.9 17.0 17.7 18.6 16.6 17.3 17.5   median 17.7
pi   --version 170.7 170.6 171.8 168.7 169.5 168.9 169.2 171.9 166.7 172.0 167.0   median 169.5
pi   cold start 188.4 190.4 189.0 189.9 189.5 190.8 189.1 189.7 190.5 189.3 187.4   median 189.5
```

Memory at ready, in MiB:

```text
rpi  12.0 12.0 12.0
pi   91.5 91.3 91.6
```

Binary size is `rpi.exe` from a `--release` build (`lto = "thin"`,
`codegen-units = 16`, `strip = "symbols"`): 22,870,528 bytes. Install footprint
for pi is the size of the `node_modules` tree produced by
`npm install --prefix .bench @earendil-works/pi-coding-agent` (403,645,836
bytes), plus the Node binary itself (95,618,048 bytes).

## Where the time goes

- **pi:** `--version` is 169.5 ms; a fully-initialised agent is 189.5 ms. So
  ~20 ms is pi's own agent initialisation on top of Node boot; the rest is Node.
- **rpi:** `--version` is 17.5 ms and cold start is 17.7 ms — effectively
  equal. Config, session, harness and the six built-in tools add nothing over
  process creation.

Two earlier runs were misleading: one reported cold start at 101 ms (the machine's
global plugin set — trap 3), and the next reported 10.1 ms (a stopwatch started
after `spawn()` — trap 4).

## Measurement traps (why these numbers are trustworthy)

Four harness bugs were found and fixed. Each one moved the numbers materially,
which is why they are documented rather than deleted:

1. **RSS sampling inside the timing window.** Spawning `tasklist` every 10 ms on
   Windows to read the other process's memory loaded the CPU and slowed both
   tools — and slowed the faster one more, flattening the real difference to
   ~220 ms for both. Memory is now measured in a separate phase; timing runs do
   no sampling.
2. **npm's shim adds a shell hop.** npm installs `pi` as `.bin/pi`, which needs
   `cmd.exe` on Windows; running it directly would charge `cmd.exe` startup to
   pi. The harness now resolves the shim to its JS entrypoint and runs `node`
   against it.
3. **rpi was not actually isolated.** The harness set `PI_OFFLINE` and
   `PI_CODING_AGENT_DIR`, which native pi honours but rpi does not — rpi reads
   `RPI_OFFLINE` / `RPI_CODING_AGENT_DIR`. So while pi ran against a fresh empty
   config dir, rpi read the real `~/.rpi/agent` and loaded the **25 plugin DLLs**
   installed on the test machine. Evidence: the `get_state` reply listed **31
   active tools**; correctly isolated it lists **6 built-ins**. Fixing the
   variables dropped cold start from 101 ms to 10 ms. `runEnvironment()` now sets
   both tools' variable names, and the placeholder key lets rpi resolve a default
   model in the empty dir.
4. **The cold-start stopwatch started after `spawn()`.** `measureColdStart` called `spawn()`
   and only then read `performance.now()`, while `measureVersion` timed from
   before `spawnSync`. Node's `spawn()` does a synchronous chunk of process
   creation — measured at ~7.3 ms for `rpi.exe` on Windows — so the cold-start number
   excluded it, reporting 10.1 ms where the true spawn→cold-start was 17.7 ms.
   The clock now starts before `spawn()`. This is also the precise definition of
   the metric: **before `spawn()` → the `get_state` response on stdout**,
   excluding process teardown and any LLM/tool work.

## What is NOT measured

Do not read this table as a claim about any of the following:

- **LLM latency or tool-loop throughput.** Those depend on the model and network,
  not on the runtime.
- **Memory growth over a long session.** Only the "just ready" RSS is measured.
- **TUI frame cost.** Both projects cache transcript rendering; that needs its
  own benchmark.
- **Any tool other than native pi.** No numbers exist here for Claude Code,
  Codex CLI, or aider, and none should be implied.

Absolute millisecond values are machine-specific. Re-run on your own hardware and
compare **ratios**, not the values quoted here.

## Reproduce

```bash
# rpi
cargo build -p rpi-cli --release

# native pi, into an isolated local prefix
npm install --prefix .bench @earendil-works/pi-coding-agent

# compare
node scripts/bench-vs-pi.mjs --pi .bench/node_modules/.bin/pi --runs 11 --warmup 3 \
  --json .bench/results.json
```

Standalone rpi numbers (binary size, `--version` timing) come from
`sh scripts/bench.sh`.
