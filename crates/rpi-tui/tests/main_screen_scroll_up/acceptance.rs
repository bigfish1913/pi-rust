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
fn assistant_rebuild_keeps_the_previous_complete_frame_visible() {
    use std::sync::mpsc;

    let term = VirtualTerminal::new(WIDTH, HEIGHT);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let root = Arc::new(Container::new());
    root.add_child(old_history());
    let assistant = Arc::new(AssistantMessageComponent::default());
    let previous_text = (0..40)
        .map(|i| format!("assistant-line-{i:03}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    assistant.update_blocks(&[
        AssistantBlock::Text("stable first block".into()),
        AssistantBlock::Text(previous_text.clone()),
    ]);
    root.add_child(assistant.clone());
    add_dock(&root);
    tui.set_layout_root(Some(root));
    tui.start_readerless();
    term.browse_history_at(10);
    let expected_document = document(&term);
    let expected_message = assistant.render(WIDTH);
    let expected_visible = term.visible();
    let erases_before = term.state.lock().unwrap().scrollback_erase_count;

    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    assistant.set_markdown_transformer(Some(Arc::new(move |text| {
        if text.contains("replacement-tail") {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }
        text.to_owned()
    })));
    let updating = assistant.clone();
    let update = std::thread::spawn(move || {
        updating.update_blocks(&[
            AssistantBlock::Text("stable first block".into()),
            AssistantBlock::Text(format!("{previous_text}\n\nreplacement-tail")),
        ]);
    });
    // A channel gates the expensive transformation at the exact point where
    // clear-then-add used to expose only the first block of a long assistant.
    // No race-inducing sleep.
    let reached_transformer = entered_rx.recv_timeout(Duration::from_secs(5));
    if reached_transformer.is_err() {
        let _ = release_tx.send(());
        update.join().unwrap();
        panic!("replacement did not reach the markdown transformer");
    }
    let during_message = assistant.render(WIDTH);
    tui.request_render(false);
    let during_document = document(&term);
    let during_visible = term.visible();
    let during_erases = term.state.lock().unwrap().scrollback_erase_count;
    release_tx.send(()).unwrap();
    update.join().unwrap();
    tui.request_render(false);

    assert_eq!(
        during_erases, erases_before,
        "rebuild erased native scrollback"
    );
    assert_eq!(
        during_message, expected_message,
        "partial assistant published"
    );
    assert_eq!(
        during_document, expected_document,
        "transient blank transcript painted"
    );
    assert_eq!(
        during_visible, expected_visible,
        "native history view jumped"
    );
    assert_eq!(term.state.lock().unwrap().history_view_top, Some(10));
    assert_history_once(&term);
    assert!(document(&term)
        .iter()
        .any(|row| row.contains("replacement-tail")));
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

        // Check every painted frame, not just a final frame that could have
        // recovered from a transient duplicate/overwrite during streaming.
        assert_eq!(
            term.state.lock().unwrap().scrollback_erase_count,
            erases_before
        );
        assert_eq!(term.state.lock().unwrap().history_view_top, Some(10));
        assert_eq!(term.visible(), visible_before);
        assert_history_once(&term);
        let rows = document(&term);
        for i in 0..lines {
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
}

struct CommandsWithCursorAboveTail {
    cursor_row: Mutex<usize>,
    cursor_at_start: bool,
}

impl Default for CommandsWithCursorAboveTail {
    fn default() -> Self {
        Self {
            cursor_row: Mutex::new(6),
            cursor_at_start: false,
        }
    }
}

impl Component for CommandsWithCursorAboveTail {
    fn render(&self, _width: usize) -> Vec<String> {
        (0..12)
            .map(|i| {
                let mut line = format!("command {i:02} complete");
                if i == *self.cursor_row.lock().unwrap() {
                    if self.cursor_at_start {
                        line.insert_str(0, CURSOR_MARKER);
                    } else {
                        line.push_str(CURSOR_MARKER);
                    }
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
    tui.set_layout_root(Some(Arc::new(CommandsWithCursorAboveTail::default())));
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

#[test]
fn offscreen_cursor_can_return_without_corrupting_the_physical_ledger() {
    let term = VirtualTerminal::new(40, 4);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let commands = Arc::new(CommandsWithCursorAboveTail::default());
    tui.set_layout_root(Some(commands.clone()));
    tui.start_readerless();
    let expected = document(&term);
    assert!(!term.state.lock().unwrap().cursor_visible);

    for row in [11, 6, 9] {
        *commands.cursor_row.lock().unwrap() = row;
        tui.request_render(false);
        assert_eq!(document(&term), expected);
        let st = term.state.lock().unwrap();
        assert_eq!(st.viewport_top, 8);
        if row >= st.viewport_top {
            assert!(st.cursor_visible);
            assert_eq!(st.cursor_row + st.viewport_top, row);
            assert_eq!(st.cursor_col, "command 00 complete".len());
        } else {
            assert!(!st.cursor_visible);
        }
    }
}

#[test]
fn column_zero_cursor_and_hidden_cursor_preference_are_respected() {
    for show_cursor in [true, false] {
        let term = VirtualTerminal::new(40, 4);
        let tui = TuiAltScreen::new(Box::new(term.clone()), show_cursor, None);
        tui.set_main_screen_mode(true);
        tui.set_layout_root(Some(Arc::new(CommandsWithCursorAboveTail {
            cursor_row: Mutex::new(11),
            cursor_at_start: true,
        })));
        tui.start_readerless();
        let st = term.state.lock().unwrap();
        assert_eq!(st.cursor_col, 0, "CSI 0 C would incorrectly move right");
        assert_eq!(st.cursor_visible, show_cursor);
    }
}

#[test]
fn forced_repaints_repair_live_rows_without_appending_or_erasing_history() {
    let term = VirtualTerminal::new(WIDTH, HEIGHT);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let root = Arc::new(Container::new());
    root.add_child(old_history());
    root.add_child(Arc::new(Text::new("complete tool command", 0, 0)));
    add_dock(&root);
    tui.set_layout_root(Some(root));
    tui.start_readerless();
    term.browse_history_at(10);
    let visible_before = term.visible();
    let expected = document(&term);
    let erases_before = term.state.lock().unwrap().scrollback_erase_count;
    for _ in 0..3 {
        // Simulate an externally damaged live cell without moving the physical
        // cursor. A forced repaint must repair it, not patch against equal rows.
        {
            let mut st = term.state.lock().unwrap();
            let row = st.viewport_top;
            st.document[row][0] = '?';
        }
        tui.render_now(true);
        assert_eq!(document(&term), expected);
        assert_eq!(
            term.state.lock().unwrap().scrollback_erase_count,
            erases_before
        );
        assert_eq!(term.state.lock().unwrap().history_view_top, Some(10));
        assert_eq!(term.visible(), visible_before);
        assert_history_once(&term);
    }
}

#[test]
fn forced_repaint_clears_line_suffixes_and_unused_live_rows() {
    let term = VirtualTerminal::new(40, 4);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let text = Arc::new(Text::new("short", 0, 0));
    tui.set_layout_root(Some(text.clone()));
    tui.start_readerless();
    let expected = document(&term);
    {
        let mut st = term.state.lock().unwrap();
        st.document[0][25] = '?';
        st.document[2][3] = '?';
    }
    tui.render_now(true);
    assert_eq!(document(&term), expected);
    assert_eq!(term.state.lock().unwrap().scrollback_erase_count, 0);

    // Subsequent appends must still start at the actual tail, not the blank
    // row visited while clearing the rest of the live screen.
    text.set_text("short\nnext command complete");
    tui.request_render(false);
    let rows = document(&term);
    assert_eq!(rows[0], "short");
    assert_eq!(rows[1], "next command complete");
    assert!(rows[2..].iter().all(String::is_empty));
}

/// Saved rows are immutable terminal snapshots, not a second copy of the
/// current logical document. Only the live tail must match root.render().
struct SavedSnapshots {
    rows: Vec<Vec<char>>,
    visible: Vec<String>,
}

impl SavedSnapshots {
    fn capture(term: &VirtualTerminal) -> Self {
        term.browse_history_at(10);
        let st = term.state.lock().unwrap();
        assert!(st.viewport_top > 10 + st.rows);
        let rows = st.document[..st.viewport_top].to_vec();
        drop(st);
        Self {
            rows,
            visible: term.visible(),
        }
    }

    fn assert_preserved(&self, term: &VirtualTerminal) {
        let st = term.state.lock().unwrap();
        assert_eq!(
            st.scrollback_erase_count, 0,
            "snapshot contract forbids ED3"
        );
        assert_eq!(st.history_view_top, Some(10), "browsing anchor moved");
        assert!(
            st.viewport_top >= self.rows.len(),
            "saved history was discarded"
        );
        assert_eq!(
            &st.document[..self.rows.len()],
            self.rows.as_slice(),
            "saved-history prefix must remain byte-identical, including padding"
        );
        drop(st);
        assert_eq!(term.visible(), self.visible, "native browsing view changed");
    }
}

fn assert_latest_live_tail(term: &VirtualTerminal, root: &dyn Component) {
    let canonical = rpi_tui::render::with_main_screen_frame(|| root.render(term.columns()));
    assert_live_rows(
        term,
        &canonical[canonical.len().saturating_sub(term.rows())..],
    );
}

fn assert_live_rows(term: &VirtualTerminal, expected: &[String]) {
    let height = term.rows();
    let mut expected: Vec<_> = expected
        .iter()
        .map(|row| rpi_tui::ansi::strip_ansi(row).trim_end().to_string())
        .collect();
    expected.resize(height, String::new());
    let st = term.state.lock().unwrap();
    let actual: Vec<_> = (st.viewport_top..st.viewport_top + height)
        .map(|i| {
            st.document
                .get(i)
                .map(|row| row.iter().collect::<String>())
                .unwrap_or_default()
                .trim_end()
                .to_string()
        })
        .collect();
    assert_eq!(
        actual, expected,
        "live tail must match the latest logical render exactly"
    );
}

fn assert_dock_once(term: &VirtualTerminal) {
    let rows = document(term);
    for marker in ["cargo test -p rpi-tui --locked", "FOOTER-STATUS"] {
        assert_eq!(
            rows.iter().filter(|row| row.contains(marker)).count(),
            1,
            "draft/footer missing or duplicated: {marker}"
        );
    }
}

fn task_rows(status: &str) -> Vec<(String, String)> {
    (0..30)
        .map(|i| (format!("snapshot-task-{i:02}"), status.to_string()))
        .collect()
}

fn plan_steps() -> String {
    (0..30)
        .map(|i| format!("- snapshot-step-{i:02}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn exercise_panel_snapshot_contract(is_plan: bool) {
    let term = VirtualTerminal::new(WIDTH, HEIGHT);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let root = Arc::new(Container::new());
    root.add_child(old_history());
    let tool = Arc::new(ToolExecutionComponent::new(
        if is_plan { "plan" } else { "todo" },
        "{}",
    ));
    if is_plan {
        tool.set_plan_state(plan_steps(), true);
    } else {
        tool.set_todo_state(task_rows("pending"));
    }
    tool.set_expanded(true);
    root.add_child(tool.clone());
    let stream = Arc::new(Text::new("", 0, 0));
    root.add_child(stream.clone());
    add_dock(&root);
    tui.set_layout_root(Some(root.clone()));
    tui.start_readerless();
    assert_latest_live_tail(&term, root.as_ref());
    let saved = SavedSnapshots::capture(&term);
    if is_plan {
        tool.set_plan_state(plan_steps(), false);
    } else {
        tool.set_todo_state(task_rows("completed"));
    }
    for expanded in [true, false, true] {
        tool.set_expanded(expanded);
        tui.request_render(false);
        saved.assert_preserved(&term);
        if expanded {
            assert_latest_live_tail(&term, root.as_ref());
        } else {
            // Collapsing the live body must not pull already-saved old message
            // rows back into the viewport as duplicates. Only the compact
            // current panel and dock remain, with cleared rows below them.
            let canonical = rpi_tui::render::with_main_screen_frame(|| root.render(WIDTH));
            assert_live_rows(&term, &canonical[HISTORY_LINES..]);
        }
        assert_history_once(&term);
        assert_dock_once(&term);
    }
    stream.set_text("normal-append-after-panel-update\nsecond-panel-append");
    tui.request_render(false);
    saved.assert_preserved(&term);
    assert_latest_live_tail(&term, root.as_ref());
    assert_history_once(&term);
    assert_dock_once(&term);
    for marker in ["normal-append-after-panel-update", "second-panel-append"] {
        assert_eq!(
            document(&term)
                .iter()
                .filter(|row| row.contains(marker))
                .count(),
            1
        );
    }
}

#[test]
fn snapshot_contract_todo_completion_and_expand_preserve_saved_rows_then_append() {
    exercise_panel_snapshot_contract(false);
}

#[test]
fn snapshot_contract_plan_ready_collapse_expand_preserve_saved_rows_then_append() {
    exercise_panel_snapshot_contract(true);
}

#[test]
fn snapshot_contract_late_startup_notice_and_clearing_reconcile_only_live_tail() {
    let term = VirtualTerminal::new(WIDTH, HEIGHT);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let root = Arc::new(Container::new());
    let notice = Arc::new(Container::new());
    root.add_child(notice.clone());
    root.add_child(old_history());
    let stream = Arc::new(Text::new("initial-output", 0, 0));
    root.add_child(stream.clone());
    add_dock(&root);
    tui.set_layout_root(Some(root.clone()));
    tui.start_readerless();
    assert_history_once(&term);
    let saved = SavedSnapshots::capture(&term);
    for message in ["startup-update-now-available\nstartup-update-details", ""] {
        notice.clear();
        if !message.is_empty() {
            notice.add_child(Arc::new(Text::new(message, 0, 0)));
        }
        tui.request_render(false);
        saved.assert_preserved(&term);
        assert_latest_live_tail(&term, root.as_ref());
        assert_history_once(&term);
        assert_dock_once(&term);
    }
    stream.set_text("");
    tui.request_render(false);
    saved.assert_preserved(&term);
    assert_latest_live_tail(&term, root.as_ref());
    assert_dock_once(&term);
    assert!(!document(&term)
        .iter()
        .any(|row| row.contains("initial-output")));
    stream.set_text("initial-output\nnormal-output-after-notice-clear");
    tui.request_render(false);
    saved.assert_preserved(&term);
    assert_latest_live_tail(&term, root.as_ref());
    assert_history_once(&term);
    assert_dock_once(&term);
    assert_eq!(
        document(&term)
            .iter()
            .filter(|row| row.contains("normal-output-after-notice-clear"))
            .count(),
        1
    );
}

#[test]
fn snapshot_contract_logical_prefix_page_eviction_does_not_rewrite_saved_history() {
    let term = VirtualTerminal::new(WIDTH, HEIGHT);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let root = Arc::new(Container::new());
    let transcript = old_history();
    root.add_child(transcript.clone());
    add_dock(&root);
    tui.set_layout_root(Some(root.clone()));
    tui.start_readerless();
    let saved = SavedSnapshots::capture(&term);
    for first in [20, 40, 60] {
        transcript.set_text(
            &(first..HISTORY_LINES)
                .map(|i| format!("old-message-{i:03}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        tui.request_render(false);
        saved.assert_preserved(&term);
        assert_latest_live_tail(&term, root.as_ref());
        assert_dock_once(&term);
    }
    // Dropped logical pages remain readable as saved snapshots, not rewritten
    // to resemble the current root. Do not require canonical historic rows.
    assert_eq!(document(&term)[0], "old-message-000");
}

#[test]
fn snapshot_contract_shrink_to_empty_and_regrowth_never_replays_old_history() {
    let term = VirtualTerminal::new(WIDTH, HEIGHT);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let root = Arc::new(Container::new());
    root.add_child(old_history());
    add_dock(&root);
    tui.set_layout_root(Some(root.clone()));
    tui.start_readerless();
    let saved = SavedSnapshots::capture(&term);
    root.clear();
    tui.request_render(false);
    saved.assert_preserved(&term);
    assert_latest_live_tail(&term, root.as_ref());
    for lines in [2, 40] {
        root.clear();
        root.add_child(Arc::new(Text::new(&output(lines), 0, 0)));
        add_dock(&root);
        tui.request_render(false);
        saved.assert_preserved(&term);
        assert_latest_live_tail(&term, root.as_ref());
        assert_dock_once(&term);
        assert_eq!(
            document(&term)
                .iter()
                .filter(|row| row.contains("old-message-000"))
                .count(),
            1
        );
    }
}

#[test]
fn snapshot_contract_historic_edit_then_normal_and_massive_append_loses_no_output() {
    let term = VirtualTerminal::new(WIDTH, HEIGHT);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let root = Arc::new(Container::new());
    root.add_child(old_history());
    let stream = Arc::new(Text::new(&output(40), 0, 0));
    root.add_child(stream.clone());
    add_dock(&root);
    tui.set_layout_root(Some(root.clone()));
    tui.start_readerless();
    let saved = SavedSnapshots::capture(&term);
    // The first output row is saved, not addressable by live-screen CSI.
    let edited = output(40).replacen("tool-output-000", "historic-output-edited", 1);
    stream.set_text(&edited);
    tui.request_render(false);
    saved.assert_preserved(&term);
    assert_latest_live_tail(&term, root.as_ref());
    for count in [1, 401] {
        let appended = (0..count)
            .map(|i| format!("post-reconcile-output-{i:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        stream.set_text(&format!("{edited}\n{appended}"));
        tui.request_render(false);
        saved.assert_preserved(&term);
        assert_latest_live_tail(&term, root.as_ref());
        assert_history_once(&term);
        assert_dock_once(&term);
        let rows = document(&term);
        for i in 0..count {
            let marker = format!("post-reconcile-output-{i:03}");
            assert_eq!(
                rows.iter().filter(|row| row.contains(&marker)).count(),
                1,
                "safe growth after reconciliation lost or duplicated {marker}"
            );
        }
    }
    // The old physical version explicitly survives the historic logical edit.
    assert_eq!(document(&term)[HISTORY_LINES], "tool-output-000");
}

#[test]
fn snapshot_contract_same_grid_resize_repairs_live_damage_without_history_replay() {
    let term = VirtualTerminal::new(WIDTH, HEIGHT);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let root = Arc::new(Container::new());
    root.add_child(old_history());
    add_dock(&root);
    tui.set_layout_root(Some(root.clone()));
    tui.start_readerless();
    let saved = SavedSnapshots::capture(&term);
    for _ in 0..2 {
        {
            let mut st = term.state.lock().unwrap();
            let top = st.viewport_top;
            st.document[top][0] = '?';
        }
        // A geometry notification at the same grid still invalidates cells,
        // but must not replay the initial full paint into native history.
        tui.note_resize();
        tui.request_render(false);
        saved.assert_preserved(&term);
        assert_latest_live_tail(&term, root.as_ref());
        assert_history_once(&term);
        assert_dock_once(&term);
    }
}

#[test]
fn snapshot_contract_short_document_clear_and_regrow_keeps_one_live_dock() {
    // Without saved history, clearing a tool/output slot must clear stale rows
    // too. This is the small-document counterpart of tail-only reconciliation.
    let term = VirtualTerminal::new(WIDTH, HEIGHT);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let root = Arc::new(Container::new());
    let text = Arc::new(Text::new(
        "short-output-0\nshort-output-1\nshort-output-2",
        0,
        0,
    ));
    root.add_child(text.clone());
    add_dock(&root);
    tui.set_layout_root(Some(root.clone()));
    tui.start_readerless();
    for value in ["", "short-regrowth", "short-regrowth\nnormal-short-append"] {
        text.set_text(value);
        tui.request_render(false);
        assert_latest_live_tail(&term, root.as_ref());
        assert_dock_once(&term);
        assert_eq!(term.state.lock().unwrap().scrollback_erase_count, 0);
        assert_eq!(term.state.lock().unwrap().viewport_top, 0);
        assert!(!document(&term)
            .iter()
            .any(|row| row.contains("short-output-")));
        // Force repaint repairs unknown live cells without treating this small
        // frame as a new initial paint or appending a second draft/footer.
        tui.render_now(true);
        assert_latest_live_tail(&term, root.as_ref());
        assert_dock_once(&term);
    }
}

#[test]
fn ambiguous_history_anchor_never_discards_unpublished_output() {
    for force_repaint in [false, true] {
        let term = VirtualTerminal::new(80, 8);
        let tui = TuiAltScreen::new(Box::new(term.clone()), false, None);
        tui.set_main_screen_mode(true);
        let mut rows = vec!["repeated-log".to_string(); 100];
        let text = Arc::new(Text::new(rows.join("\n"), 0, 0));
        tui.set_layout_root(Some(text.clone()));
        tui.start_readerless();
        let saved = SavedSnapshots::capture(&term);
        rows[0] = "edited-old-log".into();
        rows.extend((0..401).map(|i| format!("unpublished-output-{i:03}")));
        text.set_text(rows.join("\n"));
        if force_repaint {
            tui.render_now(true);
        } else {
            tui.request_render(false);
        }
        saved.assert_preserved(&term);
        assert_latest_live_tail(&term, text.as_ref());
        let actual = document(&term);
        for i in 0..401 {
            let marker = format!("unpublished-output-{i:03}");
            assert_eq!(
                actual.iter().filter(|row| **row == marker).count(),
                1,
                "{marker}"
            );
        }
        assert_eq!(term.state.lock().unwrap().viewport_top, 493);
        // The reconciled frame must become the next publication baseline.
        tui.request_render(false);
        assert_eq!(document(&term), actual);
    }
}

#[test]
fn equal_length_edits_never_republish_saved_history() {
    let term = VirtualTerminal::new(80, 8);
    term.state.lock().unwrap().scrollback_limit = Some(128);
    let tui = TuiAltScreen::new(Box::new(term.clone()), false, None);
    tui.set_main_screen_mode(true);
    let mut rows = (0..100)
        .map(|i| format!("history-{i:03}"))
        .collect::<Vec<_>>();
    let text = Arc::new(Text::new(rows.join("\n"), 0, 0));
    tui.set_layout_root(Some(text.clone()));
    tui.start_readerless();
    let saved = SavedSnapshots::capture(&term);
    // Break the live anchor and the last saved block, while leaving earlier
    // unique saved blocks intact. Their cutoff is NOT the publication frontier.
    for revision in 1..=40 {
        rows[91] = format!("edited-saved-{revision:03}");
        rows[92] = format!("edited-live-{revision:03}");
        rows[99] = format!("edited-tail-{revision:03}");
        text.set_text(rows.join("\n"));
        tui.request_render(false);
        saved.assert_preserved(&term);
        assert_latest_live_tail(&term, text.as_ref());
        assert_eq!(
            term.state.lock().unwrap().viewport_top,
            92,
            "revision {revision}"
        );
        assert_eq!(document(&term).len(), 100);
    }
}

#[test]
fn snapshot_contract_rewrap_and_large_append_in_one_frame_keeps_every_new_row() {
    let term = VirtualTerminal::new(WIDTH, HEIGHT);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let root = Arc::new(Container::new());
    let history = old_history();
    root.add_child(history.clone());
    let stream = Arc::new(Text::new(&output(40), 0, 0));
    root.add_child(stream.clone());
    add_dock(&root);
    tui.set_layout_root(Some(root.clone()));
    tui.start_readerless();
    let saved = SavedSnapshots::capture(&term);
    // An earlier Markdown reflow and a large output batch can be coalesced in
    // the same scheduled frame. Tail-only clipping would lose most new rows.
    history.set_text(&format!(
        "late-historical-insertion\n{}",
        old_history().get_text()
    ));
    let appended = (0..401)
        .map(|i| format!("coalesced-new-row-{i:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    stream.set_text(&format!("{}\n{appended}", output(40)));
    tui.request_render(false);
    saved.assert_preserved(&term);
    assert_latest_live_tail(&term, root.as_ref());
    assert_history_once(&term);
    assert_dock_once(&term);
    let rows = document(&term);
    for i in 0..401 {
        let marker = format!("coalesced-new-row-{i:03}");
        assert_eq!(
            rows.iter().filter(|row| row.contains(&marker)).count(),
            1,
            "{marker}"
        );
    }
}

#[test]
fn snapshot_contract_removing_a_large_panel_never_replays_saved_message_rows() {
    let term = VirtualTerminal::new(WIDTH, HEIGHT);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let root = Arc::new(Container::new());
    root.add_child(old_history());
    let panel_slot = Arc::new(Container::new());
    let tool = Arc::new(ToolExecutionComponent::new("todo", "{}"));
    tool.set_todo_state(task_rows("pending"));
    tool.set_expanded(true);
    panel_slot.add_child(tool);
    root.add_child(panel_slot.clone());
    let stream = Arc::new(Text::new("", 0, 0));
    root.add_child(stream.clone());
    add_dock(&root);
    tui.set_layout_root(Some(root.clone()));
    tui.start_readerless();
    let saved = SavedSnapshots::capture(&term);
    panel_slot.clear();
    tui.request_render(false);
    saved.assert_preserved(&term);
    assert_history_once(&term);
    assert_dock_once(&term);
    let canonical = rpi_tui::render::with_main_screen_frame(|| root.render(WIDTH));
    assert_live_rows(&term, &canonical[HISTORY_LINES..]);
    stream.set_text("first-output-after-panel-removal\nsecond-output-after-panel-removal");
    tui.request_render(false);
    saved.assert_preserved(&term);
    assert_history_once(&term);
    assert_dock_once(&term);
    let canonical = rpi_tui::render::with_main_screen_frame(|| root.render(WIDTH));
    assert_live_rows(&term, &canonical[HISTORY_LINES..]);
}

#[test]
fn compact_current_panels_keep_latest_status_output_and_editor_together() {
    let term = VirtualTerminal::new(WIDTH, HEIGHT);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let root = Arc::new(Container::new());
    root.add_child(old_history());
    let stream = Arc::new(Text::new(&output(40), 0, 0));
    root.add_child(stream.clone());
    let todo = Arc::new(ToolExecutionComponent::new("todo", "{}"));
    todo.set_todo_state(task_rows("pending"));
    root.add_child(todo.clone());
    let plan = Arc::new(ToolExecutionComponent::new("plan", "{}"));
    plan.set_plan_state(plan_steps(), true);
    root.add_child(plan.clone());
    add_dock(&root);
    tui.set_layout_root(Some(root));
    tui.start_readerless();
    let saved = SavedSnapshots::capture(&term);
    for count in [41, 42, 100] {
        if count == 42 {
            todo.set_todo_state(task_rows("completed"));
            plan.set_plan_state(plan_steps(), false);
        }
        stream.set_text(&output(count));
        tui.request_render(false);
        saved.assert_preserved(&term);
        assert_history_once(&term);
        assert_dock_once(&term);
        let st = term.state.lock().unwrap();
        let live = st.document[st.viewport_top..]
            .iter()
            .map(|row| row.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            live.contains(&format!("tool-output-{:03}", count - 1)),
            "{live}"
        );
        assert!(
            live.contains(if count < 42 {
                "Plan · Planning"
            } else {
                "Plan · Ready"
            }),
            "{live}"
        );
        assert!(
            live.contains(if count < 42 {
                "0/30 completed"
            } else {
                "30/30 completed"
            }),
            "{live}"
        );
        assert!(live.contains("cargo test -p rpi-tui --locked"), "{live}");
        assert!(live.contains("FOOTER-STATUS"), "{live}");
        drop(st);
        let rows = document(&term);
        for i in 0..count {
            let marker = format!("tool-output-{i:03}");
            assert_eq!(
                rows.iter().filter(|row| row.contains(&marker)).count(),
                1,
                "{marker}"
            );
        }
    }
}

#[test]
fn repaint_coalesced_with_growth_publishes_all_novel_rows_before_the_latest_tail() {
    for resize in [false, true] {
        let term = VirtualTerminal::new(WIDTH, HEIGHT);
        let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
        tui.set_main_screen_mode(true);
        let root = Arc::new(Container::new());
        root.add_child(old_history());
        let stream = Arc::new(Text::new(&output(40), 0, 0));
        root.add_child(stream.clone());
        add_dock(&root);
        tui.set_layout_root(Some(root.clone()));
        tui.start_readerless();
        let saved = SavedSnapshots::capture(&term);
        stream.set_text(&output(140));
        if resize {
            tui.note_resize();
        } else {
            tui.render_now(true);
        }
        saved.assert_preserved(&term);
        assert_latest_live_tail(&term, root.as_ref());
        assert_history_once(&term);
        assert_dock_once(&term);
        let rows = document(&term);
        for i in 0..140 {
            let marker = format!("tool-output-{i:03}");
            assert_eq!(
                rows.iter().filter(|row| row.contains(&marker)).count(),
                1,
                "coalesced resize={resize} lost/duplicated {marker}"
            );
        }
    }
}

#[test]
fn historical_edit_and_large_shrink_do_not_reintroduce_saved_messages() {
    let term = VirtualTerminal::new(WIDTH, HEIGHT);
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let root = Arc::new(Container::new());
    let history = old_history();
    root.add_child(history.clone());
    let stream = Arc::new(Text::new(&output(40), 0, 0));
    root.add_child(stream.clone());
    add_dock(&root);
    tui.set_layout_root(Some(root.clone()));
    tui.start_readerless();
    let saved = SavedSnapshots::capture(&term);
    history.set_text(
        &history
            .get_text()
            .replacen("old-message-000", "historic-row-edited", 1),
    );
    stream.set_text("");
    tui.request_render(false);
    saved.assert_preserved(&term);
    assert_history_once(&term);
    assert_dock_once(&term);
    let canonical = rpi_tui::render::with_main_screen_frame(|| root.render(WIDTH));
    assert_live_rows(&term, &canonical[HISTORY_LINES..]);
    stream.set_text("novel-output-after-shrink");
    tui.request_render(false);
    saved.assert_preserved(&term);
    assert_history_once(&term);
    assert_dock_once(&term);
    assert_eq!(
        document(&term)
            .iter()
            .filter(|row| row.contains("novel-output-after-shrink"))
            .count(),
        1
    );
}
