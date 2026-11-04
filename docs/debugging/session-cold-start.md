# JSONL historical-session cold start

For the repository-wide regression inventory and CI coverage, see
[Regression testing](../maintaining/testing.md).

A fresh native RPC process validates history before publishing `ready`. Limiting
`get_messages` to 40 messages does not limit the number of log records that must
be recovered. In particular, old histories may contain hundreds of thousands of
`assistant_frame` records even when the displayed transcript is small.

## Recovery implementation

The stable ranged-reader path recognizes append/clear frame metadata without
building the discarded frame JSON tree. It still validates the complete JSON
value, sequence, lane and permanent ID reservation. Commit records retain the
canonical decoder because their entry binding participates in recovery.

Unrecognized representations and ambiguous known duplicate fields fall back to
the canonical codec. Stale sequences always use full mutation comparison, not
byte equality: different whitespace or object-key order can represent the same
mutation. Corruption and torn-tail policy are unchanged.

Retained cold rows bind to the original reader generation and keep their own
SHA-256, length and identity checks. Old query plans remain immutable when a
clear retires matching rows. An auxiliary active-frame index prevents repeated
clears from walking all prior runs; it does not remove records from history or
release their IDs.

Resource-binding preflight still completes before executable extensions load.
Its private facts-only loader validates record identities and sequences, but
need not retain frame-read locations: its only output is metadata and session
values, not a writable/queryable storage handle. It must not be used for ordinary
restore, forks, detached snapshots or resource transitions.

The whole-file legacy digest guards an eventual progress-format upgrade. Modern
histories and facts-only preflight do not use that upgrade capability, so they
avoid computing an unused whole-file digest. Per-row integrity checks remain.
Legacy writable restore keeps the original digest and upgrade checks.

Set `RPI_SESSION_DIAGNOSTICS=1` on an **isolated** diagnostic process to obtain a
numeric `history_streaming` timing. The phase is emitted only on successful
completion; missing output is not proof of success. It excludes later harness,
companion-progress and Web hydration work. It contains no session IDs or bodies.

## Startup phase diagnostics

On an **isolated** diagnostic CLI process, set `RPI_STARTUP_DIAGNOSTICS=1`
(exactly `1`) to emit fixed-label spans to stderr. The default is off. Records
contain only `pid`, process-local `span`, fixed `phase`/`event` labels, and numeric
`at_ns`/`elapsed_ns`; they contain no session IDs, paths, arguments or error text.
The monotonic `at_ns` origin is the first enabled span, not process creation.
RPC stdout and ready semantics are unchanged.

The CLI phases distinguish cwd lookup, extension preparation/registration,
binding-facts preflight, model resolution, harness construction, writable restore,
metadata lookup, resource observation/inventory binding and RPC startup hooks.
`startup_to_harness` ends before the RPC lifecycle hooks and ready enqueue; use
external ready latency and `RPI_RPC_DIAGNOSTICS=1` for those boundaries.
`RPI_SESSION_DIAGNOSTICS=1` supplies the inner successful history-scan timings.

Match begin/end records by **both pid and span**. Spans nest: extension preparation
includes preflight, and harness construction includes writable restore. Do not
sum overlapping durations. `end` records a completed measured operation, not
HTTP/business success. Early errors, cancellation and unwinding leave
`incomplete`, never a fabricated successful end. A process kill may prevent any
terminal record, so an unmatched begin is also incomplete, not proof of a hang's
cause. Repeated metadata lookups are separate spans, including later calls to
the same instrumented helper.

These opt-in timings add stderr overhead and do not reconstruct past requests.
Native spans alone do not measure Web lifecycle-lock wait, host attachment,
HTTP transport or browser paint. Web request correlation and diagnostic rollout
must be handled separately; do not infer them from native pid/span values.

## Reproducible performance gate

Use `scripts/resource-cold-start.py` with a release candidate binary (and,
optionally, `--baseline` for paired AB/BA comparison). It generates only
synthetic histories, uses fresh homes/projects/processes for every observation,
blocks normal resource discovery, and never prompts a provider or invokes tools.
It records hashes, ready/message/state timings, peak memory and process cleanup.
The output directory must be new and outside the checkout.

```bash
python -B scripts/resource-cold-start.py \
  --baseline C:/bin/baseline-rpi.exe \
  --candidate C:/bin/candidate-rpi.exe \
  --output C:/bench/cold-start-new \
  --frames 701000 --entries 21733 --repeats 5 \
  --shapes legacy modern
```

