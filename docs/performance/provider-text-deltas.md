# Exact text-delta compaction measurements

Only consecutive text deltas for one content index can omit their snapshot.
The first delta keeps its full authoritative baseline. Borrowed comparisons check
the appended text, text metadata, other content blocks and every message field;
all unsupported events reset eligibility. The existing wire protocol and host
start/end behavior stay unchanged. Full snapshots still use their existing Arcs.

The real mapper tests compare every emitted prefix and cancellation result against
full frames, including Unicode, empty deltas, block switches, authoritative
overrides and metadata changes. This establishes lossless reconstruction, not a
general permission to omit snapshots.

These are medians from three release runs of the same fixture before rebasing onto
`7a272ec`, with the environment and limitations in
[the conversion report](provider-snapshots.md). The original measured compaction
revision is `88d2a39`, retained on `bench/provider-series-measured` in the fork.
32 KiB output, 64-byte deltas, one immediate consumer:

| Metric | Baseline | Conversion removal | Compaction |
|---|---:|---:|---:|
| Allocations | 103,153 | 66,188 | 17,654 |
| Allocated bytes (MiB) | 100.64 | 88.77 | 19.10 |
| Transmitted JSON bytes | 8,687,082 | 8,687,082 | 124,766 |
| Elapsed ms (min–max) | 102.41 (83.24–104.67) | 37.30 (37.20–46.43) | 15.22 (15.06–15.24) |

This fixture transmits 98.6% fewer JSON bytes. It does not establish linear CPU
complexity: producer snapshots and full-message comparisons still accumulate
work. Shared-machine timings are diagnostic, not controlled speedup estimates.

Raw distribution: [compacted pipeline](results/provider-compact.jsonl).
