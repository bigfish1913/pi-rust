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

use rpi_agent::{AgentEmitter, AgentEvent, AgentMessage, CollectorEmitter};
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

/// The loop's only other exit is "the model stopped asking for tools", which
/// nothing bounds — one observed session ran 112 turns / 867 s in a single run
/// (`docs/llm-repetition-forensics.md` §二). The run budget is the backstop:
/// when a model keeps emitting tool calls forever, the run must end on a known
/// ceiling and say so, so a truncated task is never mistaken for a finished one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_budget_stops_a_looping_run_and_records_why() {
    // A model that never stops asking for tools: far more scripted turns than
    // the ceiling, so only the guard can end this run.
    let script = FauxScript::new();
    let step = || FauxStep::tool_call("bash", serde_json::json!({ "command": "echo tick" }));
    script.set_responses((0..200).map(|_| step()).collect());
    let (harness, provider, _env) = harness_with(script).await;

    let result = harness
        .prompt_text("loop until the budget stops you", vec![])
        .await
        .expect("the run must terminate, not spin");

    // It terminated "Completed" (the loop's normal exit path) — which is exactly
    // why the reason has to be recorded separately.
    assert!(
        matches!(result.outcome, HarnessRunOutcome::Completed { .. }),
        "expected a Completed outcome, got {:?}",
        result.outcome
    );

    let calls = provider
        .state()
        .call_count
        .load(std::sync::atomic::Ordering::Relaxed);
    let ceiling = rpi_harness::run_budget::DEFAULT_MAX_TURNS_PER_RUN as usize;
    assert_eq!(
        calls, ceiling,
        "the run must stop exactly at the turn ceiling, not run on"
    );

    // The stop reason is on the lane, rendered like any other custom message.
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
    let notices: Vec<String> = path
        .iter()
        .filter_map(|entry| match entry {
            rpi_harness::session::types::Entry::Message(message_entry) => {
                match &message_entry.message {
                    rpi_agent::AgentMessage::Custom(custom)
                        if custom.role == rpi_harness::messages::CUSTOM_ROLE =>
                    {
                        Some(
                            custom
                                .content
                                .iter()
                                .filter_map(|content| match content {
                                    rpi_ai::types::Content::Text(text) => Some(text.text.clone()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        )
                    }
                    _ => None,
                }
            }
            _ => None,
        })
        .filter(|text| text.contains("Stopped after"))
        .collect();
    assert_eq!(
        notices.len(),
        1,
        "exactly one budget notice must be recorded; got {notices:?}"
    );
    assert!(
        notices[0].contains(&ceiling.to_string()) && notices[0].contains("unfinished"),
        "the notice must name the budget and warn the work may be incomplete: {}",
        notices[0]
    );
}

/// A crash mid-stream must not cost the whole run.
///
/// Before frame progress, a run persisted nothing until it finished, so killing
/// the process after minutes of work left only the user's prompt
/// (`docs/llm-repetition-forensics.md` §十一). Frames are appended to the record
/// stream as they arrive, so the committed prefix can be replayed.
///
/// The crash is simulated faithfully: the run future is dropped mid-stream by
/// aborting its task, which is exactly what process death does to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crashed_run_can_be_salvaged_from_its_committed_frames() {
    // A deliberately slow stream: enough tokens that the run is still streaming
    // when we abort it, so the crash lands mid-message.
    let long = "the quick brown fox jumps over the lazy dog ".repeat(60);
    let script = FauxScript::new()
        .with_tokens_per_second(20.0)
        .with_text(long.clone());
    let (harness, _provider, _env) = harness_with(script).await;

    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("do something long", vec![]).await })
    };

    // Wait until the frames are durable, then kill the run.
    let session = harness.session().clone();
    let mut committed = 0usize;
    for _ in 0..200 {
        let records = session
            .find_records(&RecordQuery {
                record_type: Some(rpi_harness::frame_progress::ASSISTANT_FRAME_RECORD_TYPE),
                ..Default::default()
            })
            .await
            .expect("frame records readable");
        committed = records.len();
        if committed >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        committed >= 3,
        "expected the start/text_start/text_delta frames to be durable before the crash, got {committed}"
    );
    // Nothing was persisted as history yet — this is the pre-fix failure mode.
    let branch = harness
        .session()
        .view("main")
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .expect("branch readable");
    assert_eq!(
        branch
            .iter()
            .filter(|entry| matches!(entry, rpi_harness::session::types::Entry::Message(_)))
            .count(),
        1,
        "only the prompt is history at this point"
    );

    run.abort();
    let _ = run.await;

    // Recover: the committed prefix becomes an interrupted message instead of
    // vanishing.
    let salvaged = rpi_harness::frame_progress::salvage_run_frames(&session, "unknown-run")
        .await
        .expect("salvage runs");
    assert!(salvaged.is_empty(), "a different run id salvages nothing");

    // Read the run id that actually wrote the frames.
    let records = session
        .find_records(&RecordQuery {
            record_type: Some(rpi_harness::frame_progress::ASSISTANT_FRAME_RECORD_TYPE),
            ..Default::default()
        })
        .await
        .expect("frame records readable");
    let run_id = records
        .iter()
        .filter_map(|record| record.run_id().map(str::to_string))
        .next()
        .expect("frames carry a run id");

    let salvaged = rpi_harness::frame_progress::salvage_run_frames(&session, &run_id)
        .await
        .expect("salvage runs");
    assert_eq!(salvaged.len(), 1, "one stream was committed");
    let message = &salvaged[0];
    assert_eq!(
        message.stop_reason,
        rpi_ai::types::StopReason::Error,
        "a salvaged message is marked as an error, not a normal stop"
    );
    assert_eq!(
        message.error_message.as_deref(),
        Some(rpi_harness::frame_progress::INTERRUPTED_NOTICE)
    );
    let salvaged_text = match message.content.first() {
        Some(rpi_ai::types::Content::Text(text)) => text.text.clone(),
        other => panic!("expected salvaged text content, got {other:?}"),
    };
    assert!(
        !salvaged_text.is_empty() && long.starts_with(&salvaged_text),
        "the content streamed before the crash must survive as a prefix of the full reply; \
         salvaged {salvaged_text:?}"
    );
    assert!(
        salvaged_text.len() < long.len(),
        "the crash landed mid-stream, so the salvage must be partial"
    );
    assert!(
        message.usage == rpi_ai::types::Usage::zero(),
        "a salvaged partial must not bill usage a retry would bill again"
    );
}

