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
//!   documented in `docs/m6-cli-open-questions.md`.
//!
//! All three drive the same `AgentHarness` via `AgentLane::prompt_text`.

use std::io::{BufRead, IsTerminal, Write};
use std::sync::{Arc, Mutex};

use rpi_agent::events::AgentEvent;
use rpi_ai::types::{AssistantMessage, Content, ImageContent, StopReason};
use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use rpi_harness::events::{HarnessEvent, RunEndOutcome};

use crate::args::Args;
use crate::remote::protocol::RemoteResponse;

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
        match lane.prompt_text(&prompt, std::mem::take(&mut images)).await {
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
                        eprintln!("run failed: {error:?}");
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

/// `rpc` mode: a headless JSONL command loop (the server-side agent).
///
/// Reads one JSON command per line from stdin (`{id?, type, ...}`), drives the
/// `main` lane, and writes one JSON object per line to stdout:
/// - fine-grained `AgentEvent`s via [`agent_event_json`] (each carries `type`),
/// - a `{"type":"response","id":<id>,"status":"ok"|"error",...}` per command.
///
/// Commands: `prompt` (streams the run), `abort`, `get_state`, `set_model`,
/// `set_thinking_level`, `set_active_tools`, `ping`, `stop`. `stop` (or stdin
/// EOF) ends the loop. This is the process `rpi-server` spawns and that
/// `rpi --connect` talks to. Mirrors TS `runRpcMode` / `modes/rpc`.
pub async fn rpc(
    harness: &AgentHarness,
    mut agent_events: Option<tokio::sync::broadcast::Receiver<AgentEvent>>,
    model_catalog: Vec<rpi_ai::Model>,
) -> i32 {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use tokio::sync::Mutex as AsyncMutex;

    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let stdout = Arc::new(AsyncMutex::new(tokio::io::stdout()));

    // Stream fine-grained AgentEvents as JSONL until the receiver closes.
    if let Some(mut rx) = agent_events.take() {
        let out = Arc::clone(&stdout);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => write_line(&out, &agent_event_json(&event)).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // Readiness marker: a host/client can wait for the first line.
    write_line(&stdout, &serde_json::json!({"type": "ready"})).await;

    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break, // EOF
            Err(error) => {
                eprintln!("[rpi] rpc stdin error: {error}");
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
                    &RemoteResponse::error(
                        serde_json::Value::Null,
                        format!("invalid json: {error}"),
                    )
                    .to_json(),
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

        if kind == "stop" {
            write_line(
                &stdout,
                &RemoteResponse::ok(id.clone(), serde_json::json!({"stopped": true})).to_json(),
            )
            .await;
            break;
        }

        let result = match kind.as_str() {
            "prompt" => rpc_prompt(&lane, &msg).await,
            "abort" => lane
                .abort()
                .await
                .map(|_| serde_json::json!({"aborted": true}))
                .map_err(|e| e.to_string()),
            "get_state" => Ok(rpc_get_state(&lane, harness).await),
            "set_model" => rpc_set_model(&lane, &model_catalog, &msg).await,
            "set_thinking_level" => rpc_set_thinking_level(&lane, &msg).await,
            "set_active_tools" => rpc_set_active_tools(&lane, &msg).await,
            "ping" => Ok(serde_json::json!({"pong": true})),
            other => Err(format!("unknown command type: {other}")),
        };

        let response = match result {
            Ok(value) => RemoteResponse::ok(id.clone(), value),
            Err(error) => RemoteResponse::error(id.clone(), error),
        };
        write_line(&stdout, &response.to_json()).await;
    }

    let _ = stdout.lock().await.flush().await;
    0
}

async fn write_line(out: &Arc<tokio::sync::Mutex<tokio::io::Stdout>>, value: &serde_json::Value) {
    use tokio::io::AsyncWriteExt;
    let mut guard = out.lock().await;
    let _ = guard.write_all(value.to_string().as_bytes()).await;
    let _ = guard.write_all(b"\n").await;
    let _ = guard.flush().await;
}

fn rpc_outcome_str(outcome: &HarnessRunOutcome) -> (&'static str, Option<String>) {
    match outcome {
        HarnessRunOutcome::Completed { final_message, .. } => {
            ("completed", Some(assistant_text(final_message)))
        }
        HarnessRunOutcome::Aborted { final_message, .. } => {
            ("aborted", Some(assistant_text(final_message)))
        }
        HarnessRunOutcome::Failed { final_message, .. } => {
            ("failed", final_message.as_ref().map(assistant_text))
        }
        HarnessRunOutcome::Suspended { .. } => ("suspended", None),
    }
}

async fn rpc_prompt(
    lane: &Arc<dyn AgentLane>,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let content = msg
        .get("content")
        .or_else(|| msg.get("prompt"))
        .and_then(|v| v.as_str())
        .ok_or("prompt requires a string `content`")?
        .to_string();
    let result = lane
        .prompt_text(&content, Vec::new())
        .await
        .map_err(|e| e.to_string())?;
    let (outcome, final_text) = rpc_outcome_str(&result.outcome);
    Ok(serde_json::json!({"outcome": outcome, "finalText": final_text}))
}

async fn rpc_get_state(lane: &Arc<dyn AgentLane>, harness: &AgentHarness) -> serde_json::Value {
    let model = lane.get_model().await.ok().map(|m| m.id);
    let thinking = lane
        .get_thinking_level()
        .await
        .ok()
        .map(|t| serde_json::to_value(t).unwrap_or(serde_json::Value::Null));
    let active_tools = lane.get_active_tools().await.unwrap_or_default();
    let leaf_id = lane.get_leaf_id().await.ok().flatten();
    serde_json::json!({
        "model": model,
        "thinkingLevel": thinking,
        "activeTools": active_tools,
        "leafId": leaf_id,
        "sessionId": harness.session().storage().metadata().id,
    })
}

async fn rpc_set_model(
    lane: &Arc<dyn AgentLane>,
    catalog: &[rpi_ai::Model],
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let id = msg
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or("set_model requires `model`")?;
    let model = catalog
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(id))
        .cloned()
        .ok_or_else(|| format!("unknown model: {id}"))?;
    lane.set_model(model).await.map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"model": id}))
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
    serde_json::to_value(crate::remote::protocol::RemoteEvent::from_agent_event(
        event,
    ))
    .unwrap_or(serde_json::Value::Null)
}

/// Emit a single harness event as a JSON line on stdout. Mirrors the TS
/// `toJsonEvent` projection (here a lossy but stable shape: `type` + the event
/// payload's key fields).
fn emit_json_event(event: &HarnessEvent) {
    let line = match event {
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
/// v1 does not switch models mid-session; see `docs/m6-cli-open-questions.md`).
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
        // Fall back to simple REPL
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

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_agent::events::AgentEvent;
    use rpi_agent::types::AgentToolResult;
    use rpi_ai::types::{
        AssistantMessage, Content, StopReason, TextContent, TextContentType, Usage,
    };
    use rpi_harness::session::types::OperationError;

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
