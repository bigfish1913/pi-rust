//! Output modes. Mirrors the v1-relevant slice of the TS
//! `packages/coding-agent/src/modes/{print-mode,json-event,rpc-mode}.ts` — the
//! three run shapes a harness-backed CLI needs:
//!
//! - [`print`] — single-shot: send the prompt(s), print the final assistant
//!   text (or the error) to stdout, exit. Mirrors TS `runPrintMode` (text mode).
//! - [`json`] — single-shot streaming: emit each harness event as a JSON line
//!   on stdout, then the final outcome. Mirrors TS `runPrintMode`
//!   (`mode === "json"`) + [`json_event::toJsonEvent`].
//! - [`interactive`] — a minimal line-oriented REPL: read prompts from stdin,
//!   run each, print the assistant text, loop until EOF / `/exit`. v1 does NOT
//!   port the TS `InteractiveMode` TUI (`modes/interactive/*` — a full terminal
//!   UI with Ink/React components); this is a deliberately minimal replacement,
//!   documented in the initial port notes (retired).
//!
//! All three drive the same `AgentHarness` via `AgentLane::prompt_text`.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::io::{BufRead, IsTerminal, Write};
use std::sync::{Arc, Mutex};

use rpi_agent::events::AgentEvent;
use rpi_ai::types::{
    AssistantMessage, AudioContent, Content, ImageContent, StopReason, UserMessage,
};
use rpi_harness::agent_harness::{
    AgentHarness, AgentLane, CompactionOutcome, CompactionResult, HarnessRunOutcome,
};
use rpi_harness::events::{HarnessEvent, RunEndOutcome};
use rpi_harness::session::session::SessionEntryCommit;
use rpi_harness::session::types::{
    BranchBounds, Entry, EntryCursor, EntryOrder, EntryQuery, LaneRecord, RecordQuery,
};

use crate::args::Args;
use crate::rpc::client_requests::{dialog_answer, ClientRequestBroker, MAX_TIMEOUT_MS};
use crate::rpc::protocol as rpc_protocol;
use crate::rpc::writer::{start_rpc_writer_with_mode, write_agent_event, write_line, RpcWriter};
#[cfg(test)]
use crate::rpc::writer::start_rpc_writer;

/// Extract the concatenated text content from an assistant message. Mirrors the
/// TS print-mode loop (`for content of assistantMsg.content if type===text`).
pub fn assistant_text(msg: &AssistantMessage) -> String {
    msg.content
        .iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect()
}

/// The exit code a run's outcome maps to. Mirrors TS print mode: error/aborted
/// ⇒ exit 1; everything else ⇒ 0.
pub fn outcome_exit_code(outcome: &HarnessRunOutcome) -> i32 {
    match outcome {
        HarnessRunOutcome::Failed { .. } | HarnessRunOutcome::Aborted { .. } => 1,
        _ => 0,
    }
}

/// `print` mode: send the initial message (prompt text + inline `@file`
/// expansions), then any follow-up messages, print the final assistant text,
/// return the exit code. Mirrors TS `runPrintMode` (text).
pub async fn print(
    harness: &AgentHarness,
    _args: &Args,
    initial: Option<String>,
    extra_messages: &[String],
    initial_images: Vec<ImageContent>,
) -> i32 {
    let lane: Arc<dyn AgentLane> = harness.lane("main");

    let mut last_exit = 0;
    let mut last_msg: Option<AssistantMessage> = None;

    // The initial prompt (and its `@file` attachments) go in one user message;
    // extra positionals are separate prompts (mirrors the TS loop).
    let mut prompts: Vec<String> = Vec::new();
    if let Some(init) = initial {
        prompts.push(init);
    }
    for m in extra_messages {
        prompts.push(m.clone());
    }

    if prompts.is_empty() {
        // Nothing to do — print mode with no prompt is a no-op success.
        return 0;
    }

    let mut images = initial_images;
    for prompt in prompts {
        let outcome = lane.prompt_text(&prompt, std::mem::take(&mut images)).await;
        // Print mode has no transcript to report into, so a write failure goes to
        // stderr. Silence here is what let a conversation stop being saved while
        // the run looked fine.
        for failure in lane.take_write_failures() {
            eprintln!(
                "session not saved: could not persist {} ({})",
                failure.what, failure.message
            );
        }
        match outcome {
            Ok(result) => {
                last_exit = outcome_exit_code(&result.outcome);
                match &result.outcome {
                    HarnessRunOutcome::Completed { final_message, .. }
                    | HarnessRunOutcome::Aborted { final_message, .. } => {
                        last_msg = Some(final_message.clone());
                    }
                    HarnessRunOutcome::Failed {
                        error,
                        final_message,
                        ..
                    } => {
                        if let Some(m) = final_message {
                            if m.stop_reason == StopReason::Error {
                                if let Some(em) = &m.error_message {
                                    eprintln!("{em}");
                                }
                            }
                        }
                        if error.code == "SessionStorage" {
                            eprintln!("{}", session_storage_notice(&error.message));
                        } else {
                            eprintln!("run failed: {error:?}");
                        }
                    }
                    HarnessRunOutcome::Suspended { .. } => {
                        eprintln!("run suspended (deferred) — resume is not supported in v1");
                        last_exit = 1;
                    }
                }
            }
            Err(e) => {
                eprintln!("prompt rejected: {e}");
                return 1;
            }
        }
    }

    // Print the final assistant text to stdout (TS: writeRawStdout text + "\n").
    if let Some(m) = &last_msg {
        match m.stop_reason {
            StopReason::Error => {
                if let Some(em) = &m.error_message {
                    eprintln!("{em}");
                }
                last_exit = 1;
            }
            StopReason::Aborted => {
                eprintln!("request aborted");
                last_exit = 1;
            }
            _ => {
                let text = assistant_text(m);
                let mut out = std::io::stdout();
                let _ = out.write_all(text.as_bytes());
                if !text.ends_with('\n') {
                    let _ = out.write_all(b"\n");
                }
                let _ = out.flush();
            }
        }
    }

    last_exit
}

