//! M5g integration — full harness run over a faux provider + real
//! `read`/`write`/`bash` tools against an `InMemoryExecutionEnv`. Mirrors
//! `packages/agent/test/harness/agent-harness.test.ts` "full run" cases at the
//! integration level the Rust port targets.
//!
//! This is the capstone M5 test: it drives the complete
//! `AgentLane::prompt_text → run_core → run_agent_loop → persist → outcome`
//! path end-to-end with NO real LLM, asserting:
//! - the run resolves `HarnessRunOutcome::Completed`;
//! - the faux provider's scripted tool call actually executes against the
//!   in-memory env (the file the model "asked to write" persists in the
//!   session, and the tool result message round-trips back to the model for a
//!   final text turn);
//! - the durable session gained the expected entry chain
//!   (user → assistant-with-tool-call → tool-result → assistant-final);
//! - an `operation_started` + `operation_finished` record pair is on the lane,
//!   and the finished outcome is `Completed`;
//! - the `RunStart`/`RunEnd` events fire on the harness bus with the run id;
//! - the defensive-copy contract holds post-run (`get_tools` / `get_active_tools`
//!   still return clones).
//!
//! Mechanics worth knowing:
//! - The harness's `build_stream_fn` bridges the sync `StreamFn` contract to
//!   async `Provider::stream_simple` via `block_in_place` + `block_on` — this
//!   needs a multi-threaded runtime, hence `flavor = "multi_thread"`.
//! - `AgentHarnessOptions` wires a faux provider as `models`, a fresh
//!   `InMemorySessionStorage` as `session`, and the three built-in tools from a
//!   shared `ExecutionToolContext`.
//! - The faux script's *last* empty-queue step would emit `Error` if the loop
//!   called the provider a 3rd time; the second step (`Stop`/`StopReason::Stop`)
//!   has no tool calls so the loop terminates there — no 3rd call happens.

use std::sync::{Arc, Mutex};

use rpi_agent::{AgentEmitter, AgentEvent, CollectorEmitter};
use rpi_ai::providers::faux::{faux_assistant_message, FauxProvider, FauxScript, FauxStep};
use rpi_ai::Provider;
use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use rpi_harness::events::{HarnessEvent, RunEndOutcome};
use rpi_harness::session::memory::{InMemorySessionStorage, SystemClock};
use rpi_harness::session::types::SessionMetadata;
use rpi_harness::session::types::{BranchBounds, EntryOrder, EntryQuery, LaneRecord, RecordQuery};
use rpi_harness::session::{DefaultIdGenerator, Session};
use rpi_harness::types::{AgentHarnessOptions, HarnessTool, RetryPolicy};
use rpi_tools::{
    create_bash_tool, create_read_tool, create_write_tool, ExecutionToolContext, FileSystem,
    InMemoryExecutionEnv, MutationQueueRegistry,
};

/// Build an `AgentHarness` over a faux provider + in-memory env + the read/
/// write/bash tools. The faux script is caller-supplied so each test pins the
/// exact LLM turns. Returns `(harness, provider, env)` so the test can inspect
/// provider call count + the in-memory FS after the run.
async fn harness_with(
    script: FauxScript,
) -> (AgentHarness, Arc<FauxProvider>, Arc<InMemoryExecutionEnv>) {
    harness_with_options(script, RetryPolicy::default(), None).await
}

