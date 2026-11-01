# Agent-loop recovery, settlement and storage contracts

## Recovery is a resumable commit protocol

`OperationFinished` is a commit barrier, not an intent to recover. Recovery must
successfully persist all necessary assistant and tool-result entries before it
closes an interrupted operation. Storage failures propagate to the caller.

- Retain each recovered stream's index and reserved assistant-entry ID.
- Persist the stream-to-entry binding before the assistant entry. A binding
  without an entry reuses that ID on the next attempt; a binding whose entry
  exists does not cause another assistant to be appended.
- Repair tool batches incrementally. Use the started call's reserved result ID,
  or an unambiguous recovery ID scoped to the assistant and call.
- Register a settled tool result's reserved ID by loop-message index before
  attempting its entry append. If that append fails, context-boundary and run-end
  fallback writes reuse the `tool_started.result_entry_id` instead of minting a
  different identity. A failed fallback leaves the operation open for recovery.
- Only a contiguous message prefix may commit during settlement. Later results,
  assistants and injected messages cannot overtake a failed earlier entry. At
  each turn boundary, retry the pending prefix before compaction, accepted
  context or another provider request. Normal final/retryable assistants retain
  their intentional deferred-commit behavior. A failed prefix retry propagates
  without terminal publication; the second hook in that turn must not retry it
  again or call compaction. The fully committed path copies no message history.
- During recovery, finish the durable tail's missing tool results before replaying
  newer streams. Otherwise even a synthetic abort stream could hide that older
  incomplete tool batch behind a new assistant boundary.
- Identify landed results by both call ID and tool name on the current lane
  branch, after that exact assistant and before the next transcript/context
  boundary. A reused ID or a result for a different tool is not evidence that
  this call finished. Provider-context repair uses the same pair on both its
  borrowed fast path and ownership-moving slow path, so a wrong-name result
  cannot replace the actual recovered output.
- For a call with `tool_started`, actual recovery also requires its reserved
  result-entry ID, not just the matching pair. A matching pair in a different
  entry is a conflict: reject the whole batch before replaying any of its tools,
  leave the operation open and preserve both history and recovery payloads.
  Do not adopt the external result, duplicate it or replay over it. The public
  unresolved-call reader applies the same checks; the run's recovery reuses its
  already-read started records, without another query or payload clone.
- The read-only runtime report uses each `tool_started` record's exact reserved
  result-entry ID plus call ID and tool name. Unrelated results anywhere else
  in session history cannot hide an in-flight/dangling invocation. Conversely,
  navigating away from a genuinely completed invocation does not make its
  referenced result disappear from the historical report. This reuses the
  existing history read without adding payload copies or another full scan query.
- Ignore extension `Entry::Custom` state when locating transcript tips. A partial
  batch can end in an already committed tool result followed by extension state.
- Recorded upstream results are never executed locally. Otherwise replay still
  requires both the original and current `Safe` policies. If a host authorization
  hook is installed, recovery does not replay tools without its authorization
  context or wait for a UI which may not exist yet; it records unknown outcomes.
- Recovery checks cancellation at entry, stream/tool boundaries and immediately
  before a permitted replay enters the underlying tool. Never detach or cancel
  the future of an already executing replay: wait for it and commit its actual
  success/failure under the reserved result ID first. Then propagate cancellation
  without starting another tool, provider request or new operation, or publishing
  `OperationFinished`/`RunEnd`. This includes cancellation during the last call.
  The old operation remains open; a later restore resumes only unresolved calls.
  Cancellation is an agent error, not a refused session write diagnostic.

A JSONL append is not assumed atomic. Tests exercise three persistence boundaries
for both assistant-frame and two-tool recovery, with definite failures and writes
which reach the append handle but return an error: twelve scenarios. Reopening
and recovering twice must leave one assistant and at most one result per call.
The JSONL backend's existing torn-tail, rollback and uncertain-append tests also
remain part of the workspace regression suite.

`tests/support/tool_result_fallback.rs` injects a failed immediate tool-result
write and checks tool-only termination and accepted-context fallback boundaries.
It also fails both writes, closes/reopens real JSONL twice, and verifies the reserved ID
survives without re-executing a `Never` tool or duplicating its result. All three
regressions failed before the message-index reservation was preserved. Tests
assert actual attempted IDs, committed content/order, execution count and the
record-log invariants, not just a successful run outcome.