/// `json` mode: emit each harness event as a JSON line on stdout, run the
/// prompts, then emit a terminal `result` line carrying the outcome + final
/// text. Mirrors TS `runPrintMode` (`mode === "json"`) streaming every event.
pub async fn json(
    harness: &AgentHarness,
    _args: &Args,
    initial: Option<String>,
    extra_messages: &[String],
    initial_images: Vec<ImageContent>,
    mut agent_events: Option<tokio::sync::broadcast::Receiver<AgentEvent>>,
) -> i32 {
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let collected: Arc<Mutex<Vec<HarnessEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let collected_for_watch = collected.clone();

    // A watch captures every event (RunStart fires inline during prompt_text,
    // before a post-call listener could attach — same reason as the M5g test).
    let mut watch = harness.events().watch(|| ());
    watch.start(Arc::new(move |event: &HarnessEvent| {
        // Emit each event live as JSON, and also buffer for the final summary.
        emit_json_event(event);
        collected_for_watch.lock().unwrap().push(event.clone());
    }));
    // Keep the watch alive for the whole run. Leaking is acceptable for a
    // single-shot CLI process (the bus outlives this scope anyway).
    std::mem::forget(watch);

    // The harness bus carries run lifecycle events; the agent receiver carries
    // the native fine-grained stream (turns, message deltas, and tools).
    let (agent_done_tx, mut agent_done_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let agent_event_task = agent_events.take().map(|mut rx| {
        tokio::spawn(async move {
            let done_tx = agent_done_tx;
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let terminal = event.is_terminal();
                        emit_agent_event(&event);
                        if terminal {
                            let _ = done_tx.send(());
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    });

    let mut prompts: Vec<String> = Vec::new();
    if let Some(init) = initial {
        prompts.push(init);
    }
    for m in extra_messages {
        prompts.push(m.clone());
    }

    let mut last_exit = 0;
    let mut final_outcome: Option<HarnessRunOutcome> = None;

    let mut images = initial_images;
    for prompt in prompts {
        match lane.prompt_text(&prompt, std::mem::take(&mut images)).await {
            Ok(result) => {
                // The harness resolves after its run outcome, while the
                // broadcast listener may still be scheduling the terminal
                // AgentEnd line. Wait briefly so JSON consumers see the full
                // lifecycle before the final result summary. The timeout is
                // deliberately bounded for custom/older harness emitters.
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(250),
                    agent_done_rx.recv(),
                )
                .await;
                last_exit = outcome_exit_code(&result.outcome);
                final_outcome = Some(result.outcome);
            }
            Err(e) => {
                // Emit a structured error line + exit.
                let line = serde_json::json!({
                    "type": "error",
                    "error": e.to_string(),
                });
                println!("{line}");
                if let Some(task) = agent_event_task {
                    task.abort();
                }
                return 1;
            }
        }
    }

    // Terminal result summary.
    let (outcome_str, final_text) = match final_outcome {
        Some(HarnessRunOutcome::Completed { final_message, .. }) => {
            ("completed", Some(assistant_text(&final_message)))
        }
        Some(HarnessRunOutcome::Aborted { final_message, .. }) => {
            ("aborted", Some(assistant_text(&final_message)))
        }
        Some(HarnessRunOutcome::Failed { final_message, .. }) => {
            let t = final_message.as_ref().map(assistant_text);
            ("failed", t)
        }
        Some(HarnessRunOutcome::Suspended { .. }) => ("suspended", None),
        None => ("idle", None),
    };
    let result_line = serde_json::json!({
        "type": "result",
        "outcome": outcome_str,
        "finalText": final_text,
    });
    println!("{result_line}");
    if let Some(task) = agent_event_task {
        task.abort();
    }
    last_exit
}

fn emit_agent_event(event: &AgentEvent) {
    println!("{}", agent_event_json(event));
}

struct RpcModeState {
    current: String,
    /// The baseline prompt may itself be None. `base_prompt_saved` keeps that
    /// distinction from "we have not captured the baseline yet".
    base_prompt: Option<String>,
    base_prompt_saved: bool,
    base_tools: Option<Vec<String>>,
}

fn mode_descriptors() -> serde_json::Value {
    serde_json::json!([
        {
            "id": "default",
            "name": "Default",
            "description": "Normal rpi tool and prompt strategy"
        },
        {
            "id": "plan",
            "name": "Plan",
            "description": "Plan-first strategy with mutating tools disabled"
        }
    ])
}

/// `rpc` mode: a headless JSONL command loop (the server-side agent).
///
/// Reads one JSON command per line from stdin (`{id?, type, ...}`), drives the
/// `main` lane, and writes one JSON object per line to stdout:
/// - fine-grained `AgentEvent`s via [`agent_event_json`] (each carries `type`),
/// - a `{"type":"response","id":<id>,"status":"ok"|"error",...}` per command.
///
/// Commands: `prompt`, `resume_pending`, and `compact` (drive the lane),
/// `abort`, `get_state`, `set_model`,
/// `set_thinking_level`, `set_active_tools`, `ping`, `stop`. `stop` (or stdin
/// EOF) ends the loop. This is the process `rpi-server` spawns and that
/// `rpi --connect` talks to. Mirrors TS `runRpcMode` / `modes/rpc`.
///
/// The extension `UiDialog` mailbox is bridged over the same channel: the host
/// emits `{"type":"ui_dialog_request","requestId","toolCallId","ui"}`, and the
/// client answers with `{"type":"ui_dialog_response","requestId","answer"}` (or
/// `"cancelled":true`). A `prompt` runs in its own task so this loop keeps
/// reading — that is what lets `abort` and dialog answers arrive mid-run.
pub async fn rpc(
    harness: &AgentHarness,
    agent_events: Option<tokio::sync::broadcast::Receiver<AgentEvent>>,
    model_catalog: Vec<rpi_ai::Model>,
    ui_dialog: rpi_extensions::UiDialogMailbox,
    oauth_snapshot: Option<Arc<rpi_extensions::RegistrySnapshot>>,
    message_updates: crate::args::RpcMessageUpdates,
) -> i32 {
    rpc_with_client(harness, agent_events, model_catalog, ui_dialog, oauth_snapshot, message_updates, None, None).await
}

pub(crate) async fn rpc_with_client(
    harness: &AgentHarness,
    agent_events: Option<tokio::sync::broadcast::Receiver<AgentEvent>>,
    model_catalog: Vec<rpi_ai::Model>,
    ui_dialog: rpi_extensions::UiDialogMailbox,
    oauth_snapshot: Option<Arc<rpi_extensions::RegistrySnapshot>>,
    message_updates: crate::args::RpcMessageUpdates,
    client_tools: Option<Arc<crate::rpc::client_tools::ClientTools>>,
    child_context: Option<&crate::session::ReloadContext>,
) -> i32 {
    rpc_with_io_child(
        harness,
        agent_events,
        model_catalog,
        ui_dialog,
        oauth_snapshot,
        message_updates,
        tokio::io::BufReader::new(tokio::io::stdin()),
        std::io::stdout(),
        client_tools,
        child_context,
        Some(Box::new(|| {
            eprintln!("[rpi] fatal RPC output/producer shutdown stall; terminating RPC process");
            // Only the real CLI owns this process. OS termination also stops
            // blocking stdout writes/tools that Rust task cancellation cannot.
            std::process::exit(1);
        })),
    )
    .await
}

#[cfg(test)]
async fn rpc_with_io<R, W>(
    harness: &AgentHarness,
    agent_events: Option<tokio::sync::broadcast::Receiver<AgentEvent>>,
    model_catalog: Vec<rpi_ai::Model>,
    ui_dialog: rpi_extensions::UiDialogMailbox,
    oauth_snapshot: Option<Arc<rpi_extensions::RegistrySnapshot>>,
    message_updates: crate::args::RpcMessageUpdates,
    input: R,
    output: W,
) -> i32
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: Write + Send + 'static,
{
    rpc_with_io_child(harness, agent_events, model_catalog, ui_dialog, oauth_snapshot, message_updates, input, output, None, None, None).await
}

#[cfg(test)]
async fn rpc_with_io_client<R, W>(
    harness: &AgentHarness,
    agent_events: Option<tokio::sync::broadcast::Receiver<AgentEvent>>,
    model_catalog: Vec<rpi_ai::Model>,
    ui_dialog: rpi_extensions::UiDialogMailbox,
    oauth_snapshot: Option<Arc<rpi_extensions::RegistrySnapshot>>,
    input: R,
    output: W,
    client_tools: Option<Arc<crate::rpc::client_tools::ClientTools>>,
) -> i32
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: Write + Send + 'static,
{
    rpc_with_io_child(harness, agent_events, model_catalog, ui_dialog, oauth_snapshot, Default::default(), input, output, client_tools, None, None).await
}

async fn rpc_with_io_child<R, W>(
    harness: &AgentHarness,
    mut agent_events: Option<tokio::sync::broadcast::Receiver<AgentEvent>>,
    model_catalog: Vec<rpi_ai::Model>,
    ui_dialog: rpi_extensions::UiDialogMailbox,
    oauth_snapshot: Option<Arc<rpi_extensions::RegistrySnapshot>>,
    message_updates: crate::args::RpcMessageUpdates,
    input: R,
    output: W,
    client_tools: Option<Arc<crate::rpc::client_tools::ClientTools>>,
    child_context: Option<&crate::session::ReloadContext>,
    escalate: Option<Box<dyn FnOnce() + Send>>,
) -> i32
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: Write + Send + 'static,
{
    let mut command_tasks = Vec::new();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    if let Err(error) = crate::rpc::system_prompt::restore_system_prompt(harness).await {
        eprintln!("[rpi] failed to restore system prompt: {error}");
        return 1;
    }
    let (stdout, writer_task, writer_failed) = start_rpc_writer_with_mode(output, message_updates);
    let watchdog = escalate.map(|escalate| RpcShutdownWatchdog::start(writer_failed.clone(), escalate));
    let pump_stop = tokio_util::sync::CancellationToken::new();
    let mode_state = Arc::new(tokio::sync::Mutex::new(RpcModeState {
        current: "default".to_string(),
        base_prompt: None,
        base_prompt_saved: false,
        base_tools: None,
    }));
    let client_request_broker = ClientRequestBroker::default();
    let mut samples = crate::rpc::sampling::Samples::default();
    let external_tools = crate::rpc::external_tools::ExternalTools::new(stdout.clone());
    let mut external_names = Vec::<String>::new();
    if let Some(client) = &client_tools {
        client.attach(stdout.clone(), harness.session().storage().metadata().id.clone());
    }
    // The selected mode is a durable native session entry. Restore it before
    // accepting requests so a new bridge process cannot advertise default
    // while the loaded session is still using plan semantics.
    if persisted_mode(harness).await.as_deref() == Some("plan") {
        if let Err(error) = rpc_apply_mode(&lane, harness, &mode_state, "plan", false).await {
            eprintln!("[rpi] failed to restore persisted plan mode: {error}");
        }
    }

    // Publish generic commit metadata after successful Session appends. The
    // event is intentionally not a durable-state payload: consumers reread
    // the canonical branch and use this only as an invalidation signal.
    let (commit_tx, mut commit_rx) = tokio::sync::broadcast::channel::<SessionEntryCommit>(256);
    harness
        .session()
        .set_entry_commit_publisher(Some(commit_tx.clone()));
    let commit_task = {
        let out = stdout.clone();
        tokio::spawn(async move {
            loop {
                if out.is_failed() { break; }
                match commit_rx.recv().await {
                    Ok(commit) => {
                        write_line(
                            &out,
                            &serde_json::json!({
                                "type": "session_entry_committed",
                                "entryId": commit.entry_id,
                                "entryType": commit.entry_type,
                                "customType": commit.custom_type,
                                "lane": commit.lane,
                            }),
                        )
                        .await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    };

    // A completion response is queued only after its final events, so a
    // subsequent prompt cannot overtake the previous run's agent_end.
    let (event_flush_tx, mut event_flush_rx) =
        tokio::sync::mpsc::unbounded_channel::<tokio::sync::oneshot::Sender<()>>();
    // Stream fine-grained AgentEvents as JSONL until the receiver closes.
    let event_task = agent_events.take().map(|mut rx| {
        let out = stdout.clone();
        let stop = pump_stop.clone();
        tokio::spawn(async move {
            loop {
                if out.is_failed() { break; }
                let received = tokio::select! {
                    _ = stop.cancelled() => {
                        drain_rpc_events(&out, &mut rx).await;
                        break;
                    }
                    Some(done) = event_flush_rx.recv() => {
                        drain_rpc_events(&out, &mut rx).await;
                        let _ = done.send(());
                        continue;
                    }
                    event = rx.recv() => event,
                };
                match received {
                    Ok(event) => write_agent_event(&out, &event).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    });

    let event_flush = event_task.as_ref().map(|_| event_flush_tx);

    // Bridge the extension `UiDialog` mailbox over the rpc channel so a
    // permission-gate extension can ask the client a question mid-run: we emit
    // `{"type":"ui_dialog_request",…}` and answer it from a
    // `{"type":"ui_dialog_response",…}` on stdin. Without this a headless run
    // leaves the mailbox detached and every `UiDialog` call fails.
    ui_dialog.attach();
    let dialog_task = {
        let mailbox = ui_dialog.clone();
        let out = stdout.clone();
        let stop = pump_stop.clone();
        tokio::spawn(async move {
            let mut shown = std::collections::HashMap::new();
            while !stop.is_cancelled() && !out.is_failed() {
                while let Some(request) = mailbox.take_pending() {
                    if out.is_failed() { break; }
                    let session_id = request.session_id.clone();
                    let event = crate::remote::protocol::RemoteEvent::UiDialogRequest {
                        request_id: request.request_id.clone(),
                        tool_call_id: request.tool_call_id,
                        ui: request.ui,
                    };
                    let mut event = serde_json::to_value(event).unwrap();
                    if let Some(id) = &session_id { event["sessionId"] = serde_json::json!(id); }
                    write_line(&out, &event).await;
                    shown.insert(request.request_id, session_id);
                }
                let closed: Vec<String> = shown
                    .keys()
                    .filter(|id| !mailbox.is_pending_wire(id))
                    .cloned()
                    .collect();
                for request_id in closed {
                    let session_id = shown.remove(&request_id).flatten();
                    let event = crate::remote::protocol::RemoteEvent::UiDialogClosed { request_id };
                    let mut event = serde_json::to_value(event).unwrap();
                    if let Some(id) = session_id { event["sessionId"] = serde_json::json!(id); }
                    write_line(&out, &event).await;
                }
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(30)) => {}
                }
            }
        })
    };

    // A `prompt` runs in a spawned task so this loop keeps reading stdin while
    // the run is active — otherwise `abort` and `ui_dialog_response` could not
    // be delivered mid-run. Only one prompt may be in flight at a time.
    let run_active = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut run_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut run_cancel = tokio_util::sync::CancellationToken::new();
    let mut cancel_on_exit = false;

    use tokio::io::AsyncBufReadExt;

    let mut lines = input.lines();
    let child_context = child_context.filter(|_| crate::session::child_events_enabled());
    let child_task = child_context.map(|ctx| {
        let (sink, task) = start_child_rpc_forwarder(stdout.clone(), ctx.action_bridge.clone());
        ctx.child_action_host.set_child_event_sink(Some(sink));
        task
    });
    let mut ready = rpc_protocol::ready(message_updates);
    if child_task.is_some() {
        ready["supportedEvents"].as_array_mut().unwrap().extend(
            ["child_start", "child_event", "child_end"].map(|name| serde_json::json!(name)),
        );
        ready["childEventsVersion"] = serde_json::json!(1);
    }
    write_line(&stdout, &ready).await;

    loop {
        if writer_failed.is_cancelled() {
            cancel_on_exit = true;
            break;
        }
        let incoming = tokio::select! {
            biased;
            _ = writer_failed.cancelled() => {
                cancel_on_exit = true;
                break;
            }
            line = lines.next_line() => line,
        };
        let line = match incoming {
            Ok(Some(line)) => line,
            Ok(None) => break, // EOF
            Err(error) => {
                eprintln!("[rpi] rpc stdin error: {error}");
                cancel_on_exit = true;
                break;
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                write_line(
                    &stdout,
                    &rpc_protocol::response(
                        None,
                        "invalid_request",
                        Err(format!("invalid json: {error}")),
                    ),
                )
                .await;
                continue;
            }
        };
        let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let kind = msg
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Client responses are deliberately not commands: they can arrive
        // while any native command or prompt is running. Resolve the
        // producer's request without consuming the response as a new command.
        if kind == "external_tool_response" {
            let result = if line.len() > rpc_protocol::MAX_FRAME_BYTES {
                Err("external response exceeds frame limit".into())
            } else {
                external_tools.respond(&msg).map(|accepted| serde_json::json!({"accepted":accepted}))
            };
            write_line(&stdout, &rpc_protocol::response(Some(id), &kind, result.map(Some))).await;
            continue;
        }
        if kind == "client_response" {
            if let Some(request_id) = msg.get("requestId").and_then(serde_json::Value::as_str) {
                let response =
                    if msg.get("cancelled").and_then(serde_json::Value::as_bool) == Some(true) {
                        Err("client request cancelled".to_owned())
                    } else if let Some(error) = msg.get("error") {
                        Err(error.to_string())
                    } else {
                        Ok(msg
                            .get("result")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null))
                    };
                if !client_request_broker.resolve(request_id, response.clone()) {
                    if let Some(client) = &client_tools {
                        let _ = client.broker.resolve(request_id, response);
                    }
                }
            }
            continue;
        }

        if rpc_protocol::SUPPORTED_COMMANDS.contains(&kind.as_str())
            && serde_json::from_value::<rpc_protocol::Request>(msg.clone()).is_err()
        {
            write_line(
                &stdout,
                &rpc_protocol::response(
                    Some(id.clone()),
                    &kind,
                    Err("invalid command payload".to_string()),
                ),
            )
            .await;
            continue;
        }

        match kind.as_str() {
            "sample" => {
                if let Err(error) = samples.start(harness, &msg, stdout.clone()).await {
                    write_line(&stdout, &rpc_protocol::response(Some(id), "sample", Err(error))).await;
                }
                continue;
            }
            "cancel_sample" => {
                let result = samples.cancel(&msg).await;
                write_line(&stdout, &rpc_protocol::response(Some(id), "cancel_sample", result.map(Some))).await;
                continue;
            }
            "client_request" => {
                let method = msg
                    .get("method")
                    .and_then(serde_json::Value::as_str)
                    .filter(|method| !method.trim().is_empty())
                    .map(str::to_owned);
                let params = msg
                    .get("params")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                let Some(method) = method else {
                    write_line(
                        &stdout,
                        &rpc_protocol::response(
                            Some(id),
                            "client_request",
                            Err("client_request requires method".into()),
                        ),
                    )
                    .await;
                    continue;
                };
                if !params.is_object() {
                    write_line(
                        &stdout,
                        &rpc_protocol::response(
                            Some(id),
                            "client_request",
                            Err("client_request params must be an object".into()),
                        ),
                    )
                    .await;
                    continue;
                }
                let timeout_ms = match msg.get("timeoutMs") {
                    None => MAX_TIMEOUT_MS,
                    Some(value) => match value.as_u64() {
                        Some(ms) if (1..=MAX_TIMEOUT_MS).contains(&ms) => ms,
                        _ => {
                            write_line(
                                &stdout,
                                &rpc_protocol::response(
                                    Some(id),
                                    "client_request",
                                    Err(format!(
                                        "timeoutMs must be between 1 and {MAX_TIMEOUT_MS}"
                                    )),
                                ),
                            )
                            .await;
                            continue;
                        }
                    },
                };
                let request = match client_request_broker.register() {
                    Ok(request) => request,
                    Err(error) => {
                        write_line(
                            &stdout,
                            &rpc_protocol::response(Some(id), "client_request", Err(error)),
                        )
                        .await;
                        continue;
                    }
                };
                let request_id = request.id.clone();
                write_line(
                    &stdout,
                    &serde_json::json!({
                        "type": "client_request",
                        "requestId": request_id.clone(),
                        // Opaque originating-command token for adapters that
                        // multiplex concurrent requests of the same method.
                        "correlationId": msg.get("correlationId").cloned(),
                        "sessionId": harness.session().storage().metadata().id,
                        "method": method,
                        "params": params,
                    }),
                )
                .await;
                command_tasks.retain(|task: &tokio::task::JoinHandle<()>| !task.is_finished());
                let out = stdout.clone();
                let cancel = pump_stop.clone();
                command_tasks.push(tokio::spawn(async move {
                    let result = request
                        .wait(std::time::Duration::from_millis(timeout_ms), &cancel)
                        .await;
                    write_line(
                        &out,
                        &serde_json::json!({
                            "type": "client_request_closed",
                            "requestId": request_id,
                        }),
                    )
                    .await;
                    write_line(
                        &out,
                        &rpc_protocol::response(Some(id), "client_request", result.map(Some)),
                    )
                    .await;
                }));
                continue;
            }
            "ui_dialog_response" => {
                let request_id = msg
                    .get("requestId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let result = match dialog_answer(&msg) {
                    Ok(Some(answer)) => ui_dialog.respond_wire(&request_id, answer),
                    Ok(None) => ui_dialog.cancel_wire(&request_id),
                    Err(error) => Err(error),
                };
                if msg.get("id").is_some() {
                    let response = match result {
                        Ok(()) => rpc_protocol::response(
                            Some(id),
                            "ui_dialog_response",
                            Ok(Some(
                                serde_json::json!({"accepted":true,"requestId":request_id}),
                            )),
                        ),
                        Err(error) => {
                            rpc_protocol::response(Some(id), "ui_dialog_response", Err(error))
                        }
                    };
                    write_line(&stdout, &response).await;
                } else if let Err(error) = result {
                    eprintln!("[rpi] ui_dialog_response: {error}");
                }
                continue;
            }
            "execute_command" => {
                command_tasks.retain(|task: &tokio::task::JoinHandle<()>| !task.is_finished());
                let snapshot = oauth_snapshot.clone();
                let request = msg.clone();
                let out = stdout.clone();
                command_tasks.push(tokio::spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        rpc_execute_command(snapshot.as_deref(), &request)
                    })
                    .await
                    .unwrap_or_else(|_| Err("command task failed".into()));
                    write_line(
                        &out,
                        &rpc_protocol::response(Some(id), "execute_command", result.map(Some)),
                    )
                    .await;
                }));
                continue;
            }
            "stop" => {
                cancel_on_exit = true;
                write_line(
                    &stdout,
                    &rpc_protocol::response(
                        Some(id.clone()),
                        "stop",
                        Ok(Some(serde_json::json!({"stopped": true}))),
                    ),
                )
                .await;
                break;
            }
            "prompt" | "resume_pending" | "compact" => {
                let resume = kind == "resume_pending";
                let compact = kind == "compact";
                if run_active.swap(true, std::sync::atomic::Ordering::AcqRel) {
                    write_line(
                        &stdout,
                        &rpc_protocol::response(
                            Some(id.clone()),
                            &kind,
                            Err("a run is already active".to_string()),
                        ),
                    )
                    .await;
                    continue;
                }
                if let Some(task) = run_task.take() {
                    let _ = task.await;
                }
                run_cancel = tokio_util::sync::CancellationToken::new();
                let cancel_task = run_cancel.clone();
                // Resuming spends the tokens the interrupted run never did, so it
                // only proceeds when recovery actually left a pending run.
                if resume && !lane.has_pending_resume().await {
                    run_active.store(false, std::sync::atomic::Ordering::Release);
                    write_line(
                        &stdout,
                        &rpc_protocol::response(
                            Some(id),
                            "resume_pending",
                            Ok(Some(serde_json::json!({"resumed": false}))),
                        ),
                    )
                    .await;
                    continue;
                }
                let ordered = match rpc_protocol::ordered_content(&msg) {
                    Ok(parts) => parts,
                    Err(error) => {
                        run_active.store(false, std::sync::atomic::Ordering::Release);
                        write_line(&stdout, &rpc_protocol::response(Some(id), "prompt", Err(error))).await;
                        continue;
                    }
                };
                let content = msg
                    .get("message")
                    .or_else(|| msg.get("content"))
                    .or_else(|| msg.get("prompt"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                let images = match msg.get("images") {
                    None => Vec::new(),
                    Some(value) => match serde_json::from_value::<Vec<ImageContent>>(value.clone())
                    {
                        Ok(images) => images,
                        Err(error) => {
                            run_active.store(false, std::sync::atomic::Ordering::Release);
                            write_line(
                                &stdout,
                                &rpc_protocol::response(
                                    Some(id),
                                    "prompt",
                                    Err(format!("invalid image content: {error}")),
                                ),
                            )
                            .await;
                            continue;
                        }
                    },
                };
                let audio = match msg.get("audio") {
                    None => Vec::new(),
                    Some(value) => match serde_json::from_value::<Vec<AudioContent>>(value.clone())
                    {
                        Ok(audio) => audio,
                        Err(error) => {
                            run_active.store(false, std::sync::atomic::Ordering::Release);
                            write_line(
                                &stdout,
                                &rpc_protocol::response(
                                    Some(id),
                                    "prompt",
                                    Err(format!("invalid audio content: {error}")),
                                ),
                            )
                            .await;
                            continue;
                        }
                    },
                };
                let lane_task = Arc::clone(&lane);
                let out_task = stdout.clone();
                let active_task = Arc::clone(&run_active);
                let mailbox_task = ui_dialog.clone();
                let flush_events = event_flush.clone();
                let Some(content) = content.or_else(|| (resume || compact || ordered.is_some()).then(String::new))
                else {
                    active_task.store(false, std::sync::atomic::Ordering::Release);
                    write_line(
                        &stdout,
                        &rpc_protocol::response(
                            Some(id),
                            "prompt",
                            Err("prompt requires a string `message`".to_string()),
                        ),
                    )
                    .await;
                    continue;
                };
                write_line(
                    &stdout,
                    &rpc_protocol::response(
                        Some(id),
                        &kind,
                        Ok(Some(serde_json::json!({"accepted": true}))),
                    ),
                )
                .await;
                let instructions = msg
                    .get("customInstructions")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                run_task = Some(tokio::spawn(async move {
                    let compaction_id = if compact {
                        Some(format!("compaction-{}", uuid::Uuid::new_v4()))
                    } else {
                        None
                    };
                    if let Some(id) = &compaction_id {
                        write_line(
                            &out_task,
                            &serde_json::json!({
                                "type": "compaction_started",
                                "compactionId": id,
                            }),
                        )
                        .await;
                    }
                    let result = if compact {
                        rpc_compact_cancellable(&lane_task, instructions, cancel_task).await
                    } else if resume {
                        rpc_resume_cancellable(&lane_task, cancel_task).await
                    } else {
                        rpc_prompt_cancellable(&lane_task, &content, images, audio, ordered, cancel_task)
                            .await
                    };
                    if let Some(id) = &compaction_id {
                        let (status, summary, error) = match &result {
                            Ok(value) if value["outcome"] == "compacted" => {
                                ("completed", value.get("summary").cloned(), None)
                            }
                            Ok(value) if value["outcome"] == "aborted" => {
                                ("cancelled", value.get("summary").cloned(), None)
                            }
                            Ok(value) => (
                                "failed",
                                value.get("summary").cloned(),
                                value.get("error").cloned(),
                            ),
                            Err(error) => ("failed", None, Some(serde_json::json!(error))),
                        };
                        write_line(
                            &out_task,
                            &serde_json::json!({
                                "type": "compaction_finished",
                                "compactionId": id,
                                "status": status,
                                "summary": summary,
                                "error": error,
                            }),
                        )
                        .await;
                    }
                    if let Some(flush) = flush_events {
                        let (done, drained) = tokio::sync::oneshot::channel();
                        if flush.send(done).is_ok() {
                            let _ = drained.await;
                        }
                    }
                    let (outcome, aborted, error) = match &result {
                        Ok(value) => {
                            let outcome = value
                                .get("outcome")
                                .and_then(|v| v.as_str())
                                .unwrap_or("completed")
                                .to_string();
                            let error = value
                                .get("error")
                                .and_then(|v| v.as_str())
                                .map(str::to_owned);
                            (outcome.clone(), outcome == "aborted", error)
                        }
                        Err(ref error) => ("failed".to_string(), false, Some(error.clone())),
                    };
                    // Recovery may leave a continuable run behind an abort, so the
                    // settlement carries what the client needs to offer "continue".
                    let pending_resume = lane_task.has_pending_resume().await;
                    let summary = result
                        .as_ref()
                        .ok()
                        .and_then(|value| value.get("summary"))
                        .cloned();
                    let settled = serde_json::json!({
                        "type": "agent_settled",
                        "runId": result.as_ref().ok().and_then(|value| value.get("runId")),
                        "outcome": outcome,
                        "aborted": aborted,
                        "error": error,
                        "summary": summary,
                        "pendingResume": pending_resume,
                    });
                    write_line(&out_task, &settled).await;
                    mailbox_task.cancel_all();
                    active_task.store(false, std::sync::atomic::Ordering::Release);
                }));
                continue;
            }
            _ => {}
        }

        // Abort retires foreground bridge requests, not independent samples.
        // Sampling callers cancel their exact sampleId; connection teardown
        // below still cancels and joins every remaining sample.
        if kind == "abort" {
            // Publish run cancellation before retiring tool waiters. A waiter
            // can complete on another worker immediately, so the reverse order
            // may settle the run as completed before it observes cancellation.
            if run_active.load(std::sync::atomic::Ordering::Acquire) {
                run_cancel.cancel();
                let _ = lane.abort().await;
            }
            client_request_broker.cancel_all();
            external_tools.abort();
        }
        let result = match kind.as_str() {
            "set_system_prompt" if run_active.load(std::sync::atomic::Ordering::Acquire) =>
                Err("cannot change system prompt while a run is active".into()),
            "set_system_prompt" => rpc_set_system_prompt(&lane, harness, &mode_state, &msg).await,
            "configure_external_tools" if run_active.load(std::sync::atomic::Ordering::Acquire) =>
                Err("cannot configure external tools while a prompt is active".into()),
            "configure_external_tools" => rpc_configure_external_tools(&lane, harness, &mode_state,
                &external_tools, &mut external_names, &msg).await,
            "set_mode" | "set_active_tools" if run_active.load(std::sync::atomic::Ordering::Acquire) =>
                Err("cannot change tools or mode while a prompt is active".into()),
            "abort" if run_active.load(std::sync::atomic::Ordering::Acquire) => {
                // This also covers abort arriving before the task enters the lane.
                run_cancel.cancel();
                Ok(serde_json::json!({"aborted": true}))
            }
            "abort" => lane
                .abort()
                .await
                .map(|_| serde_json::json!({"aborted": true}))
                .map_err(|e| e.to_string()),
            "get_state" => Ok(rpc_get_state(&lane, harness, &mode_state).await),
            "get_context_usage" => Ok(rpc_context_usage(&lane, harness).await),
            "set_compaction_enabled" => rpc_set_compaction_enabled(harness, &msg).await,
            "get_modes" => Ok(rpc_get_modes(&mode_state).await),
            "set_mode" => rpc_set_mode(&lane, harness, &mode_state, &msg).await,
            "get_commands" => Ok(rpc_get_commands(oauth_snapshot.as_deref())),
            "get_messages" => rpc_get_messages(&lane, harness, &msg).await,
            "get_history" => rpc_get_history(&lane, harness, &msg).await,
            "get_session_branch" => rpc_get_session_branch(harness, &msg).await,
            "append_entry" => rpc_append_entry(harness, &msg).await,
            "get_stats" => rpc_get_stats(&lane, harness).await,
            "get_queue" => rpc_get_queue(&lane, harness, &msg).await,
            "queue_message" => rpc_queue_message(&lane, &msg).await,
            "cancel_queued" => rpc_cancel_queued(&lane, &msg).await,
            "clear_queue" => rpc_clear_queue(&lane).await,
            "navigate" => rpc_navigate(&lane, &msg).await,
            "get_available_models" => Ok(rpc_get_available_models(&model_catalog)),
            "get_auth_status" => Ok(rpc_get_auth_status(&model_catalog, oauth_snapshot.as_ref())),
            "oauth" => rpc_oauth(&oauth_snapshot, &msg),
            "set_api_key" => rpc_set_api_key(&msg).await,
            "set_model" => rpc_set_model(&lane, &model_catalog, &msg).await,
            "get_available_thinking_levels" => Ok(rpc_get_available_thinking_levels(&lane).await),
            "set_thinking_level" => rpc_set_thinking_level(&lane, &msg).await,
            "get_available_tools" => Ok(rpc_get_available_tools(&lane, harness).await),
            "set_active_tools" if mode_state.lock().await.current == "plan" =>
                Err("cannot override active tools in plan mode".into()),
            "set_active_tools" => rpc_set_active_tools(&lane, &msg).await,
            "ping" => Ok(serde_json::json!({"pong": true})),
            other => Err(format!("unknown command type: {other}")),
        };

        let response = rpc_protocol::response(Some(id.clone()), &kind, result.map(Some));
        write_line(&stdout, &response).await;
    }

    // Bound the entire teardown, including native tools/provider joins, not
    // just the async writer handle. Only successful joins disarm escalation.
    if let Some(watchdog) = &watchdog { watchdog.arm(); }
    if writer_failed.is_cancelled() {
        cancel_on_exit = true;
        pump_stop.cancel();
    }
    // Samples never survive EOF, stop or output failure.
    samples.shutdown().await;
    // EOF closes the input half of the protocol, not an accepted prompt.
    // There can be no more dialog answers, so release approvals, then await the
    // run before SessionShutdown can unload plugins or the runtime exits.
    if let Some(ctx) = child_context {
        // Revoke owners before releasing the sink. Each child owns its sink
        // until its terminal callback finishes; channel EOF joins that drain.
        if let Some(bridge) = ctx.action_bridge.lock().unwrap().as_ref() {
            bridge.invalidate();
        }
        ctx.child_action_host.set_child_event_sink(None);
    }
    ui_dialog.detach();
    external_tools.shutdown();
    client_request_broker.cancel_all();
    if let Some(client) = &client_tools {
        client.shutdown().await;
    }
    if cancel_on_exit {
        run_cancel.cancel();
    }
    for task in command_tasks {
        let _ = task.await;
    }
    let mut exit_code = 0;
    if let Some(task) = run_task {
        if let Err(error) = task.await {
            eprintln!("[rpi] rpc prompt task failed: {error}");
            exit_code = 1;
        }
    }
    if let Some(task) = child_task {
        let _ = task.await;
    }
    pump_stop.cancel();
    harness.session().set_entry_commit_publisher(None);
    // Accepted writers have finished. Close the last local sender and let the
    // receiver forward queued commits before the output writer is flushed.
    drop(commit_tx);
    if let Err(error) = commit_task.await {
        eprintln!("[rpi] rpc commit event task failed: {error}");
        exit_code = 1;
    }
    let _ = dialog_task.await;
    if let Some(task) = event_task {
        let _ = task.await;
    }
    // All producers have joined; close the queue and flush every queued line.
    drop(stdout);
    match writer_task.await {
        Ok(Ok(())) => {}
        error => {
            eprintln!("[rpi] rpc output failed: {error:?}");
            exit_code = 1;
        }
    }
    if writer_failed.is_cancelled() { exit_code = 1; }
    if let Some(watchdog) = watchdog { watchdog.joined(); }
    exit_code
}

/// Process-owner policy, deliberately outside RpcWriter. A dedicated OS thread
/// enforces the deadline even if Tokio workers or native tool joins are stuck.
/// A timed-out JoinHandle is NOT evidence that a blocking write has stopped.
struct RpcShutdownWatchdog {
    arm: std::sync::mpsc::Sender<bool>,
    thread: std::thread::JoinHandle<()>,
}

impl RpcShutdownWatchdog {
    fn start(failed: tokio_util::sync::CancellationToken, escalate: Box<dyn FnOnce() + Send>) -> Self {
        let (arm, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut deadline = None;
            loop {
                match rx.recv_timeout(std::time::Duration::from_millis(10)) {
                    Ok(true) => break, // all producers AND writer actually joined
                    Ok(false) => { deadline.get_or_insert_with(|| std::time::Instant::now() + std::time::Duration::from_secs(2)); }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        // Owner unwound without proving joins: fail closed.
                        deadline.get_or_insert_with(|| std::time::Instant::now() + std::time::Duration::from_secs(2));
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
                if failed.is_cancelled() {
                    deadline.get_or_insert_with(|| std::time::Instant::now() + std::time::Duration::from_secs(2));
                }
                if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                    escalate();
                    break;
                }
            }
        });
        Self { arm, thread }
    }

    fn arm(&self) { let _ = self.arm.send(false); }

    fn joined(self) {
        let _ = self.arm.send(true);
        let _ = self.thread.join();
    }
}

fn start_child_rpc_forwarder(
    out: RpcWriter,
    bridges: crate::session::ActionBridgeCell,
) -> (crate::child_agents::ChildEventSink, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(serde_json::Value, tokio::sync::oneshot::Sender<()>)>(64);
    let task = tokio::spawn(async move {
        while let Some((envelope, admitted)) = rx.recv().await {
            write_line(&out, &envelope).await;
            if out.is_failed() { break; }
            // A child runner cannot issue tool UI until its start envelope is
            // admitted to the same writer queue used by those UI requests.
            let _ = admitted.send(());
        }
    });
    let sink: crate::child_agents::ChildEventSink = Arc::new(move |envelope| {
        let tx = tx.clone();
        let bridges = bridges.clone();
        Box::pin(async move {
            let (admitted, admission) = tokio::sync::oneshot::channel();
            if tx.send((envelope, admitted)).await.is_err() || admission.await.is_err() {
                if let Some(bridge) = bridges.lock().unwrap().as_ref() {
                    bridge.invalidate();
                }
            }
        })
    });
    (sink, task)
}

fn rpc_get_commands(snapshot: Option<&rpi_extensions::RegistrySnapshot>) -> serde_json::Value {
    let commands: Vec<_> = snapshot
        .into_iter()
        .flat_map(|s| s.commands())
        .map(
            |command| serde_json::json!({"name": command.name, "description": command.description}),
        )
        .collect();
    serde_json::json!({"commands": commands})
}

fn rpc_execute_command(
    snapshot: Option<&rpi_extensions::RegistrySnapshot>,
    request: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let name = request
        .get("command")
        .and_then(|v| v.as_str())
        .filter(|name| !name.trim_start_matches('/').is_empty())
        .ok_or("command must be a non-empty string")?;
    let args = match request.get("args") {
        None => "",
        Some(value) => value.as_str().ok_or("args must be a string")?,
    };
    let command = snapshot
        .into_iter()
        .flat_map(|s| s.commands())
        .find(|c| c.name.trim_start_matches('/') == name.trim_start_matches('/'))
        .ok_or_else(|| format!("unknown command: {name}"))?;
    let input = serde_json::json!({"command": name, "args": args}).to_string();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut out = rpi_plugin_sdk::StbString::empty();
        let rc = (command.handler)(
            rpi_plugin_sdk::StbStringRef::from_str(&input),
            &mut out,
            command.user_data,
        );
        // SAFETY: the callback transfers its output using the host allocator,
        // matching the TUI command invocation contract.
        let out = unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(
                out,
                Some(rpi_extensions::host_free_string),
            )
        };
        if rc != 0 {
            return Err(format!("command failed: {name} (code {rc})"));
        }
        serde_json::from_str(&out.to_string_lossy())
            .map_err(|_| format!("command returned invalid JSON: {name}"))
    }))
    .unwrap_or_else(|_| Err(format!("command panicked: {name}")))
}

async fn drain_rpc_events(
    out: &RpcWriter,
    events: &mut tokio::sync::broadcast::Receiver<AgentEvent>,
) {
    loop {
        if out.is_failed() { break; }
        match events.try_recv() {
            Ok(event) => write_agent_event(out, &event).await,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
}

/// Cancellation is delivered even if abort/stop arrives before prompt_text has
/// registered a run. Once active, await its normal aborted outcome and cleanup.
async fn rpc_prompt_cancellable(
    lane: &Arc<dyn AgentLane>,
    content: &str,
    images: Vec<ImageContent>,
    audio: Vec<AudioContent>,
    ordered: Option<Vec<Content>>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<serde_json::Value, String> {
    let prompt = rpc_prompt_text(lane, content, images, audio, ordered);
    tokio::pin!(prompt);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            match lane.abort().await {
                Ok(_) => prompt.await,
                Err(_) => Err("Operation aborted".to_string()),
            }
        }
        result = &mut prompt => result,
    }
}

/// Compact the branch, unless the caller cancels first.
async fn rpc_compact_cancellable(
    lane: &Arc<dyn AgentLane>,
    instructions: Option<String>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<serde_json::Value, String> {
    let compact = async {
        match lane.compact(instructions.as_deref()).await {
            Ok(result) => Ok(compaction_json(&result)),
            Err(error) => Err(error.to_string()),
        }
    };
    tokio::pin!(compact);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = lane.abort().await;
            compact.await
        }
        result = &mut compact => result,
    }
}

/// Reduce a compaction result to what a client needs: the verdict, and a line
/// a transcript can print.
fn compaction_json(result: &CompactionResult) -> serde_json::Value {
    let mut value = match &result.outcome {
        CompactionOutcome::Completed { leaf_id, entry } => {
            let (tokens_before, retained) = match entry {
                rpi_harness::session::types::Entry::Compaction(compaction) => {
                    (compaction.tokens_before, compaction.retained_tail.len())
                }
                _ => (0, 0),
            };
            serde_json::json!({
                "outcome": "compacted",
                "leafId": leaf_id,
                "tokensBefore": tokens_before,
                "retainedMessages": retained,
                "summary": format!(
                    "Conversation compacted · {} tokens before, {} messages kept.",
                    tokens_before, retained
                ),
            })
        }
        CompactionOutcome::Declined { leaf_id } => serde_json::json!({
            "outcome": "declined",
            "leafId": leaf_id,
            "summary": "Nothing to compact yet.",
        }),
        CompactionOutcome::Aborted { leaf_id } => serde_json::json!({
            "outcome": "aborted",
            "leafId": leaf_id,
            "summary": "Compaction aborted.",
        }),
        CompactionOutcome::Failed { leaf_id, error } => serde_json::json!({
            "outcome": "failed",
            "leafId": leaf_id,
            "error": error.message,
            "summary": format!("Compaction failed: {}", error.message),
        }),
    };
    value["runId"] = serde_json::json!(result.run_id);
    value
}

/// Drive the run recovery left behind, unless the caller cancels first.
///
/// `Ok(None)` from the lane means there was nothing to resume: the run is
/// reported as `skipped` so the client can settle without inventing an outcome.
async fn rpc_resume_cancellable(
    lane: &Arc<dyn AgentLane>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<serde_json::Value, String> {
    let resume = async {
        match lane.resume_pending().await {
            Ok(Some(result)) => {
                let (outcome, final_text, error) = rpc_outcome_str(&result.outcome);
                Ok(serde_json::json!({
                    "runId": result.run_id,
                    "outcome": outcome,
                    "finalText": final_text,
                    "error": error,
                    "resumed": true,
                }))
            }
            Ok(None) => Ok(serde_json::json!({"outcome": "skipped", "resumed": false})),
            Err(error) => Err(error.to_string()),
        }
    };
    tokio::pin!(resume);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = lane.abort().await;
            resume.await
        }
        result = &mut resume => result,
    }
}

fn rpc_outcome_str(outcome: &HarnessRunOutcome) -> (&'static str, Option<String>, Option<String>) {
    match outcome {
        HarnessRunOutcome::Completed { final_message, .. } => {
            ("completed", Some(assistant_text(final_message)), None)
        }
        HarnessRunOutcome::Aborted { final_message, .. } => {
            ("aborted", Some(assistant_text(final_message)), None)
        }
        // A failed run can end with nothing to say, which is exactly when the
        // reason matters: rate limits and provider errors live here.
        HarnessRunOutcome::Failed {
            final_message,
            error,
            ..
        } => (
            "failed",
            final_message.as_ref().map(assistant_text),
            Some(error.message.clone()),
        ),
        HarnessRunOutcome::Suspended { .. } => ("suspended", None, None),
    }
}

async fn rpc_prompt_text(
    lane: &Arc<dyn AgentLane>,
    content: &str,
    images: Vec<ImageContent>,
    audio: Vec<AudioContent>,
    ordered: Option<Vec<Content>>,
) -> Result<serde_json::Value, String> {
    let result = if let Some(parts) = ordered {
        lane.prompt_message(rpi_agent::message::AgentMessage::User(UserMessage::new(
            rpi_ai::types::UserContent::Blocks(parts), rpc_now_ms(),
        ))).await
    } else {
        lane.prompt_text_with_media(content, images, audio).await
    }.map_err(|e| e.to_string())?;
    let (outcome, final_text, error) = rpc_outcome_str(&result.outcome);
    Ok(
        serde_json::json!({"runId":result.run_id, "outcome": outcome, "finalText": final_text, "error": error}),
    )
}

async fn rpc_get_state(
    lane: &Arc<dyn AgentLane>,
    harness: &AgentHarness,
    mode_state: &Arc<tokio::sync::Mutex<RpcModeState>>,
) -> serde_json::Value {
    let model = lane.get_model().await.ok();
    let thinking = lane
        .get_thinking_level()
        .await
        .ok()
        .map(|t| serde_json::to_value(t).unwrap_or(serde_json::Value::Null));
    let active_tools = lane.get_active_tools().await.unwrap_or_default();
    let leaf_id = lane.get_leaf_id().await.ok().flatten();
    let context = rpc_context_usage(lane, harness).await;
    let active_operation = lane
        .snapshot()
        .await
        .ok()
        .and_then(|snapshot| snapshot.active)
        .map(|active| {
            serde_json::json!({
                "runId": active.run_id,
                "lane": active.lane,
                "kind": format!("{:?}", active.kind).to_lowercase(),
            })
        });
    let queued = lane.queued_messages().await.unwrap_or_default();
    let stats = rpc_get_stats(lane, harness).await.ok();
    let mode = mode_state.lock().await.current.clone();
    serde_json::json!({
        "modelId": model.as_ref().map(|model| model.id.clone()),
        "model": model.as_ref().map(|model| model.id.clone()),
        "provider": model.as_ref().map(|model| model.provider.clone()),
        "thinkingLevel": thinking,
        "activeTools": active_tools,
        "modeId": mode,
        "systemPrompt": harness.get_system_prompt().await.ok().flatten(),
        "availableModes": mode_descriptors(),
        "leafId": leaf_id,
        "context": context,
        "compactionEnabled": harness.compaction_settings().enabled,
        "sessionId": harness.session().storage().metadata().id,
        "activeOperation": active_operation,
        "queue": {
            "steering": queued.steering,
            "followUp": queued.follow_up,
            "steeringCount": queued.steering.len(),
            "followUpCount": queued.follow_up.len(),
        },
        "stats": stats,
    })
}

fn rpc_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn rpc_message_text(value: &serde_json::Value) -> String {
    let Some(content) = value.get("content") else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block.get("text").and_then(|text| text.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn rpc_get_history(
    lane: &Arc<dyn AgentLane>,
    harness: &AgentHarness,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let limit = msg
        .get("limit")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize;
    let after_seq = msg.get("afterSeq").and_then(serde_json::Value::as_u64);
    let before_seq = msg.get("beforeSeq").and_then(serde_json::Value::as_u64);
    let mut records = harness
        .session()
        .find_records(&RecordQuery {
            lane: Some(lane.name().to_string()),
            after_seq,
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .map_err(|error| error.to_string())?;
    if let Some(before) = before_seq {
        records.retain(|record| record.seq() < before);
    }
    let total = records.len();
    let has_more = records.len() > limit;
    if before_seq.is_some() {
        if has_more {
            records = records.split_off(records.len() - limit);
        }
    } else if after_seq.is_some() {
        records.truncate(limit);
    } else if has_more {
        records = records.split_off(records.len() - limit);
    }
    let first_seq = records.first().map(LaneRecord::seq);
    let last_seq = records.last().map(LaneRecord::seq);
    let open_operations = harness
        .session()
        .find_open_operations(lane.name(), Some(50))
        .await
        .map_err(|error| error.to_string())?;
    Ok(serde_json::json!({
        "records": records.iter().map(|record| serde_json::to_value(record).unwrap_or(serde_json::Value::Null)).collect::<Vec<_>>(),
        "total": total,
        "limit": limit,
        "afterSeq": after_seq,
        "beforeSeq": before_seq,
        "firstSeq": first_seq,
        "lastSeq": last_seq,
        "hasMore": has_more,
        "leafId": lane.get_leaf_id().await.ok().flatten(),
        "openOperations": open_operations.iter().map(|record| serde_json::to_value(record).unwrap_or(serde_json::Value::Null)).collect::<Vec<_>>(),
    }))
}

async fn rpc_get_session_branch(
    harness: &AgentHarness,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    // Envelope fields are not query arguments. Everything else is validated by
    // the same strict parser used by the runtime action.
    let mut args = msg.clone();
    if let Some(args) = args.as_object_mut() {
        args.remove("type");
        args.remove("id");
    }
    let request = branch_entry_query(&args)?;
    let entries = find_branch_entries(harness, &request).await?;
    let entries = entries
        .iter()
        .map(|entry| serde_json::to_value(entry).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::json!({
        "entries": entries,
        "count": entries.len(),
        "filtered": true,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BranchEntryQuery {
    pub custom_types: Vec<String>,
    pub order: EntryOrder,
    pub limit: Option<usize>,
    pub after_seq: Option<u64>,
}

impl BranchEntryQuery {
    fn entry_queries(&self) -> Vec<EntryQuery> {
        // A global top-L entry must be within its type's top L under the same
        // order/cursor. Bound cloned payloads before the global merge as well.
        let per_query_limit = self.limit;
        self.custom_types
            .iter()
            .map(|custom_type| EntryQuery {
                entry_type: Some("custom"),
                custom_type: Some(custom_type.clone()),
                order: Some(self.order),
                limit: per_query_limit,
                cursor: self.after_seq.map(|after_seq| EntryCursor { after_seq }),
            })
            .collect()
    }
}

pub(crate) async fn find_branch_entries(
    harness: &AgentHarness,
    request: &BranchEntryQuery,
) -> Result<Vec<Entry>, String> {
    // Pin both the storage binding and branch tip once. Session handles follow
    // harness session switches, and resolving the live leaf per type can merge
    // entries from different branches while a query is in flight.
    let storage = harness.session().storage();
    let leaf_id = storage
        .get_lanes()
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|pointer| pointer.lane == "main")
        .ok_or_else(|| "Lane not found: main".to_string())?
        .leaf_id;
    let Some(start) = leaf_id else {
        return Ok(Vec::new());
    };
    let bounds = BranchBounds {
        start: Some(start.clone()),
        ..Default::default()
    };
    let mut entries = Vec::new();
    for query in request.entry_queries() {
        entries.extend(
            storage
                .find_entries_on_branch(&query, &bounds, &start)
                .await
                .map_err(|error| error.to_string())?,
        );
    }
    if request.custom_types.len() > 1 {
        let mut seen = HashSet::with_capacity(entries.len());
        entries.retain(|entry| seen.insert(entry.id().to_owned()));
        match request.order {
            EntryOrder::OldestFirst => entries.sort_by_key(Entry::seq),
            EntryOrder::NewestFirst => entries.sort_by_key(|entry| Reverse(entry.seq())),
        }
        if let Some(limit) = request.limit {
            entries.truncate(limit);
        }
    }
    Ok(entries)
}

/// Canonical custom-entry query. Reject unknown/legacy arguments even when
/// valid modern arguments are also present; never fall back to a full scan.
pub(crate) fn branch_entry_query(msg: &serde_json::Value) -> Result<BranchEntryQuery, String> {
    let args = msg
        .as_object()
        .ok_or("get_session_branch: query must be an object")?;
    for key in args.keys() {
        if !matches!(key.as_str(), "customTypes" | "afterSeq" | "limit" | "order") {
            return Err(format!(
                "get_session_branch: unknown query argument `{key}`"
            ));
        }
    }
    let values = args
        .get("customTypes")
        .and_then(serde_json::Value::as_array)
        .filter(|values| !values.is_empty())
        .ok_or(
            "get_session_branch: `customTypes` is required and must be a non-empty string array",
        )?;
    let mut custom_types = Vec::with_capacity(values.len());
    for value in values {
        let custom_type = value
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or("get_session_branch: every `customTypes` item must be a non-empty string")?;
        if !custom_types.iter().any(|existing| existing == custom_type) {
            custom_types.push(custom_type.to_owned());
        }
    }
    let limit = match msg.get("limit") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => {
            let limit = value
                .as_u64()
                .filter(|limit| *limit > 0)
                .ok_or("get_session_branch: `limit` must be a positive integer")?;
            Some(usize::try_from(limit).map_err(|_| "get_session_branch: `limit` is too large")?)
        }
    };
    let order = match msg.get("order") {
        None => EntryOrder::OldestFirst,
        Some(serde_json::Value::String(value)) if value == "oldestFirst" => EntryOrder::OldestFirst,
        Some(serde_json::Value::String(value)) if value == "newestFirst" => EntryOrder::NewestFirst,
        Some(_) => {
            return Err("get_session_branch: `order` must be oldestFirst or newestFirst".into())
        }
    };
    let after_seq = match msg.get("afterSeq") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .ok_or("get_session_branch: `afterSeq` must be a non-negative integer")?,
        ),
    };
    Ok(BranchEntryQuery {
        custom_types,
        order,
        limit,
        after_seq,
    })
}

async fn rpc_append_entry(
    harness: &AgentHarness,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let custom_type = msg
        .get("customType")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "append_entry requires a non-empty string `customType`".to_string())?;
    let entry_id = harness
        .session()
        .append_custom_entry(custom_type, msg.get("data").cloned())
        .await
        .map_err(|error| error.to_string())?;
    Ok(serde_json::json!({"entryId": entry_id, "customType": custom_type}))
}

async fn rpc_set_system_prompt(
    lane: &Arc<dyn AgentLane>,
    harness: &AgentHarness,
    mode_state: &Arc<tokio::sync::Mutex<RpcModeState>>,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    use crate::rpc::system_prompt::{persisted_snapshot, PromptSnapshot, validate_prompt, SYSTEM_PROMPT_ENTRY};
    if lane.snapshot().await.map_err(|error| error.to_string())?.active.is_some() {
        return Err("cannot change system prompt while a run is active".into());
    }
    // Plan mode owns a temporary prompt overlay and its saved base. Do not
    // silently replace either half of that transaction.
    if mode_state.lock().await.current != "default" {
        return Err("return to default mode before changing system prompt".into());
    }
    let prompt = msg.get("prompt").and_then(serde_json::Value::as_str)
        .ok_or("set_system_prompt requires string `prompt`")?;
    validate_prompt(prompt)?;
    let mode = match msg.get("mode") {
        None => "append",
        Some(value) => value.as_str().ok_or("mode must be append or replace")?,
    };
    let old = harness.get_system_prompt().await.map_err(|error| error.to_string())?;
    let previous = persisted_snapshot(harness).await?;
    let snapshot = PromptSnapshot::configure(previous.as_ref(), old.clone(), prompt, mode)?;
    let target = &snapshot.effective;
    if previous.as_ref().is_some_and(|p| p.version == 2 && p.client_prompt == prompt && p.mode == mode) {
        return Ok(serde_json::json!({"systemPrompt": target, "mode": mode}));
    }
    harness.set_system_prompt(Some(target.clone())).await.map_err(|error| error.to_string())?;
    if let Err(error) = harness.append_custom_entry(
        SYSTEM_PROMPT_ENTRY,
        Some(serde_json::to_value(&snapshot).map_err(|error| error.to_string())?),
    ).await {
        harness.set_system_prompt(old).await
            .map_err(|rollback| format!("system prompt persist failed: {error}; rollback failed: {rollback}"))?;
        return Err(format!("system prompt change rolled back: {error}"));
    }
    Ok(serde_json::json!({"systemPrompt": target, "mode": mode}))
}

async fn persisted_mode(harness: &AgentHarness) -> Option<String> {
    let query = EntryQuery {
        custom_type: Some("rpi_mode".to_string()),
        order: Some(EntryOrder::OldestFirst),
        ..Default::default()
    };
    harness
        .find_entries_on_branch(&query, &BranchBounds::default())
        .await
        .ok()?
        .into_iter()
        .rev()
        .find_map(|entry| match entry {
            rpi_harness::session::types::Entry::Custom(custom)
                if custom.custom_type == "rpi_mode" =>
            {
                custom.data.and_then(|data| {
                    data.get("modeId")
                        .and_then(|id| id.as_str())
                        .map(str::to_owned)
                })
            }
            _ => None,
        })
        .filter(|mode| matches!(mode.as_str(), "default" | "plan"))
}

async fn rpc_get_modes(mode_state: &Arc<tokio::sync::Mutex<RpcModeState>>) -> serde_json::Value {
    let state = mode_state.lock().await;
    serde_json::json!({"currentModeId": state.current, "availableModes": mode_descriptors()})
}

const PLAN_READ_ONLY_TOOLS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "search",
    "codegraph",
    "codegraph_search",
    "webfetch",
    "websearch",
    "todo",
];

async fn rpc_set_mode(
    lane: &Arc<dyn AgentLane>,
    harness: &AgentHarness,
    mode_state: &Arc<tokio::sync::Mutex<RpcModeState>>,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mode = msg
        .get("modeId")
        .and_then(serde_json::Value::as_str)
        .ok_or("set_mode requires `modeId`")?;
    rpc_apply_mode(lane, harness, mode_state, mode, true).await
}

async fn rpc_apply_mode(
    lane: &Arc<dyn AgentLane>,
    harness: &AgentHarness,
    mode_state: &Arc<tokio::sync::Mutex<RpcModeState>>,
    mode: &str,
    persist: bool,
) -> Result<serde_json::Value, String> {
    if !matches!(mode, "default" | "plan") {
        return Err(format!("unknown mode: {mode}"));
    }
    if lane
        .snapshot()
        .await
        .map_err(|error| error.to_string())?
        .active
        .is_some()
    {
        return Err("cannot change session mode while a run is active".into());
    }

    let mut state = mode_state.lock().await;
    if state.current == mode {
        return Ok(serde_json::json!({
            "currentModeId": state.current,
            "availableModes": mode_descriptors()
        }));
    }
    let old_prompt = harness
        .get_system_prompt()
        .await
        .map_err(|e| e.to_string())?;
    let old_tools = lane.get_active_tools().await.map_err(|e| e.to_string())?;
    let old_base_prompt_saved = state.base_prompt_saved;
    let old_base_tools = state.base_tools.clone();

    let (target_prompt, target_tools, new_base_prompt, new_base_prompt_saved, new_base_tools) =
        if mode == "plan" {
            let base_prompt = if state.current == "default" {
                old_prompt.clone()
            } else if state.base_prompt_saved {
                state.base_prompt.clone()
            } else {
                old_prompt.clone()
            };
            let base_tools = if state.current == "default" {
                old_tools.clone()
            } else {
                state.base_tools.clone().unwrap_or(old_tools.clone())
            };
            let tools = base_tools
                .iter()
                .filter(|name| PLAN_READ_ONLY_TOOLS.contains(&name.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            let prompt = format!(
            "{}\n\nYou are in plan mode. Analyze the request and produce an actionable plan. Only read-only tools are enabled; do not modify files or run mutating commands.",
            base_prompt.clone().unwrap_or_default()
        );
            (Some(prompt), tools, base_prompt, true, Some(base_tools))
        } else {
            (
                if state.base_prompt_saved {
                    state.base_prompt.clone()
                } else {
                    old_prompt.clone()
                },
                state.base_tools.clone().unwrap_or(old_tools.clone()),
                None,
                false,
                None,
            )
        };

    // Apply as one transaction. If either native mutation or durable mode
    // recording fails, restore both fields before reporting the old mode.
    let apply = async {
        lane.set_active_tools(target_tools.clone())
            .await
            .map_err(|error| error.to_string())?;
        harness
            .set_system_prompt(target_prompt.clone())
            .await
            .map_err(|error| error.to_string())?;
        if persist {
            harness
                .append_custom_entry("rpi_mode", Some(serde_json::json!({"modeId": mode})))
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok::<(), String>(())
    }
    .await;
    if let Err(error) = apply {
        let _ = lane.set_active_tools(old_tools).await;
        let _ = harness.set_system_prompt(old_prompt).await;
        state.base_prompt_saved = old_base_prompt_saved;
        state.base_tools = old_base_tools;
        return Err(format!("mode change rolled back: {error}"));
    }

    state.base_prompt = new_base_prompt;
    state.base_prompt_saved = new_base_prompt_saved;
    state.base_tools = new_base_tools;
    state.current = mode.to_string();
    Ok(serde_json::json!({
        "currentModeId": state.current,
        "availableModes": mode_descriptors()
    }))
}

async fn rpc_set_compaction_enabled(
    harness: &AgentHarness,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let enabled = msg
        .get("enabled")
        .and_then(serde_json::Value::as_bool)
        .ok_or("set_compaction_enabled requires boolean `enabled`")?;
    harness
        .set_compaction_enabled(enabled)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"enabled": enabled}))
}

async fn rpc_get_stats(
    lane: &Arc<dyn AgentLane>,
    harness: &AgentHarness,
) -> Result<serde_json::Value, String> {
    let stats = lane.get_stats().await.map_err(|error| error.to_string())?;
    let records = harness
        .session()
        .find_records(&RecordQuery {
            lane: Some(lane.name().to_string()),
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .map_err(|error| error.to_string())?;
    let mut operation_counts = std::collections::BTreeMap::<String, u64>::new();
    for record in &records {
        let kind = match record {
            LaneRecord::OperationStarted(started) => {
                let key = started.intent.kind();
                *operation_counts.entry(key.to_string()).or_default() += 1;
                if matches!(
                    &started.intent,
                    rpi_harness::session::types::OperationIntent::Run {
                        resume_data: Some(_),
                        ..
                    }
                ) {
                    *operation_counts.entry("resume".to_string()).or_default() += 1;
                }
                continue;
            }
            LaneRecord::OperationFinished(_) => "operation_finished",
            LaneRecord::RetryPending(_) => "retry_pending",
            LaneRecord::Usage(_) => "usage",
            _ => continue,
        };
        *operation_counts.entry(kind.to_string()).or_default() += 1;
    }
    let count = |name: &str| operation_counts.get(name).copied().unwrap_or_default();
    let model = lane.get_model().await.ok();
    let input = stats.cached_tokens + stats.uncached_tokens;
    Ok(serde_json::json!({
        "messageCount": stats.message_count,
        "operationCount": count("operation_finished"),
        "retryCount": count("retry_pending"),
        "compactionCount": count("compaction"),
        "resumeCount": count("resume"),
        "operationCounts": operation_counts,
        "provider": model.as_ref().map(|value| value.provider.clone()),
        "modelId": model.as_ref().map(|value| value.id.clone()),
        "cachedTokens": stats.cached_tokens,
        "uncachedTokens": stats.uncached_tokens,
        "totalTokens": stats.total_tokens,
        "costTotal": stats.cost_total,
        "cacheHitRatio": if input > 0 { Some(stats.cached_tokens as f64 / input as f64) } else { None::<f64> },
    }))
}

async fn rpc_get_queue(
    lane: &Arc<dyn AgentLane>,
    harness: &AgentHarness,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let include_completed = msg
        .get("includeCompleted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let records = harness
        .session()
        .find_records(&RecordQuery {
            lane: Some(lane.name().to_string()),
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .map_err(|error| error.to_string())?;
    let mut cancelled = std::collections::HashMap::<String, u64>::new();
    for record in &records {
        if let LaneRecord::QueueCancelled(record) = record {
            cancelled.insert(record.entry_id.clone(), record.base.seq);
        }
    }
    let mut items = Vec::new();
    for record in records {
        let LaneRecord::QueueEnqueued(record) = record else {
            continue;
        };
        let Some(entry_id) = record.target.get("id").and_then(|value| value.as_str()) else {
            continue;
        };
        let (status, completed_seq) = if let Some(seq) = cancelled.get(entry_id) {
            ("cancelled", Some(*seq))
        } else if let Some(entry) = harness.get_entry(entry_id).await.ok().flatten() {
            ("consumed", Some(entry.base().seq))
        } else {
            ("queued", None)
        };
        if status != "queued" && !include_completed {
            continue;
        }
        let message = record
            .target
            .get("message")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        items.push(serde_json::json!({
            "entryId": entry_id,
            "queue": record.queue,
            "status": status,
            "text": rpc_message_text(&message),
            "message": message,
            "runId": record.run_id,
            "seq": record.base.seq,
            "completedSeq": completed_seq,
        }));
    }
    Ok(
        serde_json::json!({"items": items, "count": items.len(), "includeCompleted": include_completed}),
    )
}

async fn rpc_queue_message(
    lane: &Arc<dyn AgentLane>,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let text = msg
        .get("message")
        .and_then(serde_json::Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| "queue_message requires a non-empty string `message`".to_string())?;
    let message = rpi_agent::message::AgentMessage::User(UserMessage::new(text, rpc_now_ms()));
    let queue = msg
        .get("deliverAs")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("followUp");
    let result = match queue {
        "steer" => lane.steer(message).await,
        "followUp" | "follow_up" => lane.follow_up(message).await,
        "nextTurn" | "next_run" => lane.next_run(message).await,
        other => return Err(format!("unknown queue kind: {other}")),
    }
    .map_err(|error| error.to_string())?;
    Ok(serde_json::json!({
        "accepted": true,
        "entryId": result.entry_id,
        "queue": queue,
    }))
}

async fn rpc_cancel_queued(
    lane: &Arc<dyn AgentLane>,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let entry_id = msg
        .get("entryId")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "cancel_queued requires `entryId`".to_string())?;
    let result = lane
        .cancel_queued(entry_id)
        .await
        .map_err(|error| error.to_string())?;
    Ok(serde_json::json!({
        "entryId": entry_id,
        "outcome": format!("{:?}", result.outcome).to_lowercase(),
    }))
}

async fn rpc_clear_queue(lane: &Arc<dyn AgentLane>) -> Result<serde_json::Value, String> {
    let result = lane
        .clear_queue()
        .await
        .map_err(|error| error.to_string())?;
    Ok(serde_json::json!({
        "cleared": result.steering.len() + result.follow_up.len(),
        "steering": result.steering,
        "followUp": result.follow_up,
    }))
}

async fn rpc_navigate(
    lane: &Arc<dyn AgentLane>,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let target_id = msg
        .get("targetId")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty());
    if lane
        .snapshot()
        .await
        .map_err(|error| error.to_string())?
        .active
        .is_some()
    {
        return Err("cannot navigate while an operation is active".to_string());
    }
    let result = lane
        .navigate_tree(target_id, false, None, None)
        .await
        .map_err(|error| error.to_string())?;
    let (outcome, leaf_id, error) = match result.outcome {
        rpi_harness::agent_harness::NavigationOutcome::Completed { new_leaf_id, .. } => {
            ("completed", new_leaf_id, None)
        }
        rpi_harness::agent_harness::NavigationOutcome::Declined { leaf_id } => {
            ("declined", leaf_id, None)
        }
        rpi_harness::agent_harness::NavigationOutcome::Aborted { leaf_id } => {
            ("aborted", leaf_id, None)
        }
        rpi_harness::agent_harness::NavigationOutcome::Failed { leaf_id, error } => {
            ("failed", leaf_id, Some(error.message))
        }
    };
    Ok(serde_json::json!({
        "runId": result.run_id,
        "outcome": outcome,
        "leafId": leaf_id,
        "error": error,
    }))
}

/// How full the model's context is, measured the way compaction measures it.
///
/// The branch is projected into model input first (that is what applies a
/// compaction boundary), so the number matches what the next provider call will
/// actually send rather than how many messages the transcript happens to hold.
async fn rpc_context_usage(lane: &Arc<dyn AgentLane>, harness: &AgentHarness) -> serde_json::Value {
    use rpi_harness::compaction::{build_session_context, estimate_context_tokens};
    use rpi_harness::session::types::{BranchBounds, EntryOrder, EntryQuery};

    let entries = harness
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .await
        .unwrap_or_default();
    let estimate = estimate_context_tokens(&build_session_context(&entries).messages);
    let window = lane
        .get_model()
        .await
        .ok()
        .map(|model| model.context_window)
        .unwrap_or(0);
    let settings = harness.compaction_settings();
    let window = window as i64;
    let threshold = window - settings.reserve_tokens;
    let percent = if window > 0 {
        (estimate.tokens as f64) * 100.0 / (window as f64)
    } else {
        0.0
    };
    serde_json::json!({
        "tokens": estimate.tokens,
        "window": window,
        "threshold": threshold,
        "percent": (percent * 10.0).round() / 10.0,
        "reserveTokens": settings.reserve_tokens,
        "keepRecentTokens": settings.keep_recent_tokens,
        "compactionEnabled": settings.enabled,
        // False once a real provider usage report backs the number; until then
        // it is a character-based estimate.
        "estimated": estimate.last_usage_index.is_none(),
        "trailingTokens": estimate.trailing_tokens,
    })
}

/// The window `get_messages` returns, expressed as a slice range plus whether an
/// older page still exists. `before` is the index of the `beforeId` entry.
fn page_window(
    total: usize,
    limit: usize,
    before: Option<usize>,
) -> (std::ops::Range<usize>, bool) {
    match before {
        Some(index) => {
            let start = index.saturating_sub(limit);
            (start..index, start > 0)
        }
        None => {
            let skip = total.saturating_sub(limit);
            (skip..total, skip > 0)
        }
    }
}

/// Read-only snapshot of the current branch, oldest first. Messages are
/// projected with the same `AgentMessage` encoding the event stream uses, so a
/// client can render a transcript without understanding session entries.
async fn rpc_get_messages(
    lane: &Arc<dyn AgentLane>,
    harness: &AgentHarness,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    use rpi_harness::session::types::{BranchBounds, Entry, EntryOrder, EntryQuery};

    const DEFAULT_LIMIT: usize = 400;
    let limit = msg
        .get("limit")
        .and_then(|value| value.as_u64())
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_LIMIT)
        .max(1);
    // `beforeId` pages backwards: the returned window ends immediately before
    // the entry with that id, so a client can walk a long branch one page at a
    // time instead of always re-reading the tail.
    let before_id = msg
        .get("beforeId")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let entries = harness
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .await
        .map_err(|error| error.to_string())?;
    let mut items: Vec<(String, serde_json::Value)> = Vec::new();
    for entry in entries {
        match entry {
            Entry::Message(entry) => {
                let mut value = serde_json::to_value(&entry.message).map_err(|e| e.to_string())?;
                // The session entry id, not the message's own id: it is what
                // `beforeId` keys on, and what makes rows stable across reloads.
                value["entryId"] = serde_json::json!(entry.base.id);
                items.push((entry.base.id, value));
            }
            // Compaction rewrites what the model sees, not the branch, so the
            // transcript keeps its messages. Marking the boundary is what lets a
            // client explain why the context suddenly shrank.
            Entry::Compaction(compaction) => items.push((
                compaction.base.id.clone(),
                serde_json::json!({
                    "role": "compaction",
                    "entryId": compaction.base.id,
                    "tokensBefore": compaction.tokens_before,
                    "retainedMessages": compaction.retained_tail.len(),
                    "timestamp": compaction.base.timestamp,
                }),
            )),
            _ => {}
        }
    }
    let total = items.len();
    let before_index = match &before_id {
        Some(before) => match items.iter().position(|(id, _)| id == before) {
            Some(index) => Some(index),
            None => return Err(format!("beforeId not found: {before}")),
        },
        None => None,
    };
    let (range, has_more) = page_window(total, limit, before_index);
    let window = items[range].to_vec();
    let first_id = window.first().map(|(id, _)| id.clone());
    let last_id = window.last().map(|(id, _)| id.clone());
    let messages = window
        .iter()
        .map(|(_, value)| value.clone())
        .collect::<Vec<_>>();
    // Read-only branch position: the leaf the snapshot ends on, and whether
    // recovery left an interrupted run that a client may want to surface.
    let leaf_id = lane.get_leaf_id().await.ok().flatten();
    let pending_resume = lane.has_pending_resume().await;
    Ok(serde_json::json!({
        "messages": messages,
        "total": total,
        "truncated": has_more,
        "limit": limit,
        "firstId": first_id,
        "lastId": last_id,
        "hasMore": has_more,
        "beforeId": before_id,
        "leafId": leaf_id,
        "pendingResume": pending_resume,
    }))
}

fn rpc_auth_provider_ids(model_catalog: &[rpi_ai::Model]) -> std::collections::BTreeSet<String> {
    let mut providers =
        std::collections::BTreeSet::from(["anthropic".to_string(), "openai".to_string()]);
    providers.extend(model_catalog.iter().map(|model| model.provider.clone()));
    if let Ok(models) = crate::config::load_models_config() {
        providers.extend(models.providers.keys().cloned());
    }
    providers
}

fn rpc_get_auth_status(
    model_catalog: &[rpi_ai::Model],
    oauth_snapshot: Option<&Arc<rpi_extensions::RegistrySnapshot>>,
) -> serde_json::Value {
    let store = crate::config::read_auth().unwrap_or_default();
    let models = crate::config::load_models_config().unwrap_or_default();
    let manifests = oauth_snapshot.map_or_else(Vec::new, |snapshot| {
        snapshot
            .providers()
            .iter()
            .filter_map(|provider| snapshot.provider_manifest(&provider.provider_id).cloned())
            .collect()
    });
    let auth_states = crate::credentials::statuses(&models, &store, &manifests);
    let providers = rpc_auth_provider_ids(model_catalog)
        .into_iter()
        .map(|provider| {
            let credential = store.get(&provider);
            let state = auth_states.get(&provider).cloned().unwrap_or(
                rpi_runtime::credentials::AuthState {
                    status: rpi_runtime::credentials::AuthStatus::Missing,
                    source: crate::credentials::CredentialSource::Missing,
                },
            );
            let oauth = matches!(credential, Some(crate::config::Credential::Oauth { .. }));
            let oauth_available =
                oauth_snapshot.is_some_and(|snapshot| snapshot.oauth_provider(&provider).is_some());
            serde_json::json!({
                "provider": provider,
                "ready": state.is_ready(),
                "source": state.source.as_json_str(),
                "status": state.status,
                "oauth": oauth,
                "oauthAvailable": oauth_available,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({"providers": providers})
}

fn rpc_oauth(
    snapshot: &Option<Arc<rpi_extensions::RegistrySnapshot>>,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let snapshot = snapshot
        .as_ref()
        .ok_or_else(|| "OAuth is unavailable without an extension provider".to_string())?;
    let provider = msg
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "oauth requires a provider".to_string())?;
    let action = msg
        .get("action")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| matches!(*value, "begin" | "exchange" | "refresh" | "revoke"))
        .ok_or_else(|| "oauth requires a valid action".to_string())?;
    let input = msg
        .get("input")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let raw = rpi_extensions::request_oauth(snapshot, provider, action, input)
        .map_err(|_| "OAuth provider request failed".to_string())?;
    let response = rpi_extensions::OAuthActionResponse::parse(raw)
        .map_err(|_| "OAuth provider returned an invalid response".to_string())?;
    match response.kind {
        rpi_extensions::OAuthResponseKind::Credential => {
            let credential = response
                .credential()
                .map_err(|_| "OAuth provider returned an invalid credential".to_string())?
                .ok_or_else(|| "OAuth provider returned no credential".to_string())?;
            crate::config::upsert_oauth_token(
                provider,
                credential.access,
                credential.refresh,
                credential.expires_at,
            )
            .map_err(|_| "OAuth credentials could not be saved".to_string())?;
            Ok(serde_json::json!({
                "provider": provider,
                "status": "authenticated"
            }))
        }
        rpi_extensions::OAuthResponseKind::Interaction => Ok(serde_json::json!({
            "provider": provider,
            "status": "interaction",
            "display": oauth_display(&response.payload)
        })),
        rpi_extensions::OAuthResponseKind::Success => Ok(serde_json::json!({
            "provider": provider,
            "status": "success"
        })),
        rpi_extensions::OAuthResponseKind::Error => {
            Err("OAuth provider rejected the request".to_string())
        }
    }
}

/// Keep provider-defined OAuth UI data deliberately narrow. In particular,
/// token-like fields never cross the RPC boundary, even if a plugin includes
/// them in an interaction payload by mistake.
fn oauth_display(payload: &serde_json::Value) -> serde_json::Value {
    let display = payload.get("display").unwrap_or(payload);
    let Some(object) = display.as_object() else {
        return serde_json::json!({});
    };
    let mut safe = serde_json::Map::new();
    for key in [
        "title",
        "url",
        "instructions",
        "user_code",
        "verification_uri",
    ] {
        if let Some(value) = object.get(key) {
            safe.insert(key.to_string(), value.clone());
        }
    }
    serde_json::Value::Object(safe)
}

async fn rpc_set_api_key(msg: &serde_json::Value) -> Result<serde_json::Value, String> {
    let provider = msg
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "set_api_key requires a provider".to_string())?;
    let api_key = msg
        .get("apiKey")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "set_api_key requires a non-empty apiKey".to_string())?;
    crate::config::upsert_credential(
        provider,
        crate::config::Credential::ApiKey {
            key: Some(api_key.to_string()),
            env: None,
        },
    )
    .map_err(|error| error.to_string())?;
    Ok(serde_json::json!({
        "provider": provider,
        "ready": true,
        "source": "auth.json",
        "requiresRestart": true,
    }))
}

fn rpc_get_available_models(catalog: &[rpi_ai::Model]) -> serde_json::Value {
    let models = catalog
        .iter()
        .map(crate::rpc::protocol::model_descriptor)
        .collect::<Vec<_>>();
    serde_json::json!({"models": models})
}

async fn rpc_get_available_thinking_levels(lane: &Arc<dyn AgentLane>) -> serde_json::Value {
    let levels = lane
        .get_model()
        .await
        .map(|model| model.supported_thinking_levels())
        .unwrap_or_default();
    serde_json::json!({"levels": levels})
}

async fn rpc_configure_external_tools(
    lane: &Arc<dyn AgentLane>,
    harness: &AgentHarness,
    mode_state: &Arc<tokio::sync::Mutex<RpcModeState>>,
    backend: &Arc<crate::rpc::external_tools::ExternalTools>,
    old_names: &mut Vec<String>,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    use rpi_tools::tools::external::{ExternalToolDefinition, validate_external_tool_definitions, create_external_tool};
    use rpi_harness::types::HarnessTool;
    if lane.snapshot().await.map_err(|e| e.to_string())?.active.is_some() {
        return Err("cannot configure external tools while a run is active".into());
    }
    let definitions: Vec<ExternalToolDefinition> = serde_json::from_value(msg["tools"].clone()).map_err(|e| e.to_string())?;
    let mut registry = harness.get_tools().await.map_err(|e| e.to_string())?;
    registry.retain(|t| !old_names.contains(&t.tool.schema().name));
    // Reserve native names even when disabled at launch; permission tools
    // cannot be impersonated by an external producer.
    let mut reserved = ["read", "write", "edit", "bash", "powershell", "docs", "grep", "find", "ls", "image", "permissions", "permission", "ask_user", "plan_mode_start", "plan_mode_complete"]
        .iter().map(|s| s.to_string()).collect::<Vec<_>>();
    reserved.extend(registry.iter().map(|t| t.tool.schema().name.clone()));
    validate_external_tool_definitions(&definitions, &reserved).map_err(|e| e.to_string())?;
    let names = definitions.iter().map(|d| d.name.clone()).collect::<Vec<_>>();
    let mut state = mode_state.lock().await;
    let mut active = lane.get_active_tools().await.map_err(|e| e.to_string())?;
    active.retain(|n| !old_names.contains(n));
    if state.current != "plan" { active.extend(names.clone()); }
    registry.extend(definitions.into_iter().map(|d| HarnessTool::new(create_external_tool(d, backend.clone()))));
    harness.set_tools(registry, Some(active.clone())).await.map_err(|e| e.to_string())?;
    // Configuration during Plan must not resurrect old or newly configured
    // external tools on return to default. Only preserve preexisting native tools.
    if let Some(base) = state.base_tools.as_mut() { base.retain(|n| !old_names.contains(n)); }
    *old_names = names.clone();
    Ok(serde_json::json!({"tools":names,"activeTools":active}))
}

async fn rpc_get_available_tools(
    lane: &Arc<dyn AgentLane>,
    harness: &AgentHarness,
) -> serde_json::Value {
    let active = lane.get_active_tools().await.unwrap_or_default();
    let tools = harness
        .get_tools()
        .await
        .map(|tools| {
            tools
                .iter()
                .map(|tool| tool.tool.schema().name.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|_| active.clone());
    serde_json::json!({"tools": tools, "activeTools": active})
}

async fn rpc_set_model(
    lane: &Arc<dyn AgentLane>,
    catalog: &[rpi_ai::Model],
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let id = msg
        .get("modelId")
        .or_else(|| msg.get("model"))
        .and_then(|v| v.as_str())
        .ok_or("set_model requires `modelId`")?;
    let provider = msg.get("provider").and_then(|v| v.as_str());
    let model = catalog
        .iter()
        .find(|m| {
            m.id.eq_ignore_ascii_case(id)
                && provider.map_or(true, |provider| m.provider.eq_ignore_ascii_case(provider))
        })
        .cloned()
        .ok_or_else(|| format!("unknown model: {id}"))?;
    lane.set_model(model).await.map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"modelId": id, "provider": provider}))
}

async fn rpc_set_thinking_level(
    lane: &Arc<dyn AgentLane>,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let value = msg
        .get("level")
        .or_else(|| msg.get("thinkingLevel"))
        .ok_or("set_thinking_level requires `level`")?;
    let level: rpi_ai::types::ThinkingLevel =
        serde_json::from_value(value.clone()).map_err(|_| {
            "invalid thinking level (off|minimal|low|medium|high|xhigh|max)".to_string()
        })?;
    lane.set_thinking_level(level)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"thinkingLevel": level}))
}

async fn rpc_set_active_tools(
    lane: &Arc<dyn AgentLane>,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let tools = msg
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .ok_or("set_active_tools requires a `tools` array")?;
    lane.set_active_tools(tools.clone())
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"activeTools": tools}))
}