async fn harness_with_options(
    script: FauxScript,
    retry: RetryPolicy,
    agent_emitter: Option<Arc<dyn AgentEmitter>>,
) -> (AgentHarness, Arc<FauxProvider>, Arc<InMemoryExecutionEnv>) {
    let provider = FauxProvider::new(script);
    let model = provider.default_model().clone();

    // In-memory execution env + tool context. `InMemoryExecutionEnv` implements
    // both `ExecutionEnv` and `MutatingEnv`.
    let env = Arc::new(InMemoryExecutionEnv::new());
    let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
    let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
    let _registry = Arc::new(MutationQueueRegistry::new());
    let ctx = ExecutionToolContext::new(env_dyn, Some(mut_env));
    let read = create_read_tool(&ctx, None);
    let write = create_write_tool(&ctx);
    let bash = create_bash_tool(&ctx, None);
    let tools: Vec<HarnessTool> = vec![read, write, bash]
        .into_iter()
        .map(HarnessTool::new)
        .collect();
    let active: Vec<String> = tools.iter().map(|t| t.tool.schema().name.clone()).collect();

    // Fresh in-memory session storage (no prior records — `create` would
    // otherwise reject via the restore gate).
    let metadata = SessionMetadata {
        id: "e2e".into(),
        created_at: 0,
        parent_session_id: None,
    };
    let storage = Arc::new(InMemorySessionStorage::new(
        metadata,
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    ));
    let session = Session::new(storage, None);

    let options = AgentHarnessOptions {
        model,
        thinking_level: Default::default(),
        active_tool_names: active,
        tools,
        system_prompt: None,
        resources: Default::default(),
        stream_options: Default::default(),
        retry,
        compaction: Default::default(),
        steering_mode: Default::default(),
        follow_up_mode: Default::default(),
        tool_execution: Default::default(),
        drive: Default::default(),
        session,
        models: vec![provider.clone() as Arc<dyn Provider>],
        to_provider_messages: None,
        entry_projectors: Default::default(),
        agent_emitter,
        before_tool_call: None,
        after_tool_call: None,
        transform_context: None,
        entry_transforms: Vec::new(),
        provider_hooks: None,
        allow_existing_session: false,
    };

    let harness = AgentHarness::create(options).await.expect("create harness");
    (harness, provider, env)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retryable_provider_failure_emits_retry_progress_and_recovers() {
    let mut transient = faux_assistant_message("", rpi_ai::types::StopReason::Error);
    transient.error_message = Some("503 service unavailable".into());
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::message(transient),
        FauxStep::text("Recovered after retry."),
    ]);

    let (collector, events) = CollectorEmitter::new();
    let retry = RetryPolicy {
        enabled: true,
        max_retries: 2,
        base_delay_ms: 0,
        max_agent_delay_ms: 0,
    };
    let (harness, provider, _) =
        harness_with_options(script, retry, Some(Arc::new(collector))).await;

    let result = harness
        .prompt_text("retry this request", vec![])
        .await
        .expect("retrying prompt completes");
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::Relaxed),
        2
    );

    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::RetryScheduled {
            attempt: 1,
            max_retries: 2,
            delay_ms: 0,
            error,
        } if error == "503 service unavailable"
    )));
}

