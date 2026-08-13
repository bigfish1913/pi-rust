# M5f (pi-harness `AgentHarness` run loop) open questions & divergences

> Written per the user's instruction "中间有问题，写文档，我睡醒统一处理"
> (if problems arise mid-way, write them to a doc; I'll review them on waking).
> These are the design divergences from the TS reference that the M5f port
> (`crates/pi-harness/src/agent_harness.rs`) introduced. None are blockers —
> all are recorded for review. The run loop builds and the M5 test suite stays
> green; the items below are deliberate v1 simplifications, not defects.

## Context recap

The TS `packages/agent/src/harness/agent-harness.ts` is a **stub**: every
operation rejects with `HarnessNotImplemented`. Per plan §2.3 refinement #2,
the Rust port treats that file as a *type contract only* and implements a real
loop on top of `pi_agent::run_agent_loop`. So the majority of the file is new
Rust, not a line-by-line port — the divergences below are about *how* the real
loop is wired, not about translating code that exists.

---

## 1. Deferred operations are detected, but `resume` is not implemented

**Where:** `AgentHarness::run_core` → `derive_outcome` (the `StopReason::Deferred`
+ `deferred: Some(handle)` arm produces `HarnessRunOutcome::Suspended`).

**What.** The harness correctly detects that a terminal assistant message
 parked on a deferred provider response and:
- does **not** write `operation_finished` (the operation stays "open"),
- does **not** emit `RunEnd` (a suspended run never emits `run_end`, matching
  `events.rs`'s `RunOutcome::Suspended` contract),
- returns `RunResult { outcome: Suspended { deferred, .. } }` to the caller.

What it does **not** do is implement `resume(deferred_handle)` — the
`AgentLane` trait method exists but, like the other deferred/v1 methods, is
parked. Resuming a deferred run would require re-entering `run_agent_loop` with
the provider's deferred-resume protocol, which is provider-specific plumbing
that v1 does not need (no provider in the v1 set emits deferred handles in
practice — faux/Anthropic immediate responses only).

**Why parked.** The deferred-response path is a provider capability the v1
provider set does not exercise. Wiring `resume` now would be dead, untestable
code. The detection + open-operation invariant (at most one open op per lane,
recoverable via `find_open_operations`) is the load-bearing part and is done.

**To revisit.** When a provider that emits deferred handles is added, implement
`resume` as a `run_core`-shaped re-entry that skips prompt persistence, reuses
the open `operation_started`, and drives `run_agent_loop` with the resume
context. The session-recovery story (an open `operation_started` with no
`operation_finished` on reload) already encodes "this run is suspended."

---

## 2. Non-main lanes reject every run operation with `InvalidLane`

**Where:** `LaneHandle`'s `AgentLane` impl — `prompt_text`/`prompt_message`/
`prompt_messages`/`skill`/`prompt_from_template`/`compact`/`navigate_tree`/
`resume`/`steer`/`follow_up`/`next_run`/`cancel_queued` all return
`HarnessError::invalid_lane(lane, "non_main_lane", "The run loop is only
available on the main lane in v1")`.

**What.** The TS harness lets any lane drive a run. The Rust port restricts the
run loop to `"main"` in v1. Non-main `LaneHandle`s still implement the **read**
API (`get_leaf_id`, get/set `model`/`thinking_level`/`active_tools`,
`record_usage`, `wait_for_idle`, `peek_action`/`execute_action`/`run_to_completion`
where applicable) against the shared `HarnessInner`, so they're not useless —
they just can't *initiate* a run.

**Why parked.** A run on a non-main lane is a branch run: it persists to that
lane's leaf chain and builds its context from that lane's branch path. The
plumbing (`branch_path_oldest_first` is main-lane-hardcoded to `"main"`; the
`operation_started` intent writes `lane: "main"`) would all need parameterizing.
That's straightforward but currently unused — v1 is single-lane. Restricting
upfront keeps the contract honest rather than silently writing non-main runs to
the main lane.

**To revisit.** Parameterize `run_core`/`compact_core`/`acquire_run` by lane
name; have `branch_path_oldest_first` take the lane; have `LaneHandle` delegate
to a shared `run_core_on_lane(lane)`. The `LaneHandle` already holds the
`Session` + `HarnessInner` + bus, so the plumbing is there.

---

## 3. `create.restore` rejects with `HarnessError::Io`, not a typed "restore" error

**Where:** `AgentHarness::create` — `find_records({limit:1})` non-empty →
`HarnessError::Io("create.restore is not implemented: session already has
records")`.

**What.** TS `create` throws `HarnessNotImplemented("create.restore")`. The
Rust `HarnessError` enum has no `NotImplemented` variant (the TS stub's
catch-all was intentionally not ported — the Rust harness isn't a stub). The
closest fit is `Io`, used for "session backend capability not present" in
keeping with how session-storage failures map.

**Why this mapping.** `HarnessError` is the 17-variant set from plan §1; adding
a `NotImplemented` variant for a single v1 gate would be noise. `Io` already
carries "the storage/backend can't do what you asked" semantics for the
JSONL/InMemory backends. The message string makes the cause explicit.

**To revisit.** If restore-from-existing-session is implemented (rebuild
`HarnessInner` from the last leaf + open-operation state), this gate flips to
the real restore path and the `Io` mapping is moot. If we instead want a
permanent "not implemented" surface for other deferred features, consider a
`NotImplemented { feature: &str }` variant then.

---

## 4. Queues, manual driving, and suspended-run recovery return `NoActiveRun`

**Where:** `LaneHandle` + `AgentHarness` `steer`/`follow_up`/`next_run`/
`cancel_queued` return `HarnessError::no_active_run("main", "<feature> is not
implemented in v1")`; `run_when_idle`/`peek_action`/`execute_action`/
`run_to_completion` likewise reject.

**What.** The TS harness has a rich action queue (steering mid-turn, follow-up
after a turn, queued runs, manual `driving` mode where the caller peeks/actions
instead of auto-driving) and an aborted/suspended-run recovery flow. None of
that is wired in v1: the harness drives one run to completion (or suspend/
abort) synchronously inside `run_core` and returns. `DrivingMode` is stored on
`HarnessInner` (defaulted `Automatic`) but never consulted — the field is
`#[allow(dead_code)]` so the contract mirrors TS `AgentHarnessOptions.drive`.

**Why parked.** The underlying `pi_agent` queue machinery (`PendingMessageQueue`,
steering/follow-up drain) is implemented and tested (M2). The harness just
doesn't expose it: a single synchronous `prompt → outcome` is the v1 surface,
and the queue would only matter for multi-turn interactive sessions. Wiring
steering/follow-up means: (a) an `enqueue` API that doesn't acquire the run
guard, (b) the run loop consulting the queue between turns (it already can via
`AgentLoopConfig`'s `get_steering_messages`/`get_follow_up_messages` hooks —
currently no-ops), (c) a deferred-run model where `prompt` returns immediately
with a run-id and the caller polls `wait_for_idle`.

**To revisit.** When the CLI (next milestone) needs interactive multi-turn, wire
the queue: expose `steer`/`follow_up` as enqueue-onto-`PendingMessageQueue`,
supply `get_steering_messages`/`get_follow_up_messages` closures to
`AgentLoopConfig` that drain it, and decide the synchronous-vs-deferred run
model. The `Manual` driving mode then also becomes meaningful.

---

## 5. `navigate_tree` returns `Declined` with the current leaf

**Where:** `AgentHarness::navigate_tree` — returns
`NavigationOutcome::Declined { leaf_id: <current leaf> }` without writing any
operation record.

**What.** TS `navigateTree` creates a new lane by forking from a target entry,
optionally summarizing the abandoned branch tail into a `branch_summary` entry,
and returns a `NavigationOutcome` pointing at the new lane. The Rust port
returns `Declined` — "I won't navigate" — and does nothing. It doesn't even
write `operation_started` (so it's a pure no-op read, not a recorded
operation).

**Why parked.** Navigation is the most harness-specific operation and the one
with the weakest test coverage in the TS suite (it's entangled with the branch
model). The underlying pieces *are* ported: `Session` lane creation/fork, the
`branch_summary` compaction path (`compaction/branch_summary.rs`), and the
`BranchSummary` custom message. What's missing is the orchestration: create the
target lane, optionally run `branch_summary` over the abandoned tail, switch the
"active" lane, and return the new leaf. That's a half-day of focused work that
v1 (single-lane) doesn't need.

**To revisit.** Implement `navigate_tree` as: `acquire_run(Navigation)` →
optionally `branch_summary` the tail → `session.fork`/`create_lane` → write
`operation_started` with `Navigation` intent → return `Navigated { lane, leaf }`.
The `branch_summary.rs` helper + `Session` fork API already exist.

---

## 6. Run-loop compaction is *pre-run only*; no mid-turn re-compaction

**Where:** `AgentHarness::run_core` evaluates `should_compact` **once**, before
the run, against the branch path including the just-persisted prompts. If it
triggers, `prepare_compaction` + `compact` run, the `Compaction` entry is
persisted, and the branch path is rebuilt before `run_agent_loop`.

**What's NOT done.** The TS harness re-evaluates compaction *between turns*
during a multi-turn run — a long run can itself push the context over the
reserve mid-flight. The Rust port does not: a single `run_agent_loop` call is
one turn-batch from the harness's view, and the loop's internal multi-turn
expansion (steering/follow-up) does not re-check compaction between its turns.
If a run grows the context past the window mid-loop, the provider will
eventually error on length (surfaced as `HarnessRunOutcome::Failed`).

**Why parked.** `run_agent_loop` is a sealed call from the harness's side — it
returns `NewMessages` only at the end. Mid-turn compaction would require either
(a) a compaction callback hook on `AgentLoopConfig` (doesn't exist) or (b) the
harness driving turn-by-turn itself (replacing `run_agent_loop` with a harness
turn loop). Pre-run compaction covers the common case (resuming a long session).
Mid-turn compaction is an optimization for very long single runs, which v1
(where runs are single-prompt/single-batch) doesn't hit.

**To revisit.** Either add a `should_compact_between_turns` hook to
`AgentLoopConfig` that the loop consults (cleaner, keeps the loop reusable), or
have the harness drive turns itself when `driving == Automatic` + compaction is
enabled (more control, duplicates loop logic). Option (a) is preferable.

---

## 7. `convert_to_llm` is the harness-level default; custom-message roles beyond the registry are dropped

**Where:** `AgentHarness::build_convert_to_llm` — uses the caller's
`to_provider_messages` override if provided, else wraps
`crate::messages::convert_to_llm`.

**What.** This matches the TS contract exactly (harness `convert_to_llm`
dispatches per role, dropping unregistered custom roles). No divergence —
recorded here only because it's the seam where a caller's custom
`CustomMessageRenderer` entries (registered on the `Agent`) would need to be
threaded through if the harness ever stops using `pi_agent`'s built-in
registry. Currently the harness rebuilds an `AgentContext` with tools only; the
agent's renderer registry isn't consulted because the harness's own
`convert_to_llm` is what feeds the provider. This is fine for the shipped roles
(`bashExecution`/`branchSummary`/`compactionSummary`) but means a caller who
registers a *new* custom role on the agent won't see it converted unless they
also supply `to_provider_messages`.

**To revisit.** If custom roles beyond the shipped three are needed, either (a)
have the harness `convert_to_llm` consult a renderer registry the caller
supplies via `AgentHarnessOptions`, or (b) document that custom roles require a
`to_provider_messages` override. (b) is the v1 posture.

---

## 8. `StreamFn` sync/async bridge uses `block_in_place` + `block_on`

**Where:** `AgentHarness::build_stream_fn` — `tokio::task::block_in_place(|| {
Handle::current().block_on(provider.stream_simple(...)) })`.

**What.** `StreamFn` must return `AssistantMessageEventStream`
*synchronously* (plan invariant #11). Provider `stream_simple` is async. The
bridge blocks the worker thread on the async stream init, which is the same
pattern `examples/minimal`'s `make_stream_fn` uses. This requires the runtime
to be multi-threaded (`block_in_place` panics on a current-thread runtime); the
harness is documented as requiring `rt-multi-thread`.

**Why this is fine but worth noting.** `block_in_place` moves the *current*
thread out of the work-stealing set while blocked, so it doesn't deadlock the
runtime as long as other workers can poll the spawned producer task. The
producer task (inside `create_assistant_message_event_stream`) is spawned onto
the runtime Handle, so it runs on another worker. This matches the example and
the M2 agent tests. The risk is only if a caller runs the harness on a
current-thread runtime — which the docs should call out.

**To revisit.** If a current-thread-runtime-friendly bridge is needed, the
alternative is to spawn the producer task and use a *sync* channel the producer
pushes to — but `AssistantMessageEventStream` is already an mpsc+oneshot
construct, so the spawn-then-return-synchronously shape is inherent. The
`block_in_place` bridge is the cleanest fit; just document the runtime
requirement.

---

## Summary: what *is* done in M5f

- `AgentHarness` + `LaneHandle` with shared `HarnessInner` (config + run guard)
  behind `Arc<Mutex>`, defensive-copy setters/getters (Clone in, Clone out).
- `AgentLane` trait fully implemented for `"main"`; non-main lanes implement the
  read API and reject run ops (divergence #2).
- `run_core`: acquire run guard → `RunStart` → `operation_started` → persist
  prompts → config snapshot → pre-run compaction (#6) → build branch context →
  wrap `convert_to_llm` → build `StreamFn` → drive `run_agent_loop` → persist
  new messages → derive outcome → `operation_finished` (skip if suspended) →
  `RunEnd` (skip if suspended) → release guard.
- Outcome derivation from terminal assistant `stop_reason`:
  `Error→Failed`, `Aborted→Aborted`, `Deferred+handle→Suspended` (#1),
  else `Completed`.
- `compact_core`: `prepare_compaction` → `compact` → persist `Compaction` entry
  → `operation_finished` → `CompactionResult`. Honors split-turn two-LLM-call
  invariant (plan #10) via the existing `compact`.
- `record_usage`, `abort`, `wait_for_idle`, get/set accessors all wired for
  `main`; `LaneHandle` shares the accessors.
- Build GREEN; existing 186 M5 tests stay green.
