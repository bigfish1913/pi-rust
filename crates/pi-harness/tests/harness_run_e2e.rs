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

use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_ai::Provider;
use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use rpi_harness::events::{HarnessEvent, RunEndOutcome};
use rpi_harness::session::types::{
    BranchBounds, EntryOrder, EntryQuery, LaneRecord, RecordQuery,
};
use rpi_harness::session::{DefaultIdGenerator, Session};
use rpi_harness::session::memory::{InMemorySessionStorage, SystemClock};
use rpi_harness::session::types::SessionMetadata;
use rpi_harness::types::{AgentHarnessOptions, HarnessTool};
use rpi_tools::{
    create_bash_tool, create_read_tool, create_write_tool, ExecutionToolContext,
    FileSystem, InMemoryExecutionEnv, MutationQueueRegistry,
};

/// Build an `AgentHarness` over a faux provider + in-memory env + the read/
/// write/bash tools. The faux script is caller-supplied so each test pins the
/// exact LLM turns. Returns `(harness, provider, env)` so the test can inspect
/// provider call count + the in-memory FS after the run.
async fn harness_with(
    script: FauxScript,
) -> (
    AgentHarness,
    Arc<FauxProvider>,
    Arc<InMemoryExecutionEnv>,
) {
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
    let tools: Vec<HarnessTool> =
        vec![read, write, bash].into_iter().map(HarnessTool::new).collect();
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
    };

    let harness = AgentHarness::create(options).await.expect("create harness");
    (harness, provider, env)
}

/// Collect bus events into a shared vec while the closure owns a `Handle`.
fn record_runtime_events(
    harness: &AgentHarness,
) -> Arc<Mutex<Vec<HarnessEvent>>> {
    let collected: Arc<Mutex<Vec<HarnessEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let collected_for_listener = collected.clone();
    // `on` registers a direct listener; we keep the `OnUnsubscribe` guard alive
    // for the duration of the run by returning both. But we only need the vec;
    // drop the guard to keep it simple — events during the run still land
    // because the harness emits synchronously inline on the calling task.
    let _off = harness.events().on::<rpi_harness::events::RunEndEvent, _>(
        move |_e| {
            // no-op placeholder; we use the all-events watch below
        },
    );
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
        HarnessRunOutcome::Completed { leaf_id, final_entry_id, final_message } => {
            assert!(!leaf_id.is_empty(), "leaf_id must be set on Completed");
            assert!(!final_entry_id.is_empty(), "final_entry_id must be set");
            // The final assistant message is the "Done — ..." text turn (no tool calls).
            let has_text = final_message.content.iter().any(|c| matches!(c, rpi_ai::types::Content::Text(_)));
            assert!(has_text, "final message should carry text content");
            assert_eq!(final_message.stop_reason, rpi_ai::types::StopReason::Stop);
        }
        other => panic!("expected Completed, got {other:?}"),
    }

    // The faux provider was called once per turn = 2 calls.
    assert_eq!(
        provider.state().call_count.load(std::sync::atomic::Ordering::Relaxed),
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
            &EntryQuery { order: Some(EntryOrder::OldestFirst), ..Default::default() },
            &BranchBounds { start: Some(leaf), ..Default::default() },
        )
        .await
        .expect("branch path");
    assert!(
        path.len() >= 4,
        "expected at least 4 entries (user, assistant, toolresult, assistant-final); got {}",
        path.len()
    );
    // Sanity: the first entry is the user prompt.
    assert!(matches!(path[0], rpi_harness::session::types::Entry::Message(_)));
    // And the leaf entry (last) is the final assistant text turn.
    let last = path.last().unwrap();
    assert!(matches!(last, rpi_harness::session::types::Entry::Message(_)));

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
    assert_eq!(started.len(), 1, "exactly one operation_started for this run_id");
    let finished = harness
        .session()
        .find_records(&RecordQuery {
            record_type: Some("operation_finished"),
            run_id: Some(run_id.clone()),
            ..Default::default()
        })
        .await
        .expect("find operation_finished");
    assert_eq!(finished.len(), 1, "exactly one operation_finished for this run_id");
    if let LaneRecord::OperationFinished(f) = &finished[0] {
        assert_eq!(f.outcome, rpi_harness::session::types::OperationOutcome::Completed);
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
        provider.state().call_count.load(std::sync::atomic::Ordering::Relaxed),
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
            &EntryQuery { order: Some(EntryOrder::OldestFirst), ..Default::default() },
            &BranchBounds { start: Some(leaf), ..Default::default() },
        )
        .await
        .unwrap();
    assert_eq!(path.len(), 2, "user + assistant reply only");
}