/// Stable JSON projection for the fine-grained agent lifecycle stream.
///
/// Delegates to the shared [`crate::remote::protocol::RemoteEvent`] so the
/// server and the `--connect` client agree on the wire shape by construction.
fn agent_event_json(event: &AgentEvent) -> serde_json::Value {
    crate::rpc::events::project(event)
}

/// Emit a single harness event as a JSON line on stdout. Mirrors the TS
/// `toJsonEvent` projection (here a lossy but stable shape: `type` + the event
/// payload's key fields).
fn emit_json_event(event: &HarnessEvent) {
    let line = match event {
        HarnessEvent::CompactionProgress(e) => {
            serde_json::json!({ "type": "compaction_progress", "lane": e.lane, "manual": e.manual, "active": e.active })
        }
        HarnessEvent::RunStart(e) => serde_json::json!({
            "type": "run_start",
            "lane": e.lane,
            "runId": e.run_id,
        }),
        HarnessEvent::RunEnd(e) => serde_json::json!({
            "type": "run_end",
            "lane": e.lane,
            "runId": e.run_id,
            "outcome": run_end_outcome_str(e.outcome),
            "leafId": e.leaf_id,
        }),
    };
    println!("{line}");
}

fn run_end_outcome_str(o: RunEndOutcome) -> &'static str {
    match o {
        RunEndOutcome::Completed => "completed",
        RunEndOutcome::Aborted => "aborted",
        RunEndOutcome::Failed => "failed",
    }
}

