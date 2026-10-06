//! Prompt execution, completion reconciliation, compaction, and clipboard operations.

use super::*;

// ===========================================================================
// Run a single prompt (streaming or blocking)
// ===========================================================================

pub(super) fn launch_external_editor(draft: String, tx: mpsc::UnboundedSender<TuiMessage>) {
    std::thread::spawn(move || {
        let file = match tempfile::Builder::new()
            .prefix("rpi-draft-")
            .suffix(".md")
            .tempfile()
        {
            Ok(file) => file,
            Err(error) => {
                let _ = tx.send(TuiMessage::ExternalEditorResult(Err(format!(
                    "Could not create editor file: {error}"
                ))));
                return;
            }
        };
        if let Err(error) = std::fs::write(file.path(), draft.as_bytes()) {
            let _ = tx.send(TuiMessage::ExternalEditorResult(Err(format!(
                "Could not write editor file: {error}"
            ))));
            return;
        }
        let editor = std::env::var("RPI_EXTERNAL_EDITOR")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| std::env::var("VISUAL").ok())
            .or_else(|| std::env::var("EDITOR").ok())
            .unwrap_or_else(|| {
                if cfg!(windows) {
                    "notepad".to_string()
                } else {
                    "nano".to_string()
                }
            });
        let status = std::process::Command::new(&editor)
            .arg(file.path())
            .status();
        let result = match status {
            Ok(status) if status.success() => std::fs::read_to_string(file.path())
                .map_err(|error| format!("Could not read editor file: {error}")),
            Ok(status) => Err(format!("External editor exited with {status}")),
            Err(error) => Err(format!(
                "Could not launch external editor `{editor}`: {error}"
            )),
        };
        let _ = tx.send(TuiMessage::ExternalEditorResult(result));
    });
}

/// Apply the authoritative run result when it wins the race with the async
/// event drain, then detach the live component from further partial updates.
pub(super) fn reconcile_streamed_assistant_completion(
    current_assistant: &Arc<std::sync::Mutex<Option<Arc<AssistantMessageComponent>>>>,
    last_assistant_text: &Mutex<String>,
    final_message: Option<&AssistantMessage>,
) {
    let final_blocks = final_message.map(assistant_blocks);
    let final_text = final_message.map(assistant_text);
    let component = current_assistant.lock().unwrap().take();

    if let Some(component) = component {
        if let Some(blocks) = final_blocks.as_deref() {
            component.update_blocks(blocks);
        }
        component.set_streaming(false);
    }

    if let Some(text) = final_text.filter(|text| !text.is_empty()) {
        *last_assistant_text.lock().unwrap() = text;
    }
}

/// Drive a single prompt through the lane. When `streaming` is true, the
/// `AgentEvent` drain task renders the response live and the completed outcome
/// reconciles its final snapshot. When false (no `event_rx`), this falls back
/// to the blocking await-final-text path.