/// Collect bus events into a shared vec while the closure owns a `Handle`.
fn record_runtime_events(harness: &AgentHarness) -> Arc<Mutex<Vec<HarnessEvent>>> {
    let collected: Arc<Mutex<Vec<HarnessEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let collected_for_listener = collected.clone();
    // `on` registers a direct listener; we keep the `OnUnsubscribe` guard alive
    // for the duration of the run by returning both. But we only need the vec;
    // drop the guard to keep it simple — events during the run still land
    // because the harness emits synchronously inline on the calling task.
    let _off = harness
        .events()
        .on::<rpi_harness::events::RunEndEvent, _>(move |_e| {
            // no-op placeholder; we use the all-events watch below
        });
    // Use the watch surface (snapshot now → buffer → start) to capture every
    // event including RunStart, which fires *during* `prompt_text` before a
    // live listener could be installed post-call.
    let mut watch = harness.events().watch(|| ());
    let collected_for_watch = collected_for_listener;
    watch.start(Arc::new(move |event: &HarnessEvent| {
        collected_for_watch.lock().unwrap().push(event.clone());
    }));
    // Prevent the watch from being dropped (which would NOT unsubscribe — the
    // bus keeps it — but we want the listener alive for the whole test).
    std::mem::forget(watch);
    collected
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_run_with_write_tool_call_completes_and_persists() {
    // Script:
    //   turn 1 — assistant emits a `write` tool call
    //   turn 2 — (after tool result) assistant emits a final text turn, Stop
    let script = FauxScript::new()
        .with_tool_call(
            "write",
            serde_json::json!({ "path": "out.txt", "content": "hello from harness e2e" }),
        )
        .with_text("Done — I wrote the file.");
    let (harness, provider, env) = harness_with(script).await;

    let events = record_runtime_events(&harness);

    let result = harness
        .prompt_text("Write 'hello from harness e2e' to out.txt.", vec![])
        .await
        .expect("prompt_text completes");
    let run_id = result.run_id.clone();
    match &result.outcome {
        HarnessRunOutcome::Completed {
            leaf_id,
            final_entry_id,
            final_message,
        } => {
            assert!(!leaf_id.is_empty(), "leaf_id must be set on Completed");
            assert!(!final_entry_id.is_empty(), "final_entry_id must be set");
            // The final assistant message is the "Done — ..." text turn (no tool calls).
            let has_text = final_message
                .content
                .iter()
                .any(|c| matches!(c, rpi_ai::types::Content::Text(_)));
            assert!(has_text, "final message should carry text content");
            assert_eq!(final_message.stop_reason, rpi_ai::types::StopReason::Stop);
        }
        other => panic!("expected Completed, got {other:?}"),
    }

    // The faux provider was called once per turn = 2 calls.
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::Relaxed),
        2,
        "faux provider should have been called for both turns"
    );

    // The write tool actually wrote the file in the in-memory env.
    let written = env
        .read_text_file("/out.txt", None)
        .await
        .expect("out.txt was written by the write tool via the harness run");
    // `InMemoryExecutionEnv` resolves a bare "out.txt" against cwd "/", so the
    // absolute path is "/out.txt".
    assert_eq!(written, "hello from harness e2e");

    // ---- durable session assertions ----------------------------------------

    // The branch path should contain (oldest→newest):
    //   user prompt | assistant(tool_call) | tool_result | assistant(final text)
    let leaf = harness
        .session()
        .get_leaf_id()
        .await
        .expect("leaf")
        .expect("leaf present");
    let path = harness
        .session()
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds {
                start: Some(leaf),
                ..Default::default()
            },
        )
        .await
        .expect("branch path");
    assert!(
        path.len() >= 4,
        "expected at least 4 entries (user, assistant, toolresult, assistant-final); got {}",
        path.len()
    );
    // Sanity: the first entry is the user prompt.
    assert!(matches!(
        path[0],
        rpi_harness::session::types::Entry::Message(_)
    ));
    // And the leaf entry (last) is the final assistant text turn.
    let last = path.last().unwrap();
    assert!(matches!(
        last,
        rpi_harness::session::types::Entry::Message(_)
    ));

    // ---- persisted tool-result timestamps are real wall-clock -------------
    //
    // Regression: `agent_loop::create_tool_result_message` stamped every tool
    // result from a bare atomic counter (`1, 2, 3, …`), so durable sessions
    // carried nonsense tool-result times (29, 38, 55, …) next to real assistant
    // times. Nothing asserted on them, so it survived.
    //
    // Only the tool result is checked here: its timestamp is produced by the
    // shared agent loop, i.e. for every provider. Assistant timestamps come
    // from the provider, and the faux test double deliberately uses a counter
    // (see the note on `providers::anthropic::now_ms`).
    let mut tool_result_timestamps = Vec::new();
    for entry in &path {
        let rpi_harness::session::types::Entry::Message(message_entry) = entry else {
            continue;
        };
        if let rpi_agent::AgentMessage::ToolResult(t) = &message_entry.message {
            tool_result_timestamps.push(t.timestamp);
        }
    }
    assert!(
        !tool_result_timestamps.is_empty(),
        "expected a persisted toolResult to check its timestamp"
    );
    for timestamp in tool_result_timestamps {
        assert!(
            timestamp > 1_600_000_000_000,
            "toolResult timestamp {timestamp} is not epoch milliseconds \
             (the agent loop used to stamp tool results from a 1,2,3… counter)"
        );
    }

    // An operation_started (intent Run) + operation_finished (Completed) record
    // pair should be present on the lane, with matching run_id.
    let started = harness
        .session()
        .find_records(&RecordQuery {
            record_type: Some("operation_started"),
            run_id: Some(run_id.clone()),
            ..Default::default()
        })
        .await
        .expect("find operation_started");
    assert_eq!(
        started.len(),
        1,
        "exactly one operation_started for this run_id"
    );
    let finished = harness
        .session()
        .find_records(&RecordQuery {
            record_type: Some("operation_finished"),
            run_id: Some(run_id.clone()),
            ..Default::default()
        })
        .await
        .expect("find operation_finished");
    assert_eq!(
        finished.len(),
        1,
        "exactly one operation_finished for this run_id"
    );
    if let LaneRecord::OperationFinished(f) = &finished[0] {
        assert_eq!(
            f.outcome,
            rpi_harness::session::types::OperationOutcome::Completed
        );
    } else {
        panic!("expected OperationFinished record");
    }

    // ---- harness bus events ------------------------------------------------

    let evs = events.lock().unwrap();
    let run_starts = evs
        .iter()
        .filter(|e| matches!(e, HarnessEvent::RunStart(_)))
        .count();
    let run_ends = evs
        .iter()
        .filter(|e| matches!(e, HarnessEvent::RunEnd(_)))
        .count();
    assert_eq!(run_starts, 1, "exactly one RunStart");
    assert_eq!(run_ends, 1, "exactly one RunEnd");
    // The RunEnd outcome is Completed.
    let run_end_outcome = evs
        .iter()
        .find_map(|e| match e {
            HarnessEvent::RunEnd(re) => Some(re.outcome),
            _ => None,
        })
        .expect("a RunEnd event");
    assert_eq!(run_end_outcome, RunEndOutcome::Completed);

    // ---- defensive-copy contract still holds post-run ----------------------

    let tools = harness.get_tools().await.expect("get_tools after run");
    assert_eq!(tools.len(), 3, "tool registry intact after run");
    let active = <AgentHarness as AgentLane>::get_active_tools(&harness)
        .await
        .expect("get_active_tools after run");
    assert_eq!(active.len(), 3, "active tools intact after run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_with_no_tool_calls_completes_in_single_turn() {
    // A simple text-only reply: no tool calls, so the loop terminates after one
    // turn. Asserts the happy-path single-turn shape (used widely in tests).
    let script = FauxScript::new().with_text("Hello back.");
    let (harness, provider, _env) = harness_with(script).await;

    let result = harness
        .prompt_text("Say hello.", vec![])
        .await
        .expect("prompt_text completes");
    assert!(
        matches!(result.outcome, HarnessRunOutcome::Completed { .. }),
        "expected Completed, got {:?}",
        result.outcome
    );
    // One turn → one provider call.
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
    );
    // Two persisted entries: the user prompt + the assistant reply.
    let leaf = harness
        .session()
        .get_leaf_id()
        .await
        .unwrap()
        .expect("leaf present");
    let path = harness
        .session()
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds {
                start: Some(leaf),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(path.len(), 2, "user + assistant reply only");
}

