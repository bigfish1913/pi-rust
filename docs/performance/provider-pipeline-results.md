# Provider and plugin performance evidence

The five changes are implemented as separate review commits, with benchmark and
validation follow-ups. Package versions are unchanged. Rust API changes need the
maintainer's breaking-release process; the C ABI uses optional trailing slots.

These measurements describe the original series before rebasing onto upstream
`7a272ec` and the subsequent terminal-ordering and DLL-test corrections. The
original measured commits are retained on the fork's
`bench/provider-series-measured` branch. Current branch verification is reported
in each PR; historical timings below are not measurements of the rebased heads.

## Reproduction and environment

The benchmark-only baseline is `0a035bd` (`bench/provider-baseline`), directly on
`534d316`. It includes the corrected runner and exactly the same pipeline fixture
and runner used for all measurements here. Snapshot and compact worktrees used
`4cb285c` and `88d2a39`, with those same two benchmark files copied in; final pipeline
measurements used `3796956`. Clean the measured workspace packages when sharing a
Cargo target directory across worktrees: Cargo can otherwise reuse a stale binary
when checkout timestamps precede its build.

Linux x86_64, Intel Xeon E5-2680 v4 @ 2.40 GHz, 28 logical CPUs; Rust 1.95.0,
LLVM 22.1.2. Release profile: thin LTO, 16 codegen units. The fixture uses two host
runtime workers. Three fresh-process repetitions per pipeline workload, covering
4/32/128 KiB, 64/1024-byte chunks, 4 KiB one-byte stress, 1/8/32 streams,
1 ms delayed consumers, callback-free SDK requests and result-only host consumers.
The conversion-only revision uses the three representative quick workloads.

This is a shared machine; compilation and other processes ran during some samples.
Elapsed times are diagnostic, not controlled speedup estimates. Allocator counters
also add overhead. Linux process CPU is rounded to 10 ms by `/usr/bin/time`;
zero means below that resolution. No network, account secrets, or live provider
was used for these performance measurements. Loopback HTTP reuse is covered by
correctness tests separately.

```bash
python3 scripts/bench-provider.py --repeats 3
# Explicitly select the benchmark feature (also the runner's default):
python3 scripts/bench-provider.py --features provider-performance
cargo test --release -p rpi-extensions --lib pending_population_benchmark -- --ignored --nocapture --test-threads=1
cargo test --release -p rpi-plugin-sdk --features provider benchmark_context_resources -- --ignored --nocapture
```

## Snapshot conversion and compact streaming

32 KiB output, 64-byte deltas, one immediate consumer. Values are medians;
elapsed time additionally gives min–max across three runs. Allocation bytes are
cumulative, not resident memory. Peak live allocations include the entire process.

| Revision | Elapsed ms (range) | Allocations | Allocated MiB | Peak live MiB | JSON bytes | Process CPU ms | Peak RSS MiB |
|---|---:|---:|---:|---:|---:|---:|---:|
| Baseline | 102.41 (83.24–104.67) | 103,153 | 100.64 | 9.01 | 8,687,082 | 90 | 16.55 |
| PR 1 | 37.30 (37.20–46.43) | 66,188 | 88.77 | 8.91 | 8,687,082 | 50 | 14.95 |
| PR 2 | 15.22 (15.06–15.24) | 17,654 | 19.10 | 8.91 | 124,766 | 20 | 17.46 |
| Bounded series | 8.79 (8.29–8.82) | 17,642 | 18.81 | 3.67 | 124,766 | 10 | 9.82 |

PR 1 removes the targeted temporary work without changing transmitted bytes.
PR 2 reduces this fixture's transmitted bytes from 8,687,082 to 124,766 (98.6%).
The mapper-equivalence tests compare emitted events, accumulated messages and
aborted results after every prefix, including Unicode, empty deltas, metadata
changes, authoritative replacements and block/index changes. Full snapshots
remain authoritative for the first text delta and unsupported transitions.

Callback-free SDK requests on the same workload show the serialization work that
PR 1 skips; PR 2 also retains snapshots through their existing `Arc`s:

| Revision | Elapsed ms (range) | Allocations | Allocated MiB |
|---|---:|---:|---:|
| Baseline | 26.22 (24.07–27.77) | 46,598 | 28.99 |
| PR 1 | 11.30 (8.12–11.39) | 5,418 | 17.29 |
| PR 2 | 8.40 (8.21–9.74) | 3,363 | 9.15 |
| Bounded series | 4.04 (4.02–4.58) | 3,354 | 8.93 |

This does **not** establish linear CPU complexity: producer snapshots and the
full-message equality/prefix checks still perform cumulative work.

## Bounded queues

Across all final pipeline samples, the bridge's maximum observed occupancy was
64 events and 303,275 accounted JSON bytes per queue. The pure-text compact
fixture hits the count limit first. Capacity-one and oversized-frame tests cover
the byte-limit path separately. Typed-queue tests assert count/byte limits,
exclusive oversized admission, lossless order, result-only drain, first-terminal
wins, terminal publication under saturation, and child-token cancellation.