/// Drive a single prompt through the lane. When `streaming` is true, the
/// `AgentEvent` drain task renders the response live and the completed outcome
/// reconciles its final snapshot. When false (no `event_rx`), this falls back
/// to the blocking await-final-text path.
pub(super) async fn run_prompt_streaming(
    lane: &Arc<dyn AgentLane>,
    prompt: &str,
    resume: bool,
    tui: &Arc<TuiAltScreen>,
    state: &Arc<TuiState>,
    streaming: bool,
    images: Vec<rpi_ai::types::ImageContent>,
) {
    state.set_status(RunStatus::Working);
    tui.request_render(false);

    let outcome = if resume {
        // Continuing an interrupted run: no new user turn, just the provider call
        // the interrupted run never reached. upstream behaves the same way.
        match lane.resume_pending().await {
            Ok(Some(result)) => Ok(result),
            // Nothing to resume after all: leave the status alone and let the
            // normal prompt path take over.
            Ok(None) => {
                state.set_status(RunStatus::Idle);
                tui.request_render(false);
                return;
            }
            Err(error) => Err(error),
        }
    } else {
        lane.prompt_text(prompt, images).await
    };

    if streaming {
        // Broadcast delivery is asynchronous: the harness result can resolve
        // before the drain task processes MessageEnd. Reconcile from the
        // authoritative outcome before detaching the component so the final
        // streamed tail cannot be left at an earlier partial snapshot.
        let final_message = match &outcome {
            Ok(result) => match &result.outcome {
                HarnessRunOutcome::Completed { final_message, .. }
                | HarnessRunOutcome::Aborted { final_message, .. } => Some(final_message),
                HarnessRunOutcome::Failed { final_message, .. } => final_message.as_ref(),
                HarnessRunOutcome::Suspended { .. } => None,
            },
            Err(_) => None,
        };
        reconcile_streamed_assistant_completion(
            &state.current_assistant,
            &state.last_assistant_text,
            final_message,
        );
    }

    state.set_status(RunStatus::Idle);
    // Guarantee the pending-messages display reconciles at the end of every
    // run/prompt — covers aborts and the non-streaming path, where no
    // `AgentEnd` event necessarily reaches the drain task.
    refresh_pending_messages(state, lane).await;

    // Authoritative context-badge fallback. The streaming drain normally
    // updates it from `MessageEnd`, but a `Lagged` broadcast or the blocking
    // path can skip that event; reconcile from the run outcome so the badge
    // always reflects the newest real response.
    if let Ok(result) = &outcome {
        let final_message: Option<&AssistantMessage> = match &result.outcome {
            HarnessRunOutcome::Completed { final_message, .. }
            | HarnessRunOutcome::Aborted { final_message, .. } => Some(final_message),
            HarnessRunOutcome::Failed { final_message, .. } => final_message.as_ref(),
            HarnessRunOutcome::Suspended { .. } => None,
        };
        if let Some(a) = final_message {
            record_context_usage(&state.footer, a);
        }
    }

    match outcome {
        Ok(result) => match &result.outcome {
            HarnessRunOutcome::Failed {
                error,
                final_message,
                ..
            } => {
                // Only add an error line if the stream did NOT already render
                // an assistant message for it (drain task leaves
                // current_assistant Some only on an abrupt end).
                // A final assistant error is emitted by the event drain only
                // in streaming mode. In regular mode there is no drain task,
                // so suppressing this branch merely hides 405/auth/network
                // diagnostics from the user.
                let already_rendered = streaming && final_message.is_some();
                if !already_rendered {
                    let msg = final_message
                        .as_ref()
                        .and_then(|m| m.error_message.clone())
                        .unwrap_or_else(|| format!("{error:?}"));
                    add_error_message(&state.chat_container, &msg);
                }
            }
            HarnessRunOutcome::Suspended { .. } => {
                add_error_message(
                    &state.chat_container,
                    "Run suspended (deferred) — resume is not supported in v1.",
                );
            }
            HarnessRunOutcome::Aborted { final_message, .. } => {
                // Aborted runs render their own partial/final message via the
                // stream; only add a note on the blocking fallback path.
                if !streaming {
                    add_error_message(&state.chat_container, "Request aborted.");
                    let _ = final_message; // (rendered by the stream in streaming mode)
                }
            }
            HarnessRunOutcome::Completed { final_message, .. } => {
                if !streaming {
                    let text = assistant_text(final_message);
                    if !text.is_empty() {
                        add_assistant_message_blocking(
                            &state.chat_container,
                            &text,
                            state.markdown_transformer(),
                        );
                        *state.last_assistant_text.lock().unwrap() = text;
                    }
                }
            }
        },
        Err(e) => {
            add_error_message(&state.chat_container, &e.to_string());
        }
    }

    tui.request_render(false);
}

/// `/compact`: drive a compaction on the lane (mirrors TS `app.compact`).
/// Reports the outcome as a transcript note; the harness progress watcher
/// displays the compaction indicator while the summary is generated.
pub(super) async fn run_compact(
    lane: &Arc<dyn AgentLane>,
    tui: &Arc<TuiAltScreen>,
    state: &Arc<TuiState>,
) {
    state.set_status(RunStatus::Working);
    tui.request_render(false);
    match lane.compact(None).await {
        Ok(_) => {
            add_note_message(&state.chat_container, "Conversation compacted.");
            // The last assistant usage describes the pre-compaction context;
            // reset the badge to `?` until the next response reports the new
            // (much smaller) prompt size — mirrors pi's `percent: null`.
            state.footer.set_context_tokens(None);
        }
        Err(e) => {
            add_error_message(&state.chat_container, &format!("Compact failed: {e}"));
        }
    }
    state.set_status(RunStatus::Idle);
    tui.request_render(false);
}

/// `/copy`: copy the last assistant reply to the clipboard. Best-effort —
/// when no clipboard is available (or the `clipboard` feature is off), prints a
/// hint instead. Mirrors the TS `/copy` (copies `this.messages.at(-1)` text).
pub(super) fn copy_last_assistant(state: &Arc<TuiState>, chat: &Arc<Container>) {
    let text = state.last_assistant_text.lock().unwrap().clone();
    if text.is_empty() {
        add_note_message(chat, "Nothing to copy yet — no assistant reply captured.");
        return;
    }
    if copy_to_clipboard(&text) {
        add_note_message(chat, "Copied last reply to the clipboard.");
    } else {
        // Clipboard unavailable — print the text to the transcript so the user
        // can select/copy it manually (degrades gracefully in headless envs).
        let preview: String = text.chars().take(200).collect();
        add_note_message(
            chat,
            &format!(
                "Clipboard unavailable. Last reply: {preview}{}",
                if text.chars().count() > 200 {
                    "…"
                } else {
                    ""
                }
            ),
        );
    }
}