`tests/support/settle_prefix_order.rs` checks that a failed result is durable before
another provider request, preserves source order in a multi-tool batch and across
consecutive tool rounds, and keeps the operation open after a failed boundary
retry. These four regressions failed before the prefix barrier. Another test
publishes a deterministic newer frame stream after an unresolved durable tool
batch, restores twice, and verifies the result precedes that stream with no
re-execution or duplication. The existing native JSONL double-reopen test also
covers cancellation-generated abort progress after a boundary failure.

`tests/tool_result_identity.rs` adds four regressions, all observed failing before
these identity checks. Runtime cases cover reused IDs across lanes and older/newer
batches, different result entry IDs, open/closed operations, incorrect tool names,
call IDs and message roles, plus exact-entry and navigation positive controls.
A real JSONL test closes/reopens twice, verifies the correct reserved synthetic
result is committed before terminal completion despite a wrong-name lookalike,
and confirms the provider context retains that exact output. An original `Never`
record still prevents execution even when the current tool declares `Safe`.
Another regression exercises both provider-context paths and repair idempotence.
Stored external results are not deleted or rewritten; only their eligibility as
completion proofs/provider results is rejected.

`tests/recovery_execution_contract.rs` adds four integration tests: reserved-ID
conflict rejection, exact-ID controls, landed unstarted-call controls and replay
cancellation. The two bug regressions failed before the fix; the controls already
passed. Conflict cases cover Memory, SQLite and real JSONL, original Never/Safe
policies, a conflict in the second call (no first-call replay), and two restores.
Cancellation cases join a paused replay, test real success/failure and single/dual
call batches, assert its actual result stays durable with no terminal/provider
request/storage-failure warning, and restore real JSONL twice without repeating
that call. An additional unit test rejects a pre-cancelled Safe replay directly
at the execution entry. The two earlier external review reproducers are retained
in the build directory; their host-pinned versions are rerun against the fix.

## Tool execution requires durable intent

`ToolExecutionStart` remains the UI/plugin event sent before argument preparation
and authorization, with its existing raw-argument payload. It is not a durable
execution checkpoint. A private run-local tool wrapper journals intent at the
actual `execute` entry, after preparation, schema validation and the host's
`before_tool_call` hook, without changing the agent event ABI or execution mode.

- A call must reference its committed assistant entry and a successfully appended
  `tool_started` before entering the underlying tool. Either write failing refuses
  that execution, preserves the first write error, and cancels the current run.
  The caller receives an error; no `OperationFinished` or `RunEnd` is published.
- `effective_args` are precisely the parameters handed to the underlying tool,
  including trusted post-validation authorization replacements. Recovery still
  follows the existing policy; it executes journal arguments directly, without
  preparing or re-authorizing them out of context.
- Blocked calls, failed validation and upstream-recorded results do not enter
  this wrapper and do not generate a local execution checkpoint. Their existing
  result/error event behavior is unchanged.
- Recheck cancellation after the checkpoint append awaits. An acknowledged
  checkpoint is conservative execution intent, not proof the call entered the
  underlying tool: cancellation/crash can still fall between those two steps.
- A failed checkpoint cancels the run but does not detach already executing
  parallel peers. The existing batch machinery waits for them to settle.
- `tool_index` counts only tool-call blocks, matching reducer validation; text or
  thinking blocks do not consume an ordinal.
- Scope call identities and result reservations to the current assistant. Clear
  those maps at the next assistant's start, so reused provider call IDs cannot
  borrow an older assistant or result entry after a write failure or blocked call.
  Prior result fallback IDs remain reserved independently by loop-message index.
- Deferred continuation does not re-emit its already persisted polled assistant.
  Seed call identity/ordinal from that real source entry, not a projected message,
  so its calls use the same execution barrier and reserved result IDs.

`tests/support/tool_execution_barrier.rs` contains nine regressions; eight were
observed failing before their corresponding fixes, and the cancellation case
protects previously correct behavior. They cover assistant/checkpoint write
failures, prepared and hook-replaced parameters, non-executing calls, cancellation
during the checkpoint write, parallel peer settlement, mixed-content ordinals and
reused-call identity/result-reservation isolation. Preparation recovery includes a
text preamble and closes/reopens real JSONL twice in sequential and parallel modes,
verifies exact execute IDs and arguments, checks that preparation runs only once,
and lands the recovered result under its reserved ID. The deferred end-to-end test
also checks its checkpoint and reserved result identity. An in-flight crash test
now waits for the actual checkpoint rather than the earlier assistant entry.