The end-to-end saturation test holds the consumer at one event, fills the FFI
bridge, then cancels or drops the consumer. It checks that the blocked delta never
commits, previously applied usage does commit, every plugin allocation is freed,
and the blocked emitter returns. Final events/results use independent capacity.

These limits cover queued payload, not RSS. Producer/mapper state, one pending
frame per stage, final messages, allocator overhead and consumer-held snapshots
remain outside the budgets. Already-built plugins gain host-bridge limits; their
own generation queues need a rebuild against the new asynchronous `push` API.

## Pending tools

The candidate's unchanged legacy driver is compared with its new wake driver;
these tool measurements are not from a rebuilt `534d316`. Three untimed repetitions
and three `/usr/bin/time` repetitions are in the raw file. The table below uses
the three timed repetitions, including an intentional 100 ms idle interval.

| Pending tools | Legacy / wake polls (median) | Legacy / wake idle blocking workers | Legacy / wake cancellation ms (median) |
|---|---:|---:|---:|
| 1 | 97 / 1 | 1 / 0 | 0.762 / 0.208 |
| 64 | 6,394 / 64 | 64 / 0 | 1.181 / 3.531 |
| 256 | 26,681 / 256 | 256 / 0 | 1.545 / 8.514 |

Wake mode removes periodic idle polling and releases all blocking workers while
pending. Its cleanup has more offload overhead: at 256 tools the timed cancellation
range was 2.838–12.166 ms, versus 1.293–2.513 ms for legacy. Do not claim a blanket
cancellation speedup. All cases had zero active blocking calls after cancellation.
Whole-process CPU (all six workloads together) was 0.36–0.41 s and peak RSS
8,840–10,296 KiB. Individual workload elapsed/startup times and blocking peaks are
in the raw output.

## Provider resource reuse

Three release runs exercised the real context driver, client lookup/construction,
and a deterministic account echo without network I/O. Each scenario makes 200
requests. Scoped resources built 200 runtimes and 200 clients; shared resources
built one of each, both sequentially and across four concurrent accounts. Every
request checked its own account result. Distinct registration instances and actual
HTTP connection reuse with different Authorization headers have separate tests.

| Scenario | Scoped / shared elapsed ms (median) | Scoped / shared range ms |
|---|---:|---:|
| Sequential | 23.034 / 15.873 | 23.028–25.879 / 15.280–16.629 |
| Four accounts concurrently | 7.475 / 7.092 | 7.274–7.767 / 6.531–7.379 |

The concurrent ranges overlap. Whole-process CPU covering all four scenarios was
0.09–0.10 s, with peak RSS 6,236–6,520 KiB; these totals cannot be assigned to either
mode. Allocation instrumentation covers the pipeline fixtures, not these resource
or tool fixtures. The reuse claims rely on creation counts and connection tests.

## Verification and limits

Passed the requested format check, warnings-as-errors workspace/all-target check,
warnings-as-errors workspace tests (Cargo reported 1,952 passed and six ignored),
provider-feature SDK tests, and the no-default-features SDK check. Targeted bridge
checks additionally exercise the SDK-to-host compaction fixture and bounded FFI
queue. The final transport-error fix has its own SDK feature-suite verification.
Windows ABI/DLL-lifecycle jobs were added; native Windows and live-provider runs
were not available in this Linux session.

The workspace run includes 38 offline harness end-to-end tests and four
remote-client smoke tests against a local fake server. Some optional tests return
early when their prerequisites are absent, so Cargo's passing total does not mean
every external integration ran. The real-server round trip was not configured,
and all three voice-plugin smoke tests skipped because `rpi_voice` was absent.
The two plugin-stub smoke tests were explicitly rerun after building its shared
library and passed through the production loader, tool and provider bridges:

```bash
RUSTFLAGS="-D warnings" cargo build -p plugin-stub --locked
RUSTFLAGS="-D warnings" cargo test -p rpi-extensions --features provider-performance --test plugin_stub_smoke --test voice_ptt_smoke --locked -- --nocapture
```

An initial isolated smoke build without `provider-performance` failed under
`-D warnings` because the unchanged `rpi-ai/src/strict_schema.rs` has unused helpers
when HTTP providers are disabled. The provider-enabled smoke build and the
requested workspace checks pass; the minimal-feature warning remains unresolved.
The Windows DLL job explicitly enables `rpi-ai/providers` to exercise its tests
with warnings denied. The DLL fixture now joins a real worker and, on Windows,
waits for the module to unload independently of its finish-callback marker.

Successful lifecycle finalization joins workers before library unload. A failed
shutdown, uncooperative foreign call, or failure to create a cleanup thread must
retain its library/context rather than risk executing unmapped code. No timeout
unloads code. Agent subscriptions and UI buses are outside this series.

Raw distributions:

- [Baseline pipeline](results/provider-baseline.jsonl)
- [Conversion-only pipeline](results/provider-snapshots.jsonl)
- [Compact pipeline](results/provider-compact.jsonl)
- [Bounded pipeline](results/provider-bounded.jsonl)
- [Pending tools](results/pending-tools.txt)
- [Resource reuse](results/provider-resources.txt)
