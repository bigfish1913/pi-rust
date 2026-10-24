# Offline provider pipeline benchmark

`scripts/bench-provider.py --quick` builds the release test fixture, then runs it
alone in a fresh process for each sample. Omit `--quick` for representative
4/32/128 KiB outputs, 64/1024-byte chunks, a 4 KiB one-byte stress case, 1/8/32
streams, delayed readers, and result-only consumption. No network or credentials
are used. Fixed model, timestamp, text, usage and identifiers make results
repeatable through the real SDK driver and host provider adapter.

The `bench/provider-baseline` branch retains the synchronous fixture for baseline
`534d316`. The snapshot-conversion revisions use that same fixture. This revision
adapts its calls to asynchronous `push` and records host-queue occupancy alongside
backpressure; the workloads and counters otherwise remain unchanged. Run it with
the `provider-performance` feature, which is also the runner's default.

JSONL includes environment/revision, elapsed time, global allocation count and
bytes, peak live allocator bytes, and transmitted JSON bytes. Linux additionally
reports process user/system CPU and peak RSS via `/usr/bin/time`. Allocation
instrumentation includes runtime and host overhead and may perturb execution;
peak live bytes are whole-process allocations, not queue payload accounting.
Use repeated samples and compare distributions; CI must not assert speedups.

Run correctness tests separately. [The measured report](provider-pipeline-results.md)
records the original measured revisions, raw distributions and their limitations.