/// Extract text from the chat container within the given screen coordinates.
/// Returns None if the selection is invalid or empty.
pub(super) fn extract_selected_text(
    chat_container: &Arc<Container>,
    start: (u16, u16),
    end: (u16, u16),
) -> Option<String> {
    use rpi_tui::Component;

    // Get the rendered lines from the chat container
    let lines = chat_container.render(80); // Use a reasonable width
    if lines.is_empty() {
        return None;
    }

    // Normalize coordinates (ensure start is before end)
    let (start_row, end_row) = if start.1 <= end.1 {
        (start.1 as usize, end.1 as usize)
    } else {
        (end.1 as usize, start.1 as usize)
    };

    // Clamp to valid range
    let start_row = start_row.min(lines.len().saturating_sub(1));
    let end_row = end_row.min(lines.len().saturating_sub(1));

    if start_row > end_row {
        return None;
    }

    // Extract the selected lines
    let selected_lines: Vec<String> = lines[start_row..=end_row].to_vec();

    if selected_lines.is_empty() {
        return None;
    }

    Some(selected_lines.join("\n"))
}

/// Best-effort clipboard write. Enabled only with the `clipboard` feature
/// (`arboard`) on non-Android platforms; otherwise returns `false` so the
/// caller degrades to a hint.
#[cfg(all(feature = "clipboard", not(target_os = "android")))]
pub(super) fn copy_to_clipboard(text: &str) -> bool {
    match arboard::Clipboard::new() {
        Ok(mut cb) => cb.set_text(text).is_ok(),
        Err(_) => false,
    }
}

#[cfg(any(not(feature = "clipboard"), target_os = "android"))]
pub(super) fn copy_to_clipboard(_text: &str) -> bool {
    false
}

/// Best-effort clipboard text read for Ctrl+V paste. The optional `clipboard`
/// feature keeps headless builds free of platform clipboard dependencies;
/// on Android the OS owns clipboard access so this degrades to `None`.
#[cfg(all(feature = "clipboard", not(target_os = "android")))]
pub(super) fn read_clipboard_text() -> Option<String> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard.get_text().ok()
}

#[cfg(any(not(feature = "clipboard"), target_os = "android"))]
pub(super) fn read_clipboard_text() -> Option<String> {
    None
}

/// Read a clipboard bitmap and normalize it to PNG for the provider-neutral
/// `ImageContent` contract. The optional clipboard feature keeps headless
/// builds free of platform clipboard dependencies.
#[cfg(all(feature = "clipboard", not(target_os = "android")))]
pub(super) fn read_clipboard_image() -> Result<Option<rpi_ai::types::ImageContent>, String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(_) => return Ok(None),
    };
    let width =
        u32::try_from(image.width).map_err(|_| "clipboard image is too wide".to_string())?;
    let height =
        u32::try_from(image.height).map_err(|_| "clipboard image is too tall".to_string())?;
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return Err("clipboard image dimensions are outside the supported range".into());
    }
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer
            .write_image_data(&image.bytes)
            .map_err(|e| e.to_string())?;
    }
    Ok(Some(rpi_ai::types::ImageContent {
        kind: rpi_ai::types::ImageContentType,
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
        mime_type: "image/png".into(),
    }))
}

pub(super) fn add_image_preview(chat: &Arc<Container>, image: &rpi_ai::types::ImageContent) {
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&image.data) {
        let mut options = ImageOptions::default();
        options.width = Some(48);
        options.alt_text = Some("Attached image".into());
        chat.add_child(Arc::new(Image::from_data(bytes, options)));
        chat.add_child(Arc::new(Spacer::new(1)));
    }
}

#[cfg(any(not(feature = "clipboard"), target_os = "android"))]
pub(super) fn read_clipboard_image() -> Result<Option<rpi_ai::types::ImageContent>, String> {
    Ok(None)
}

/// Blocking fallback (no `event_rx`): render the final assistant text as a
/// single `AssistantMessageComponent`, mirroring the pre-streaming behavior.
/// `transformer` is the live assistant-markdown transformer (B5e); `None` is
/// the identity path. The blocking path only fires when `event_rx` is absent,
/// so it shares the same transformer the streaming path installs on its
/// components.
pub(super) fn add_assistant_message_blocking(
    container: &Arc<Container>,
    text: &str,
    transformer: Option<MarkdownTransformer>,
) {
    if text.is_empty() {
        return;
    }
    let msg = Arc::new(AssistantMessageComponent::new(
        AssistantMessageOptions::default(),
    ));
    if let Some(t) = &transformer {
        msg.set_markdown_transformer(Some(t.clone()));
    }
    msg.update_text(text);
    container.add_child(msg);
    container.add_child(Arc::new(Spacer::new(1)));
}