## Authoritative plugin settlement

The CLI's shared session construction bridges main-lane harness `RunEnd` to the
existing plugin `AgentSettled` event. This is independent of print, JSON, RPC and
interactive output adapters. Completed, aborted and failed operations settle;
suspended operations remain open. Native per-attempt `AgentEnd` is not a substitute.

The bus owns the observer, which resolves the current extension session on each
invocation. Session clones keep callback libraries alive through dispatch; reload
replaces the target, and bus destruction releases the observer. RunEnd callbacks
run without harness/message-write locks held. Their context writes are drained
before another operation is admitted.

Langfuse closes a generation on `MessageEnd` and its root only on authoritative
settlement, so retry generations do not split into separate roots. Durable's
existing idempotent settlement handler remains the owner of its workflow journal.
`TurnEnd` and goal turn budgets retain their original per-turn semantics.

Native event projection checks the active snapshot and tag subscriptions before
serializing a payload. An entry cleanup guard releases constructed event strings
once, including stale-snapshot and no-handler paths. Plugins borrow event strings;
they do not own or free the host's dispatch allocation.

## Bounded history reads and indexed SQLite snapshots

`SessionStorage::record_page` is a required storage API, not a fallback which
clones the complete history. Memory, JSONL and SQLite select and count within one
state snapshot. Bounds are exclusive, selected records are returned oldest first,
and `from_end` selects the latest matching window. Only selected payloads are
cloned; an exact total can still require an O(N) scan.

SQLite's `state_json` uses `format: "indexed-session-v1"`. Entry and record
payloads occur once; the chronological log holds indexes. ID lookup, used-ID and
open-operation indexes are rebuilt on load. Loading rejects unknown formats,
duplicate IDs, invalid/out-of-order indexes, duplicate/unreferenced payloads and
invalid log sequences. This format intentionally has no legacy snapshot reader or
automatic migration. Custom storage implementations must implement `record_page`.

## Branch-scoped goal state and durable outbox

Goal and orchestrator query the latest `goal-state` custom entry on the current
branch using `customTypes`, `order: newestFirst` and `limit: 1`. Readers require
`filtered: true` and reject malformed replies. Neither production nor library
code falls back to `.rpi/goals.json`. Orchestrator retains its explicit environment
override and subagent exclusion; goal budgets are still charged per `TurnEnd`.

`GetSessionBranch` additionally returns a pinned `sessionId`, `leafId` (nullable)
and `scopedAppend: true`. Goal requires this capability. Each state mutation calls
`AppendEntry` immediately with an `expected` guard containing that snapshot and
the latest custom-state entry ID. The host validates session, leaf and latest state
under its message-write gate. Ordinary extension state, native transcript commits,
navigation and session switching share that gate. Navigation refuses active runs;
scoped writes refuse the recovery window, which cannot hold a gate while replaying
external tools. Callbacks and notice events run only after releasing the gate.
Direct low-level `Session` mutations are not part of this extension API contract.

Goal no longer keeps process-global pending state, pending notices or injection
fingerprints. A successful mutation is a durable state append, not an in-memory
stage. Its `notice` is a persistent outbox snapshot: UUID revision, goal payload
and optional budget-pause reason. Ordinary turn accounting retains that notice;
meaningful updates replace it. Unchanged updates create no new state or revision.
Only successful scoped acknowledgements with a nonempty entry ID count as success.
A missing/uncertain acknowledgement is an error: callers reload the branch before
making another mutation, never flush a global cached state into a different branch.

At safe turn/command boundaries the plugin reloads the current branch and delivers
its notice through generic scoped `AppendEntry` with `outboxKey`. The host dedupes
against messages on that branch since the last compaction and accepted-but-pending
context records. Reusing a key for a different payload fails. Durable reserved
message IDs acknowledge queue acceptance, not premature transcript insertion:
context waits until the assistant/tool-result batch is committed. Pending notices
only drain on a branch containing their source-state entry. A real JSONL reopen
recovers an accepted pending target and a second reopen reuses the same message ID.
A compaction can reissue the current notice because deduplication must not depend
on messages already removed from model context. This is a delivery protocol, not a
claim of exactly-once external business effects or automatic state-mutation retries.

