//! Interactive TUI example demonstrating the pi-tui library.
//!
//! This example creates an interactive TUI with:
//! - A scrollable transcript area
//! - An editor for input
//! - Keyboard navigation

use std::io::IsTerminal;
use std::sync::Arc;

use crossterm::event::{Event, KeyCode, KeyModifiers};

use rpi_tui::{
    Container, Editor, EditorOptions, EditorStyle, Focusable, FollowMode, ProcessTerminal,
    ScrollView, ScrollViewOptions, Spacer, StackChild, StackEntry,
    Text, TuiAltScreen, VStack, TUI,
};

#[tokio::main]
async fn main() {
    if !std::io::stdout().is_terminal() {
        println!("This example requires a terminal. Please run in a real terminal.");
        return;
    }

    println!("pi-tui Interactive Example");
    println!("===========================");
    println!();
    println!("Controls:");
    println!("  - Type to enter text in the editor");
    println!("  - Enter: Send message (appears in transcript)");
    println!("  - Shift+Enter: Clear and submit");
    println!("  - Arrow keys: Navigate in editor");
    println!("  - Ctrl+C: Exit");
    println!();
    println!("Press any key to start...");

    // Wait for a key press
    let _ = crossterm::event::read();

    run_interactive().await;
}

async fn run_interactive() {
    // Create terminal
    let terminal = Box::new(ProcessTerminal::new());

    // Create TUI
    let tui = Arc::new(TuiAltScreen::new(terminal, true, None));

    // Create transcript container
    let transcript = Arc::new(Container::new());
    transcript.add_child(Arc::new(Text::new("Welcome to pi-tui Interactive Demo", 1, 0)));
    transcript.add_child(Arc::new(Spacer::new(1)));
    transcript.add_child(Arc::new(Text::new("Your messages will appear here:", 1, 0)));
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
        Arc::new(rpi_tui::keybindings::Keybindings::new()),
    ));

    // Clone for closure
    let transcript_clone = transcript.clone();
    let scroll_view_clone = scroll_view.clone();
    let tui_clone = tui.clone();

    // Set submit handler
    let editor_weak = Arc::downgrade(&editor);
    editor.on_submit(Arc::new(move |text: &str| {
        if text.trim().is_empty() {
            return;
        }

        // Add message to transcript
        let msg = Arc::new(Text::new(format!("> {}", text), 1, 0));
        transcript_clone.add_child(msg);
        transcript_clone.add_child(Arc::new(Spacer::new(1)));

        // Add a simulated response
        let response = format!("You said: \"{}\"", text);
        let resp = Arc::new(Text::new(response, 1, 0));
        transcript_clone.add_child(resp);
        transcript_clone.add_child(Arc::new(Spacer::new(1)));

        // Re-set layout root to trigger re-render
        if let Some(tui) = editor_weak.upgrade().and_then(|_| Some(tui_clone.clone())) {
            // Create new layout with updated content
            let dock = Container::new();
            dock.add_child(Arc::new(Spacer::new(1)));
            if let Some(e) = editor_weak.upgrade() {
                dock.add_child(e);
            }

            let root = VStack::from_children(vec![
                StackChild::Entry(StackEntry::new(scroll_view_clone.clone()).grow(1).min_size(1)),
                StackChild::Entry(StackEntry::new(Arc::new(dock))),
            ]);

            tui.set_layout_root(Some(Arc::new(root)));
        }
    }));

    // Create dock (bottom area with editor)
    let dock = Container::new();
    dock.add_child(Arc::new(Spacer::new(1)));
    dock.add_child(editor.clone());

    // Create footer
    let footer = Arc::new(Text::new("Ctrl+C: Exit | Enter: Send | Arrow keys: Navigate", 1, 0));

    // Create root layout
    let root = VStack::from_children(vec![
        StackChild::Entry(StackEntry::new(scroll_view.clone()).grow(1).min_size(1)),
        StackChild::Entry(StackEntry::new(Arc::new(dock))),
        StackChild::Entry(StackEntry::new(footer)),
    ]);

    // Set layout and start
    tui.set_layout_root(Some(Arc::new(root)));
    tui.set_focus(Some(editor.clone()));
    editor.set_focused(true);
    tui.start();

    // Event loop
    let running = Arc::new(std::sync::Mutex::new(true));
    let running_clone = running.clone();
    let tui_clone = tui.clone();
    let editor_clone = editor.clone();

    // Spawn input handling thread
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

                // Forward to editor
                editor_clone.handle_key(key);
                tui_clone.request_render(false);
            }
        }
    });

    // Wait until not running
    while *running.lock().unwrap() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Stop TUI
    tui.stop(Default::default());
    println!("\nGoodbye!");
}