/// A run that finishes normally retires its frames: they were progress, not
/// history, so a later recovery must find nothing to salvage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_completed_run_leaves_no_frames_to_salvage() {
    let script = FauxScript::new().with_text("all done");
    let (harness, _provider, _env) = harness_with(script).await;

    let result = harness
        .prompt_text("finish cleanly", vec![])
        .await
        .expect("prompt completes");
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));

    let records = harness
        .session()
        .find_records(&RecordQuery {
            record_type: Some(rpi_harness::frame_progress::ASSISTANT_FRAME_RECORD_TYPE),
            ..Default::default()
        })
        .await
        .expect("frame records readable");
    assert!(
        !records.is_empty(),
        "frames were recorded while the run was in flight"
    );

    let salvaged =
        rpi_harness::frame_progress::salvage_run_frames(harness.session(), &result.run_id)
            .await
            .expect("salvage runs");
    assert!(
        salvaged.is_empty(),
        "a committed run must not be salvaged: {salvaged:?}"
    );
}

/// Reopen an existing session in a fresh harness.
///
/// This is what a user does after a crash (`rpi` again on the same session), and
/// opening the harness is what runs `finish_interrupted_operation`.
async fn reopen_harness(session: rpi_harness::session::session::Session) -> AgentHarness {
    let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
    let model = provider.default_model().clone();
    let options = AgentHarnessOptions {
        model,
        thinking_level: Default::default(),
        active_tool_names: Vec::new(),
        tools: Vec::new(),
        system_prompt: None,
        resources: Default::default(),
        stream_options: Default::default(),
        retry: RetryPolicy::default(),
        compaction: Default::default(),
        steering_mode: Default::default(),
        follow_up_mode: Default::default(),
        tool_execution: Default::default(),
        drive: Default::default(),
        session,
        models: vec![provider as Arc<dyn Provider>],
        to_provider_messages: None,
        entry_projectors: Default::default(),
        agent_emitter: None,
        before_tool_call: None,
        after_tool_call: None,
        transform_context: None,
        entry_transforms: Vec::new(),
        provider_hooks: None,
        allow_existing_session: true,
    };
    AgentHarness::create(options)
        .await
        .expect("reopen harness on the crashed session")
}

