//! TUI-based interactive mode for pi-cli.
//!
//! This module provides a full-screen terminal UI with:
//! - Scrollable transcript of messages
//! - Editor for input
//! - Keyboard shortcuts for navigation

use std::io::IsTerminal;
use std::sync::Arc;

use crossterm::event::{Event, KeyCode, KeyModifiers};

use rpi_ai::types::{AssistantMessage, Content};
use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use rpi_tui::{
    Container, Editor, EditorOptions, EditorStyle, Focusable, FollowMode,
    ProcessTerminal, ScrollView, ScrollViewOptions, Spacer, StackChild,
    StackEntry, Text, TuiAltScreen, VStack, TUI,
};

use crate::args::Args;

/// Extract the concatenated text content from an assistant message.
fn assistant_text(msg: &AssistantMessage) -> String {
    msg.content
        .iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect()
}

/// The exit code a run's outcome maps to.
fn outcome_exit_code(outcome: &HarnessRunOutcome) -> i32 {
    match outcome {
        HarnessRunOutcome::Failed { .. } | HarnessRunOutcome::Aborted { .. } => 1,
        _ => 0,
    }
}

/// TUI-based interactive mode.
pub async fn interactive_tui(
    harness: &AgentHarness,
    _args: &Args,
    initial: Option<String>,
    extra_messages: &[String],
) -> i32 {
    let lane: Arc<dyn AgentLane> = harness.lane("main");

    // Create terminal
    let terminal = Box::new(ProcessTerminal::new());

    // Create TUI
    let tui = Arc::new(TuiAltScreen::new(terminal, true, None));

    // Create transcript container
    let transcript = Arc::new(Container::new());
    transcript.add_child(Arc::new(Text::new("rpi interactive TUI", 1, 0)));
    transcript.add_child(Arc::new(Spacer::new(1)));
    transcript.add_child(Arc::new(Text::new("Type your message and press Enter to send.", 1, 0)));
    transcript.add_child(Arc::new(Text::new("Ctrl+C: Exit | Shift+Enter: Submit", 1, 0)));
    transcript.add_child(Arc::new(Spacer::new(1)));

    // Create scroll view for transcript
    let scroll_view = Arc::new(ScrollView::new(
        transcript.clone(),
        ScrollViewOptions {
            follow: FollowMode::End,
            primary: true,
            ..Default::default()
        },
    ));

    // Create editor
    let editor = Arc::new(Editor::new(
        EditorOptions {
            padding_x: 1,
            placeholder: Some("Type a message...".to_string()),
            ..Default::default()
        },
        EditorStyle::default(),
        Arc::new(rpi_tui::Keybindings::new()),
    ));

    // Clone for closures
    let transcript_clone = transcript.clone();
    let _scroll_view_clone = scroll_view.clone();
    let _tui_clone = tui.clone();
    let _lane_clone = lane.clone();
    let editor_for_closure = editor.clone();
    let tui_for_closure = tui.clone();

    // Set submit handler - for now we'll handle submission manually
    // (The editor's submit callback would need to spawn an async task)
    editor.on_submit(Arc::new(move |text: &str| {
        if text.trim().is_empty() {
            return;
        }

        // Add user message to transcript
        let user_msg = Arc::new(Text::new(format!("> {}", text), 1, 0));
        transcript_clone.add_child(user_msg);
        transcript_clone.add_child(Arc::new(Spacer::new(1)));
    }));

    // Create footer
    let footer = Arc::new(Text::new("Ctrl+C: Exit | Enter: New line | Shift+Enter: Send | Arrow keys: Navigate", 1, 0));

    // Create root layout
    let root = VStack::from_children(vec![
        StackChild::Entry(StackEntry::new(scroll_view.clone()).grow(1).min_size(1)),
        StackChild::Entry(StackEntry::new(Arc::new(create_dock(&editor)))),
        StackChild::Entry(StackEntry::new(footer)),
    ]);

    // Set layout and start
    tui.set_layout_root(Some(Arc::new(root)));
    tui.set_focus(Some(editor.clone()));
    editor.set_focused(true);
    tui.start();

    // Run initial prompts
    let mut prompts: Vec<String> = Vec::new();
    if let Some(init) = initial {
        prompts.push(init);
    }
    for m in extra_messages {
        prompts.push(m.clone());
    }

    // Track running state
    let running = Arc::new(std::sync::Mutex::new(true));
    let running_clone = running.clone();
    let _tui_clone2 = tui.clone();

    // Clone for event loop
    let event_editor = editor_for_closure.clone();
    let event_tui = tui_for_closure.clone();

    // Event loop
    tokio::task::spawn_blocking(move || {
        loop {
            if !*running_clone.lock().unwrap() {
                break;
            }

            if let Ok(Event::Key(key)) = crossterm::event::read() {
                // Check for Ctrl+C
                if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
                    *running_clone.lock().unwrap() = false;
                    break;
                }

                // Check for Ctrl+L (clear)
                if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('l') {
                    // Clear transcript (would need additional implementation)
                    continue;
                }

                // Forward to editor
                event_editor.handle_key(key);
                event_tui.request_render(false);
            }
        }
    });

    // Process initial prompts
    for prompt in prompts {
        if !*running.lock().unwrap() {
            break;
        }

        // Add user message to transcript
        let user_msg = Arc::new(Text::new(format!("> {}", prompt), 1, 0));
        transcript.add_child(user_msg);
        transcript.add_child(Arc::new(Spacer::new(1)));

        // Send to agent
        match lane.prompt_text(&prompt, Vec::new()).await {
            Ok(result) => {
                match &result.outcome {
                    HarnessRunOutcome::Completed { final_message, .. }
                    | HarnessRunOutcome::Aborted { final_message, .. } => {
                        let text = assistant_text(final_message);
                        if !text.is_empty() {
                            let assistant_msg = Arc::new(Text::new(text, 1, 0));
                            transcript.add_child(assistant_msg);
                            transcript.add_child(Arc::new(Spacer::new(1)));
                        }
                    }
                    HarnessRunOutcome::Failed { error, final_message, .. } => {
                        let err_msg = if let Some(m) = final_message {
                            if let Some(em) = &m.error_message {
                                format!("Error: {}", em)
                            } else {
                                format!("Error: {:?}", error)
                            }
                        } else {
                            format!("Error: {:?}", error)
                        };
                        let err_text = Arc::new(Text::new(err_msg, 1, 0));
                        transcript.add_child(err_text);
                        transcript.add_child(Arc::new(Spacer::new(1)));
                    }
                    HarnessRunOutcome::Suspended { .. } => {
                        let msg = Arc::new(Text::new("Run suspended - resume not supported", 1, 0));
                        transcript.add_child(msg);
                        transcript.add_child(Arc::new(Spacer::new(1)));
                    }
                }
                rebuild_layout(&tui, &scroll_view, &editor);
            }
            Err(e) => {
                let err_text = Arc::new(Text::new(format!("Prompt rejected: {}", e), 1, 0));
                transcript.add_child(err_text);
                transcript.add_child(Arc::new(Spacer::new(1)));
                rebuild_layout(&tui, &scroll_view, &editor);
            }
        }
    }

    // Wait until not running
    while *running.lock().unwrap() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Stop TUI
    tui.stop(Default::default());
    println!("\nGoodbye!");

    0
}

/// Create the dock (bottom area with editor).
fn create_dock(editor: &Arc<Editor>) -> Container {
    let dock = Container::new();
    dock.add_child(Arc::new(Spacer::new(1)));
    dock.add_child(editor.clone());
    dock
}

/// Rebuild the layout after content changes.
fn rebuild_layout(tui: &Arc<TuiAltScreen>, scroll_view: &Arc<ScrollView>, editor: &Arc<Editor>) {
    let footer = Arc::new(Text::new("Ctrl+C: Exit | Enter: New line | Shift+Enter: Send", 1, 0));

    let root = VStack::from_children(vec![
        StackChild::Entry(StackEntry::new(scroll_view.clone()).grow(1).min_size(1)),
        StackChild::Entry(StackEntry::new(Arc::new(create_dock(editor)))),
        StackChild::Entry(StackEntry::new(footer)),
    ]);

    tui.set_layout_root(Some(Arc::new(root)));
    tui.request_render(false);
}

/// Check if the terminal supports TUI mode.
pub fn is_tui_supported() -> bool {
    std::io::stdout().is_terminal()
}