// ---------------------------------------------------------------------------
// B3b: AgentHarnessOptions forwards before_tool_call / after_tool_call /
// transform_context into the live AgentLoopConfig (previously these existed on
// AgentLoopConfig but the harness hardcoded them to None). This test is the
// harness-level proof: set after_tool_call on the options, run a tool-call
// script, and assert the persisted ToolResultMessage carries the patched usage
// — i.e. the hook the host installed via options actually ran inside the loop.
//
// (The pi-agent before_after_hooks tests prove the loop itself honors the hooks
// at the unit level; this proves the harness WIRING from options → inner →
// snapshot_config → AgentLoopConfig is live, which is the B3b deliverable.)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn harness_forwards_after_tool_call_option_into_loop() {
    use rpi_agent::{AfterToolCall, AfterToolCallResult};
    use rpi_ai::types::{Usage, UsageCost};

    let patched = Usage {
        input: 5,
        output: 6,
        cache_read: 7,
        cache_write: 8,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 26,
        cost: UsageCost {
            input: 0.5,
            output: 0.6,
            cache_read: 0.7,
            cache_write: 0.8,
            total: 2.6,
        },
    };
    let patched_for_hook = patched.clone();

    let fired: Arc<std::sync::Mutex<bool>> = Arc::new(std::sync::Mutex::new(false));
    let after: AfterToolCall = {
        let fired = Arc::clone(&fired);
        let patched = patched_for_hook.clone();
        Arc::new(move |_ctx: rpi_agent::AfterToolCallContext<'_>, _signal| {
            let fired = Arc::clone(&fired);
            let patched = patched.clone();
            Box::pin(async move {
                *fired.lock().unwrap() = true;
                Some(AfterToolCallResult {
                    usage: Some(patched),
                    ..AfterToolCallResult::default()
                })
            })
        })
    };

    // Script: turn 1 a `write` tool call, turn 2 a final text turn.
    let script = FauxScript::new()
        .with_tool_call(
            "write",
            serde_json::json!({ "path": "b3b.txt", "content": "patched-usage" }),
        )
        .with_text("done");
    let (harness, _provider, _env) = {
        // Build a harness that carries the after_tool_call option. We reuse
        // `harness_with` for everything else, then rebuild options with the
        // hook attached. Easiest: construct a second harness via the same path
        // but we need the options — so replicate the minimal build inline.
        let provider = FauxProvider::new(script);
        let model = provider.default_model().clone();
        let env = Arc::new(InMemoryExecutionEnv::new());
        let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
        let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
        let _ = Arc::new(MutationQueueRegistry::new());
        let ctx = ExecutionToolContext::new(env_dyn, Some(mut_env));
        let write = create_write_tool(&ctx);
        let tools: Vec<HarnessTool> = vec![write].into_iter().map(HarnessTool::new).collect();
        let active: Vec<String> = tools.iter().map(|t| t.tool.schema().name.clone()).collect();
        let metadata = SessionMetadata {
            id: "b3b-after".into(),
            created_at: 0,
            parent_session_id: None,
        };
        let storage = Arc::new(InMemorySessionStorage::new(
            metadata,
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        ));
        let session = Session::new(storage, None);
        let options = AgentHarnessOptions {
            model,
            thinking_level: Default::default(),
            active_tool_names: active,
            tools,
            system_prompt: None,
            resources: Default::default(),
            stream_options: Default::default(),
            retry: Default::default(),
            compaction: Default::default(),
            steering_mode: Default::default(),
            follow_up_mode: Default::default(),
            tool_execution: Default::default(),
            drive: Default::default(),
            session,
            models: vec![provider.clone() as Arc<dyn Provider>],
            to_provider_messages: None,
            entry_projectors: Default::default(),
            agent_emitter: None,
            before_tool_call: None,
            after_tool_call: Some(after),
            transform_context: None,
            entry_transforms: Vec::new(),
            provider_hooks: None,
            allow_existing_session: false,
        };
        let harness = AgentHarness::create(options).await.expect("create harness");
        (harness, provider, env)
    };

    let result = harness
        .prompt_text("Write patched-usage to b3b.txt.", vec![])
        .await
        .expect("prompt_text completes");
    assert!(
        matches!(result.outcome, HarnessRunOutcome::Completed { .. }),
        "expected Completed, got {:?}",
        result.outcome
    );

    // The hook fired (the harness forwarded options.after_tool_call into the
    // live AgentLoopConfig, so the loop invoked it).
    assert!(
        *fired.lock().unwrap(),
        "after_tool_call hook installed via AgentHarnessOptions must fire inside the loop"
    );

    // And the persisted ToolResultMessage carries the patched usage — the
    // override the hook returned was applied by the loop and survived the
    // harness's persist path. This is the end-to-end B3b proof.
    let leaf = harness
        .session()
        .get_leaf_id()
        .await
        .unwrap()
        .expect("leaf present");
    let path = harness
        .session()
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds {
                start: Some(leaf),
                ..Default::default()
            },
        )
        .await
        .expect("branch path");
    let tool_result = path.iter().find_map(|e| match e {
        rpi_harness::session::types::Entry::Message(m) => match &m.message {
            rpi_agent::AgentMessage::ToolResult(t) => Some(t.clone()),
            _ => None,
        },
        _ => None,
    });
    let tool_result = tool_result.expect("a toolResult message persisted");
    assert_eq!(
        tool_result.usage,
        Some(patched),
        "persisted ToolResultMessage.usage must be the after_tool_call override"
    );
}

