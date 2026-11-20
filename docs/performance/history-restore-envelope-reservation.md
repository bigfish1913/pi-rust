# Bounded JSONL envelope reservation

Follow-up to [prepare profiling](history-restore-prepare-profile.md), based on
`fcef7819`. This is a small allocation optimization, not a replacement JSON codec.

## Implementation and compatibility

The shared header/mutation `parse_object` attempts to deserialize just the outer
object into a `serde_json::Map` with capacity 12. The two measured transcripts
mostly have 8–12 outer fields; roughly 17% have 14. Capacities 8, 12 and 16 were
explored; 12 had the lowest cumulative requested bytes among those candidates.

Only this outer object is reserved. Every field value, including arbitrary
nested objects/arrays, strings, numbers and private protocols, still goes through
the existing `serde_json::Value` decoder. This avoids multiplying speculative
capacity across user-provided nested objects. Empty objects are not reserved.
Insertion order and duplicate-key last-wins behavior use the same Map operations.

If the first **decoded** key starts with `$serde_json::private::`, or the attempt
returns any error, the original Value parser runs on the already-read line. This
preserves escaped private keys, RawValue/feature-unified protocols, non-object
roots, error classification/priority and exact public error messages. Invalid
inputs can therefore be parsed twice; this is deliberately an error-path tradeoff,
not a reason to reread storage or cache prior verification.

No schema, dependency, hash, source read, selected-payload read, resource admission,
writer-lease/verification/repair ordering or operation-settlement rule changes.
The rejected direct tool-result decoder remains absent.

## Fixed-snapshot host comparison

macOS ARM64, rustc 1.100.0, ordinary workspace release configuration (thin LTO,
16 codegen units, stripped symbols), same serde_json features and Cargo.lock.
Baseline: `/tmp/rpi-prepare-profile` (profiling code before this reservation).
Candidate: `/tmp/rpi-envelope-reserved`.

Both original sessions were read only. Transcript and progress companions were
copied after checking source size/mtime before and after copying. Every trial
used a fresh isolated session root and a fresh offline RPC process, alternating
A/B and B/A order. Stop/kill and temporary-root cleanup were performed after ready.
This is host-process cold start, **not** OS page-cache cold start.

An initial 11-pair batch began with a competing compiler still active. In that
batch the large prepared-replay median was 458.7 → 470.7ms, illustrating why a
single noisy batch is insufficient. A later 21-pair batch began/ended without a
compiler among the high-CPU processes; ordinary desktop activity remained. The
following are independent medians from that later batch, not additive spans:

| Phase (ms) | Large baseline | Large reserved | Small baseline | Small reserved |
| --- | ---: | ---: | ---: | ---: |
| Replay decode | 217.7 | 202.5 | 138.4 | 130.3 |
| Prepared replay | 460.2 | 447.4 | 248.5 | 240.3 |
| Extension prepare | 512.3 | 497.7 | 296.4 | 282.9 |
| Harness create | 788.8 | 771.4 | 4.8 | 5.0 |
| Harness build | 1029.5 | 1011.6 | 113.2 | 112.4 |
| Startup to harness | 1542.4 | 1515.2 | 409.6 | 397.5 |

Large: **51,774,213 bytes / 12,690 mutation rows / 1 open operation**.
Small: **16,542,757 bytes / 8,632 mutation rows / 0 open operations**.
These sources have grown since earlier experiments. Do not compare their absolute
totals against the earlier ~731ms result or attribute the large create cost to a
specific recovery action without further profiling.

Replay decode improved in **17/21 paired trials for each snapshot**. A descriptive
10,000-resample paired bootstrap of the median saved milliseconds gave 95% ranges
of **2.6–20.1ms** (large) and **2.9–13.3ms** (small). The large total-startup range
was **−2.2–75.2ms**, crossing zero: the total median is encouraging, not proof of
a reliable large-session end-to-end speedup. The small total range was 1.3–30.7ms.
These local, non-independent desktop trials are not cross-platform guarantees.

## Read-only allocation probe

The example imports the actual private envelope helper, not a copy of the
optimization. Input reading and parity checking happen before measurement. It
parses/drops one row at a time and does not open storage, acquire a lease or repair
anything. Output contains only labels/numbers, never payloads.

```bash
cargo run -p rpi-harness --example jsonl_envelope_probe --release --locked -- \
  /absolute/path/to/frozen-copy.jsonl
```

| Metric | Large baseline | Large reserved | Small baseline | Small reserved |
| --- | ---: | ---: | ---: | ---: |
| Allocation/reallocation calls | 585,312 | 536,757 | 398,545 | 365,536 |
| Cumulative requested bytes | 161,092,052 | 146,884,028 | 84,925,922 | 75,294,482 |
| Peak outstanding layout bytes | 1,230,374 | 1,230,166 | 495,568 | 495,360 |

Calls fell **8.30% / 8.28%**, requested bytes **8.82% / 11.34%**. Reallocations count
the new requested size, not just growth. Peak is allocator-layout accounting,
**not RSS**, allocator-internal transient peak or total startup memory. The
already-loaded input is excluded. Latency loops use an audit allocator wrapper
and are not substitutes for the uninstrumented host comparison above.

A separate seven-pass codec probe keeps the **current typed decoder identical**
on both branches; only root parsing differs. Medians were 210.568 → 196.934ms
(large) and 129.040 → 120.191ms (small). Recognized cold frames are filtered as in
the existing stage probe; this does not model full duplicate/backward replay.

```bash
RPI_CODEC_BENCH_TRANSCRIPT=/absolute/path/to/frozen-copy.jsonl \
  cargo test -p rpi-harness --lib --release --locked \
  benchmark_reserved_envelope_with_current_typed_decoder -- --ignored --nocapture
```

## Validation and artifacts

- Harness lib: **496 passed, 4 ignored**.
- Resource lifecycle integration: **5 passed**; startup diagnostics: **6 passed**.
- CLI all-targets locked check and ordinary release build passed.
- New differential coverage: 2,000 generated nested cases, private/escaped keys,
  duplicate/order behavior, Unicode, truncation, number and recursion boundaries.
  The frozen decoder now has its own original root parser, not the optimized one.
- Both real snapshots passed full-row release parity; the allocation example
  checks Value equality **and serialized order/number representation**.
- Formatting/diff checks passed. Cross-platform CI has not run for this branch.

Raw local results: `/tmp/value-envelope-startup-{large,small}-steady.log`,
`/tmp/value-envelope-codec-{large,small}-steady.log`, and
`/tmp/value-envelope-allocation-{large,small}.log`. The initial/loaded exploratory
logs are separate. The ready/drain runner is `/tmp/bench_envelope_ready.py`.
Frozen real-content copies are deleted after measurement and never committed.