These contracts intentionally require coordinated host/goal updates. Legacy goal
states without the outbox fields are rejected rather than silently migrated.

## Independent assistant progress and physical JSONL retirement

`SessionStorage::append_progress` and `read_progress` are required backend APIs.
New frame recorders send only `Append`, `ClearStream` and `ClearRun` through them;
`CommitStream` remains an immutable history record. Its reserved entry ID and
historical sequence are necessary for idempotent recovery and retry ordering.
Progress sequences are independent: never compare them with history cursors.
Progress does not enter `SessionState` history, record pagination, transcript
statistics or the shared entry/record ID index.

- Memory keeps shared live payloads and an active-ID index outside session state.
  Append validation is batch-sized; retirement releases payloads, oversized vector
  capacity and the retired IDs. A failed batch publishes no in-memory projection.
- SQLite uses independent `progress_heads` and `progress_frames` tables. A single
  transaction validates session/lane/IDs, assigns progress sequences and applies
  append/retirement. Reads observe the database across connections; deletes
  cascade with the owning session. Payload rows are deleted, but the database
  file need not shrink without SQLite vacuuming; this is not secure erasure.
- JSONL uses `<session>.progress.jsonl`. A checkpoint declares
  `format: assistant-progress-v1`, session identity, watermark and live frames;
  later append batches are deltas. Clear operations atomically replace the
  sidecar with a checkpoint containing only survivors. They neither delete main
  log lines nor renumber historical records. Checkpoint construction does not
  clone the payloads being retired. Every later append opens the current file,
  never a handle to the pre-rename inode.

New JSONL headers advertise `progressFormat: assistant-progress-v1`; the empty
companion is initialized before that header is written. A missing required
companion is an error, not an empty successful recovery. Unmarked JSONL is a
history-only artifact: import or fork it before recording independent progress.
CLI import copies and validates the pair, rejects foreign identity/corruption,
locks external sources, and verifies unchanged history around its sidecar copy.
The same-process case also checks history because CLI locks outlive hot switches.
Temporary import files are not session-picker candidates. Forks get a new empty
companion, deletion owns both files and staging residue, and both session listing
paths skip sidecars without parsing their potentially large checkpoints.

Recovery merges live payloads with durable stream-to-entry bindings without
comparing the two sequence domains. It persists recovered assistant/tool entries,
retires that run's payloads, then writes `OperationFinished`. Failure before the
barrier remains retryable; failure after a successful retirement does not lose
transcript identity. Successful retirement physically removes the payload from
the JSONL sidecar; its small commit binding deliberately remains in history.

A live agent-loop error (for example, a provider stream closing without a terminal
event) must flush the frame writer before reading progress for recovery. Accepted
channel writes alone are not durable progress. A failed flush propagates without
`OperationFinished` or `RunEnd`; a failed recovery entry append likewise leaves
the run open with its payload and stable binding. Once recovery has persisted the
transcript, the error path retires progress and records the original `agent_error`
outcome. Retirement failure at that point is safe to retry from terminal proof.
`ErrorRecovery` has its own optional frame-diagnostic flush counter.

`progress_error_recovery.rs` drives the real loop with a truncated provider and a
storage probe that blocks writes before the backing store's lock. Reads therefore
cannot accidentally wait for pending writes and hide the missing flush. Tests
verify no recovery read or terminal publication before release, full queued text
recovery, flush/entry failures remaining open, stable-ID recovery after reopen,
and terminal-proof cleanup after failed retirement. All four regressions failed
before the error-path barrier was added.

Normal settlement can persist `OperationFinished` even when best-effort frame
retirement fails. Required `SessionStorage::prune_completed_progress` retries that
retirement synchronously when restoring a harness or preparing a pinned session
switch. Only a durable terminal record in the same session and `(lane, run_id)`
can authorize deletion; open/suspended operations and unknown run IDs survive.
The API returns the number of removed payloads without consuming either sequence
domain or touching history/statistics. No-op cleanup does not clone payloads or
publish a checkpoint. Memory borrows history to collect terminal scope metadata;
JSONL uses the existing detached checkpoint owner and poison/reopen protocol;
SQLite uses one materialized-set delete against current persisted records, not a
possibly stale in-memory mirror or a per-payload history scan. A failed switch
leaves the old binding and queues unpublished. Tests cover all terminal outcomes,
unknown/open runs, cross-lane/session isolation, stale SQLite connections, atomic
publication failures, caller cancellation and repeated reopen.