/// `interactive` mode: uses TUI if terminal supports it, falls back to minimal REPL.
///
/// `event_rx` carries the live `AgentEvent` stream (drained by the TUI to
/// render streaming responses). The REPL fallback ignores it.
///
/// `model_catalog` is the resolved provider's full model list, passed through
/// so the TUI's `/model` selector can display available models (read-only —
/// v1 does not switch models mid-session; see the initial port notes (retired)).
pub async fn interactive(
    harness: &AgentHarness,
    event_rx: Option<tokio::sync::broadcast::Receiver<rpi_agent::AgentEvent>>,
    args: &Args,
    model_catalog: Vec<rpi_ai::Model>,
    initial: Option<String>,
    extra_messages: &[String],
    initial_images: Vec<ImageContent>,
    theme: Option<&str>,
    no_themes: bool,
    reload_context: &crate::session::ReloadContext,
) -> i32 {
    // ---- Session lifecycle (P1): BeforeTuiStart ----
    // `BeforeTuiStart`: extensions may prepare (connect a server, load a
    // resource) before the interactive UI initializes. Runs with a per-event
    // timeout; a handler returning `EVENT_HANDLER_ABORT` **vetoes** startup —
    // we print the reason and exit without entering the TUI. No-op when no
    // extension subscribes.
    if let Some(reason) = crate::session::dispatch_session_event_async(
        reload_context,
        rpi_plugin_sdk::EventTag::BeforeTuiStart,
    )
    .await
    {
        eprintln!("[rpi] startup vetoed by extension: {reason}");
        return crate::app::EXIT_VETOED;
    }

    // Check if TUI is supported
    let force_tui = std::env::var("RPI_FORCE_TUI")
        .map(|v| v == "1")
        .unwrap_or(false);
    let code = if force_tui || crate::interactive_tui::is_tui_supported() {
        // Use TUI-based interactive mode
        crate::interactive_tui::interactive_tui(
            harness,
            event_rx,
            args,
            model_catalog,
            initial,
            extra_messages,
            initial_images,
            theme,
            no_themes,
            reload_context,
        )
        .await
    } else {
        // Fall back to simple REPL. No transcript container to hold notes, so
        // the startup notices print as plain lines instead.
        for notice in &reload_context.startup_notices {
            eprintln!("note: {notice}");
        }
        interactive_repl(harness, args, initial, extra_messages, initial_images).await
    };

    // `SessionShutdown`: the interactive session closed (TUI/REPL returned) —
    // extensions release resources, disconnect, persist state. Fired after the
    // UI returns so it always runs (even on early-return paths inside the TUI).
    // A veto here is **advisory** (the session is already closing) — warn and
    // continue.
    if let Some(reason) = crate::session::dispatch_session_event_async(
        reload_context,
        rpi_plugin_sdk::EventTag::SessionShutdown,
    )
    .await
    {
        tracing::warn!(
            "[rpi] extension vetoed SessionShutdown (ignored, session closing): {reason}"
        );
    }

    code
}