/// The whole point of frame progress, end to end: crash mid-stream, reopen the
/// session, and get the committed prefix back as an interrupted message instead
/// of an empty run.
///
/// Exercises the real recovery entry point (`finish_interrupted_operation`, run
/// by `AgentHarness::create`) rather than calling salvage directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reopening_a_crashed_session_restores_the_committed_prefix() {
    let long = "the quick brown fox jumps over the lazy dog ".repeat(60);
    let script = FauxScript::new()
        .with_tokens_per_second(20.0)
        .with_text(long.clone());
    let (harness, _provider, _env) = harness_with(script).await;

    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("do something long", vec![]).await })
    };
    let session = harness.session().clone();
    for _ in 0..200 {
        let records = session
            .find_records(&RecordQuery {
                record_type: Some(rpi_harness::frame_progress::ASSISTANT_FRAME_RECORD_TYPE),
                ..Default::default()
            })
            .await
            .expect("frame records readable");
        if records.len() >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    run.abort();
    let _ = run.await;

    // The crash left the operation dangling: nothing closed it.
    let open = session
        .find_open_operations("main", None)
        .await
        .expect("open operations readable");
    assert_eq!(
        open.len(),
        1,
        "a crashed run leaves exactly one open operation"
    );

    drop(harness);
    let recovered = reopen_harness(session).await;

    // Recovery closed the operation and preserved the output.
    let open = recovered
        .session()
        .find_open_operations("main", None)
        .await
        .expect("open operations readable");
    assert!(
        open.is_empty(),
        "reopening the session must close the interrupted operation, still open: {open:?}"
    );

    let entries = recovered
        .session()
        .view("main")
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .expect("branch readable");
    let messages: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            rpi_harness::session::types::Entry::Message(message) => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(messages.len(), 2, "prompt + recovered assistant message");

    let assistant = match &messages[1].message {
        AgentMessage::Assistant(assistant) => assistant.clone(),
        other => panic!("expected an assistant message, got {other:?}"),
    };
    assert_eq!(
        assistant.stop_reason,
        rpi_ai::types::StopReason::Error,
        "the recovered message is marked as interrupted"
    );
    assert_eq!(
        assistant.error_message.as_deref(),
        Some(rpi_harness::frame_progress::INTERRUPTED_NOTICE)
    );
    let text = match assistant.content.first() {
        Some(rpi_ai::types::Content::Text(text)) => text.text.clone(),
        other => panic!("expected recovered text content, got {other:?}"),
    };
    assert!(
        !text.is_empty() && long.starts_with(&text) && text.len() < long.len(),
        "the recovered text must be the partial prefix committed before the crash, got {text:?}"
    );
}

