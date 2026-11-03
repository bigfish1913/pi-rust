//! Acceptance contracts for the macOS/iTerm2 native-scrollback report.
//!
//! These are protocol/model tests, not an iTerm2 visual test. The timer fixture
//! uses a real tool panel and bounded waits for an observable elapsed-label change.
//! Run `cargo test -p rpi-tui --test main_screen_scroll_up acceptance::`.
//! Keep failing contracts visible: do not ignore them or assert the broken output.

use super::*;
use rpi_tui::{ansi::CURSOR_MARKER, BashExecutionComponent, Component, ToolExecutionComponent};
use std::time::{Duration, Instant};

const WIDTH: usize = 80;
const HEIGHT: usize = 12;
const HISTORY_LINES: usize = 80;

fn old_history() -> Arc<Text> {
    Arc::new(Text::new(
        &(0..HISTORY_LINES)
            .map(|i| format!("old-message-{i:03}"))
            .collect::<Vec<_>>()
            .join("\n"),
        0,
        0,
    ))
}

fn output(lines: usize) -> String {
    (0..lines)
        .map(|i| format!("tool-output-{i:03}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn add_dock(root: &Container) {
    let editor = Arc::new(rpi_tui::Editor::simple());
    editor.set_text("cargo test -p rpi-tui --locked");
    editor.set_focused(true);
    root.add_child(editor);
    root.add_child(Arc::new(Text::new("FOOTER-STATUS", 0, 0)));
}

fn document(term: &VirtualTerminal) -> Vec<String> {
    term.state
        .lock()
        .unwrap()
        .document
        .iter()
        .map(|row| row.iter().collect::<String>().trim_end().to_string())
        .collect()
}

fn assert_history_once(term: &VirtualTerminal) {
    let rows = document(term);
    for i in 0..HISTORY_LINES {
        let message = format!("old-message-{i:03}");
        assert_eq!(rows[i], message, "history row {i} moved or was overwritten");
        assert_eq!(
            rows.iter().filter(|row| *row == &message).count(),
            1,
            "history row {i} was duplicated"
        );
    }
}

#[test]
fn timer_only_updates_preserve_native_history_without_erasing_scrollback() {
    // e59a6bb added changed_in_scrollback -> ED(3) for this case. A changing
    // elapsed label is not new transcript content and must not destroy the
    // native history that the user is currently reading.
    let term = VirtualTerminal::new(WIDTH, HEIGHT);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let root = Arc::new(Container::new());
    root.add_child(old_history());
    let tool = Arc::new(ToolExecutionComponent::new(
        "read",
        r#"{"path":"large-file.rs"}"#,
    ));
    tool.set_result(&output(40), false);
    tool.set_expanded(true);
    tool.set_running();
    root.add_child(tool.clone());
    add_dock(&root);
    tui.set_layout_root(Some(root));
    tui.start_readerless();

    assert!(term.state.lock().unwrap().viewport_top > HISTORY_LINES);
    term.browse_history_at(10);
    let visible_before = term.visible();
    let erases_before = term.state.lock().unwrap().scrollback_erase_count;
    let mut erase_deltas = Vec::new();
    let mut previous_tool = tool.render(WIDTH);
    for _ in 0..3 {
        // Do not assume a fixed sleep advances the formatted timer: wait for
        // the real component to change, with a deadline so a frozen timer fails.
        // No tool output, arguments, status or draft changes during this loop.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let rendered = tool.render(WIDTH);
            if rendered != previous_tool {
                previous_tool = rendered;
                break;
            }
            assert!(Instant::now() < deadline, "live tool timer did not advance");
            std::thread::sleep(Duration::from_millis(5));
        }
        tui.request_render(false);
        erase_deltas.push(term.state.lock().unwrap().scrollback_erase_count - erases_before);
    }

    assert_eq!(
        erase_deltas,
        vec![0, 0, 0],
        "timer-only refreshes must not erase native scrollback (ED 3)"
    );
    assert_eq!(term.state.lock().unwrap().history_view_top, Some(10));
    assert_eq!(term.visible(), visible_before, "native history view jumped");
    assert_history_once(&term);
}

#[test]
fn streamed_output_growth_preserves_history_commands_and_draft() {
    // Positive control: native browsing must not prevent real output growth,
    // duplicate an already-written message, or truncate either command line.
    let command = "printf 'streamed-command-marker'";
    let term = VirtualTerminal::new(WIDTH, HEIGHT);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let root = Arc::new(Container::new());
    root.add_child(old_history());
    let bash = Arc::new(BashExecutionComponent::new(command));
    bash.set_expanded(true);
    bash.append_output(&output(40));
    root.add_child(bash.clone());
    add_dock(&root);
    tui.set_layout_root(Some(root));
    tui.start_readerless();

    term.browse_history_at(10);
    let visible_before = term.visible();
    let erases_before = term.state.lock().unwrap().scrollback_erase_count;
    for lines in 41..=43 {
        bash.append_output(&output(lines));
        tui.request_render(false);
    }

    assert_eq!(
        term.state.lock().unwrap().scrollback_erase_count,
        erases_before
    );
    assert_eq!(term.state.lock().unwrap().history_view_top, Some(10));
    assert_eq!(term.visible(), visible_before);
    assert_history_once(&term);
    let rows = document(&term);
    for i in 0..43 {
        let line = format!("tool-output-{i:03}");
        assert_eq!(
            rows.iter().filter(|row| row.contains(&line)).count(),
            1,
            "streamed output missing or duplicated: {line}"
        );
    }
    for line in [command, "cargo test -p rpi-tui --locked", "FOOTER-STATUS"] {
        assert_eq!(
            rows.iter().filter(|row| row.contains(line)).count(),
            1,
            "command/draft/footer missing, truncated or duplicated: {line}"
        );
    }
}

struct CommandsWithCursorAboveTail;

impl Component for CommandsWithCursorAboveTail {
    fn render(&self, _width: usize) -> Vec<String> {
        (0..12)
            .map(|i| {
                let mut line = format!("command {i:02} complete");
                if i == 6 {
                    line.push_str(CURSOR_MARKER);
                }
                line
            })
            .collect()
    }

    fn invalidate(&self) {}

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[test]
fn cursor_above_live_viewport_does_not_duplicate_or_drop_command_rows() {
    // af296aa / PR #33 added the cursor-above-viewport CSI T branch. SD inserts
    // blanks in the live screen; it cannot retrieve saved document rows.
    let term = VirtualTerminal::new(40, 4);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    tui.set_layout_root(Some(Arc::new(CommandsWithCursorAboveTail)));
    tui.start_readerless();

    let expected: Vec<_> = (0..12)
        .map(|i| format!("command {i:02} complete"))
        .collect();
    assert_eq!(document(&term), expected);
    tui.request_render(false);
    assert_eq!(
        document(&term),
        expected,
        "unchanged repaint changed history"
    );
}