/// Simple REPL-based interactive mode (fallback for non-TTY environments).
pub async fn interactive_repl(
    harness: &AgentHarness,
    #[allow(unused_variables)] args: &Args,
    initial: Option<String>,
    extra_messages: &[String],
    initial_images: Vec<ImageContent>,
) -> i32 {
    // Debug: confirm we entered REPL mode
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let stdin = std::io::stdin();
    let is_tty = stdin.is_terminal();

    if is_tty {
        println!(
            "rpi interactive (v1 minimal REPL). Type /exit to quit, /abort to cancel a run.\n"
        );
    }

    // Run the initial prompt + extra messages first (same as print mode).
    let mut prompts: Vec<String> = Vec::new();
    if let Some(init) = initial {
        prompts.push(init);
    }
    for m in extra_messages {
        prompts.push(m.clone());
    }
    let mut images = initial_images;
    for prompt in prompts {
        if let Err(code) = run_one(&lane, &prompt, std::mem::take(&mut images)).await {
            return code;
        }
    }

    // Then read lines from stdin until EOF / `/exit`.
    let mut line = String::new();
    loop {
        if is_tty {
            print!("> ");
            let _ = std::io::stdout().flush();
        }
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "/exit" || trimmed == "/quit" {
            break;
        }
        if trimmed == "/abort" {
            let _ = lane.abort().await;
            eprintln!("(aborted)");
            continue;
        }
        if let Err(code) = run_one(&lane, trimmed, Vec::new()).await {
            return code;
        }
    }
    0
}

