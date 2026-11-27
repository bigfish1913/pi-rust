# Historical-session prepare profiling

## Scope

Base: `61198572` (PR #389). This change adds opt-in diagnostics and a read-only
codec-stage probe. It does **not** change decoding, resource admission, repair,
writer ownership, or recovery decisions, and makes no new startup-speedup claim.

CLI spans are enabled with `RPI_STARTUP_DIAGNOSTICS=1`. Accumulated replay timings
are enabled with `RPI_SESSION_DIAGNOSTICS=1`. They log fixed labels and numbers,
never paths, IDs, tool arguments, or payload contents.

The replay accumulator checks the environment only at construction and samples
no clock when disabled. It emits once per successful replay, not per row:

- `replay_scan`: bounded reads, whole-source hash and row copying.
- `replay_classify`: UTF-8 validation and cold-frame recognition.
- `replay_decode`: canonical mutation decoder.
- `replay_index_apply`: reducer/index updates, cold-location digests and duplicate
  checks. Includes lock acquisition, but not the later destruction of every local.

For accumulated phases, `input` is physical mutation rows; `output` is the number
of completed timing samples, **not** accepted entries. The scan includes its final
EOF attempt. Missing output is not proof that replay succeeded. Header handling,
repair, teardown and instrumentation overhead are not all covered by these four
buckets; they must not be presented as an exhaustive accounting.

## Host measurements

macOS ARM64, rustc 1.100.0, ordinary workspace release profile. Each run starts a
fresh RPC/offline host with an isolated copy of a fixed transcript and progress
companion, then stops it after ready. OS page cache is not cold. Five runs per
snapshot; the following are independent phase medians and need not add up.

The initial diagnostic batch overlapped unrelated compiler/test processes and
was used only for exploration. A second batch was run after those processes had
finished, with no compiler/test process observed in before/after CPU snapshots.
Normal desktop activity still exists; these are local diagnostics, not a
controlled performance comparison.

| Phase (ms) | Large: 50,696,302 bytes, 12,535 physical rows | Small: 16,397,975 bytes, 8,477 physical rows |
| --- | ---: | ---: |
| `extension_prepare` | 461.8 | 239.1 |
| `prepared_replay` | 422.3 | 197.7 |
| `extension_register` (load/register/validation) | 34.8 | 35.8 |
| `replay_scan` | 87.2 | 27.5 |
| `replay_classify` | 21.7 | 10.6 |
| `replay_decode` | 196.5 | 107.3 |
| `replay_index_apply` | 103.7 | 46.0 |

Resource selection loading, facts extraction and binding validation were each
under 1ms at the median. Prepared replay is the largest part of extension prepare;
resource discovery/registration is not the majority of that span.

The large snapshot has one open operation, while the small snapshot has none.
Its `harness_create` median was 634.2ms in this batch, versus 3.1ms for the small
snapshot. The large source has changed since PR #389's experiment, so neither
these full startup numbers nor the old ~731ms total should be compared as an
optimization/regression result. This measurement alone does not attribute the
new create cost to a particular repair action.

## Reproducing the codec decomposition

Use an immutable **copy** of a real transcript; do not use an actively written
source. The test never opens storage, acquires a lease, or modifies any files.

```bash
RPI_CODEC_BENCH_TRANSCRIPT=/absolute/path/to/frozen-copy.jsonl \
  cargo test -p rpi-harness --lib --release --locked \
  benchmark_mutation_decode_stages_on_real_transcript \
  -- --ignored --nocapture
```

The probe selects rows using the same cold-frame recognizer, but does not model
full replay's duplicate/backward sequence handling. It is a codec-stage probe,
not a replacement recovery benchmark. For these particular fixtures no rows
were filtered: their historical assistant frames are commit-stream records.
Parsing, typed schema decoding and destruction have separate clock measurements.
The full transcript is read before measurement.

| Codec-stage median (ms, five passes) | Large: 12,534 mutation rows | Small: 8,476 mutation rows |
| --- | ---: | ---: |
| JSON to `Value` object construction | 128.138 | 74.128 |
| Typed mutation decoding | 51.458 | 33.209 |
| Mutation destruction | 5.287 | 3.656 |

These are standalone in-process stage measurements, not independent savings
that can be subtracted from a host's startup time.

## Rejected experiment

An uncommitted prototype decoded ordinary tool-result messages directly through
the existing concrete schema rather than serde's internally tagged AgentMessage
buffer. An initial differential test found different numeric content-tag
semantics/diagnostics (`expected variant identifier` vs a variant-index error).
A conservative fallback restored tested parity, including 2,000 generated nested
cases, but the isolated 2,170-message benchmark showed no stable benefit:
7.961ms tagged vs 7.884ms direct at the median, with overlapping noisy samples.
Input-tree copies were outside timing for both algorithms.

All prototype production code and prototype-only tests were removed. There is
no new decoding fast path in this change. Avoid adding a second schema decoder
or bypassing validation to chase this small component.

The next worthwhile investigation is allocation/string-unescaping cost in
JSON-to-Value parsing, not another tool-identity shortcut. Full source hashing,
selected-payload reads, schema checks and lease-before-repair ordering remain
required.

## Validation and local artifacts

- Harness lib: 494 passed, 3 ignored (includes the new read-only probe).
- Resource lifecycle integration: 5 passed.
- Startup diagnostics: 6 passed.
- CLI all-targets locked check, release build, formatting and diff checks passed.
- No dependencies or transcript-format changes. Cross-platform CI not run for
  this local branch yet; this is not another claim of cross-platform speedup.

Local measurements: `/tmp/prepare-profile-{large,small}-idle.log` and
`/tmp/prepare-codec-stages-{large,small}.log`. Exploratory/rejected-experiment logs
are separate. Real transcript copies are not committed and are deleted after
measurement; original sessions were read only.