/// Recovery must be idempotent: opening the same crashed session again must not
/// append a second copy of the salvaged message.
///
/// The early return in `finish_interrupted_operation` (no open operation → do
/// nothing) is what guarantees this, and nothing else in the recovery path
/// writes a `ClearRun`, so this is the invariant worth pinning.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovering_the_same_crashed_session_twice_does_not_duplicate() {
    let long = "the quick brown fox jumps over the lazy dog ".repeat(60);
    let script = FauxScript::new()
        .with_tokens_per_second(20.0)
        .with_text(long);
    let (harness, _provider, _env) = harness_with(script).await;

    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("do something long", vec![]).await })
    };
    let session = harness.session().clone();
    for _ in 0..200 {
        let records = session
            .find_records(&RecordQuery {
                record_type: Some(rpi_harness::frame_progress::ASSISTANT_FRAME_RECORD_TYPE),
                ..Default::default()
            })
            .await
            .expect("frame records readable");
        if records.len() >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    run.abort();
    let _ = run.await;
    drop(harness);

    let count_messages = |harness: &AgentHarness| {
        let session = harness.session().clone();
        async move {
            session
                .view("main")
                .find_entries(&EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                })
                .await
                .expect("branch readable")
                .iter()
                .filter(|entry| matches!(entry, rpi_harness::session::types::Entry::Message(_)))
                .count()
        }
    };

    let first = reopen_harness(session).await;
    assert_eq!(
        count_messages(&first).await,
        2,
        "first recovery: prompt + interrupted message"
    );

    // Open the recovered session a second time.
    let second = reopen_harness(first.session().clone()).await;
    assert_eq!(
        count_messages(&second).await,
        2,
        "second recovery must not append the salvaged message again"
    );
}
/// A tool that never returns, so a run can be killed *during* tool execution —
/// the common crash case, and the one that produces an assistant message whose
/// tool calls have no results.
struct HangingTool {
    schema: rpi_ai::types::Tool,
}

impl HangingTool {
    fn new() -> Self {
        Self {
            schema: rpi_ai::types::Tool {
                name: "slow_tool".to_string(),
                description: "never returns".to_string(),
                parameters: rpi_ai::types::Schema::new(
                    serde_json::json!({ "type": "object", "properties": {} }),
                ),
                constrained_sampling: None,
            },
        }
    }
}

#[async_trait::async_trait]
impl rpi_agent::AgentTool for HangingTool {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "slow_tool"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Value,
        _signal: tokio_util::sync::CancellationToken,
        _on_update: Arc<dyn Fn(rpi_agent::ToolResultPartial) + Send + Sync>,
    ) -> Result<rpi_agent::AgentToolResult, rpi_agent::AgentError> {
        // Never resolves: the run is stuck inside tool execution.
        std::future::pending::<()>().await;
        unreachable!()
    }
}

/// Build a harness whose only tool hangs.
async fn harness_with_hanging_tool(script: FauxScript) -> (AgentHarness, Arc<FauxProvider>) {
    let provider = FauxProvider::new(script);
    let model = provider.default_model().clone();
    let env = Arc::new(InMemoryExecutionEnv::new());
    let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
    let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
    let _registry = Arc::new(MutationQueueRegistry::new());
    let ctx = ExecutionToolContext::new(env_dyn, Some(mut_env));
    let tools: Vec<HarnessTool> = vec![
        HarnessTool::new(Arc::new(HangingTool::new())),
        HarnessTool::new(create_read_tool(&ctx, None)),
    ];
    let active: Vec<String> = tools.iter().map(|t| t.tool.schema().name.clone()).collect();

    let metadata = SessionMetadata {
        id: "crash-tool".into(),
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
        retry: RetryPolicy::default(),
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
        provider_hooks: None,
        allow_existing_session: false,
    };
    let harness = AgentHarness::create(options).await.expect("create harness");
    (harness, provider)
}

