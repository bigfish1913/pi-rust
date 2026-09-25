# rpi 0.1.28

This release makes **crash recovery** real end to end, finishes the deferred
(long-poll) provider path, makes the steering queue durable, and wires the
harness-relevant settings keys so a `settings.json` copied from native pi
actually changes behaviour.

## Highlights

### Crash recovery and durable progress

Previously a run persisted nothing until it finished, so a crash after minutes of
work left only the user's prompt. Every crash point is now covered by a test
(`crates/pi-harness/tests/harness_run_e2e.rs`, `frame_progress_recovery.rs`).

- **Frame-level progress**: every streamed assistant frame is written to the
  **record stream** (`LaneRecord::AssistantFrame`) as it arrives. Frames are
  progress, not history — they never enter the branch the model is built from.
  On restart the committed prefix is replayed into an interrupted message.
- **Tool calls always get a result**: an assistant message whose tool calls have
  no results is rejected by every provider, so each unresolved call is either
  replayed (only when the recorded policy *and* the current tool declaration both
  say `Safe`, using the arguments read back from `tool_started`) or recorded as
  "outcome unknown".
- **Commit-on-settle**: an assistant message carrying tool calls is committed the
  moment it settles, with a reserved entry id recorded in a `step_attempt`; its
  `tool_started` frame can then be written before the tool runs. Committing also
  retires that stream (`ClearStream`), which is what stops a crash in a later turn
  from replaying — and duplicating — the earlier ones.
- **Startup continuation**: a run with no new prompt *is* a continuation. Three
  entry points are recognised — before the first request, during tool execution,
  and while waiting to retry — each bounded by a resume budget so a run that keeps
  dying cannot restart the work forever.
- **Retry intent**: crashing during retry backoff now **retries** instead of
  salvaging the failed attempt as history. This needs its own record
  (`retry_pending`): frames carry no terminal stop reason, and a failed attempt
  leaves no other trace.
- **Queue durability**: steering, follow-up and `nextRun` messages are recorded
  on enqueue and rebuilt on start, so a message typed while the agent works is no
  longer lost with the process. Injected messages are committed at their own
  settle, and cancelling or clearing the queue is recorded too.

### Deferred (long-poll) provider responses

`resume_deferred` was `not_implemented`; the whole suspend path now works.

- `pi-ai`: new `DeferredProvider` capability plus a **defaulted**
  `Provider::deferred()`, so the other provider implementations need no stub.
- `pi-agent`: new `run_agent_loop_from_assistant`. A polled assistant message
  arrives from outside the loop, so its tool calls have not run — and a request
  carrying unanswered tool calls is rejected. That entry point executes them
  first, in a single attempt (retrying would run the tools twice).
- `pi-harness`: `resume_deferred` polls, records the result, stays suspended when
  the provider hands back another handle, and otherwise settles the parked
  operation and continues.

### Settings parity

`retry`, `compaction`, `steeringMode` and `followUpMode` now come from
`settings.json` (they were hardcoded defaults, so a native `settings.json` parsed
cleanly and did nothing). `sessionDir`, `shellCommandPrefix` and `httpProxy` are
wired, and native's key names `enabledModels` / `httpIdleTimeoutMs` are accepted.

### Documentation

- `docs/llm-repetition-forensics.md` §11: the full evidence chain, including a
  per-state comparison against native pi's durable state machine (which concluded
  the state machine itself is not needed — rpi derives the same outcomes from the
  branch plus the record stream) and the three places where earlier conclusions in
  that document were wrong and were corrected.
- `docs/native-pi-missing-features.md`: §7's deferred caveat closed, §15's stale
  "settings fields are unconsumed" claim replaced with a verified three-way split
  (renamed keys / reachable elsewhere / capability genuinely absent).

## Verification

- `cargo fmt --all -- --check`
- `cargo check --workspace --all-targets --locked`
- `cargo test --workspace --locked` — 63 suites, ~1276 tests, 0 failures, 0 warnings
- `task dry-run RELEASE_VERSION=0.1.28`

## Install

```bash
cargo install rpi-cli --version 0.1.28
```
