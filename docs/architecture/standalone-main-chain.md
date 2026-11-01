# Standalone host message-chain contracts

The host input/provider/tool/transcript/result path works without loading any
extension or calling a durable tool. Workflow extensions may observe or build on
host acknowledgements; they do not supply the host's queue or terminal correctness.
This is a focused supplement to [agent-loop recovery and settlement](agentloop-recovery-settlement.md).

## Queue acceptance, cancellation and nextRun

- A reserved message ID acknowledges accepted queue storage, not transcript
  insertion, provider receipt or completion of an external side effect.
- Enqueue, cancellation and draft restoration publish their in-memory change
  only after the storage call succeeds. Short owned tasks hold the message gate
  across storage acceptance and memory publication, so dropping an API caller
  cannot abandon that second half. These tasks do not run external callbacks.
- `clear_queue` batches all lane-local steering/follow-up cancellations. A failed
  batch returns an error, not successful draft text/images. NextRun is deliberately
  not part of draft restoration; other lanes are not cleared.
- `SessionStorage::append_queue_cancellations` defaults to an unsupported error,
  rather than a misleading sequence of individually acknowledged writes. Custom
  backends must implement this batch contract to support cancel/clear.
- Memory validates the entire batch under its state lock; SQLite validates and
  commits via its existing snapshot/CAS path; JSONL validates in the existing
  write owner and uses its batch append/error-cutback path. JSONL multi-line
  writes are **not** claimed to be power-loss atomic, and filesystem operations
  are not evidence of an fsync guarantee.
- NextRun reserves a lane-local FIFO prefix without removing it. A reserved entry
  is removed only after its entry append succeeds. Failure/drop releases any
  uncommitted reservations, preserving IDs and order for delivery in this same
  process. A prefix already committed is not replayed. This does not change the
  existing queued-input-before-native-prompt ordering or NextTurn behavior.
- Steering/follow-up handoffs also reserve, rather than remove, their lane's
  pending FIFO items. Their run-local reservation releases uncommitted IDs on
  failure/drop; an entry ACK removes only that committed ID. Short owned entry
  writes keep ACK and queue publication together, including fallback settlement.
  In-flight reservations cannot be cancelled/cleared or shown as pending drafts.
  `OneAtATime`/`All`, lane isolation and committed-prefix ordering are preserved.

## Run and tool ownership

Parallel execute/finalize futures belong to their agent loop; dropping its caller
cannot detach a live tool task and immediately admit recovery of that same call.
Already-ready peers keep progressing while later sequential preparation hooks or
completion events await. Completion events remain in completion order and result
messages in source order. Tool panics become error results under the original call
identity. Dropping an execute future also closes its late-update gate.

Releasing admission cancels both root and child run signals. This ownership covers
the async execute/finalize futures, not arbitrary background work a tool spawns or
an external effect already accepted elsewhere. It is not forced termination of
`spawn_blocking`, rollback, or an exactly-once side-effect guarantee.

## Stable terminal results

Normal runs, failed runs and deferred completion use the same finalization path:

1. Settle transcript writes and drain accepted context/its callbacks.
2. Capture the terminal leaf, final persisted entry and terminal assistant entry.
3. Append `operation_finished` with an optional, ID-only `result` identity.
4. Only after that storage ACK, release the message gate and publish `RunEnd`.
5. Drain context accepted by observers before releasing operation admission.

`RunResult`, `RunEnd` and `get_result(run_id)` use that same pre-observer identity;
observer writes and later runs/navigation do not redefine an earlier result.
Queries are restricted to the requested lane and run and do not scan for the
session's globally latest assistant. A failure with no assistant retains `None`.
Missing references and storage read errors are errors, not guessed results.
Suspended operations stay open and do not publish terminal events.

The new optional record field is serde-compatible with old logs. However, an old
or recovery terminal record without a trustworthy identity cannot reconstruct a
result reliably: `get_result` explicitly errors instead of combining the current
leaf with a historical/new assistant. Loading and recovery of those logs remain
supported. Rust callers constructing record/query structs must account for the
new fields (or use query defaults); this is not a claim of Rust source compatibility.

## Tool inspection and borrowed queries

Tool inspection uses the current branch and latest matching call. A native
`tool_started` checkpoint binds its assistant and reserved result entry, including
tool name, instead of treating a repeated provider call ID as execution identity.
A fork resolves inherited native checkpoints by the selected branch assistant's
exact entry ID, including its source lane; a same-ID imported result cannot turn
that native call into legacy matching. The checkpoint's result must still be on
the inspected branch.
Imported calls without checkpoints use ordered call/result matching bounded by
assistant/user turn boundaries. Pending/absent results are not reported complete.

Queue-kind, target-ID, pending-only and tool-call filters run on borrowed reducer
records before result cloning and limits. Pending entries exclude cancelled or
already committed targets. This prevents unrelated historical queue bodies from
being recopied on each short input. The reducer can still scan O(history) records;
there is no new pending database/index or duplicate transcript.

## Verification

- `tests/standalone_main_chain.rs` plus `tests/support/standalone_cases.rs`:
  deterministic JSONL write failures/partial batches, dropped callers, partial
  NextRun commits, lane/branch boundaries, stable result restoration, terminal
  ACK/event ordering and checkpoint/legacy tool identities.
- `tests/support/standalone_lifecycle.rs`: inherited checkpoint identities,
  dropped root runs, steering/follow-up handoffs before/during entry ACK, and
  same-process/reopened pending/committed-prefix agreement.
- `rpi-agent/tests/structured_tool_lifecycle.rs`: owned parallel execute drop,
  late-update suppression on drop, preparation/event backpressure progress and
  panic result identity; existing ordering/hooks tests remain in force.
- `tests/standalone_storage.rs`: cancellation batch validation and pending-query
  filters on Memory, SQLite and JSONL.
- `tests/standalone_allocations.rs`: cumulative caller-thread allocation while
  appending a short input after 100 cancelled 64-KiB steering payloads. This is
  not a latency benchmark, peak-memory measurement or release-build claim.
- `tests/deferred_settlement.rs`: terminal result/query agreement in addition to
  existing deferred continuation, admission and single-RunEnd assertions.

These contracts do not promise exactly-once execution of external business effects,
survival of unacknowledged writes, or unlimited recovery from storage corruption.
