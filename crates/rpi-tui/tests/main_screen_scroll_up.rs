//! Regression test for main-screen mode: when a streamed message re-wraps
//! *above* the visible viewport, the differential renderer must scroll up to
//! the changed range instead of painting the transcript over the bottom dock.
//!
//! The test drives `TuiAltScreen` through a tiny virtual terminal that models
//! the main buffer (document + viewport) so we can assert what is actually
//! visible after a long-transcript streaming update.

use std::sync::{Arc, Mutex};

use rpi_tui::{Container, Focusable, Text, TuiAltScreen, TUI};

/// Shared virtual-terminal state so the test can keep a handle to the same
/// screen the `TuiAltScreen` writes into.
#[derive(Default)]
struct VtState {
    columns: usize,
    rows: usize,
    /// The full main-buffer document (grows as content is written).
    document: Vec<Vec<char>>,
    /// Document index currently at the top of the screen.
    viewport_top: usize,
    cursor_row: usize,
    cursor_col: usize,
}

#[derive(Clone)]
struct VirtualTerminal {
    state: Arc<Mutex<VtState>>,
}

impl VirtualTerminal {
    fn new(columns: usize, rows: usize) -> Self {
        let state = VtState {
            columns,
            rows,
            document: vec![vec![' '; columns]; rows],
            viewport_top: 0,
            cursor_row: 0,
            cursor_col: 0,
        };
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    fn columns(&self) -> usize {
        self.state.lock().unwrap().columns
    }

    fn rows(&self) -> usize {
        self.state.lock().unwrap().rows
    }

    fn ensure_row(&self, st: &mut VtState, index: usize) {
        while st.document.len() <= index {
            st.document.push(vec![' '; st.columns]);
        }
    }

    /// Visible screen as strings (top to bottom).
    fn visible(&self) -> Vec<String> {
        let st = self.state.lock().unwrap();
        (st.viewport_top..st.viewport_top + st.rows)
            .map(|i| {
                st.document
                    .get(i)
                    .map(|row| row.iter().collect())
                    .unwrap_or_default()
            })
            .collect()
    }

    /// A single document (scrollback) row as a string.
    fn document_row(&self, index: usize) -> String {
        let st = self.state.lock().unwrap();
        st.document
            .get(index)
            .map(|row| row.iter().collect())
            .unwrap_or_default()
    }

    fn put_char(&self, ch: char) {
        let mut st = self.state.lock().unwrap();
        // Wrap only when the cursor is *past* the right edge (the character
        // written at the last column must not wrap until the next one).
        if st.cursor_col >= st.columns {
            st.cursor_col = 0;
            st.cursor_row += 1;
            if st.cursor_row >= st.rows {
                st.cursor_row = st.rows - 1;
                st.viewport_top += 1;
            }
        }
        let index = st.viewport_top + st.cursor_row;
        self.ensure_row(&mut st, index);
        let col = st.cursor_col.min(st.columns - 1);
        st.document[index][col] = ch;
        st.cursor_col += 1;
    }

    fn linefeed(&self) {
        let mut st = self.state.lock().unwrap();
        st.cursor_row += 1;
        if st.cursor_row >= st.rows {
            st.cursor_row = st.rows - 1;
            st.viewport_top += 1;
        }
    }

    fn carriage_return(&self) {
        self.state.lock().unwrap().cursor_col = 0;
    }

    fn scroll_down(&self, n: usize) {
        let mut st = self.state.lock().unwrap();
        st.viewport_top = st.viewport_top.saturating_sub(n);
    }

    fn feed(&self, data: &str) {
        let bytes = data.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            match b {
                b'\x1b' => {
                    i += 1;
                    if i >= bytes.len() {
                        break;
                    }
                    match bytes[i] {
                        b'[' => {
                            i += 1;
                            let start = i;
                            while i < bytes.len()
                                && !(bytes[i] as char).is_ascii_alphabetic()
                                && bytes[i] != b'@'
                                && bytes[i] != b'`'
                            {
                                i += 1;
                            }
                            if i >= bytes.len() {
                                break;
                            }
                            let params = std::str::from_utf8(&bytes[start..i]).unwrap_or("");
                            let final_byte = bytes[i];
                            i += 1;
                            let nums: Vec<usize> = params
                                .split(';')
                                .map(|p| p.parse::<usize>().unwrap_or(1))
                                .collect();
                            match final_byte {
                                b'A' => {
                                    let n = nums.first().copied().unwrap_or(1);
                                    let mut st = self.state.lock().unwrap();
                                    if st.cursor_row >= n {
                                        st.cursor_row -= n;
                                    } else {
                                        // Moving above the top reveals content above.
                                        let over = n - st.cursor_row;
                                        st.cursor_row = 0;
                                        st.viewport_top = st.viewport_top.saturating_sub(over);
                                    }
                                }
                                b'B' => {
                                    let n = nums.first().copied().unwrap_or(1);
                                    let mut st = self.state.lock().unwrap();
                                    let next = st.cursor_row + n;
                                    if next < st.rows {
                                        st.cursor_row = next;
                                    } else {
                                        // Moving below the bottom scrolls up.
                                        st.viewport_top += next - st.rows + 1;
                                        st.cursor_row = st.rows - 1;
                                    }
                                }
                                b'C' => {
                                    let n = nums.first().copied().unwrap_or(1);
                                    let mut st = self.state.lock().unwrap();
                                    st.cursor_col = (st.cursor_col + n).min(st.columns - 1);
                                }
                                b'H' | b'f' => {
                                    let row = nums.first().copied().unwrap_or(1).saturating_sub(1);
                                    let col = nums.get(1).copied().unwrap_or(1).saturating_sub(1);
                                    let mut st = self.state.lock().unwrap();
                                    st.cursor_row = row.min(st.rows - 1);
                                    st.cursor_col = col.min(st.columns - 1);
                                }
                                b'J' => {
                                    // Clear screen (2J): blank the visible rows.
                                    let mut st = self.state.lock().unwrap();
                                    let top = st.viewport_top;
                                    for row in top..top + st.rows {
                                        self.ensure_row(&mut st, row);
                                        st.document[row] = vec![' '; st.columns];
                                    }
                                }
                                b'K' => {
                                    // Clear to end of line (2K).
                                    let mut st = self.state.lock().unwrap();
                                    let index = st.viewport_top + st.cursor_row;
                                    self.ensure_row(&mut st, index);
                                    for c in st.cursor_col..st.columns {
                                        st.document[index][c] = ' ';
                                    }
                                }
                                b'T' => {
                                    let n = nums.first().copied().unwrap_or(1);
                                    self.scroll_down(n);
                                }
                                _ => {
                                    // Ignore unsupported sequences (2026h/l, 25h/l, ...).
                                }
                            }
                        }
                        _ => {}
                    }
                }
                b'\r' => {
                    self.carriage_return();
                    i += 1;
                }
                b'\n' => {
                    self.linefeed();
                    i += 1;
                }
                _ => {
                    let len = if b & 0x80 == 0 {
                        1
                    } else if b & 0xE0 == 0xC0 {
                        2
                    } else if b & 0xF0 == 0xE0 {
                        3
                    } else {
                        4
                    };
                    let end = (i + len).min(bytes.len());
                    let s = std::str::from_utf8(&bytes[i..end]).unwrap_or(" ");
                    if let Some(ch) = s.chars().next() {
                        self.put_char(ch);
                    }
                    i = end;
                }
            }
        }
    }
}

