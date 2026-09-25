# rpi vs native Pi — performance comparison

A measured, reproducible comparison between `rpi` (this project, Rust) and
native `pi` (the original TypeScript implementation, published as
`@earendil-works/pi-coding-agent`).

Reproduce with:

```bash
# 1. get the comparison target
npm install --prefix .bench @earendil-works/pi-coding-agent

# 2. measure both
cargo build -p rpi-cli --release
node scripts/bench-vs-pi.mjs --pi .bench/node_modules/.bin/pi --runs 9 --warmup 2
```

## Result

| Metric | rpi 0.3.0 (Rust) | pi 0.87.1 (TypeScript) | Difference |
| ------ | ---------------: | ---------------------: | ---------- |
| `--version` (median) | **17.9 ms** | 172.9 ms | **9.7× faster** |
| RPC ready (median) | **103.6 ms** | 176.4 ms | **1.7× faster** |
| RSS at ready | **18.6 MiB** | 91.6 MiB | **4.9× smaller** |
| Install footprint | **23.9 MiB** (one binary) | ~513 MiB | **~21× smaller** |

"RPC ready" is time from process spawn to the answer to a real
`{"type":"get_state"}` request on the JSONL RPC channel — i.e. from launch to a
usable agent, not just a parsed argument list.

These numbers were measured on the 0.3.0 build. 0.3.1 is a patch release with no
functional change (it repairs the build on `main` and how the crates present
on crates.io/docs.rs), so the results apply to it unchanged and were not
re-measured.

## Environment

| | |
| --- | --- |
| Machine | Windows 11 (26200), x86_64 |
| Shell | Git Bash (MINGW64) |
| Rust | rustc 1.97.1, `--release` (`lto = "thin"`, `strip = "symbols"`) |
| Node | v25.9.0 |
| rpi | 0.3.0 (`target/release/rpi.exe`) |
| pi | 0.87.1 (npm, run as `node dist/bundle/cli.js`) |
| Runs | 2 warmup + 9 measured, medians reported |

## Methodology

The comparison is only meaningful if both tools do the same work under the same
conditions, so the harness enforces it:

- **Same endpoint.** Both are asked for `get_state` over the same JSONL RPC
  protocol. This endpoint is not invented here — native pi's own
  `scripts/profile-coding-agent-node.mjs` defines "RPC startup" as exactly this
  round trip, so rpi is being held to the other project's own definition.
- **Cold and isolated.** A fresh, empty `PI_CODING_AGENT_DIR` per run, so no
  pre-existing session, extension, provider config or cache is loaded. Both
  tools honour this variable, and rpi deliberately mirrors native pi's
  `PI_OFFLINE` behaviour so the offline settings cannot favour either side.
- **Offline.** `PI_OFFLINE=1`, `PI_SKIP_VERSION_CHECK=1`, and provider
  credentials cleared — neither does network work, so the numbers measure the
  runtime rather than the network.
- **Neutral working directory.** Both run in a throwaway empty directory, so
  neither scans this repository (thousands of files, a `.rpi/` directory) on
  startup.
- **No sampling inside the timing window.** Reading another process's RSS on
  Windows means spawning `tasklist`, which costs tens of milliseconds. Doing that
  on a timer measurably inflated the first version of these numbers (both tools
  were dragged to ~220 ms). Memory is now measured in a separate phase, after the
  agent is ready, and the timing runs do no sampling at all.
- **No shell in the path.** npm installs `pi` behind a `.bin/pi` shim; on Windows
  that shim needs `cmd.exe`, which would add shell startup to pi's time. The
  harness resolves the shim to its JavaScript entrypoint and runs it under `node`
  directly, so what is measured is Node startup, not `cmd.exe`.

## Raw runs

```
rpi  --version (ms)  19.8 17.8 20.0 17.8 17.8 19.4 17.7 17.9 19.6   median 17.9
rpi  rpc ready (ms) 111.2 103.6 105.1 100.7 100.9 106.4 109.0 103.5 103.6   median 103.6
rpi  rss (MiB)       18.6 18.6 18.6

pi   --version (ms) 172.8 171.1 181.2 173.6 172.9 175.0 173.2 171.6 170.6   median 172.9
pi   rpc ready (ms) 174.7 174.0 172.7 181.1 174.6 187.4 176.4 176.9 180.4   median 176.4
pi   rss (MiB)       92.3 91.6 91.5
```

## What the numbers actually say

The interesting part is not the ratio, it is where each tool spends its time.

**pi's cost is almost entirely Node.js startup.** `--version` takes 172.9 ms and
the full agent takes 176.4 ms — initialising the agent adds ~3.5 ms on top of
booting the runtime. There is very little for the pi project to optimise here;
the floor is the runtime.

**rpi's cost is mostly its own initialisation.** Process start is 17.9 ms, but
reaching a usable agent takes 103.6 ms. So ~86 ms — 83% of rpi's startup — is
runtime initialisation (config, session store, tool and resource registry),
not process or loader overhead.

That is a concrete, actionable finding: **further startup wins for rpi have to
come from lazy or parallel initialisation of the agent, not from making the
binary smaller or the process spawn faster.** A 2× reduction in that 86 ms would
put rpi's time-to-usable-agent near 60 ms and widen the gap to ~3×. See
[ROADMAP.md](../ROADMAP.md).

## Install footprint

| | Size | Notes |
| --- | --- | --- |
| rpi | **23.9 MiB** | one statically-linked binary; no runtime prerequisite |
| pi install tree | 411 MiB | `npm install` of the package and its dependencies |
| Node.js runtime | 101.9 MiB | required before pi can run at all |
| **pi total** | **~513 MiB** | |

`node_modules` size depends on platform, deduplication and whether the
installation is shared with other packages, so treat 411 MiB as "this install on
this machine" rather than a fixed property of pi. The Node runtime prerequisite
is not optional, though.

## What this does NOT measure

Stated plainly, because a benchmark table invites over-reading:

- **No LLM work.** Everything here is startup and idle memory. Time-to-first-token,
  tokens/sec, and prompt-cache behaviour are properties of the provider, and
  neither tool is measured on them.
- **No tool-loop throughput.** How fast each runs a batch of tool calls, or
  handles a long agentic session, is not measured.
- **No large-session behaviour.** Both start against an empty session. Memory
  growth over a long conversation, and compaction cost, are not covered.
- **No TUI rendering.** Only RPC mode is measured; interactive frame cost is a
  separate question.
- **One machine, one version pair.** These are absolute numbers from one box.
  The ratios should be re-measured on your own hardware before being quoted —
  `scripts/bench-vs-pi.mjs` exists so that takes one command.
