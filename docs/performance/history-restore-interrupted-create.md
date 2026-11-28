# Interrupted-create recovery: bound payload hydration

Follow-up to [envelope reservation](history-restore-envelope-reservation.md), based
on `def19ea6`. This change targets interrupted recovery, not every session open.

## Locate the cost before changing recovery

New opt-in `RPI_SESSION_DIAGNOSTICS` spans split create admission, progress prune,
open-operation lookup, retry/resume checks, interrupted repair, frame retirement,
terminal close and context flush. Repair further separates tool records, branch
payload hydration, tool-batch settlement and stream recovery. Labels are static;
outputs are counts, not payloads, paths or identifiers. Disabled spans do not read
the clock. Events finish on success; nested spans must not be added together.

New snapshots differed from the preceding report: the large session no longer
had an open operation, while the smaller session now did. The first five
profiling-only isolated trials measured create at about **10.5ms / 391.4ms**.
For the interrupted smaller case, branch-tail hydration, batch-outcome hydration
and the user-tip check each took about **116–121ms**. The first two each returned
**3,024 branch entries**. This evidence identified three broad payload reads;
it does not retrospectively explain the preceding report's large-session cost.

## Implementation and safety

- Tail repair and user-tip inspection read newest-first pages of 16 entries,
  stopping at the same conversation/metadata boundaries as before. The leaf is
  pinned across queries. Every query retains the full ancestry walk: no
  `stop_at_id` shortcut can hide missing ancestors or cycles.
- After 128 skipped entries, fall back to the old full query. This bounds the
  number of repeated full metadata walks and avoids an O(N²) custom-only tail.
  The fallback can hydrate the whole branch, as before; this is not a universal
  constant-memory promise. Diagnostic counts include fallback rereads.
- Batch-outcome lookup first queries and **reads/verifies** the assistant on the
  pinned branch, then reads its oldest-first descendants using its sequence.
  It does not trust an identity index instead of reading the anchor. The existing
  complete batch scan still checks `(tool_call_id, tool_name)` and reserved result
  IDs, including conflicting results, before recovery can execute anything.
- Missing anchors retain the original full-read/error path before reporting
  `Recovery assistant is not on the current lane branch`. No-anchor callers
  retain their old full-read path too.
- Selected payloads are read and verified on every invocation. No cross-attempt
  payload/identity cache was added. Corruption and short reads propagate out of
  repair/outcome lookup; the user-tip predicate keeps its prior best-effort
  `false` behavior on read errors.

No codec/schema/error-text changes, resource-admission bypass, stat fast path,
lease/verification/repair reordering, tool policy change, or lifecycle reorder.
The global recovery sweep and closed-operation unflushed-frame reporting remain
unchanged. Prune, tail settlement, stream repair, retirement and terminal close
still occur in the original order.

## Frozen fixtures and method

macOS ARM64, ordinary locked workspace release configuration: thin LTO,
16 codegen units, stripped symbols; unchanged dependency/feature configuration.
Original transcripts and progress companions were read only, copied after
checking size/mtime around copying, and never opened for recovery in place.

| Frozen input | Main JSONL bytes | Physical mutation rows | Open operations |
| --- | ---: | ---: | ---: |
| Large / control | 55,238,972 | 12,983 | 0 |
| Small / interrupted | 16,809,809 | 8,849 | 1 |

Physical mutation rows are not the same as selected branch entries. Inputs have
grown and their operation states changed since the previous report. Do not
compare absolute startup totals across those snapshots.

### Isolated create probe

`harness_create_probe` copies a frozen input and optional progress companion to
its own temporary root, performs prepare/commit and create, then closes/drains,
releases the lease and deletes its root. Empty tools/models prevent tool replay;
it never runs the agent, a provider or extensions. This is **not** CLI resource
preflight or full startup. Source copying/load occurs outside create timing.

```bash
cargo run -p rpi-harness --example harness_create_probe --release --locked -- \
  /absolute/path/to/frozen-copy.jsonl
```

Seven new-process alternating A/B, B/A pairs compared the profiling-only variant
against the bounded-query variant, with identical diagnostic stage boundaries:

| Small interrupted phase (ms) | Profiling only | Bounded queries |
| --- | ---: | ---: |
| Create total | 422.33 | 39.11 |
| Tail payload read | 126.73 | 1.80 |
| Batch-outcome payload read | 130.70 | 1.82 |
| Tail batch settlement | 139.54 | 2.04 |
| User-tip check | 128.10 | 1.92 |
| Stream recovery | 11.76 | 9.98 |