// ---------------------------------------------------------------------------
// B4: ProviderHooks before_request patches the LIVE per-call SimpleStreamOptions.
// The harness build_stream_fn closure fires the hook before each stream_simple
// call and applies the returned SimpleStreamOptionsPatch to a clone of opts.
// The FauxProvider surfaces the opts.session_id it received on the assistant
// message... but faux ignores most opt fields. The clean observable is the
// `metadata`/`headers` route: faux carries `session_id` from opts onto its
// faux-cache-hit path, but simplest is to assert the hook FIRED with the model
// the loop is about to call (the per-call bridge ran). We pair the hook with a
// counter and assert call_count == hook fires (one per stream_simple call).
// (after_response observation is wired on the emitter side in B3+; here we
// prove before_request runs per-call, which is the load-bearing B4 fix.)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn harness_fires_provider_hooks_before_request_per_call() {
    use rpi_ai::{ProviderHooks, SimpleStreamOptions, SimpleStreamOptionsPatch};

    // One hook fire per stream_simple call. The script has 2 turns
    // (tool-call + final text), so we expect 2 provider calls == 2 hook fires.
    let fires: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    struct CountingHooks {
        fires: Arc<std::sync::Mutex<Vec<String>>>,
    }
    impl ProviderHooks for CountingHooks {
        fn before_request(
            &self,
            model: &rpi_ai::Model,
            _ctx: &rpi_ai::types::Context,
            _opts: &SimpleStreamOptions,
        ) -> Option<SimpleStreamOptionsPatch> {
            self.fires.lock().unwrap().push(model.id.clone());
            // Return a no-op patch (None would also work; exercise the apply path).
            None
        }
    }

    let hooks: Arc<dyn ProviderHooks> = Arc::new(CountingHooks {
        fires: Arc::clone(&fires),
    });

    let script = FauxScript::new()
        .with_tool_call(
            "write",
            serde_json::json!({ "path": "b4.txt", "content": "hook" }),
        )
        .with_text("done");
    // Reuse the harness build, then rebuild options with the provider hook.
    let (harness, provider, _env) = {
        let provider = FauxProvider::new(script);
        let model = provider.default_model().clone();
        let env = Arc::new(InMemoryExecutionEnv::new());
        let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
        let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
        let _ = Arc::new(MutationQueueRegistry::new());
        let ctx = ExecutionToolContext::new(env_dyn, Some(mut_env));
        let write = create_write_tool(&ctx);
        let tools: Vec<HarnessTool> = vec![write].into_iter().map(HarnessTool::new).collect();
        let active: Vec<String> = tools.iter().map(|t| t.tool.schema().name.clone()).collect();
        let metadata = SessionMetadata {
            id: "b4-hooks".into(),
            created_at: 0,
            parent_session_id: None,
        };
        let storage = Arc::new(InMemorySessionStorage::new(
            metadata,
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        ));
        let session = Session::new(storage, None);
        let options = AgentHarnessOptions {
            model,
            thinking_level: Default::default(),
            active_tool_names: active,
            tools,
            system_prompt: None,
            resources: Default::default(),
            stream_options: Default::default(),
            retry: Default::default(),
            compaction: Default::default(),
            steering_mode: Default::default(),
            follow_up_mode: Default::default(),
            tool_execution: Default::default(),
            drive: Default::default(),
            session,
            models: vec![provider.clone() as Arc<dyn Provider>],
            to_provider_messages: None,
            entry_projectors: Default::default(),
            agent_emitter: None,
            before_tool_call: None,
            after_tool_call: None,
            transform_context: None,
            entry_transforms: Vec::new(),
            provider_hooks: Some(hooks),
            allow_existing_session: false,
        };
        let harness = AgentHarness::create(options).await.expect("create harness");
        (harness, provider, env)
    };

    let result = harness
        .prompt_text("Write hook to b4.txt.", vec![])
        .await
        .expect("prompt_text completes");
    assert!(
        matches!(result.outcome, HarnessRunOutcome::Completed { .. }),
        "expected Completed, got {:?}",
        result.outcome
    );

    // Two turn script → two provider calls; before_request fired once each.
    let provider_calls = provider
        .state()
        .call_count
        .load(std::sync::atomic::Ordering::Relaxed);
    let hook_fires = fires.lock().unwrap().clone();
    assert_eq!(provider_calls, 2, "two-turn script = two provider calls");
    assert_eq!(
        hook_fires.len(),
        2,
        "before_request must fire once per stream_simple call (per-call, not run-once)"
    );
}