/// Crashing *during* tool execution must not leave a transcript the next
/// request cannot be built from.
///
/// This is the failure mode of salvaging an assistant message verbatim: the
/// committed partial carries the tool call, its result never landed, and every
/// provider rejects an assistant message whose tool calls have no results
/// (Anthropic: "`tool_use` ids were found without `tool_result` blocks"). The
/// salvage therefore pairs each unresolved call with a synthetic error result,
/// which also tells the model and the user that the outcome is unknown rather
/// than "not applied".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crashing_during_a_tool_call_leaves_a_valid_transcript() {
    let script = FauxScript::new().with_tool_call("slow_tool", serde_json::json!({}));
    let (harness, _provider) = harness_with_hanging_tool(script).await;

    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("run the slow tool", vec![]).await })
    };
    let session = harness.session().clone();

    // Wait until the tool call itself is committed in the frames.
    let mut saw_tool_call = false;
    for _ in 0..300 {
        let records = session
            .find_records(&RecordQuery {
                record_type: Some(rpi_harness::frame_progress::ASSISTANT_FRAME_RECORD_TYPE),
                ..Default::default()
            })
            .await
            .expect("frame records readable");
        saw_tool_call = records.iter().any(|record| {
            matches!(record, LaneRecord::AssistantFrame(f)
                if f.frame.as_ref().and_then(|v| v.get("type")).and_then(|v| v.as_str())
                    == Some("toolcall_end"))
        });
        if saw_tool_call {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        saw_tool_call,
        "the tool call must be committed before the crash for this test to mean anything"
    );

    // Kill the run inside tool execution: no tool result will ever exist.
    run.abort();
    let _ = run.await;

    // The write side closes the loop with the read side: the running tool is
    // recorded *before* it runs, so recovery can name the tool whose side effect
    // is unknown instead of reporting a generic "a tool might have run".
    let records = session
        .find_records(&RecordQuery {
            lane: Some("main".to_string()),
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .expect("records readable");
    let started = records
        .iter()
        .find_map(|record| match record {
            LaneRecord::ToolStarted(started) if started.tool_name == "slow_tool" => Some(started),
            _ => None,
        })
        .expect("the running tool must be recorded before it runs");
    assert_eq!(
        started.effective_args,
        serde_json::json!({}),
        "the record must carry the arguments the tool was called with"
    );

    let report = rpi_harness::runtime::SessionRuntime::new(session.clone())
        .recover_lane("main")
        .await
        .expect("lane recovery runs");
    assert!(
        report
            .tool_frames
            .iter()
            .any(|frame| frame.tool_name == "slow_tool"),
        "recovery must track the in-flight tool: {:?}",
        report.tool_frames
    );
    // While the run is still open the recorded tool is ordinary in-flight work,
    // not yet a finding: only a run that *died* is worth reporting, and at this
    // point nothing has closed the operation. The finding (`DanglingToolFrame`)
    // appears once the operation is closed without its result landing, which is
    // covered by `runtime`'s `an_unlanded_tool_frame_names_the_tool`.
    assert!(
        !report.findings.iter().any(|finding| matches!(
            finding,
            rpi_harness::runtime::RecoveryFinding::DanglingToolFrame { .. }
        )),
        "an open run's tool is in flight, not dangling: {:?}",
        report.findings
    );
    assert!(
        report.findings.iter().any(|finding| matches!(
            finding,
            rpi_harness::runtime::RecoveryFinding::OrphanedOperation { .. }
        )),
        "the crash left the operation open: {:?}",
        report.findings
    );
    assert!(
        !report.is_corrupt(),
        "an interrupted tool is not log corruption: {report:?}"
    );

    drop(harness);

    let recovered = reopen_harness(session).await;
    // Read the *branch path* specifically: the repaired sequence has to be
    // reachable from the leaf, because that is what the next request is built
    // from. Asserting on all entries would pass even if the results were
    // orphaned off the branch.
    let entries = recovered
        .session()
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .await
        .expect("branch readable");

    let mut tool_call_ids: Vec<String> = Vec::new();
    let mut result_ids: Vec<String> = Vec::new();
    for entry in &entries {
        if let rpi_harness::session::types::Entry::Message(message) = entry {
            match &message.message {
                AgentMessage::Assistant(assistant) => {
                    for block in &assistant.content {
                        if let rpi_ai::types::Content::ToolCall(call) = block {
                            tool_call_ids.push(call.id.clone());
                        }
                    }
                }
                AgentMessage::ToolResult(result) => {
                    assert!(
                        result.is_error,
                        "the stand-in result must be marked as an error"
                    );
                    let text = match result.content.first() {
                        Some(rpi_ai::types::Content::Text(text)) => text.text.clone(),
                        other => panic!("expected text in the stand-in result, got {other:?}"),
                    };
                    assert!(
                        text.contains("unknown"),
                        "the stand-in result must say the outcome is unknown, got {text:?}"
                    );
                    result_ids.push(result.tool_call_id.clone());
                }
                _ => {}
            }
        }
    }

    assert!(
        !tool_call_ids.is_empty(),
        "the recovered transcript must still carry the interrupted tool call"
    );
    for id in &tool_call_ids {
        assert!(
            result_ids.contains(id),
            "every recovered tool call needs a result or the next request is invalid; \
             {id} has none (calls={tool_call_ids:?}, results={result_ids:?})"
        );
    }
}

/// A real run's record log must be a legal product of the record protocol.
///
/// This is the end-to-end check on the write side: commit-on-settle commits an
/// assistant message carrying tool calls *before* its tools run, and records a
/// `step_attempt` naming that entry. The reducer then deep-checks the log —
/// `step_attempt` series/result consistency, `write_deferred` targets, and the
/// `tool_started` ↔ assistant-ordinal match. Writing records the validator would
/// reject is worse than writing none, because a corrupt log blocks recovery.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_real_run_leaves_a_valid_record_log() {
    let script = FauxScript::new()
        .with_tool_call(
            "write",
            serde_json::json!({ "path": "out.txt", "content": "hello" }),
        )
        .with_text("done");
    let (harness, _provider, _env) = harness_with(script).await;

    let result = harness
        .prompt_text("write a file", vec![])
        .await
        .expect("run completes");
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));

    let session = harness.session();
    let records = session
        .find_records(&RecordQuery {
            lane: Some("main".to_string()),
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .expect("records readable");
    let entries = session
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .expect("entries readable");
    let open_operations = session
        .find_open_operations("main", None)
        .await
        .expect("open operations readable");

    // The settled assistant step must be recorded, and its `result_entry_id`
    // must name an entry that really landed with that message's content.
    let attempts: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            LaneRecord::StepAttempt(attempt) => Some(attempt),
            _ => None,
        })
        .collect();
    assert!(
        !attempts.is_empty(),
        "a run that streamed an assistant message must record its step"
    );
    for attempt in &attempts {
        let landed = entries.iter().any(|entry| {
            entry.id() == attempt.result_entry_id
                && matches!(
                    entry,
                    rpi_harness::session::types::Entry::Message(message)
                        if message.message.is_assistant()
                )
        });
        assert!(
            landed,
            "step attempt {} names result entry {} which did not land as an assistant message",
            attempt.base.id, attempt.result_entry_id
        );
    }

    // And the log as a whole is legal.
    let slice = rpi_harness::session::RecordLogSlice {
        lane: "main".to_string(),
        open_operations,
        records,
        entries,
    };
    rpi_harness::session::validate_record_log(&slice)
        .expect("a completed run must leave a legal record log");
}

