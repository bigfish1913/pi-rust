# Pending-tool wake measurements

The optional trailing registration slot enables an owned asynchronous driver.
Each execute, install, poll, cancel and destroy call runs serially on the blocking
pool. Pending operations await a notification or cancellation without occupying
a blocking worker. Legacy plugins retain the existing polling driver; updated
plugins select it when the host's new slot is absent.

The release benchmark compares the unchanged legacy path with the wake path in
the same candidate. It does not compare two historical builds. Run it alone:

```bash
cargo test --release -p rpi-extensions --features rpi-ai/providers --lib pending_population_benchmark -- --ignored --nocapture --test-threads=1
```

The original measured code is retained on the fork's
`bench/provider-series-measured` branch. Three timed runs on the shared Linux
machine described in [the snapshot report](provider-snapshots.md), with a 100 ms
idle interval per case, gave these medians:

| Tools | Legacy / wake polls | Legacy / wake idle workers | Legacy / wake cancellation ms |
|---|---:|---:|---:|
| 1 | 97 / 1 | 1 / 0 | 0.762 / 0.208 |
| 64 | 6,394 / 64 | 64 / 0 | 1.181 / 3.531 |
| 256 | 26,681 / 256 | 256 / 0 | 1.545 / 8.514 |

Wake mode eliminates periodic idle polling. Cleanup adds dispatch overhead; the
256-tool cancellation range was 2.838–12.166 ms, versus 1.293–2.513 ms for legacy.
There is no claim of a cancellation speedup. All cases had zero active blocking
calls after cancellation. Whole-process CPU was 0.36–0.41 s and peak RSS was
8,840–10,296 KiB; these totals cover all six workloads, not individual modes.

Correctness tests cover early and duplicate wakes, waking during poll, immediate
completion, cancellation, caller drop, late callbacks, serialized access and
exactly-once destruction. Retired host tokens reject stale notifications without
dereferencing freed callback memory. Plugin destroy must join its own workers.

Raw samples: [pending tools](results/pending-tools.txt).