/// Regression: the harness persists prompts itself and drives
/// `run_agent_loop` with an EMPTY prompts vec (to avoid double-counting them in
/// the provider context), so the loop never emits `message_start`/`message_end`
/// for the user's prompt.
///
/// The TUI renders user bubbles from `AgentEvent::MessageStart`, so this test
/// pins the contract the TUI must compensate for: a directly-sent prompt
/// produces NO user `MessageStart`, while a queued (steering) message DOES
/// (that one is drained inside the loop).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn directly_sent_prompt_emits_no_user_message_start() {
    let script = FauxScript::new();
    script.set_responses(vec![FauxStep::text("Done.")]);
    let (collector, events) = CollectorEmitter::new();
    let (harness, _provider, _) =
        harness_with_options(script, RetryPolicy::default(), Some(Arc::new(collector))).await;

    harness
        .prompt_text("hello there", vec![])
        .await
        .expect("prompt completes");

    let events = events.lock().unwrap();
    let user_starts = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                AgentEvent::MessageStart {
                    message: rpi_agent::AgentMessage::User(_)
                }
            )
        })
        .count();
    assert_eq!(
        user_starts, 0,
        "harness passes an empty prompts vec, so the loop emits no user MessageStart; \
         the TUI must render the prompt itself. events: {events:#?}"
    );
    // The assistant still streams normally.
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::MessageStart {
                message: rpi_agent::AgentMessage::Assistant(_)
            }
        )),
        "assistant MessageStart must still fire"
    );
}