/// A retried attempt must not be committed: the harness re-runs the whole loop,
/// so persisting the failed attempt would leave a dead assistant message in
/// history and put two assistant turns in a row in front of the provider.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retried_attempt_is_not_committed() {
    let mut transient = faux_assistant_message("", rpi_ai::types::StopReason::Error);
    transient.error_message = Some("503 service unavailable".into());
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::message(transient),
        FauxStep::text("Recovered after retry."),
    ]);
    let (harness, provider, _env) = harness_with_options(
        script,
        RetryPolicy {
            enabled: true,
            max_retries: 3,
            ..RetryPolicy::default()
        },
        None,
    )
    .await;

    let result = harness
        .prompt_text("do the thing", vec![])
        .await
        .expect("run completes");
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::Relaxed),
        2,
        "the provider should have been retried once"
    );

    // Exactly one assistant message in history: the recovered one. The failed
    // attempt's message must not be present.
    let entries = harness
        .session()
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .expect("entries readable");
    let assistants: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            rpi_harness::session::types::Entry::Message(message) => match &message.message {
                AgentMessage::Assistant(assistant) => Some(assistant.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        assistants.len(),
        1,
        "the failed attempt must not be committed, got {} assistant messages",
        assistants.len()
    );
    assert_eq!(assistants[0].stop_reason, rpi_ai::types::StopReason::Stop);
    assert!(
        assistants[0].error_message.is_none(),
        "the committed message must be the recovered one, not the failed attempt"
    );
}