/// Run a single prompt in interactive mode, printing the assistant reply (or
/// the error). Returns `Ok(())` on success/soft-failure, `Err(exit_code)` on a
/// hard rejection.
async fn run_one(
    lane: &Arc<dyn AgentLane>,
    prompt: &str,
    images: Vec<ImageContent>,
) -> Result<(), i32> {
    match lane.prompt_text(prompt, images).await {
        Ok(result) => {
            match &result.outcome {
                HarnessRunOutcome::Completed { final_message, .. }
                | HarnessRunOutcome::Aborted { final_message, .. } => {
                    let text = assistant_text(final_message);
                    if !text.is_empty() {
                        println!("{text}");
                    }
                }
                HarnessRunOutcome::Failed {
                    error,
                    final_message,
                    ..
                } => {
                    if let Some(m) = final_message {
                        if let Some(em) = &m.error_message {
                            eprintln!("error: {em}");
                        }
                    }
                    eprintln!("run failed: {error:?}");
                }
                HarnessRunOutcome::Suspended { .. } => {
                    eprintln!("run suspended (deferred) — resume not supported in v1");
                }
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("prompt rejected: {e}");
            Err(1)
        }
    }
}

/// The print-mode (headless) message for a refused session write.
///
/// Print mode has no transcript to fall back on and no interactive `/export`
/// either — the process is about to exit. Naming a recovery the user cannot run
/// from here would be worse than saying what was lost, so this deliberately does
/// not recommend exporting (unlike the TUI, where `/export` does work).
fn session_storage_notice(message: &str) -> String {
    format!(
        "conversation not saved: {message} — the session log is missing this turn \
         and print mode exits without writing it"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_agent::events::AgentEvent;
    use rpi_agent::types::AgentToolResult;
    use rpi_ai::types::{
        AssistantMessage, Content, StopReason, TextContent, TextContentType, Usage,
    };
    use rpi_harness::session::types::OperationError;

    #[test]
    fn session_branch_query_requires_canonical_arguments() {
        let query = branch_entry_query(&serde_json::json!({
            "customTypes": ["alpha", "beta", "alpha"], "afterSeq": 42,
        }))
        .unwrap();
        assert_eq!(query.custom_types, ["alpha", "beta"]);
        assert_eq!(query.order, EntryOrder::OldestFirst);
        assert_eq!(query.after_seq, Some(42));
        assert!(query.entry_queries().iter().all(|q| q.limit.is_none()));
        for invalid in [
            serde_json::json!({}),
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!({"customTypes": []}),
            serde_json::json!({"customTypes": "alpha"}),
            serde_json::json!({"customTypes": [""]}),
            serde_json::json!({"customTypes": [" \t "]}),
            serde_json::json!({"customTypes": ["alpha", 1]}),
            serde_json::json!({"customTypes": ["alpha"], "limit": 0}),
            serde_json::json!({"customTypes": ["alpha"], "limit": 1.5}),
            serde_json::json!({"customTypes": ["alpha"], "order": "sideways"}),
            serde_json::json!({"customTypes": ["alpha"], "order": 1}),
            serde_json::json!({"customTypes": ["alpha"], "afterSeq": -1}),
        ] {
            assert!(branch_entry_query(&invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn session_branch_multi_type_limit_is_pushed_to_each_storage_query() {
        for order in ["oldestFirst", "newestFirst"] {
            let request = branch_entry_query(&serde_json::json!({
                "customTypes": ["alpha", "beta", "alpha"],
                "limit": 2, "order": order, "afterSeq": 42,
            }))
            .unwrap();
            let queries = request.entry_queries();
            assert_eq!(queries.len(), 2);
            for query in queries {
                assert_eq!(query.limit, Some(2));
                assert_eq!(query.order, Some(request.order));
                assert_eq!(query.cursor, Some(EntryCursor { after_seq: 42 }));
            }
        }
    }

    #[tokio::test]
    async fn session_branch_exact_type_identities_round_trip_rpc_and_actions() {
        use rpi_extensions::RuntimeActionHost;
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let (host, cell) = crate::extensions_actions::HarnessActionHost::new_empty(
            vec![],
            std::env::temp_dir(),
            tokio::runtime::Handle::current(),
            Default::default(),
            rpi_extensions::ToolCallContext::default(),
        );
        crate::extensions_actions::HarnessActionHost::set_harness(&cell, Arc::new(harness.clone()));
        let names = ["alpha", " alpha ", "beta", " beta "];
        for (index, name) in names.iter().enumerate() {
            let args = serde_json::json!({"customType": name, "data": {"n": index}});
            if index % 2 == 0 {
                host.append_entry(args).await.unwrap();
            } else {
                rpc_append_entry(&harness, &args).await.unwrap();
            }
        }
        let request = branch_entry_query(&serde_json::json!({
            "customTypes": ["alpha", " alpha ", "beta", " beta ", "alpha"],
        }))
        .unwrap();
        assert_eq!(request.custom_types, names);
        let mut input = String::new();
        let mut expected = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let args = serde_json::json!({"customTypes": [name]});
            let result = host.get_session_branch(args.clone()).await.unwrap();
            assert_eq!(result["count"], 1);
            assert_eq!(result["entries"][0]["customType"], *name);
            assert_eq!(result["entries"][0]["data"]["n"], index);
            let mut envelope = args;
            envelope["type"] = serde_json::json!("get_session_branch");
            envelope["id"] = serde_json::json!(index);
            input.push_str(&format!("{envelope}\n"));
            expected.push(result);
        }
        let output = RpcOutput::default();
        assert_eq!(
            rpc_with_io(
                &harness,
                None,
                vec![],
                rpi_extensions::UiDialogMailbox::new(),
                None,
                crate::args::RpcMessageUpdates::default(),
                tokio::io::BufReader::new(input.as_bytes()),
                output.clone(),
            )
            .await,
            0
        );
        let messages = output.messages();
        for (index, expected) in expected.into_iter().enumerate() {
            let response = messages
                .iter()
                .find(|message| message["id"] == index)
                .unwrap();
            assert_eq!(response["data"], expected);
        }
    }

    #[tokio::test]
    async fn session_branch_filters_merge_and_respect_fork_bounds() {
        use rpi_harness::session::types::{ForkOptions, ForkPosition, SessionRepo};
        use rpi_harness::session::{CounterIdGenerator, FakeClock, InMemorySessionRepo, Session};
        let repo = InMemorySessionRepo::new(
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        );
        let storage = repo.create(&Default::default()).await.unwrap();
        let session = Session::new(Arc::new(storage), None);
        let harness = AgentHarness::create(rpi_harness::types::AgentHarnessOptions {
            session,
            ..Default::default()
        })
        .await
        .unwrap();
        let session = harness.session();
        let empty = rpc_get_session_branch(
            &harness,
            &serde_json::json!({"customTypes":["alpha","beta"]}),
        )
        .await
        .unwrap();
        assert_eq!(empty["entries"], serde_json::json!([]));
        assert_eq!(empty["filtered"], true);
        let mut ids = Vec::new();
        let mut fork_target = None;
        for (index, kind) in ["alpha", "noise", "beta", "alpha", "noise", "beta"]
            .into_iter()
            .enumerate()
        {
            if index == 2 {
                fork_target = Some(
                    session
                        .append_message(rpi_agent::AgentMessage::User(UserMessage::new(
                            "fork target",
                            0,
                        )))
                        .await
                        .unwrap(),
                );
            }
            ids.push(session.append_custom_entry(kind, None).await.unwrap());
        }
        // Neither ordinary messages nor durable lane records are custom entries.
        session
            .append_message(rpi_agent::AgentMessage::User(UserMessage::new("noise", 0)))
            .await
            .unwrap();
        session
            .append_record(
                serde_json::from_value(serde_json::json!({
                    "type":"usage", "id":"usage-noise", "seq":0, "timestamp":0,
                    "lane":"main", "usage":Usage::zero(), "cause":"adjustment"
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let run = |args| {
            let harness = &harness;
            async move { rpc_get_session_branch(harness, &args).await.unwrap() }
        };
        let all = run(serde_json::json!({"customTypes":["alpha","beta","alpha"]})).await;
        let entry_ids = |value: &serde_json::Value| -> Vec<String> {
            value["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["id"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(
            entry_ids(&all),
            vec![
                ids[0].clone(),
                ids[2].clone(),
                ids[3].clone(),
                ids[5].clone()
            ]
        );
        assert_eq!(all["count"], 4);
        assert_eq!(all["filtered"], true);
        let cursor = all["entries"][1]["seq"].as_u64().unwrap();
        for (order, expected) in [
            (
                "oldestFirst",
                vec![ids[0].clone(), ids[2].clone(), ids[3].clone()],
            ),
            (
                "newestFirst",
                vec![ids[5].clone(), ids[3].clone(), ids[2].clone()],
            ),
        ] {
            let result =
                run(serde_json::json!({"customTypes":["alpha","beta"], "order":order, "limit":3}))
                    .await;
            assert_eq!(entry_ids(&result), expected);
            let result = run(serde_json::json!({"customTypes":["alpha","beta"], "order":order, "afterSeq":cursor, "limit":1})).await;
            assert_eq!(
                entry_ids(&result),
                vec![if order == "oldestFirst" {
                    ids[3].clone()
                } else {
                    ids[0].clone()
                }]
            );
        }
        let single = run(serde_json::json!({"customTypes":["beta"], "afterSeq":all["entries"][3]["seq"], "order":"newestFirst", "limit":1})).await;
        assert_eq!(entry_ids(&single), vec![ids[2].clone()]);

        // Rewind and append: old descendants and other lanes must not leak in.
        session.move_lane("main", Some(&ids[2])).await.unwrap();
        let branch_tip = session.append_custom_entry("alpha", None).await.unwrap();
        session.create_lane("other", Some(&ids[5])).await.unwrap();
        let branch = run(serde_json::json!({"customTypes":["alpha","beta"]})).await;
        assert_eq!(
            entry_ids(&branch),
            vec![ids[0].clone(), ids[2].clone(), branch_tip]
        );
        let empty =
            run(serde_json::json!({"customTypes":["alpha","beta"], "afterSeq":u64::MAX - 1})).await;
        assert!(entry_ids(&empty).is_empty());

        // Fork before the message preceding beta; custom entries remain covered.
        let fork = repo
            .fork(
                &session.storage().metadata(),
                &Default::default(),
                &ForkOptions::Branch {
                    entry_id: fork_target,
                    position: Some(ForkPosition::Before),
                },
            )
            .await
            .unwrap();
        harness
            .set_session(Session::new(Arc::new(fork), None))
            .await
            .unwrap();
        let forked = run(
            serde_json::json!({"customTypes":["alpha","beta"], "order":"newestFirst", "limit":2}),
        )
        .await;
        assert_eq!(entry_ids(&forked), vec![ids[0].clone()]);
    }

    #[tokio::test]
    async fn session_branch_rpc_and_action_share_strict_query_behavior() {
        use rpi_extensions::RuntimeActionHost;
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let (host, cell) = crate::extensions_actions::HarnessActionHost::new_empty(
            vec![],
            std::env::temp_dir(),
            tokio::runtime::Handle::current(),
            Default::default(),
            rpi_extensions::ToolCallContext::default(),
        );
        crate::extensions_actions::HarnessActionHost::set_harness(&cell, Arc::new(harness.clone()));
        // AppendEntry deliberately keeps its distinct singular write protocol.
        host.append_entry(serde_json::json!({"customType":"alpha", "data":{"n":1}}))
            .await
            .unwrap();
        rpc_append_entry(
            &harness,
            &serde_json::json!({"customType":"beta", "data":{"n":2}}),
        )
        .await
        .unwrap();
        let valid =
            serde_json::json!({"customTypes":["alpha","beta"], "limit":1, "order":"newestFirst"});
        let expected = host.get_session_branch(valid.clone()).await.unwrap();
        assert_eq!(expected["entries"][0]["customType"], "beta");
        let mut requests = vec![serde_json::json!({})];
        for key in [
            "customType",
            "custom_type",
            "custom_types",
            "after_seq",
            "unknown",
        ] {
            let value = if key == "after_seq" {
                serde_json::json!(0)
            } else {
                serde_json::json!("alpha")
            };
            requests.push(serde_json::json!({key:value.clone()}));
            let mut mixed = valid.clone();
            mixed[key] = value;
            requests.push(mixed);
        }
        let mut input = String::new();
        let mut envelope = valid;
        envelope["type"] = serde_json::json!("get_session_branch");
        envelope["id"] = serde_json::json!("modern");
        input.push_str(&format!("{envelope}\n"));
        for (index, args) in requests.iter().enumerate() {
            let error = host.get_session_branch(args.clone()).await.unwrap_err();
            assert!(error.contains("get_session_branch"), "{error}");
            let mut envelope = args.clone();
            envelope["type"] = serde_json::json!("get_session_branch");
            envelope["id"] = serde_json::json!(index);
            assert_eq!(
                rpc_get_session_branch(&harness, &envelope)
                    .await
                    .unwrap_err(),
                error
            );
            input.push_str(&format!("{envelope}\n"));
        }
        let output = RpcOutput::default();
        assert_eq!(
            rpc_with_io(
                &harness,
                None,
                vec![],
                rpi_extensions::UiDialogMailbox::new(),
                None,
                crate::args::RpcMessageUpdates::default(),
                tokio::io::BufReader::new(input.as_bytes()),
                output.clone(),
            )
            .await,
            0
        );
        let messages = output.messages();
        assert_eq!(
            messages.iter().find(|m| m["id"] == "modern").unwrap()["data"],
            expected
        );
        for index in 0..requests.len() {
            let response = messages.iter().find(|m| m["id"] == index).unwrap();
            assert_eq!(response["success"], false, "{response}");
        }
    }

    #[tokio::test]
    async fn rpc_commands_list_and_execute_registered_handler() {
        extern "C" fn echo(
            input: rpi_plugin_sdk::StbStringRef,
            out: *mut rpi_plugin_sdk::StbString,
            _: *mut std::ffi::c_void,
        ) -> i32 {
            // SAFETY: the RPC invoker provides live input and output pointers.
            unsafe {
                *out = rpi_plugin_sdk::StbString::from_string(input.as_str().to_owned());
            }
            0
        }
        let mut registry = rpi_extensions::ExtensionRegistry::new();
        registry.register_command(
            "echo".into(),
            "Echo arguments".into(),
            echo,
            std::ptr::null_mut(),
        );
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let output = RpcOutput::default();
        let input = concat!(
            "{\"id\":1,\"type\":\"get_commands\"}\n",
            "{\"id\":2,\"type\":\"execute_command\",\"command\":\"/echo\",\"args\":\"hello world\"}\n",
            "{\"id\":3,\"type\":\"execute_command\",\"command\":\"missing\"}\n",
            "{\"id\":4,\"type\":\"execute_command\",\"command\":\"echo\",\"args\":42}\n"
        );
        assert_eq!(
            rpc_with_io(
                &harness,
                None,
                vec![],
                rpi_extensions::UiDialogMailbox::new(),
                Some(Arc::new(registry.snapshot())),
                crate::args::RpcMessageUpdates::default(),
                tokio::io::BufReader::new(input.as_bytes()),
                output.clone()
            )
            .await,
            0
        );
        let messages = output.messages();
        let response = |id| messages.iter().find(|m| m["id"] == id).unwrap();
        assert_eq!(response(1)["data"]["commands"][0]["name"], "echo");
        assert_eq!(
            response(2)["data"],
            serde_json::json!({"command":"/echo", "args":"hello world"})
        );
        assert_eq!(response(3)["success"], false);
        assert_eq!(response(4)["success"], false);
        assert_eq!(rpc_get_commands(None), serde_json::json!({"commands":[]}));
    }

    #[test]
    fn rpc_auth_status_shares_header_readiness_and_expired_token_state() {
        let _guard = crate::config::test_support::env_lock().lock().unwrap();
        struct RestoreConfig(Option<std::ffi::OsString>);
        impl Drop for RestoreConfig {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                    None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
                }
            }
        }
        let _restore = RestoreConfig(std::env::var_os(crate::config::CONFIG_DIR_ENV));
        let directory = tempfile::tempdir().unwrap();
        std::env::set_var(crate::config::CONFIG_DIR_ENV, directory.path());
        std::fs::write(directory.path().join("models.json"), serde_json::json!({"providers": {
            "header-gateway": {"api":"openai-completions", "headers":{"Authorization":"Bearer header-secret"}, "models":[{"id":"model"}]},
            "expired-account": {"api":"openai-completions", "models":[]}
        }}).to_string()).unwrap();
        std::fs::write(directory.path().join("auth.json"), serde_json::json!({
            "expired-account": {"type":"oauth", "access":"expired-secret", "refresh":"refresh-secret", "expires":1}
        }).to_string()).unwrap();
        let status = rpc_get_auth_status(&[], None);
        let providers = status["providers"].as_array().unwrap();
        let header = providers
            .iter()
            .find(|row| row["provider"] == "header-gateway")
            .unwrap();
        assert_eq!(header["ready"], true);
        assert_eq!(header["source"], "headers");
        assert_eq!(header["status"], "ready");
        let expired = providers
            .iter()
            .find(|row| row["provider"] == "expired-account")
            .unwrap();
        assert_eq!(expired["ready"], false);
        assert_eq!(expired["status"], "expired");
        assert!(!status.to_string().contains("secret"));
    }

    #[test]
    fn oauth_display_drops_token_like_fields() {
        let display = oauth_display(&serde_json::json!({
            "display": {
                "title": "Sign in",
                "url": "https://example.test/authorize",
                "instructions": ["Paste the code"],
                "access_token": "secret-access",
                "refresh_token": "secret-refresh",
            }
        }));
        assert_eq!(display["title"], "Sign in");
        assert!(display.get("access_token").is_none());
        assert!(display.get("refresh_token").is_none());
    }

    #[test]
    fn rpc_message_text_joins_content_blocks_with_real_newlines() {
        let text = rpc_message_text(&serde_json::json!({
            "content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"}
            ]
        }));
        assert_eq!(text, "first\nsecond");
        assert!(!text.contains("\\n"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rpc_child_callback_reaches_writer_in_fifo_order() {
        let (parent, _) = rpc_test_harness("child live output", 1000.0).await;
        let output = RpcOutput::default();
        let (writer, writer_task, _) = start_rpc_writer(output.clone());
        let (sink, forwarder) = start_child_rpc_forwarder(writer.clone(), Arc::new(Mutex::new(None)));
        let children = crate::child_agents::ChildAgents::default();
        children.set_event_sink(Some(sink));
        let started = children.handle(&parent, 7, serde_json::json!({
            "op":"start", "task":"report", "tools":[]
        })).await.unwrap();
        let id = started["childId"].as_str().unwrap();
        output.wait_for(|m| m["type"] == "child_end").await;
        children.set_event_sink(None);
        forwarder.await.unwrap();
        drop(writer);
        writer_task.await.unwrap().unwrap();
        let messages = output.messages();
        assert_eq!(messages.first().unwrap()["type"], "child_start");
        assert_eq!(messages.last().unwrap()["type"], "child_end");
        assert!(messages.iter().any(|m| m["type"] == "child_event"));
        assert_eq!(messages.iter().filter(|m| m["type"] == "child_end").count(), 1);
        for message in &messages {
            assert_eq!(message["sessionId"], id);
            assert_eq!(message["childId"], id);
            assert_eq!(message["parentSessionId"], parent.session().storage().metadata().id);
            assert!(message.get("toolCallId").is_none());
        }
        assert_ne!(id, parent.session().storage().metadata().id);
        assert!(rpc_protocol::ready(Default::default())["supportedEvents"].as_array().unwrap()
            .iter().all(|v| v != "child_start"));
    }

    #[derive(Clone, Default)]
    struct RpcOutput(Arc<Mutex<Vec<u8>>>);

    impl Write for RpcOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl RpcOutput {
        fn messages(&self) -> Vec<serde_json::Value> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }

        async fn wait_for(
            &self,
            predicate: impl Fn(&serde_json::Value) -> bool,
        ) -> serde_json::Value {
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if let Some(message) = self.messages().into_iter().find(&predicate) {
                        return message;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("RPC message not received")
        }
    }

    include!("rpc/external_tools_configuration_tests.rs");
    include!("rpc/system_prompt_tests.rs");

    async fn rpc_test_harness(
        text: &str,
        tokens_per_second: f64,
    ) -> (AgentHarness, Arc<rpi_ai::providers::faux::FauxProvider>) {
        let provider = rpi_ai::providers::faux::FauxProvider::new(
            rpi_ai::providers::faux::FauxScript::new()
                .with_text(text)
                .with_tokens_per_second(tokens_per_second),
        );
        let options = rpi_harness::types::AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider.clone()],
            ..Default::default()
        };
        (AgentHarness::create(options).await.unwrap(), provider)
    }

    fn start_test_rpc(
        harness: AgentHarness,
        ui: rpi_extensions::UiDialogMailbox,
    ) -> (
        tokio::io::DuplexStream,
        RpcOutput,
        tokio::task::JoinHandle<i32>,
    ) {
        let (client, input) = tokio::io::duplex(4096);
        let output = RpcOutput::default();
        let captured = output.clone();
        let task = tokio::spawn(async move {
            rpc_with_io(
                &harness,
                None,
                vec![],
                ui,
                None,
                crate::args::RpcMessageUpdates::default(),
                tokio::io::BufReader::new(input),
                captured,
            )
            .await
        });
        (client, output, task)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_eof_waits_for_accepted_prompt() {
        use tokio::io::AsyncWriteExt;
        let (harness, _) = rpc_test_harness("completed", 100.0).await;
        let ui = rpi_extensions::UiDialogMailbox::new();
        let (mut client, output, task) = start_test_rpc(harness, ui.clone());
        client
            .write_all(b"{\"type\":\"prompt\",\"id\":1,\"message\":\"test\"}\n")
            .await
            .unwrap();
        client.shutdown().await.unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let response = output
            .messages()
            .into_iter()
            .find(|m| m["id"] == 1)
            .unwrap();
        assert_eq!(response["command"], "prompt");
        assert_eq!(response["data"]["accepted"], true);
        assert!(output.messages().iter().any(|message| {
            message["type"] == "agent_settled" && message["outcome"] == "completed"
        }));
        assert!(!ui.is_attached());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_append_entry_publishes_generic_commit_event() {
        use tokio::io::AsyncWriteExt;
        let (harness, _) = rpc_test_harness("unused", 0.0).await;
        let (mut client, output, task) =
            start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
        client
            .write_all(
                br#"{"type":"append_entry","id":1,"customType":"durable-state","data":{"revision":1}}
"#,
            )
            .await
            .unwrap();

        let event = output
            .wait_for(|message| message["type"] == "session_entry_committed")
            .await;
        assert_eq!(event["customType"], "durable-state");
        assert_eq!(event["entryType"], "custom");
        assert_eq!(event["lane"], "main");
        assert!(event["entryId"].as_str().is_some());

        client
            .write_all(
                br#"{"type":"stop","id":2}
"#,
            )
            .await
            .unwrap();
        assert_eq!(task.await.unwrap(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_prompt_publishes_committed_message_event() {
        use tokio::io::AsyncWriteExt;
        let (harness, _) = rpc_test_harness("completed", 0.0).await;
        let (mut client, output, task) =
            start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
        client
            .write_all(b"{\"type\":\"prompt\",\"id\":1,\"message\":\"test\"}\n")
            .await
            .unwrap();
        let event = output
            .wait_for(|message| {
                message["type"] == "session_entry_committed" && message["entryType"] == "message"
            })
            .await;
        assert_eq!(event["lane"], "main");
        assert!(event["entryId"].as_str().is_some());
        output
            .wait_for(|message| message["type"] == "agent_settled")
            .await;
        client
            .write_all(b"{\"type\":\"stop\",\"id\":2}\n")
            .await
            .unwrap();
        assert_eq!(task.await.unwrap(), 0);
    }

    #[tokio::test]
    async fn rpc_stop_flushes_all_accepted_entry_invalidations() {
        let (harness, _) = rpc_test_harness("unused", 0.0).await;
        let output = RpcOutput::default();
        let mut input = String::new();
        for id in 1..=4 {
            input.push_str(&format!(
                "{{\"type\":\"append_entry\",\"id\":{id},\"customType\":\"durable-state\"}}\n"
            ));
        }
        input.push_str("{\"type\":\"stop\",\"id\":5}\n");
        let result = rpc_with_io(
            &harness,
            None,
            vec![],
            rpi_extensions::UiDialogMailbox::new(),
            None,
            crate::args::RpcMessageUpdates::default(),
            tokio::io::BufReader::new(input.as_bytes()),
            output.clone(),
        )
        .await;
        assert_eq!(result, 0);
        let messages = output.messages();
        let commits: std::collections::HashSet<_> = messages
            .iter()
            .filter(|m| m["type"] == "session_entry_committed")
            .map(|m| m["entryId"].as_str().unwrap())
            .collect();
        assert_eq!(
            commits.len(),
            4,
            "stop must flush every committed entry: {messages:?}"
        );
    }

    #[tokio::test]
    async fn rpc_eof_flushes_queued_entry_invalidations() {
        let (harness, _) = rpc_test_harness("unused", 0.0).await;
        let output = RpcOutput::default();
        let input = b"{\"type\":\"append_entry\",\"id\":1,\"customType\":\"durable-state\",\"data\":{\"revision\":1}}\n";
        let result = rpc_with_io(
            &harness,
            None,
            vec![],
            rpi_extensions::UiDialogMailbox::new(),
            None,
            crate::args::RpcMessageUpdates::default(),
            tokio::io::BufReader::new(&input[..]),
            output.clone(),
        )
        .await;
        assert_eq!(result, 0);
        let messages = output.messages();
        assert!(
            messages
                .iter()
                .any(|m| m["type"] == "response" && m["id"] == 1 && m["success"] == true),
            "append succeeded: {messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|m| m["type"] == "session_entry_committed"),
            "EOF must flush the queued commit event: {messages:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_ordered_mixed_and_malformed_legacy_rejected_before_acceptance() {
        use tokio::io::AsyncWriteExt;
        let (harness, provider) = rpc_test_harness("must not run", 0.0).await;
        let (mut client, output, task) = start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
        for (index, (key, value)) in [
            ("message", serde_json::json!("legacy")),
            ("prompt", serde_json::json!("legacy")),
            ("images", serde_json::json!([{"data":"AA==","mimeType":"image/png"}])),
            ("audio", serde_json::json!([{"data":"AA==","mimeType":"audio/wav"}])),
            ("images", serde_json::json!({"bad":"type"})),
            ("audio", serde_json::json!("bad type")),
            ("images", serde_json::json!(null)),
            ("audio", serde_json::json!([{}])),
        ].into_iter().enumerate() {
            let id = index + 10;
            let mut command = serde_json::json!({"type":"prompt", "id":id,
                "content":[{"type":"text", "text":"ordered"}]});
            command[key] = value;
            client.write_all(format!("{command}\n").as_bytes()).await.unwrap();
            let response = output.wait_for(|m| m["type"] == "response" && m["id"] == id).await;
            assert_eq!(response["success"], false, "{command}: {response}");
            assert!(!output.messages().iter().any(|m| m["id"] == id && m["data"]["accepted"] == true));
            let query = serde_json::json!({"type":"get_messages", "id":id + 100});
            client.write_all(format!("{query}\n").as_bytes()).await.unwrap();
            let history = output.wait_for(|m| m["type"] == "response" && m["id"] == id + 100).await;
            assert_eq!(history["data"]["messages"].as_array().unwrap().iter()
                .filter(|m| m["role"] == "user").count(), 0, "{command}");
            assert_eq!(provider.state().call_count.load(std::sync::atomic::Ordering::Relaxed), 0);
        }
        client.write_all(b"{\"type\":\"stop\",\"id\":999}\n").await.unwrap();
        assert_eq!(task.await.unwrap(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_ordered_prompt_commits_and_reaches_provider_in_order() {
        use tokio::io::AsyncWriteExt;
        use rpi_ai::providers::faux::{FauxStep, faux_assistant_message};
        // A real tiny PNG avoids the harness's intentional invalid-image
        // placeholder conversion; assert exact bytes as well as block order.
        let parts = serde_json::json!([
            {"type":"text","text":"before"},
            {"type":"image","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==","mimeType":"image/png"},
            {"type":"text","text":"between"},
            {"type":"audio","data":"AA==","mimeType":"audio/wav"},
            {"type":"text","text":"after"}
        ]);
        let expected = parts.clone();
        let (harness, provider) = rpc_test_harness("unused", 0.0).await;
        provider.script().set_responses(vec![FauxStep::factory(move |context| {
            let messages = serde_json::to_value(&context.messages).unwrap();
            let user = messages.as_array().unwrap().iter().find(|m| m["role"] == "user").unwrap();
            assert_eq!(user["content"], expected, "provider input order");
            faux_assistant_message("completed", rpi_ai::types::StopReason::Stop)
        })]);
        let (mut client, output, task) = start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
        let command = serde_json::json!({"type":"prompt","id":1,"content":parts});
        client.write_all(format!("{command}\n").as_bytes()).await.unwrap();
        let event = output.wait_for(|m| m["type"] == "session_entry_committed" && m["entryType"] == "message").await;
        assert!(event["entryId"].as_str().is_some());
        let settled = output.wait_for(|m| m["type"] == "agent_settled").await;
        assert_eq!(settled["outcome"], "completed");
        client.write_all(b"{\"type\":\"get_messages\",\"id\":2}\n").await.unwrap();
        let history = output.wait_for(|m| m["type"] == "response" && m["id"] == 2).await;
        let user = history["data"]["messages"].as_array().unwrap().iter().find(|m| m["role"] == "user").unwrap();
        assert_eq!(user["content"], parts, "committed input order");
        client.write_all(b"{\"type\":\"stop\",\"id\":3}\n").await.unwrap();
        assert_eq!(task.await.unwrap(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_compaction_settlement_identifies_its_durable_operation() {
        let (harness, _) = rpc_test_harness("unused", 0.0).await;
        let output = RpcOutput::default();
        let code = rpc_with_io(
            &harness,
            None,
            vec![],
            rpi_extensions::UiDialogMailbox::new(),
            None,
            crate::args::RpcMessageUpdates::default(),
            tokio::io::BufReader::new(&b"{\"type\":\"compact\",\"id\":1}\n"[..]),
            output.clone(),
        )
        .await;
        assert_eq!(code, 0);
        let messages = output.messages();
        let settled = messages
            .iter()
            .find(|message| message["type"] == "agent_settled")
            .unwrap();
        assert_eq!(settled["outcome"], "declined");
        let run_id = settled["runId"]
            .as_str()
            .expect("compaction settlement needs its durable run id");
        let records = harness
            .session()
            .find_records(&Default::default())
            .await
            .unwrap();
        assert!(records.iter().any(|record| matches!(record,
            rpi_harness::session::types::LaneRecord::OperationFinished(finished)
                if finished.run_id == run_id)));
    }

    #[tokio::test]
    async fn rpc_eof_drains_queued_final_events() {
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let (tx, rx) = tokio::sync::broadcast::channel(256);
        for _ in 0..100 {
            tx.send(AgentEvent::AgentStart).unwrap();
        }
        tx.send(AgentEvent::AgentEnd { messages: vec![] }).unwrap();
        let output = RpcOutput::default();
        let code = rpc_with_io(
            &harness,
            Some(rx),
            vec![],
            rpi_extensions::UiDialogMailbox::new(),
            None,
            crate::args::RpcMessageUpdates::default(),
            tokio::io::BufReader::new(&b""[..]),
            output.clone(),
        )
        .await;
        assert_eq!(code, 0);
        let messages = output.messages();
        assert_eq!(
            messages.len(),
            102,
            "ready plus every queued event must be flushed"
        );
        assert_eq!(messages.last().unwrap()["type"], "agent_end");
    }

    fn assert_rpc_update_mode(
        messages: &[serde_json::Value],
        mode: crate::args::RpcMessageUpdates,
    ) {
        let updates: Vec<_> = messages
            .iter()
            .filter(|m| {
                matches!(
                    m["eventType"].as_str(),
                    Some("text_delta" | "thinking_delta" | "toolcall_delta")
                )
            })
            .collect();
        assert!(
            !updates.is_empty(),
            "expected streaming deltas: {messages:?}"
        );
        for update in updates {
            let snapshot = mode == crate::args::RpcMessageUpdates::Snapshot;
            assert_eq!(update.get("message").is_some(), snapshot, "{update}");
            assert_eq!(
                update["assistantMessageEvent"].get("partial").is_some(),
                snapshot,
                "{update}"
            );
            assert_eq!(update["eventType"], update["assistantMessageEvent"]["type"]);
            assert_eq!(
                update["contentIndex"],
                update["assistantMessageEvent"]["contentIndex"]
            );
            assert_eq!(update["delta"], update["assistantMessageEvent"]["delta"]);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_selected_updates_survive_prompt_eof_and_stop() {
        use tokio::io::AsyncWriteExt;
        for mode in [
            crate::args::RpcMessageUpdates::CompactDeltas,
            crate::args::RpcMessageUpdates::Snapshot,
        ] {
            for stop in [false, true] {
                let (harness, _) =
                    rpc_test_harness("one two three four five six seven eight", 100.0).await;
                let (emitter, events) = rpi_agent::events::BroadcastEmitter::new(256);
                harness
                    .set_agent_emitter(Some(Arc::new(emitter)))
                    .await
                    .unwrap();
                let (mut client, input) = tokio::io::duplex(4096);
                let output = RpcOutput::default();
                let captured = output.clone();
                let task = tokio::spawn(async move {
                    rpc_with_io(
                        &harness,
                        Some(events),
                        vec![],
                        rpi_extensions::UiDialogMailbox::new(),
                        None,
                        mode,
                        tokio::io::BufReader::new(input),
                        captured,
                    )
                    .await
                });
                let ready = output.wait_for(|m| m["type"] == "ready").await;
                assert_eq!(ready["semantics"]["messageUpdates"], mode.as_str());
                client
                    .write_all(b"{\"type\":\"prompt\",\"id\":1,\"message\":\"test\"}\n")
                    .await
                    .unwrap();
                if stop {
                    output.wait_for(|m| m["eventType"] == "text_delta").await;
                    client
                        .write_all(b"{\"type\":\"stop\",\"id\":2}\n")
                        .await
                        .unwrap();
                } else {
                    client.shutdown().await.unwrap();
                }
                assert_eq!(
                    tokio::time::timeout(std::time::Duration::from_secs(3), task)
                        .await
                        .unwrap()
                        .unwrap(),
                    0
                );
                let messages = output.messages();
                assert_rpc_update_mode(&messages, mode);
                assert!(messages.iter().any(|m| m["type"] == "message_end"));
                assert!(messages.iter().any(|m| m["type"] == "agent_end"));
                assert!(messages.iter().any(|m| m["type"] == "agent_settled"));
                if !stop {
                    assert!(messages
                        .iter()
                        .any(|m| m["type"] == "agent_settled" && m["outcome"] == "completed"));
                }
            }
        }
    }

    #[tokio::test]
    async fn rpc_normal_and_explicit_drain_use_the_same_immutable_writer_mode() {
        for mode in [
            crate::args::RpcMessageUpdates::CompactDeltas,
            crate::args::RpcMessageUpdates::Snapshot,
        ] {
            let output = RpcOutput::default();
            let (writer, task, _) = start_rpc_writer_with_mode(output.clone(), mode);
            let cloned_writer = writer.clone();
            let mut message = rpi_ai::AssistantMessage::empty(rpi_ai::Api::Faux, "faux", "m", 42);
            message.content = vec![rpi_ai::Content::text("snapshot")];
            let message = Arc::new(message);
            let (tx, mut rx) = tokio::sync::broadcast::channel(16);
            for kind in 0..3 {
                let assistant_message_event = match kind {
                    0 => rpi_ai::AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: "text".into(),
                        partial: message.clone(),
                    },
                    1 => rpi_ai::AssistantMessageEvent::ThinkingDelta {
                        content_index: 1,
                        delta: "thought".into(),
                        partial: message.clone(),
                    },
                    _ => rpi_ai::AssistantMessageEvent::ToolCallDelta {
                        content_index: 2,
                        delta: "{}".into(),
                        partial: message.clone(),
                    },
                };
                let event = AgentEvent::MessageUpdate { message_id: None,
                    message: message.clone(),
                    assistant_message_event,
                };
                write_agent_event(&writer, &event).await;
                tx.send(event).unwrap();
            }
            drain_rpc_events(&cloned_writer, &mut rx).await;
            drop(cloned_writer);
            drop(writer);
            task.await.unwrap().unwrap();
            let messages = output.messages();
            assert_eq!(messages.len(), 6);
            assert_eq!(messages[..3], messages[3..]);
            assert_rpc_update_mode(&messages, mode);
        }
    }

    #[tokio::test]
    async fn rpc_queued_deltas_are_flushed_on_eof_and_stop_in_selected_mode() {
        for mode in [
            crate::args::RpcMessageUpdates::CompactDeltas,
            crate::args::RpcMessageUpdates::Snapshot,
        ] {
            for input in ["", "{\"type\":\"stop\",\"id\":1}\n"] {
                let harness = AgentHarness::create(Default::default()).await.unwrap();
                let (tx, rx) = tokio::sync::broadcast::channel(256);
                let mut message =
                    rpi_ai::AssistantMessage::empty(rpi_ai::Api::Faux, "faux", "m", 42);
                message.content = vec![rpi_ai::Content::text("snapshot")];
                let message = Arc::new(message);
                for _ in 0..100 {
                    tx.send(AgentEvent::MessageUpdate { message_id: None,
                        message: message.clone(),
                        assistant_message_event: rpi_ai::AssistantMessageEvent::TextDelta {
                            content_index: 0,
                            delta: "x".into(),
                            partial: message.clone(),
                        },
                    })
                    .unwrap();
                }
                tx.send(AgentEvent::AgentEnd { messages: vec![] }).unwrap();
                let output = RpcOutput::default();
                assert_eq!(
                    rpc_with_io(
                        &harness,
                        Some(rx),
                        vec![],
                        rpi_extensions::UiDialogMailbox::new(),
                        None,
                        mode,
                        tokio::io::BufReader::new(input.as_bytes()),
                        output.clone()
                    )
                    .await,
                    0
                );
                let messages = output.messages();
                assert_eq!(
                    messages
                        .iter()
                        .filter(|m| m["eventType"] == "text_delta")
                        .count(),
                    100
                );
                assert_eq!(
                    messages.iter().find(|m| m["type"] == "ready").unwrap()["semantics"]
                        ["messageUpdates"],
                    mode.as_str()
                );
                assert_rpc_update_mode(&messages, mode);
                assert!(messages.iter().any(|m| m["type"] == "agent_end"));
            }
        }
    }

    struct BackpressuredOutput {
        entered: Arc<std::sync::atomic::AtomicBool>,
        release: Arc<std::sync::atomic::AtomicBool>,
        output: RpcOutput,
    }

    impl Write for BackpressuredOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.entered
                .store(true, std::sync::atomic::Ordering::SeqCst);
            while !self.release.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            self.output.write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_owner_stalled_sink_escalates_before_sink_release() {
        use std::sync::atomic::{AtomicBool, Ordering};
        // Exercise both the command and agent-event producers, including EOF
        // while a write is blocked. Library tests inject policy, never exit.
        for events_only in [false, true] {
            let harness = AgentHarness::create(Default::default()).await.unwrap();
            let release = Arc::new(AtomicBool::new(false));
            struct ReleaseOnDrop(Arc<AtomicBool>);
            impl Drop for ReleaseOnDrop {
                fn drop(&mut self) { self.0.store(true, Ordering::SeqCst); }
            }
            let _cleanup = ReleaseOnDrop(release.clone());
            let escalated = Arc::new(AtomicBool::new(false));
            let observed = escalated.clone();
            let unblock = release.clone();
            let (tx, rx) = tokio::sync::broadcast::channel(256);
            for _ in 0..150 { tx.send(AgentEvent::AgentStart).unwrap(); }
            let commands = (0..150).map(|id| format!("{{\"type\":\"get_state\",\"id\":{id}}}\n")).collect::<String>();
            let input = if events_only { "" } else { commands.as_str() };
            let result = tokio::time::timeout(std::time::Duration::from_secs(6), rpc_with_io_child(
                &harness, if events_only { Some(rx) } else { None }, vec![],
                rpi_extensions::UiDialogMailbox::new(), None, Default::default(),
                tokio::io::BufReader::new(input.as_bytes()),
                BackpressuredOutput {
                    entered: Arc::new(AtomicBool::new(false)), release: release.clone(),
                    output: RpcOutput::default(),
                }, None, None, Some(Box::new(move || {
                    assert!(!unblock.load(Ordering::SeqCst), "escalation must precede sink release");
                    observed.store(true, Ordering::SeqCst);
                    unblock.store(true, Ordering::SeqCst);
                })),
            )).await.unwrap();
            assert_eq!(result, 1);
            assert!(escalated.load(Ordering::SeqCst));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_output_backpressure_is_bounded_and_keeps_workers_responsive() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let release = Arc::new(AtomicBool::new(false));
        struct ReleaseOnDrop(Arc<AtomicBool>);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let _release = ReleaseOnDrop(release.clone());
        let entered = Arc::new(AtomicBool::new(false));
        let output = RpcOutput::default();
        let (writer, task, _) = start_rpc_writer_with_mode(
            BackpressuredOutput {
                entered: entered.clone(),
                release: release.clone(),
                output: output.clone(),
            },
            crate::args::RpcMessageUpdates::default(),
        );
        write_line(&writer, &serde_json::json!({"id": 0})).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !entered.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let producer_writer = writer.clone();
        let producer = tokio::spawn(async move {
            for id in 1..=100 {
                write_line(&producer_writer, &serde_json::json!({"id": id})).await;
            }
        });
        tokio::time::timeout(
            std::time::Duration::from_millis(150),
            tokio::time::sleep(std::time::Duration::from_millis(20)),
        )
        .await
        .unwrap();
        assert!(
            !producer.is_finished(),
            "the bounded queue must apply backpressure"
        );
        release.store(true, Ordering::SeqCst);
        producer.await.unwrap();
        drop(writer);
        task.await.unwrap().unwrap();
        let messages = output.messages();
        assert_eq!(messages.len(), 101);
        for (index, message) in messages.iter().enumerate() {
            assert_eq!(message["id"], index);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_completion_response_follows_final_event_under_backpressure() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (harness, _) = rpc_test_harness("completed", 0.0).await;
        let (emitter, events) = rpi_agent::events::BroadcastEmitter::new(256);
        harness
            .set_agent_emitter(Some(Arc::new(emitter)))
            .await
            .unwrap();
        let output = RpcOutput::default();
        let release = Arc::new(AtomicBool::new(false));
        let release_writer = release.clone();
        let release_thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            release_writer.store(true, Ordering::SeqCst);
        });
        let code = rpc_with_io(
            &harness,
            Some(events),
            vec![],
            rpi_extensions::UiDialogMailbox::new(),
            None,
            crate::args::RpcMessageUpdates::default(),
            tokio::io::BufReader::new(
                &b"{\"type\":\"prompt\",\"id\":1,\"message\":\"test\"}\n"[..],
            ),
            BackpressuredOutput {
                entered: Arc::new(AtomicBool::new(false)),
                release,
                output: output.clone(),
            },
        )
        .await;
        release_thread.join().unwrap();
        assert_eq!(code, 0);
        let messages = output.messages();
        let end = messages
            .iter()
            .position(|message| message["type"] == "agent_end")
            .unwrap();
        let settled = messages
            .iter()
            .position(|message| message["type"] == "agent_settled")
            .unwrap();
        assert!(end < settled);
        let run_id = messages[settled]["runId"].as_str().unwrap();
        assert!(!run_id.is_empty(), "settlement identifies its durable run");
        let records = harness
            .session()
            .find_records(&Default::default())
            .await
            .unwrap();
        assert!(records.iter().any(|record| matches!(record,
            rpi_harness::session::types::LaneRecord::OperationFinished(finished)
                if finished.run_id == run_id)));
        assert!(messages.iter().any(|message| {
            message["type"] == "response" && message["data"]["accepted"] == true
        }));
        assert!(messages.iter().any(|message| {
            message["type"] == "agent_settled" && message["outcome"] == "completed"
        }));
    }

    #[tokio::test]
    async fn rpc_output_failure_closes_without_waiting_for_more_input() {
        struct BrokenOutput;
        impl Write for BrokenOutput {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed client output",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let (_client, input) = tokio::io::duplex(4096);
        let ui = rpi_extensions::UiDialogMailbox::new();
        let code = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            rpc_with_io(
                &harness,
                None,
                vec![],
                ui.clone(),
                None,
                crate::args::RpcMessageUpdates::default(),
                tokio::io::BufReader::new(input),
                BrokenOutput,
            ),
        )
        .await
        .unwrap();
        assert_eq!(code, 1);
        assert!(!ui.is_attached());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_stop_aborts_and_joins_active_prompt() {
        use tokio::io::AsyncWriteExt;
        let (harness, provider) = rpc_test_harness("still streaming the response", 20.0).await;
        let lane = harness.lane("main");
        let (mut client, output, task) =
            start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
        client
            .write_all(b"{\"type\":\"prompt\",\"id\":1,\"message\":\"test\"}\n")
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while provider
                .state()
                .call_count
                .load(std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        client
            .write_all(b"{\"type\":\"stop\",\"id\":2}\n")
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let messages = output.messages();
        assert!(messages.iter().any(|message| {
            message["type"] == "agent_settled"
                && (message["outcome"] == "aborted" || message["outcome"] == "failed")
        }));
        assert_eq!(
            messages.iter().find(|m| m["id"] == 2).unwrap()["data"]["stopped"],
            true
        );
        assert!(
            lane.abort().await.is_err(),
            "RPC returned with an active lane"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_immediate_stop_cancels_prompt_before_or_after_lane_start() {
        use tokio::io::AsyncWriteExt;
        let (harness, _) = rpc_test_harness("not completed", 20.0).await;
        let lane = harness.lane("main");
        let (mut client, output, task) =
            start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
        client.write_all(b"{\"type\":\"prompt\",\"id\":1,\"message\":\"test\"}\n{\"type\":\"stop\",\"id\":2}\n").await.unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let messages = output.messages();
        assert!(messages.iter().any(|message| {
            message["type"] == "agent_settled"
                && (message["outcome"] == "aborted" || message["outcome"] == "failed")
        }));
        assert!(lane.abort().await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_dialog_answers_and_ping_are_read_while_prompt_runs() {
        use tokio::io::AsyncWriteExt;
        let (harness, _) =
            rpc_test_harness("the streamed response finishes after the dialog", 30.0).await;
        let ui = rpi_extensions::UiDialogMailbox::new();
        let (mut client, output, task) = start_test_rpc(harness, ui.clone());
        output.wait_for(|m| m["type"] == "ready").await;
        client
            .write_all(b"{\"type\":\"prompt\",\"id\":1,\"message\":\"test\"}\n")
            .await
            .unwrap();
        ui.handle(serde_json::json!({"requestId":"approval", "toolCallId":"call", "ui":{}}))
            .unwrap();
        let dialog = output.wait_for(|m| m["type"] == "ui_dialog_request").await;
        let wire = dialog["requestId"].as_str().unwrap();
        let response = serde_json::json!({"type":"ui_dialog_response","requestId":wire,"answer":true});
        client.write_all(format!("{response}\n{{\"type\":\"ping\",\"id\":2}}\n").as_bytes()).await.unwrap();
        output.wait_for(|m| m["id"] == 2).await;
        assert_eq!(ui.poll("approval").unwrap()["answer"], true);
        output
            .wait_for(|m| m["type"] == "ui_dialog_closed" && m["requestId"] == wire)
            .await;
        client.shutdown().await.unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(output.messages().iter().any(|message| {
            message["type"] == "agent_settled" && message["outcome"] == "completed"
        }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_notifies_client_when_an_approval_is_cancelled() {
        use tokio::io::AsyncWriteExt;
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let ui = rpi_extensions::UiDialogMailbox::new();
        let (mut client, output, task) = start_test_rpc(harness, ui.clone());
        output.wait_for(|m| m["type"] == "ready").await;
        ui.handle(serde_json::json!({"requestId":"cancelled", "toolCallId":"c", "ui":{}}))
            .unwrap();
        let dialog = output.wait_for(|m| m["type"] == "ui_dialog_request").await;
        ui.cancel_tool_call("c");
        output
            .wait_for(|m| m["type"] == "ui_dialog_closed" && m["requestId"] == dialog["requestId"])
            .await;
        // The notification must not consume the cancellation meant for the plugin.
        assert_eq!(ui.poll("cancelled").unwrap()["status"], "cancelled");
        client.shutdown().await.unwrap();
        assert_eq!(task.await.unwrap(), 0);
    }

    #[tokio::test]
    async fn rpc_url_actions_reach_plugin_without_collapsing_decline_into_cancel() {
        use tokio::io::AsyncWriteExt;
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let ui = rpi_extensions::UiDialogMailbox::new();
        let (mut client, output, task) = start_test_rpc(harness, ui.clone());
        output.wait_for(|m| m["type"] == "ready").await;
        for (index, action) in ["accept", "decline", "cancel"].into_iter().enumerate() {
            let request_id = format!("url-{index}");
            ui.handle(serde_json::json!({"requestId":request_id,"ui":{
                "kind":"url","url":"https://example.com","message":"Open URL"
            }}))
            .unwrap();
            let dialog = output
                .wait_for(|m| m["type"] == "ui_dialog_request" && m["ui"]["kind"] == "url"
                    && m["requestId"].as_str().is_some_and(|id| ui.is_pending_wire(id)))
                .await;
            let wire = &dialog["requestId"];
            // Invalid actions must leave the request pending.
            let invalid = serde_json::json!({"type":"ui_dialog_response", "id":100 + index,
                "requestId":wire,"urlElicitation":{"action":"invalid"}});
            client
                .write_all(format!("{invalid}\n").as_bytes())
                .await
                .unwrap();
            let ack = output.wait_for(|m| m["id"] == 100 + index).await;
            assert_eq!(ack["success"], false);
            assert!(ui.is_pending(&request_id));
            let response = serde_json::json!({"type":"ui_dialog_response", "id":200 + index,
                "requestId":wire,"urlElicitation":{"action":action},"cancelled":action != "accept"});
            client
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
            assert_eq!(
                output.wait_for(|m| m["id"] == 200 + index).await["success"],
                true
            );
            let result = ui.poll(&request_id).unwrap();
            assert_eq!(result["answer"]["urlElicitation"]["action"], action);
            assert!(!ui.is_pending(&request_id));
            client
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        }
        client.shutdown().await.unwrap();
        assert_eq!(task.await.unwrap(), 0);
    }

    #[tokio::test]
    async fn rpc_client_requests_correlate_same_method_and_retire_late_replies() {
        use tokio::io::AsyncWriteExt;
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let (mut client, output, task) =
            start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
        for id in [1, 2] {
            let request = serde_json::json!({"type":"client_request", "id":id,
                "correlationId":format!("origin-{id}"),
                "method":"fs/read_text_file","params":{"path":format!("C:/file-{id}")}});
            client
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
        }
        let first = output
            .wait_for(|m| m["type"] == "client_request" && m["params"]["path"] == "C:/file-1")
            .await;
        let second = output
            .wait_for(|m| m["type"] == "client_request" && m["params"]["path"] == "C:/file-2")
            .await;
        assert_ne!(first["requestId"], second["requestId"]);
        assert_eq!(first["correlationId"], "origin-1");
        assert_eq!(second["correlationId"], "origin-2");
        for (id, event) in [(2, second), (1, first)] {
            let response = serde_json::json!({"type":"client_response", "requestId":event["requestId"],"result":{"content":format!("reply-{id}")}});
            client
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
            let result = output
                .wait_for(|m| m["type"] == "response" && m["id"] == id)
                .await;
            assert_eq!(result["data"]["content"], format!("reply-{id}"));
            output
                .wait_for(|m| {
                    m["type"] == "client_request_closed" && m["requestId"] == event["requestId"]
                })
                .await;
            client
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        }
        client.write_all(b"{\"type\":\"client_request\",\"id\":3,\"method\":\"fs/read_text_file\",\"params\":{},\"timeoutMs\":1}\n").await.unwrap();
        let expired = output
            .wait_for(|m| m["type"] == "client_request" && m["params"] == serde_json::json!({}))
            .await;
        assert_eq!(output.wait_for(|m| m["id"] == 3).await["success"], false);
        let late = serde_json::json!({"type":"client_response","requestId":expired["requestId"],"result":{"content":"late"}});
        client
            .write_all(format!("{late}\n").as_bytes())
            .await
            .unwrap();
        client.write_all(b"{\"type\":\"client_request\",\"id\":4,\"method\":\"fs/read_text_file\",\"params\":{\"path\":\"abort\"}}\n").await.unwrap();
        let aborted = output
            .wait_for(|m| m["type"] == "client_request" && m["params"]["path"] == "abort")
            .await;
        client
            .write_all(b"{\"type\":\"abort\",\"id\":5}\n")
            .await
            .unwrap();
        assert_eq!(output.wait_for(|m| m["id"] == 4).await["success"], false);
        output
            .wait_for(|m| {
                m["type"] == "client_request_closed" && m["requestId"] == aborted["requestId"]
            })
            .await;
        client.write_all(b"{\"type\":\"client_request\",\"id\":6,\"method\":\"fs/read_text_file\",\"params\":{\"path\":\"eof\"}}\n").await.unwrap();
        let eof = output
            .wait_for(|m| m["type"] == "client_request" && m["params"]["path"] == "eof")
            .await;
        client.shutdown().await.unwrap();
        assert_eq!(task.await.unwrap(), 0);
        let messages = output.messages();
        assert!(messages
            .iter()
            .any(|m| m["type"] == "client_request_closed" && m["requestId"] == eof["requestId"]));
        for id in [1, 2, 3, 4, 6] {
            assert_eq!(
                messages
                    .iter()
                    .filter(|m| m["type"] == "response" && m["id"] == id)
                    .count(),
                1
            );
        }
    }

    #[tokio::test]
    async fn rpc_accepts_native_dialog_responses_without_consuming_plugin_answers() {
        use crate::remote::protocol::RemoteCommand;
        use tokio::io::AsyncWriteExt;
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let ui = rpi_extensions::UiDialogMailbox::new();
        let (mut client, output, task) = start_test_rpc(harness, ui.clone());
        output.wait_for(|m| m["type"] == "ready").await;
        for (request_id, answer, response_id) in [
            (
                "answered",
                Some(serde_json::json!({"values":["No"],"kind":"selector"})),
                5,
            ),
            ("cancelled", None, 6),
        ] {
            ui.handle(serde_json::json!({"requestId":request_id,"toolCallId":request_id,"ui":{}}))
                .unwrap();
            let dialog = output
                .wait_for(|m| m["type"] == "ui_dialog_request" && m["toolCallId"] == request_id)
                .await;
            let command = RemoteCommand::UiDialogResponse {
                request_id: dialog["requestId"].as_str().unwrap().into(),
                cancelled: answer.is_none(),
                answer,
            }
            .with_id(response_id);
            client
                .write_all(format!("{command}\n").as_bytes())
                .await
                .unwrap();
            let ack = output.wait_for(|m| m["id"] == response_id).await;
            assert_eq!(ack["data"]["accepted"], true);
            let result = ui.poll(request_id).unwrap();
            if request_id == "answered" {
                assert_eq!(result["answer"]["values"][0], "No");
            } else {
                assert_eq!(result["status"], "cancelled");
            }
        }
        client.shutdown().await.unwrap();
        assert_eq!(task.await.unwrap(), 0);
    }

    fn assistant(text: &str, stop: StopReason) -> AssistantMessage {
        AssistantMessage {
            role: rpi_ai::types::AssistantRole,
            content: vec![Content::Text(TextContent {
                kind: TextContentType,
                text: text.into(),
                text_signature: None,
            })],
            api: rpi_ai::Api::AnthropicMessages,
            provider: "anthropic".into(),
            model: "claude-sonnet-5".into(),
            message_id: None,
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        }
    }

    #[test]
    fn assistant_text_concatenates_text_blocks() {
        let m = assistant("hello", StopReason::Stop);
        assert_eq!(assistant_text(&m), "hello");
    }

    #[test]
    fn outcome_exit_code_maps_failed_aborted_to_1() {
        let failed = HarnessRunOutcome::Failed {
            leaf_id: "l".into(),
            error: OperationError {
                code: "boom".into(),
                message: "boom".into(),
            },
            final_entry_id: None,
            final_message: None,
        };
        assert_eq!(outcome_exit_code(&failed), 1);
        let completed = HarnessRunOutcome::Completed {
            leaf_id: "l".into(),
            final_entry_id: "e".into(),
            final_message: assistant("ok", StopReason::Stop),
        };
        assert_eq!(outcome_exit_code(&completed), 0);
    }

    #[tokio::test]
    async fn rpc_dialog_wire_ownership_survives_collisions_and_late_cleanup() {
        use tokio::io::AsyncWriteExt;
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let ui = rpi_extensions::UiDialogMailbox::new();
        let (mut client, output, task) = start_test_rpc(harness, ui.clone());
        output.wait_for(|m| m["type"] == "ready").await;
        let sessions = [None, Some("child-a"), Some("child-b")];
        let mut dialogs = Vec::new();
        for session in sessions {
            ui.handle_with_session(serde_json::json!({"requestId":"same", "toolCallId":"same-call",
                "sessionId":"forged", "ui":{"message":session.unwrap_or("root")}}), session.map(str::to_owned)).unwrap();
            let dialog = output.wait_for(|m| m["type"] == "ui_dialog_request"
                && m["ui"]["message"] == session.unwrap_or("root")).await;
            assert_ne!(dialog["requestId"], "same");
            assert_eq!(dialog.get("sessionId").and_then(|v| v.as_str()), session);
            dialogs.push(dialog);
        }
        // The old caller-local ID is not a transport capability, even at root.
        client.write_all(b"{\"type\":\"ui_dialog_response\",\"id\":70,\"requestId\":\"same\",\"answer\":\"forged\"}\n").await.unwrap();
        assert_eq!(output.wait_for(|m| m["id"] == 70).await["success"], false);
        ui.cancel_tool_call_with_session("same-call", Some("child-a".into()));
        let closed = output.wait_for(|m| m["type"] == "ui_dialog_closed" && m["requestId"] == dialogs[1]["requestId"]).await;
        assert_eq!(closed["sessionId"], "child-a");
        for i in [2, 0, 1] {
            let response = serde_json::json!({"type":"ui_dialog_response","id":80+i,
                "requestId":dialogs[i]["requestId"],"answer":i,"sessionId":"forged"});
            client.write_all(format!("{response}\n").as_bytes()).await.unwrap();
            assert_eq!(output.wait_for(|m| m["id"] == 80+i).await["success"], i != 1);
            let result = ui.handle_with_session(serde_json::json!({"op":"poll","requestId":"same"}), sessions[i].map(str::to_owned)).unwrap();
            assert_eq!(result["requestId"], "same");
            if i == 1 {
                assert_eq!(result["status"], "cancelled");
            } else {
                assert_eq!(result["answer"], i);
                let closed = output.wait_for(|m| m["type"] == "ui_dialog_closed" && m["requestId"] == dialogs[i]["requestId"]).await;
                assert_eq!(closed.get("sessionId").and_then(|v| v.as_str()), sessions[i]);
            }
            // Retirement cannot answer a reopened local request or a sibling.
            client.write_all(format!("{response}\n").as_bytes()).await.unwrap();
            client.write_all(format!("{{\"type\":\"ping\",\"id\":{}}}\n", 90+i).as_bytes()).await.unwrap();
            output.wait_for(|m| m["id"] == 90+i).await;
        }
        client.shutdown().await.unwrap();
        assert_eq!(task.await.unwrap(), 0);
    }

    #[test]
    fn run_end_outcome_str_roundtrip() {
        assert_eq!(run_end_outcome_str(RunEndOutcome::Completed), "completed");
        assert_eq!(run_end_outcome_str(RunEndOutcome::Aborted), "aborted");
        assert_eq!(run_end_outcome_str(RunEndOutcome::Failed), "failed");
    }

    #[test]
    fn agent_event_projection_keeps_terminal_and_tool_payloads() {
        let end = agent_event_json(&AgentEvent::AgentEnd { messages: vec![] });
        assert_eq!(end["type"], "agent_end");
        assert_eq!(end["messages"], serde_json::json!([]));

        let tool = agent_event_json(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            result: AgentToolResult::text("hello"),
            is_error: false,
        });
        assert_eq!(tool["type"], "tool_execution_end");
        assert_eq!(tool["result"]["content"][0]["text"], "hello");
        assert_eq!(tool["result"]["terminate"], false);

        let retry = agent_event_json(&AgentEvent::RetryScheduled {
            attempt: 3,
            max_retries: 10,
            delay_ms: 8_000,
            error: "503 service unavailable".into(),
        });
        assert_eq!(retry["type"], "retry_scheduled");
        assert_eq!(retry["attempt"], 3);
        assert_eq!(retry["maxRetries"], 10);
        assert_eq!(retry["delayMs"], 8_000);
        assert_eq!(retry["error"], "503 service unavailable");
    }
}

#[cfg(test)]
mod page_window_tests {
    use super::page_window;

    #[test]
    fn tail_is_kept_without_before_id() {
        // 12 items, limit 5 -> keep the last 5, more behind.
        assert_eq!(page_window(12, 5, None), (7..12, true));
        // Fewer items than the limit -> everything, nothing behind.
        assert_eq!(page_window(3, 5, None), (0..3, false));
    }

    #[test]
    fn before_id_opens_the_window_immediately_before_it() {
        // Page back from index 10 with limit 4 -> [6,10), more behind.
        assert_eq!(page_window(12, 4, Some(10)), (6..10, true));
        // Same, but reaching the very start -> no more behind.
        assert_eq!(page_window(12, 10, Some(10)), (0..10, false));
    }

    #[test]
    fn limit_larger_than_the_branch_never_underflows() {
        assert_eq!(page_window(5, 100, Some(2)), (0..2, false));
        assert_eq!(page_window(5, 100, None), (0..5, false));
    }
}

#[cfg(test)]
mod session_storage_notice_tests {
    use super::session_storage_notice;

    /// Print mode cannot recover a refused write by exporting: it is about to
    /// exit and offers no interactive `/export`. The message must say what was
    /// lost instead of naming a recovery the user cannot run from here.
    #[test]
    fn the_headless_notice_does_not_promise_an_unavailable_recourse() {
        let notice = session_storage_notice("disk full");
        assert!(notice.contains("disk full"), "{notice}");
        assert!(
            !notice.contains("/export"),
            "print mode has no interactive /export: {notice}"
        );
        assert!(
            notice.contains("print mode exits"),
            "the notice must say the turn is not coming back: {notice}"
        );
    }
}
