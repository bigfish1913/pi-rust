# Provider snapshot conversion measurements

The bridge now retains the typed partial message directly, skips event JSON
construction when no callback is installed, and moves the host's owned snapshot
into deserialization. Deferred-start frames retain their complete envelope.
There are no public Rust API or C ABI changes in this step.

These measurements compare baseline `534d316` with the original conversion-only
implementation `4cb285c`, before rebasing onto upstream `7a272ec`. The same offline
fixture was used for both. Its benchmark-only baseline is `0a035bd` on the fork's
`bench/provider-baseline` branch; the original candidate is retained in
`bench/provider-series-measured`. Run `python3 scripts/bench-provider.py --quick
--repeats 3` for the representative cases.

Linux x86_64, Xeon E5-2680 v4, 28 logical CPUs, Rust 1.95.0, thin LTO release
profile. Three fresh-process repetitions. The machine was shared, including
concurrent compilation: timings are diagnostic and do not establish a controlled
speedup. Allocator instrumentation includes runtime and host overhead.

32 KiB text, 64-byte deltas, one immediate consumer; medians:

| Metric | Baseline | Conversion removal |
|---|---:|---:|
| Allocations | 103,153 | 66,188 |
| Allocated bytes (MiB) | 100.64 | 88.77 |
| Peak live allocations (MiB) | 9.01 | 8.91 |
| Transmitted JSON bytes | 8,687,082 | 8,687,082 |
| Elapsed ms (min–max) | 102.41 (83.24–104.67) | 37.30 (37.20–46.43) |

With no emit callback, allocations fell from 46,598 to 5,418 and allocated bytes
from 28.99 MiB to 17.29 MiB. Full snapshots still accumulate CPU and memory costs.
The deterministic regression fixtures check all event families, callback absence,
rejection and cancellation, terminal errors, malformed snapshots, deferred starts,
tool identity and byte-for-byte compatibility with the previous serializer.

Raw distributions: [baseline](results/provider-baseline.jsonl) and
[conversion removal](results/provider-snapshots.jsonl).