Tail hydration fell **3,024 → 16** entries; outcome hydration **3,024 → 2**
(anchor plus descendant). Both variants had `open_before=1`, `pending_resume=false`.
Large/control create was 12.31 → 9.89ms; this exploratory difference is not an
end-to-end claim. Ordinary desktop background activity was not controlled.

### Complete offline RPC host comparison

Baseline `/tmp/rpi-envelope-reserved`: `def19ea6`. Candidate
`/tmp/rpi-create-tail-cli`: new diagnostics plus bounded queries. Both used the
same diagnostic environment; the candidate's additional diagnostics overhead is
included. Each trial copied the same frozen main/progress pair to a new root,
started a new offline RPC process, drained stderr after ready, then stopped the
process and removed its root. Host-process cold start is **not OS page-cache
cold start**.

A first 21-pair batch overlapped other compiler/browser work. Its small create
and startup medians were 427.1 → 38.7ms and 896.5 → 460.2ms; all raw results were
retained separately. No other user's work was stopped. A later, quieter batch
also used **21 alternating pairs per input**, with no compiler in the before/after
high-CPU snapshots and no own build/test during measurement. Desktop load
remained; endpoint snapshots are not a continuous activity audit.

Independent medians from that later batch (not additive cost components):

| Phase (ms) | Large baseline | Large bounded | Small baseline | Small bounded |
| --- | ---: | ---: | ---: | ---: |
| Prepared replay | 495.6 | 482.2 | 234.9 | 251.3 |
| Extension prepare | 546.4 | 529.8 | 280.7 | 300.2 |
| Recovery sweep | 159.7 | 158.9 | 67.9 | 68.9 |
| Harness create | 8.6 | 8.8 | 380.1 | 34.7 |
| Harness build | 265.3 | 270.1 | 490.7 | 145.7 |
| Startup to harness | 802.2 | 799.2 | 791.4 | 442.3 |

Small/interrupted create fell **90.9%**, observed total startup **44.1%**.
Create/build/startup each won **21/21** pairs. A descriptive paired bootstrap
(10,000 median resamples, seed 20261120) gave 95% saved-time ranges of
**343.4–362.7ms / 342.0–351.2ms / 336.1–360.2ms**, respectively.
The control startup range was **−7.8–23.0ms**, crossing zero. Its create median
was about 0.2ms slower with the new diagnostic spans. Untouched phases fluctuated;
the small recovery-sweep median was about 1ms slower. Do not attribute replay or
extension-prepare changes to this optimization, or claim a general speedup for
closed sessions. Local desktop pairs are not independent cross-platform samples.

## Validation and artifacts

- Harness lib: **504 passed, 4 ignored**, including eight new tests for bounded
  selection, custom/result/metadata boundaries, context-marker eligibility,
  pinned leaves, long-prefix fallback, batch membership/reused call IDs, and
  repeated selected-payload corruption/short-read failures without repair writes.
- Recovery execution contract and tool-result identity integrations: **8 passed**.
- CLI resource lifecycle: **5 passed**; CLI all-targets locked check passed.
- Ordinary CLI release build and release create-probe build passed.
- Expanded `cargo test -p rpi-harness --tests --locked -- --skip
  v2_allows_256_noop_receipts_and_replays_at_the_limit_without_append`:
  **1,154 passed, 7 ignored, 1 filtered**, across 56 top-level test executables
  (excluding a nested subprocess result from the aggregate). This includes the
  lib and recovery tests above; the counts are not additional independent cases.
- The initial unfiltered debug integration run hit the 600-second tool budget
  while that receipt-limit stress case was still running. The case was then run
  alone in release: **1 passed**, 168.19 seconds after compilation. Thus the
  default debug suite excluding that one case passed, and that case passed in
  release; a completed unfiltered all-debug run is not claimed.
- CLI startup diagnostics: **6 passed**; harness examples locked check, formatting
  and diff checks passed.

Local scoped data: `/tmp/create-tail-results.jsonl`; complete startup logs:
`/tmp/create-tail-startup-{large,small}-{loaded,steady}.log`. CPU endpoints:
`/tmp/create-cli-cpu-{before,after}-{loaded,steady}.log`. The ready/drain runner
is `/tmp/bench_envelope_ready.py`. Frozen real-content copies were deleted after
measurement; no benchmark host remains. No cross-platform CI has run yet.