The default candidate gate is median ready <= 2000 ms and maximum <= 3000 ms.
Every message query must also take <= 100 ms (`--max-get-messages-ms`), and every
state query <= 1500 ms (`--max-get-state-ms`). Missing/nonfinite measurements,
incomplete repeat groups and semantic failures cannot pass the gate.
A failed latency gate returns nonzero even when every functional observation
passes. Baseline latency is informational; baseline protocol/cleanup failures
still fail the run. Do not hide failures by rerunning into the same directory.

Use `--frames 80 --entries 12 --repeats 1` for a small protocol smoke. Test modern
alone with `--shapes modern`; legacy includes the one-time locked progress-format
upgrade and may require additional history copying and revalidation. Any changed
legacy budget must be explicit in the command/report, not silently treated as a
pass against the default two-second budget.

These are fresh-process measurements, **not flushed disk-cache measurements**.
Record machine load and build profile. A successful native-ready gate is not a
browser first-paint SLA: Web open/attach, HTTP, authentication and rendering add
time. In particular, do not extrapolate loopback static-assets measurements to a
remote Vite development frontend.

## Permanent historical-frame regression

The versioned `historical-frame-heavy-v1` scenario models the record-count shape
of a slow large-history open without accessing or copying any actual session:
**701,000 synthetic append frames**, 21,733 synthetic message entries, 256-byte
frame deltas, and a commit every 4,000 appends (including the final partial batch).
Its generator/defaults are covered by `scripts/test_resource_cold_start.py`.
This is not an exact replica of the original message/custom-entry distribution.

Run the complete regression with one command:

```bash
task test:cold-start
```

It builds a locked/offline release binary, runs the Python controller tests and
small real-RPC smoke, then measures **five fresh processes per format**. No old
binary or prebuilt history/index is required. It validates the last 40 messages'
count/order/content, idle/resume state, successful stop and owned-process/history
cleanup. Each observation deletes only its generated history/companion/lock,
retaining reports. Cleanup failure aborts further observations.

The default diagnostics live outside the checkout in the system temporary
directory (`rpi-cold-start-<uuid>-modern` and `...-legacy`). To pin build and report
locations outside the checkout, for example on Windows:

```bash
task test:cold-start \
  RESOURCE_CARGO_TARGET_DIR=C:/bench/cold-start-cargo \
  COLD_START_DIAGNOSTICS_DIR=C:/bench/cold-start-new
```

`COLD_START_BINARY` overrides the measured binary, `RESOURCE_PYTHON` selects the
Python executable, and trailing Task CLI arguments (`-- ...`) override the probe
arguments explicitly. Keep the default full counts for performance regression;
small overrides exercise protocol/controller behavior only.

The existing `resources` job in `.github/workflows/ci.yml` also runs both complete
history shapes, with three fresh processes each, reusing its release binary.
This job already gates stable and preview release automation. Shared hosted
runners use separately declared, more tolerant budgets; **they do not establish
the local two-second target**. Initial CI budgets must be checked against hosted
runner results rather than treated as a cross-machine SLA.

| Entry point | Modern ready median / max | Legacy upgrade ready median / max | Message query max | State query max |
| --- | --- | --- | --- | --- |
| `task test:cold-start` | 2000 / 3000 ms | 3500 / 5000 ms | 100 ms | 1500 ms |
| CI `resources` | 4000 / 8000 ms | 6000 / 10000 ms | 250 ms | 2000 ms |

Legacy's separate budget reflects the additional checked one-time format
upgrade; **passing that budget does not establish the default two-second target**. The standalone script
still defaults to 2000/3000 ms for either shape; the Task/CI commands supply their
legacy limits explicitly, and every report records the limits used.

CI uploads `historical-cold-start-diagnostics` on success or failure. Each format
has `results.json` with gates/measurements and executable hashes, `reproduce.json`
with parameters and generator hash, plus per-observation reports/protocol logs.
Raw generated history and configuration files are not included in the artifact.
Keep diagnostics, logs and measurement results outside Git. Isolation is not an
OS sandbox or a network firewall: use only trusted candidate/baseline binaries. A fresh process is still
not a flushed disk-cache measurement and native ready is not Web first paint.

Run only the fast controller suite (no Cargo, no real binary required):

```bash
python -B scripts/test_resource_cold_start.py
```

Set `RPI_COLD_START_BINARY` to a release CLI path to include the optional real-RPC
80-frame/12-entry legacy+modern smoke; otherwise that test is explicitly skipped.

For correctness, run the codec differential, JSONL streaming/cold-frame/progress,
resource preflight, fork and storage-contract tests. Build/test with
`--locked --offline`, and use `-- --test-threads=1` for this workspace's test
suite. Optimization must not remove recovery facts, skip invalid payloads beyond
the existing policy, send an early/fake ready, or rely on a previous process's
warm cache.