Append errors verify the cached SHA-256 fingerprint of the entire prior prefix
and the exact attempted suffix before truncating a failed tail; normal successful
batches update the fingerprint incrementally, without rereading old payloads.
If rollback or rename outcome cannot be established, that instance is poisoned
until reopened. Append, checkpoint publication and read-time tail repair have
detached owners: cancelling their callers cannot release the gate while a write
is still running. Reopen repairs only a syntactically torn last delta, rejects
complete corrupt records, and normalizes a complete last value missing its final
newline. Unpublished `.tmp` files are not authoritative. These guarantees use the
existing filesystem write/flush/rename abstraction, not stronger power-loss or
`fsync` guarantees.

Regression coverage includes three-backend history-cursor equivalence, invalid
batches/IDs/lanes, scoped retirement, cross-connection SQLite reads, fork/delete,
recovery through the reserved assistant ID followed by two reopens, partial and
full-write errors, failed rollback, checkpoint staging/rename failures, caller
cancellation during append/rename/read repair, and torn/corrupt tails. A native
JSONL workload (256 payload records plus one survivor) measures actual sidecar
bytes: **330,924 before retirement, 1,385 after**, with the 109-byte main history
unchanged. This is a synthetic file-size result, not an RSS or production-speed
claim.

## Deliberate boundaries

- Historical embedded JSONL frame records are not migrated or physically removed.
  Physical retirement applies to the new independent progress store.
- Failed retirement retains payloads (or requires reopen on uncertain publication)
  rather than risking committed transcript data; restore/session-switch cleanup
  retries using durable terminal proof. No background GC worker or goal/delegation
  policy change is introduced.
- SQLite still persists a whole snapshot on history mutation; progress writes no
  longer rewrite that snapshot, but other history writes retain the existing model.
- Lifecycle events and extension business journals are not redundant chat copies
  and have not been deleted or merged.
- Only the synthetic file-size reduction above was measured; no RSS or production
  throughput improvement is claimed.
- The host and adjacent Langfuse changes should ship together. There is no
  compatibility fallback to infer final settlement from `MessageEnd`.

## Validation

Host: `cargo test --workspace --locked --offline -- --test-threads=1` and
`cargo fmt --all -- --check`. Serial execution avoids an existing process-env
race between child-budget and environment-parsing tests; the parallel failure
log is retained separately from the final passing regression log.
Adjacent packages: library tests for durable, todo, goal, orchestrator, run-stats,
Langfuse, permission-gate, permissions, extcore, MCP core and ACP. Final results:
2641 host tests passed (8 ignored), and 434 package library tests passed (2 ignored).
The targeted recovery/context/JSONL suite plus harness library tests passed 397
checks. The 29 live-recovery/fallback/order/execution/identity/contract tests also
passed 30 repeated runs (870 executions) with `--test-threads=29` on all three
integration binaries.
One of the ignored host tests is the separately invoked native goal DLL smoke below, which passed.
No networked Langfuse smoke or installed-user-environment rollout was performed.

Build the matching package checkout's goal cdylib with `cargo build -p rpi-goal
--locked --offline`, then invoke the host's opt-in smoke (Windows example):

```bash
RPI_GOAL_SMOKE_DLL='/absolute/path/to/debug/rpi_goal.dll' \
  cargo test -p rpi-cli --lib native_goal_outbox_smoke --locked --offline \
  -- --ignored --test-threads=1
```

The smoke loads the real DLL through public CLI session construction, executes
native ABI goal tools, switches branches and JSONL sessions without goal events,
exercises turn budgets and idempotent notices, and reloads the plugin. It uses no
provider request, network access or background goal worker.

Clippy is not a green check: strict mode stops on existing SDK lints, and default
mode also stops at the pre-existing `rpi-extensions/src/actions.rs` raw-pointer
argument lint. No unrelated lint suppression was added.