impl rpi_tui::Terminal for VirtualTerminal {
    fn info(&self) -> rpi_tui::TerminalInfo {
        rpi_tui::TerminalInfo {
            columns: self.columns(),
            rows: self.rows(),
            ..Default::default()
        }
    }

    fn write(&self, data: &str) {
        self.feed(data);
    }

    fn hide_cursor(&self) {}
    fn show_cursor(&self) {}
    fn move_cursor(&self, _row: usize, _col: usize) {}
    fn clear_screen(&self) {}
    fn set_title(&self, _title: &str) {}
    fn enable_mouse(&self) {}
    fn disable_mouse(&self) {}
    fn enter_raw_mode(&self) {}
    fn exit_raw_mode(&self) {}
    fn refresh_size(&self) {}
    fn start(
        &self,
        _on_input: Box<dyn Fn(rpi_tui::InputEvent) + Send + Sync>,
        _on_resize: Box<dyn Fn() + Send + Sync>,
    ) {
    }
    fn stop(&self) {}
    fn is_tty(&self) -> bool {
        true
    }
    fn set_progress(&self, _active: bool) {}
    fn flush(&self) {}
}

#[test]
fn long_transcript_stream_rewrite_keeps_dock_visible() {
    let term = VirtualTerminal::new(40, 8);
    let tui = Arc::new(TuiAltScreen::new(Box::new(term.clone()), true, None));
    tui.set_main_screen_mode(true);

    // Build a long transcript + a real editor (which carries the cursor marker
    // that drives the final cursor positioning) + a footer marker, so the first
    // frame scrolls the viewport well past the top.
    let transcript = Arc::new(Container::new());
    for i in 0..100 {
        transcript.add_child(Arc::new(Text::new(&format!("line {i:03}"), 0, 0)));
    }
    let editor = Arc::new(rpi_tui::Editor::simple());
    editor.set_focused(true);
    let footer = Arc::new(Text::new("FOOTER-STATUS", 0, 0));
    let root = Arc::new(Container::new());
    root.add_child(transcript.clone());
    root.add_child(editor.clone());
    root.add_child(footer.clone());

    tui.set_layout_root(Some(root.clone()));
    tui.start_readerless();

    // The first frame pins the footer at the bottom of the viewport.
    let visible = term.visible();
    assert!(
        visible.last().unwrap().contains("FOOTER-STATUS"),
        "first frame should show the dock at the bottom: {visible:?}"
    );

    // Simulate a long streamed message re-wrapping *above* the visible tail:
    // replace a transcript line far above the viewport so the changed range
    // starts before the current viewport top, and also insert an extra line so
    // the write spans past the bottom of the screen (forcing a scroll).
    transcript.clear();
    for i in 0..101 {
        let text = if i == 50 {
            "line 050 REWRAPPED".to_string()
        } else if i > 50 {
            format!("line {:03}", i - 1)
        } else {
            format!("line {i:03}")
        };
        transcript.add_child(Arc::new(Text::new(&text, 0, 0)));
    }
    tui.request_render(false);

    let visible = term.visible();
    assert!(
        visible.last().unwrap().contains("FOOTER-STATUS"),
        "after a streamed rewrite above the viewport the dock must stay visible: {visible:?}"
    );
    assert!(
        visible.iter().any(|row| row.contains('─')),
        "the editor border (input area) must remain visible after the rewrite: {visible:?}"
    );
    assert!(
        term.document_row(50).contains("REWRAPPED"),
        "the rewrite must land on document row 50, not be painted at the wrong row: {:?}",
        term.document_row(50)
    );

    // A second streamed update must also keep the dock pinned; a corrupted
    // viewport bookkeeping drifts further on each frame and eventually hides it.
    transcript.clear();
    for i in 0..102 {
        let text = if i == 51 {
            "line 051 REWRAPPED".to_string()
        } else if i > 51 {
            format!("line {:03}", i - 2)
        } else {
            format!("line {i:03}")
        };
        transcript.add_child(Arc::new(Text::new(&text, 0, 0)));
    }
    tui.request_render(false);

    let visible = term.visible();
    assert!(
        visible.last().unwrap().contains("FOOTER-STATUS"),
        "the dock must remain pinned across repeated streamed rewrites: {visible:?}"
    );
}
