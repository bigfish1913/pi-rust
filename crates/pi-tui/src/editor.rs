//! Editor component for multi-line text editing.
//!
//! Provides a text editor with cursor positioning, selection, and keyboard input.

use std::any::Any;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyModifiers};

use super::component::{Component, Focusable};
use super::keybindings::Keybindings;
use crate::ansi::CURSOR_MARKER;
use crate::kill_ring::{KillRing, PushOptions};
use crate::undo_stack::UndoStack;

/// Editor style configuration.
#[derive(Debug, Clone)]
pub struct EditorStyle {
    pub prompt: String,
    pub placeholder: String,
}

impl Default for EditorStyle {
    fn default() -> Self {
        Self {
            prompt: "> ".to_string(),
            placeholder: String::new(),
        }
    }
}

/// Editor options.
#[derive(Debug, Clone, Default)]
pub struct EditorOptions {
    /// Padding on left and right.
    pub padding_x: usize,
    /// Maximum visible lines for autocomplete.
    pub autocomplete_max_visible: usize,
    /// Placeholder text when empty.
    pub placeholder: Option<String>,
    /// Initial text content.
    pub initial_text: Option<String>,
}

/// Editor state.
#[derive(Debug, Clone)]
struct EditorState {
    /// Lines of text.
    lines: Vec<String>,
    /// Cursor row (line index).
    cursor_row: usize,
    /// Cursor column (character index within line).
    cursor_col: usize,
    /// Selection anchor (row, col) if any.
    selection_anchor: Option<(usize, usize)>,
    /// Scroll offset for multi-line.
    #[allow(dead_code)]
    scroll_offset: usize,
    /// Whether the editor is focused.
    focused: bool,
    /// History of submitted lines.
    history: Vec<String>,
    /// History index for navigation.
    history_index: usize,
}

impl Default for EditorState {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            selection_anchor: None,
            scroll_offset: 0,
            focused: false,
            history: Vec::new(),
            history_index: 0,
        }
    }
}

/// Greatest char-boundary byte index in `s` that is `<= idx` (clamped to
/// `s.len()`). The editor tracks `cursor_col` as a **byte** index and keeps it
/// on a char boundary; this helper restores that invariant when a caller
/// supplies a column that may be mid-character (e.g. clamping after moving
/// across lines of differing widths) and when converting external char counts.
///
/// Why byte indices: every `String` mutation the editor performs
/// (`insert`, `remove`, `truncate`, `drain`) and the `len()` comparisons used
/// for clamping are byte-indexed in `std`. Treating `cursor_col` as a byte
/// offset (advanced by `ch.len_utf8()`, never `+1`) keeps all of those sound
/// for multibyte input. Movement ops use this helper to step by *character*
/// while storing bytes.
fn snap_boundary(s: &str, idx: usize) -> usize {
    let idx = idx.min(s.len());
    // A valid char boundary (including the END of the string) is kept as-is —
    // the old `take_while(b <= idx)` returned the START of the char AT `idx`
    // for boundary positions, so a caret placed exactly after the last char
    // (e.g. autocomplete accept: `set_cursor(0, len)`) snapped one char EARLY
    // and the terminal showed "/mode|l" with the final char dangling past the
    // caret. Mid-character positions snap DOWN to the char start instead.
    if s.is_char_boundary(idx) {
        return idx;
    }
    s.char_indices()
        .take_while(|(b, _)| *b < idx)
        .last()
        .map(|(b, _)| b)
        .unwrap_or(0)
}

/// Editor - A multi-line text editor component.
pub struct Editor {
    state: Mutex<EditorState>,
    options: EditorOptions,
    #[allow(dead_code)]
    style: EditorStyle,
    #[allow(dead_code)]
    keybindings: Arc<Keybindings>,
    on_submit: Mutex<Option<Arc<dyn Fn(&str) + Send + Sync>>>,
    on_change: Mutex<Option<Arc<dyn Fn(&str) + Send + Sync>>>,
    /// Undo history (text + caret snapshots). Pushed before each mutation;
    /// consecutive character insertions coalesce into one entry.
    undo_stack: Mutex<UndoStack<EditorSnapshot>>,
    /// Popped undo entries, for Ctrl+R redo.
    redo_stack: Mutex<Vec<EditorSnapshot>>,
    /// Emacs-style kill ring for Ctrl+K/U/W kills; Ctrl+Y yanks.
    kill_ring: Mutex<KillRing>,
    /// The last mutation kind, for undo coalescing + kill accumulation.
    last_action: Mutex<Option<&'static str>>,
    /// Character-jump mode (pi `jumpForward`/`jumpBackward`): `Some(1)` waits
    /// for the next printable char to jump forward to, `Some(-1)` backward.
    /// The next character input consumes it instead of inserting.
    jump_mode: Mutex<Option<i32>>,
}

/// A restorable editor state snapshot (text + caret). Selection/scroll/focus
/// are not part of undo (mirrors the TS `EditorSnapshot` minus paste state).
#[derive(Clone)]
struct EditorSnapshot {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
}

impl Editor {
    /// Create a new editor with options.
    pub fn new(options: EditorOptions, style: EditorStyle, keybindings: Arc<Keybindings>) -> Self {
        let mut state = EditorState::default();
        if let Some(text) = &options.initial_text {
            state.lines = text.lines().map(|s| s.to_string()).collect();
            if state.lines.is_empty() {
                state.lines.push(String::new());
            }
        }

        Self {
            state: Mutex::new(state),
            options,
            style,
            keybindings,
            on_submit: Mutex::new(None),
            on_change: Mutex::new(None),
            undo_stack: Mutex::new(UndoStack::with_max_size(200)),
            redo_stack: Mutex::new(Vec::new()),
            kill_ring: Mutex::new(KillRing::new()),
            last_action: Mutex::new(None),
            jump_mode: Mutex::new(None),
        }
    }

    /// Create a new editor with default options.
    pub fn simple() -> Self {
        Self::new(
            EditorOptions::default(),
            EditorStyle::default(),
            Arc::new(Keybindings::new()),
        )
    }

    /// Set the text content.
    pub fn set_text(&self, text: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.lines = if text.is_empty() {
                vec![String::new()]
            } else {
                text.lines().map(|s| s.to_string()).collect()
            };
            if state.lines.is_empty() {
                state.lines.push(String::new());
            }
            state.cursor_row = 0;
            state.cursor_col = 0;
        }
    }

    /// Get the text content.
    pub fn get_text(&self) -> String {
        self.state
            .lock()
            .map(|s| s.lines.join("\n"))
            .unwrap_or_default()
    }

    /// Clear the editor.
    pub fn clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.lines = vec![String::new()];
            state.cursor_row = 0;
            state.cursor_col = 0;
            state.selection_anchor = None;
        }
    }

    /// Set callback for when text is submitted (Enter pressed).
    pub fn on_submit(&self, callback: Arc<dyn Fn(&str) + Send + Sync>) {
        if let Ok(mut cb) = self.on_submit.lock() {
            *cb = Some(callback);
        }
    }

    /// Set callback for when text changes.
    pub fn on_change(&self, callback: Arc<dyn Fn(&str) + Send + Sync>) {
        if let Ok(mut cb) = self.on_change.lock() {
            *cb = Some(callback);
        }
    }

    /// Get cursor position.
    pub fn cursor_position(&self) -> (usize, usize) {
        self.state
            .lock()
            .map(|s| (s.cursor_row, s.cursor_col))
            .unwrap_or((0, 0))
    }

    /// Set the cursor position (clamped to text bounds), clearing any
    /// in-progress selection. Used by autocomplete to place the caret after
    /// accepting a suggestion (`set_text` resets the caret to the start).
    ///
    /// `col` is interpreted as a **byte** offset (callers in the autocomplete
    /// path pass byte indices from `String::len`/`char_indices`) and snapped to
    /// the nearest preceding char boundary so multibyte input never lands
    /// mid-character.
    pub fn set_cursor(&self, row: usize, col: usize) {
        if let Ok(mut state) = self.state.lock() {
            let row = row.min(state.lines.len().saturating_sub(1));
            let col = snap_boundary(&state.lines[row], col);
            state.cursor_row = row;
            state.cursor_col = col;
            state.selection_anchor = None;
        }
    }

    /// Snapshot the current text + caret state (for undo/redo).
    fn snapshot_state(&self) -> EditorSnapshot {
        let state = self.state.lock().unwrap();
        EditorSnapshot {
            lines: state.lines.clone(),
            cursor_row: state.cursor_row,
            cursor_col: state.cursor_col,
        }
    }

    /// Restore a snapshot, notifying change observers.
    fn restore_state(&self, snap: &EditorSnapshot) {
        if let Ok(mut state) = self.state.lock() {
            state.lines = snap.lines.clone();
            state.cursor_row = snap.cursor_row;
            state.cursor_col = snap.cursor_col;
            // Snapshots don't carry the selection; undo/redo ends it.
            state.selection_anchor = None;
        }
        self.notify_change();
    }

    /// Record an undo snapshot before a mutation. `action` names the mutation
    /// kind: consecutive `"insert"` actions coalesce into a single undo entry
    /// (typing a word undoes as one step, mirroring the TS coalescing); any
    /// other mutation starts a fresh entry. A new mutation clears the redo
    /// stack (the classic undo/redo semantics).
    fn push_undo(&self, action: &'static str) {
        let coalesce = action == "insert" && *self.last_action.lock().unwrap() == Some("insert");
        if !coalesce {
            let snap = self.snapshot_state();
            if let Ok(mut stack) = self.undo_stack.lock() {
                stack.push(snap);
            }
            if let Ok(mut redo) = self.redo_stack.lock() {
                redo.clear();
            }
        }
        *self.last_action.lock().unwrap() = Some(action);
    }

    /// Undo the last mutation (Ctrl+-). Restores the prior snapshot; the
    /// undone state moves to the redo stack for Ctrl+R.
    pub fn undo(&self) {
        let popped = self.undo_stack.lock().unwrap().pop();
        let Some(snap) = popped else { return };
        // The current state is the redo target.
        self.redo_stack.lock().unwrap().push(self.snapshot_state());
        self.restore_state(&snap);
        *self.last_action.lock().unwrap() = None;
    }

    /// Redo the last undone mutation (Ctrl+R).
    pub fn redo(&self) {
        let popped = self.redo_stack.lock().unwrap().pop();
        let Some(snap) = popped else { return };
        self.undo_stack.lock().unwrap().push(self.snapshot_state());
        self.restore_state(&snap);
        *self.last_action.lock().unwrap() = None;
    }

    /// Push killed (deleted) text onto the kill ring, accumulating consecutive
    /// kills into one entry (mirrors the TS kill-ring semantics).
    fn kill(&self, text: String, prepend: bool) {
        if text.is_empty() {
            return;
        }
        let accumulate = *self.last_action.lock().unwrap() == Some("kill");
        if let Ok(mut ring) = self.kill_ring.lock() {
            ring.push(
                &text,
                PushOptions {
                    prepend,
                    accumulate,
                },
            );
        }
        *self.last_action.lock().unwrap() = Some("kill");
    }

    /// Yank (paste) the most recent kill at the cursor (Ctrl+Y). Replaces an
    /// active selection.
    pub fn yank(&self) {
        let text = self.kill_ring.lock().unwrap().peek().map(str::to_string);
        let Some(text) = text else { return };
        self.push_undo("yank");
        if self.has_selection() {
            self.delete_selection_no_undo();
        }
        self.insert_no_undo(&text);
        *self.last_action.lock().unwrap() = Some("yank");
    }

    /// Yank-pop: rotate the kill ring and yank the next entry (Alt+Y).
    pub fn yank_pop(&self) {
        if self.kill_ring.lock().unwrap().len() <= 1 {
            return;
        }
        self.kill_ring.lock().unwrap().rotate();
        self.yank();
    }

    // -------------------------------------------------------------------
    // Selection (Shift+arrows select, Ctrl+X/C/V cut/copy/paste).
    // -------------------------------------------------------------------

    /// Whether a selection is active.
    pub fn has_selection(&self) -> bool {
        self.state.lock().unwrap().selection_anchor.is_some()
    }

    /// The ordered selection span `(start_row, start_col, end_row, end_col)`
    /// with start <= end (byte offsets, both on char boundaries).
    fn selection_ordered(&self) -> Option<(usize, usize, usize, usize)> {
        let state = self.state.lock().unwrap();
        let (ar, ac) = state.selection_anchor?;
        let (br, bc) = (state.cursor_row, state.cursor_col);
        Some(if (ar, ac) <= (br, bc) {
            (ar, ac, br, bc)
        } else {
            (br, bc, ar, ac)
        })
    }

    /// The selected text ("\n"-joined across rows), if any.
    pub fn selected_text(&self) -> Option<String> {
        let (sr, sc, er, ec) = self.selection_ordered()?;
        let lines = self.state.lock().unwrap().lines.clone();
        if sr == er {
            Some(lines[sr][sc..ec].to_string())
        } else {
            let mut out = String::new();
            out.push_str(&lines[sr][sc..]);
            for r in sr + 1..er {
                out.push('\n');
                out.push_str(&lines[r]);
            }
            out.push('\n');
            out.push_str(&lines[er][..ec]);
            Some(out)
        }
    }

    /// The selection span on `row`, as `(start_col, end_col)` byte offsets,
    /// for render-time highlighting. `None` when the row is outside the span.
    fn selection_span_on_row(&self, state: &EditorState, row: usize) -> Option<(usize, usize)> {
        let (ar, ac) = state.selection_anchor?;
        let (br, bc) = (state.cursor_row, state.cursor_col);
        let (sr, sc, er, ec) = if (ar, ac) <= (br, bc) {
            (ar, ac, br, bc)
        } else {
            (br, bc, ar, ac)
        };
        if row < sr || row > er {
            return None;
        }
        if sr == er {
            (sc != ec).then_some((sc, ec))
        } else if row == sr {
            (sc < state.lines[row].len()).then_some((sc, state.lines[row].len()))
        } else if row == er {
            (ec > 0).then_some((0, ec))
        } else {
            (!state.lines[row].is_empty()).then_some((0, state.lines[row].len()))
        }
    }

    /// Seed the selection anchor at the current caret (Shift+direction start).
    fn begin_selection(&self) {
        let mut state = self.state.lock().unwrap();
        if state.selection_anchor.is_none() {
            state.selection_anchor = Some((state.cursor_row, state.cursor_col));
        }
    }

    /// Clear the active selection (plain cursor movement / edits).
    fn clear_selection(&self) {
        self.state.lock().unwrap().selection_anchor = None;
    }

    /// Delete the selected span, leaving the caret at its start. Pushes an
    /// undo snapshot; no-op without a selection.
    fn delete_selection(&self) {
        if !self.has_selection() {
            return;
        }
        self.push_undo("edit");
        self.delete_selection_no_undo();
    }

    /// Delete the selected span WITHOUT an undo snapshot (caller pushed one,
    /// e.g. a replace-through-insert coalesced with the insertion).
    fn delete_selection_no_undo(&self) {
        let (sr, sc, er, ec) = match self.selection_ordered() {
            Some(s) => s,
            None => return,
        };
        let mut state = self.state.lock().unwrap();
        if sr == er {
            state.lines[sr].replace_range(sc..ec, "");
            state.cursor_row = sr;
            state.cursor_col = sc;
        } else {
            let mut joined = state.lines[sr][..sc].to_string();
            joined.push_str(&state.lines[er][ec..]);
            state.lines.drain(sr + 1..=er);
            state.lines[sr] = joined;
            state.cursor_row = sr;
            state.cursor_col = sc;
        }
        state.selection_anchor = None;
        drop(state);
        self.notify_change();
    }

    /// Copy the selection onto the kill ring (Ctrl+C). No-op without one.
    pub fn copy_selection(&self) -> bool {
        let Some(text) = self.selected_text() else {
            return false;
        };
        if text.is_empty() {
            return false;
        }
        if let Ok(mut ring) = self.kill_ring.lock() {
            ring.push(&text, PushOptions::default());
        }
        true
    }

    /// Cut the selection (copy + delete, Ctrl+X). Returns whether anything
    /// was cut.
    pub fn cut_selection(&self) -> bool {
        if !self.copy_selection() {
            return false;
        }
        self.delete_selection();
        true
    }

    /// Insert text at cursor position. Replaces an active selection (one undo
    /// step: the snapshot is taken before the selection is deleted).
    pub fn insert(&self, text: &str) {
        self.push_undo("insert");
        if self.has_selection() {
            self.delete_selection_no_undo();
        }
        self.insert_no_undo(text);
    }

    /// Insert without an undo snapshot (caller pushed one — yank/replace
    /// paths). Notifies change observers.
    fn insert_no_undo(&self, text: &str) {
        if let Ok(mut state) = self.state.lock() {
            for ch in text.chars() {
                if ch == '\n' {
                    // Split line at cursor
                    let row = state.cursor_row;
                    let col = state.cursor_col;
                    let current_line = &mut state.lines[row];
                    let after = current_line.split_off(col);
                    state.lines.insert(row + 1, after);
                    state.cursor_row += 1;
                    state.cursor_col = 0;
                } else {
                    let row = state.cursor_row;
                    let col = state.cursor_col;
                    state.lines[row].insert(col, ch);
                    // Advance by the BYTE length of the inserted char so
                    // `cursor_col` stays a valid char boundary (and a sound
                    // byte index for the next `String` mutation). The old
                    // `+= 1` was a char count and panicked on multibyte input
                    // the moment a second keystroke landed mid-character.
                    state.cursor_col += ch.len_utf8();
                }
            }
        }
        self.notify_change();
    }

    /// Delete character before cursor (Backspace). With a selection, deletes
    /// the selection instead.
    fn backspace(&self) {
        if self.has_selection() {
            self.delete_selection();
            return;
        }
        self.push_undo("edit");
        if let Ok(mut state) = self.state.lock() {
            if state.cursor_col > 0 {
                let row = state.cursor_row;
                // Step back by the byte length of the char immediately
                // before the cursor, then remove that char. (`remove` takes
                // a byte index and must land on a boundary.) Compute the
                // length through a shared borrow first so the mutable borrow
                // for `remove` is the only outstanding alias.
                let prev_len = state.lines[row][..state.cursor_col]
                    .chars()
                    .last()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                let col = state.cursor_col - prev_len;
                state.lines[row].remove(col);
                state.cursor_col = col;
            } else if state.cursor_row > 0 {
                // Join with previous line
                let row = state.cursor_row;
                let current = state.lines.remove(row);
                state.cursor_row -= 1;
                let prev_row = state.cursor_row;
                state.cursor_col = state.lines[prev_row].len();
                state.lines[prev_row].push_str(&current);
            }
        }
        self.notify_change();
    }

    /// Delete character at cursor (Delete). With a selection, deletes the
    /// selection instead.
    fn delete(&self) {
        if self.has_selection() {
            self.delete_selection();
            return;
        }
        self.push_undo("edit");
        if let Ok(mut state) = self.state.lock() {
            let row = state.cursor_row;
            let line_len = state.lines[row].len();
            if state.cursor_col < line_len {
                let col = state.cursor_col;
                state.lines[row].remove(col);
            } else if row < state.lines.len() - 1 {
                // Join with next line
                let next = state.lines.remove(row + 1);
                state.lines[row].push_str(&next);
            }
        }
        self.notify_change();
    }

    /// Move cursor left.
    fn cursor_left(&self) {
        if let Ok(mut state) = self.state.lock() {
            if state.cursor_col > 0 {
                let row = state.cursor_row;
                let line = &state.lines[row];
                // Retrear by the byte length of the preceding char so the
                // caret stays on a char boundary.
                let prev_len = line[..state.cursor_col]
                    .chars()
                    .last()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                state.cursor_col -= prev_len;
            } else if state.cursor_row > 0 {
                state.cursor_row -= 1;
                state.cursor_col = state.lines[state.cursor_row].len();
            }
        }
    }

    /// Move cursor right.
    fn cursor_right(&self) {
        if let Ok(mut state) = self.state.lock() {
            let row = state.cursor_row;
            let line = &state.lines[row];
            if state.cursor_col < line.len() {
                // Advance by the byte length of the char at the cursor.
                let ch_len = line[state.cursor_col..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                state.cursor_col += ch_len;
            } else if row < state.lines.len() - 1 {
                state.cursor_row += 1;
                state.cursor_col = 0;
            }
        }
    }

    /// Move cursor up.
    fn cursor_up(&self) {
        if let Ok(mut state) = self.state.lock() {
            if state.cursor_row > 0 {
                state.cursor_row -= 1;
                let new_row = state.cursor_row;
                let max = state.lines[new_row].len();
                // The byte column may fall mid-character in the new (shorter
                // or differently-encoded) line; snap to a boundary.
                state.cursor_col = snap_boundary(&state.lines[new_row], state.cursor_col.min(max));
            }
        }
    }

    /// Move cursor down.
    fn cursor_down(&self) {
        if let Ok(mut state) = self.state.lock() {
            if state.cursor_row < state.lines.len() - 1 {
                state.cursor_row += 1;
                let new_row = state.cursor_row;
                let max = state.lines[new_row].len();
                state.cursor_col = snap_boundary(&state.lines[new_row], state.cursor_col.min(max));
            }
        }
    }

    /// Move cursor to start of line.
    fn cursor_home(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.cursor_col = 0;
        }
    }

    /// Move cursor to end of line.
    fn cursor_end(&self) {
        if let Ok(mut state) = self.state.lock() {
            let row = state.cursor_row;
            state.cursor_col = state.lines[row].len();
        }
    }

    /// Kill the word before the cursor (Ctrl+W / Alt+Backspace): Emacs
    /// backward-kill-word — skip the current word, then the intervening
    /// delimiters, landing on the previous word's start.
    fn kill_word_backward(&self) {
        self.push_undo("kill");
        let killed = if let Ok(mut state) = self.state.lock() {
            let row = state.cursor_row;
            let col = state.cursor_col;
            let start = crate::word_navigation::find_word_backward(&state.lines[row], col);
            let dead: String = state.lines[row].drain(start..col).collect();
            state.cursor_col = start;
            dead
        } else {
            String::new()
        };
        self.kill(killed, true);
        self.notify_change();
    }

    /// Kill the word after the cursor (Alt+D / Alt+Delete).
    fn kill_word_forward(&self) {
        self.push_undo("kill");
        let killed = if let Ok(mut state) = self.state.lock() {
            let row = state.cursor_row;
            let col = state.cursor_col;
            // Kill only the current word (not the trailing delimiter) —
            // `find_word_end` stops at the last word char.
            let end = crate::word_navigation::find_word_end(&state.lines[row], col);
            let dead: String = state.lines[row].drain(col..end).collect();
            dead
        } else {
            String::new()
        };
        self.kill(killed, false);
        self.notify_change();
    }

    /// Jump to the previous word boundary (Alt+Left).
    fn cursor_word_left(&self) {
        if let Ok(mut state) = self.state.lock() {
            let row = state.cursor_row;
            state.cursor_col =
                crate::word_navigation::find_word_backward(&state.lines[row], state.cursor_col);
        }
    }

    /// Jump to the next word boundary (Alt+Right).
    fn cursor_word_right(&self) {
        if let Ok(mut state) = self.state.lock() {
            let row = state.cursor_row;
            state.cursor_col =
                crate::word_navigation::find_word_forward(&state.lines[row], state.cursor_col);
        }
    }

    /// Page-scroll the editor caret up/down by a page (pi `tui.editor.pageUp/
    /// pageDown`): move the cursor row by a page and clamp the column to the
    /// target line (page = 10 rows; a terminal-height-aware version would need
    /// the viewport, which the component doesn't own).
    fn page_scroll(&self, dir: i32) {
        const PAGE: usize = 10;
        if let Ok(mut state) = self.state.lock() {
            let target = if dir > 0 {
                (state.cursor_row + PAGE).min(state.lines.len().saturating_sub(1))
            } else {
                state.cursor_row.saturating_sub(PAGE)
            };
            state.cursor_row = target;
            let max = state.lines[target].len();
            state.cursor_col = snap_boundary(&state.lines[target], state.cursor_col.min(max));
        }
    }

    /// Jump the caret to the next/previous occurrence of `target` (pi
    /// `jumpToChar`): scan the current line from after/before the caret, then
    /// onward/backward line by line. Byte-safe: `find`/`rfind` return char
    /// boundaries and all search offsets start on one.
    fn jump_to_char(&self, target: char, dir: i32) {
        let forward = dir > 0;
        let mut state = self.state.lock().unwrap();
        let start_row = state.cursor_row;
        let mut row = start_row;
        let step: isize = if forward { 1 } else { -1 };
        loop {
            let line = &state.lines[row];
            let search_from = if row == start_row {
                if forward {
                    (state.cursor_col + target.len_utf8()).min(line.len())
                } else {
                    state.cursor_col.saturating_sub(target.len_utf8())
                }
            } else if forward {
                0
            } else {
                line.len()
            };
            let found = if forward {
                line[search_from..].find(target).map(|i| search_from + i)
            } else {
                line[..search_from].rfind(target)
            };
            if let Some(idx) = found {
                state.cursor_row = row;
                state.cursor_col = idx;
                return;
            }
            let next = row as isize + step;
            if next < 0 || next >= state.lines.len() as isize {
                return;
            }
            row = next as usize;
        }
    }

    /// Submit current text (Enter).
    fn submit(&self) {
        let text = self.get_text();
        if let Ok(mut state) = self.state.lock() {
            if !text.is_empty() {
                state.history.push(text.clone());
                state.history_index = state.history.len();
            }
        }
        // Submitted text is committed — don't let Ctrl+- resurrect it.
        self.undo_stack.lock().unwrap().clear();
        self.redo_stack.lock().unwrap().clear();
        *self.last_action.lock().unwrap() = None;
        if let Ok(cb) = self.on_submit.lock() {
            if let Some(callback) = cb.as_ref() {
                callback(&text);
            }
        }
        self.clear();
    }

    /// Notify change callback.
    fn notify_change(&self) {
        if let Ok(cb) = self.on_change.lock() {
            if let Some(callback) = cb.as_ref() {
                let text = self.get_text();
                callback(&text);
            }
        }
    }

    /// Handle keyboard input.
    pub fn handle_key(&self, key: crossterm::event::KeyEvent) -> bool {
        match (key.modifiers, key.code) {
            // Navigation — plain movement clears any active selection;
            // Shift+movement extends it.
            (KeyModifiers::NONE, KeyCode::Left) => {
                self.clear_selection();
                self.cursor_left();
            }
            (KeyModifiers::NONE, KeyCode::Right) => {
                self.clear_selection();
                self.cursor_right();
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                self.clear_selection();
                self.cursor_up();
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                self.clear_selection();
                self.cursor_down();
            }
            (KeyModifiers::NONE, KeyCode::Home) => {
                self.clear_selection();
                self.cursor_home();
            }
            (KeyModifiers::NONE, KeyCode::End) => {
                self.clear_selection();
                self.cursor_end();
            }
            (KeyModifiers::SHIFT, KeyCode::Left) => {
                self.begin_selection();
                self.cursor_left();
            }
            (KeyModifiers::SHIFT, KeyCode::Right) => {
                self.begin_selection();
                self.cursor_right();
            }
            (KeyModifiers::SHIFT, KeyCode::Up) => {
                self.begin_selection();
                self.cursor_up();
            }
            (KeyModifiers::SHIFT, KeyCode::Down) => {
                self.begin_selection();
                self.cursor_down();
            }
            (KeyModifiers::SHIFT, KeyCode::Home) => {
                self.begin_selection();
                self.cursor_home();
            }
            (KeyModifiers::SHIFT, KeyCode::End) => {
                self.begin_selection();
                self.cursor_end();
            }

            // Editing
            (KeyModifiers::NONE, KeyCode::Backspace) => self.backspace(),
            (KeyModifiers::NONE, KeyCode::Delete) => self.delete(),
            (KeyModifiers::NONE, KeyCode::Enter) => {
                // Submit on Enter (mirrors TS `tui.input.submit`).
                self.submit();
            }
            // Shift+Enter inserts a newline (mirrors `tui.input.newLine`).
            (KeyModifiers::SHIFT, KeyCode::Enter) => self.insert("\n"),

            // Ctrl shortcuts
            (KeyModifiers::CONTROL, KeyCode::Char('a')) => self.cursor_home(),
            (KeyModifiers::CONTROL, KeyCode::Char('e')) => self.cursor_end(),
            (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
                // Kill to end of line (push the killed text onto the ring).
                self.push_undo("kill");
                let killed = if let Ok(mut state) = self.state.lock() {
                    let row = state.cursor_row;
                    let col = state.cursor_col;
                    let dead: String = state.lines[row].drain(col..).collect();
                    state.lines[row].truncate(col);
                    dead
                } else {
                    String::new()
                };
                self.kill(killed, false);
                self.notify_change();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                // Kill to start of line (killed text prepends to the ring entry
                // so Ctrl+Y pastes it back in the same order).
                self.push_undo("kill");
                let killed = if let Ok(mut state) = self.state.lock() {
                    let row = state.cursor_row;
                    let col = state.cursor_col;
                    let before: String = state.lines[row].drain(..col).collect();
                    state.cursor_col = 0;
                    before
                } else {
                    String::new()
                };
                self.kill(killed, true);
                self.notify_change();
            }
            // Undo / redo (pi binds undo to Ctrl+-; Ctrl+R redo is a Rust port
            // convenience since pi has no redo binding).
            (KeyModifiers::CONTROL, KeyCode::Char('-')) => self.undo(),
            (KeyModifiers::CONTROL, KeyCode::Char('r')) => self.redo(),
            // Selection cut/copy/paste (Ctrl+X / Ctrl+C / Ctrl+V).
            (KeyModifiers::CONTROL, KeyCode::Char('x')) => {
                self.cut_selection();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                self.copy_selection();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('v')) => {
                self.yank();
            }
            // Kill-ring yank (Ctrl+Y) / yank-pop (Alt+Y).
            (KeyModifiers::CONTROL, KeyCode::Char('y')) => self.yank(),
            (KeyModifiers::ALT, KeyCode::Char('y')) => self.yank_pop(),
            // Delete the word before the cursor, killing it (Alt+Backspace).
            (KeyModifiers::CONTROL, KeyCode::Char('w')) => self.kill_word_backward(),
            (KeyModifiers::ALT, KeyCode::Backspace) => self.kill_word_backward(),
            (KeyModifiers::ALT, KeyCode::Char('d')) | (KeyModifiers::ALT, KeyCode::Delete) => {
                self.kill_word_forward();
            }
            // Word navigation (Alt+Left / Alt+Right — pi's word-jump bindings).
            (KeyModifiers::ALT, KeyCode::Left) => self.cursor_word_left(),
            (KeyModifiers::ALT, KeyCode::Right) => self.cursor_word_right(),
            // Ctrl+J inserts a newline (pi tui.input.newLine alongside
            // Shift+Enter). Ctrl+D deletes the char forward (deleteCharForward).
            (KeyModifiers::CONTROL, KeyCode::Char('j')) => self.insert_no_undo(
                "
",
            ),
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => self.delete(),
            // Editor page scroll (pi tui.editor.pageUp/pageDown — the
            // unmodified PageUp/Down are the alt-screen transcript scroll).
            (KeyModifiers::CONTROL, KeyCode::PageUp) => self.page_scroll(-1),
            (KeyModifiers::CONTROL, KeyCode::PageDown) => self.page_scroll(1),
            // Character jump mode (pi jumpForward/jumpBackward): Ctrl+] jumps
            // forward to the next typed char, Ctrl+Alt+] backward. NB: a
            // CONTROL|ALT pattern in a match arm is an or-pattern (matches
            // EITHER modifier), so the backward arm uses a guard.
            (_, KeyCode::Char(']')) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+Alt+] jumps backward, Ctrl+] forward. A bare CONTROL
                // arm wouldn't match the CONTROL|ALT combination.
                if key.modifiers.contains(KeyModifiers::ALT) {
                    *self.jump_mode.lock().unwrap() = Some(-1);
                } else {
                    *self.jump_mode.lock().unwrap() = Some(1);
                }
            }

            // Regular character input — unless a character jump is pending
            // (Ctrl+] / Ctrl+Alt+] consumed the key as the jump target).
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                let jump = self.jump_mode.lock().unwrap().take();
                match jump {
                    Some(dir) => self.jump_to_char(c, dir),
                    None => self.insert(&c.to_string()),
                }
            }

            _ => return false,
        }
        true
    }
}

impl Component for Editor {
    fn render(&self, width: usize) -> Vec<String> {
        let state = self.state.lock().map(|s| s.clone()).unwrap_or_default();
        let mut content_lines = Vec::new();

        let pad = " ".repeat(self.options.padding_x);
        // `prompt` is no longer rendered (mirrors the TS editor, which draws a
        // bordered box with padding-only lines and no `> ` prefix — see
        // `editor.ts:539-578`). The `EditorStyle.prompt` field is retained for
        // API compatibility but unread here.
        for (row_idx, line) in state.lines.iter().enumerate() {
            // Placeholder (dim) only on the single empty line at the cursor —
            // honors callers that set `placeholder`; production drops it.
            let content =
                if line.is_empty() && row_idx == state.cursor_row && state.lines.len() == 1 {
                    if let Some(placeholder) = &self.options.placeholder {
                        format!("\x1b[2m{}\x1b[22m", placeholder)
                    } else {
                        String::new()
                    }
                } else {
                    line.clone()
                };

            // Selection highlight: inverse-video the span on this row (byte
            // offsets are char boundaries — both anchor and caret always sit
            // on one).
            let mut content = content;
            if let Some((sc, ec)) = self.selection_span_on_row(&state, row_idx) {
                if ec <= content.len()
                    && sc <= ec
                    && content.is_char_boundary(sc)
                    && content.is_char_boundary(ec)
                {
                    let before = &content[..sc];
                    let sel = &content[sc..ec];
                    let after = &content[ec..];
                    content = format!("{before}\x1b[7m{sel}\x1b[27m{after}");
                }
            }

            let visible_w = crate::ansi::visible_width(&content);
            // Pad the line out to the full width (left padding + content +
            // right padding) so the border box has a uniform interior width
            // (mirrors editor.ts:522-578).
            let right_pad = " ".repeat(
                width
                    .saturating_sub(self.options.padding_x)
                    .saturating_sub(visible_w),
            );
            let right_pad_cursor =
                if state.focused && row_idx == state.cursor_row && content.is_empty() {
                    // Reserve room for the empty-cursor (`\x1b[7m \x1b[0m`) so
                    // the trailing cursor doesn't wrap past the right border.
                    right_pad
                        .get(..right_pad.len().saturating_sub(1))
                        .unwrap_or("")
                        .to_string()
                } else {
                    right_pad
                };

            let mut rendered = format!("{pad}{content}{right_pad_cursor}");

            // Cursor marker if this focused row holds the cursor. `cursor_col`
            // is a byte offset into `line`; the rendered prefix up to the caret
            // is `pad` (ASCII, `padding_x` bytes) + `line[..cursor_col]`.
            // Because `content` may carry ANSI escapes (placeholder branch),
            // and the prefix length is computed in *bytes* of the source line,
            // snap the insertion point to the nearest preceding char boundary
            // in `rendered` before byte-slicing — never slice mid-character.
            if state.focused && row_idx == state.cursor_row {
                let prefix_bytes = self.options.padding_x + state.cursor_col;
                let pos = snap_boundary(&rendered, prefix_bytes);
                rendered = format!("{}{}{}", &rendered[..pos], CURSOR_MARKER, &rendered[pos..]);
            }

            content_lines.push(rendered);
        }

        // Guard against an empty `lines` vector (the invariant is `[""]`, but
        // be defensive): emit one blank interior row so the box still renders.
        if content_lines.is_empty() {
            content_lines.push(format!(
                "{pad}{}",
                " ".repeat(width.saturating_sub(self.options.padding_x))
            ));
        }

        // Full-width top + bottom border, colored via the theme border color
        // (mirrors editor.ts:494,530,587). The global `theme()` is read-only
        // after OnceLock init; `Color::fg` wraps the `─` run in the escape.
        let border_color = crate::theme::theme().colors.border;
        let horizontal = "─".repeat(width);
        let border = border_color.fg(&horizontal);

        let mut lines = Vec::with_capacity(content_lines.len() + 2);
        lines.push(border.clone());
        lines.extend(content_lines);
        lines.push(border);
        lines
    }

    fn invalidate(&self) {
        // Editor doesn't have external cache
    }

    fn handle_input(&self, _data: &str) -> bool {
        // Parse key event from crossterm format
        // This is a simplified version - in real usage, you'd receive KeyEvent directly
        false
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Focusable for Editor {
    fn set_focused(&self, focused: bool) {
        if let Ok(mut state) = self.state.lock() {
            state.focused = focused;
        }
    }

    fn is_focused(&self) -> bool {
        self.state.lock().map(|s| s.focused).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_boundary_keeps_char_boundaries_including_end() {
        // The regression: a caret placed exactly at the END of the text (what
        // autocomplete accept does — `set_cursor(0, text.len())`) used to snap
        // one char EARLY, rendering "/mode|l" instead of "/model|".
        assert_eq!(snap_boundary("/mo", 3), 3);
        assert_eq!(snap_boundary("/model", 6), 6);
        assert_eq!(snap_boundary("hello", 5), 5);
        // Interior boundaries are kept as-is.
        assert_eq!(snap_boundary("/model", 0), 0);
        assert_eq!(snap_boundary("/model", 2), 2);
        // Out-of-range clamps to the end (a valid boundary).
        assert_eq!(snap_boundary("/mo", 99), 3);
    }

    #[test]
    fn snap_boundary_mid_multibyte_snaps_down_to_char_start() {
        // "你" occupies bytes 0..3: a position inside it must snap to 0.
        assert_eq!(snap_boundary("你", 1), 0);
        assert_eq!(snap_boundary("你", 2), 0);
        // Position after it (end of string) stays 3.
        assert_eq!(snap_boundary("你", 3), 3);
        // Mixed: "a你b" — 你 is bytes 1..4; end of string is 5.
        assert_eq!(snap_boundary("a你b", 5), 5);
        assert_eq!(snap_boundary("a你b", 2), 1);
    }

    #[test]
    fn left_arrow_keeps_text_in_render() {
        use crate::component::Component;
        use crossterm::event::{KeyCode, KeyModifiers};
        let mk = |m, c| crossterm::event::KeyEvent::new(c, m);
        let editor = Editor::simple();
        editor.set_focused(true);
        editor.insert("hello world");
        editor.handle_key(mk(KeyModifiers::NONE, KeyCode::Left));
        let rendered = editor.render(40).join(
            "
",
        );
        assert!(
            rendered.contains("hello"),
            "text must survive Left, got: {rendered:?}"
        );
    }
    #[test]
    fn test_char_jump_and_page_scroll() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mk = |m, c| crossterm::event::KeyEvent::new(c, m);
        let editor = Editor::simple();

        // Character jump: from the end, Ctrl+Alt+] then 'x' jumps backward to
        // the previous 'x' ("alpha xray xray" — the second "xray" starts at 11).
        editor.insert("alpha xray xray");
        editor.handle_key(mk(KeyModifiers::CONTROL | KeyModifiers::ALT, KeyCode::End)); // caret at end
        editor.handle_key(mk(
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyCode::Char(']'),
        ));
        // The next printable char is consumed as the jump target.
        editor.handle_key(mk(KeyModifiers::NONE, KeyCode::Char('x')));
        assert_eq!(editor.cursor_position(), (0, 11));

        // Page scroll on a multi-line buffer clamps to the last row.
        editor.set_text(
            "l0
l1
l2
l3
l4
l5
l6
l7
l8
l9
l10
l11
l12",
        );
        editor.set_cursor(0, 2);
        editor.handle_key(mk(KeyModifiers::CONTROL, KeyCode::PageDown));
        let (row, _) = editor.cursor_position();
        assert!(row >= 10, "page-down moves ~10 rows, got {row}");
        editor.handle_key(mk(KeyModifiers::CONTROL, KeyCode::PageUp));
        let (row2, _) = editor.cursor_position();
        assert!(row2 <= 2, "page-up returns near the start, got {row2}");
    }
    #[test]
    fn test_word_ops_and_new_keys() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mk = |m, c| crossterm::event::KeyEvent::new(c, m);
        let editor = Editor::simple();
        editor.insert("alpha beta gamma");

        // Ctrl+W kills the word before the cursor.
        editor.handle_key(mk(KeyModifiers::CONTROL, KeyCode::End));
        editor.handle_key(mk(KeyModifiers::CONTROL, KeyCode::Char('w')));
        assert_eq!(editor.get_text(), "alpha beta ");

        // Alt+Left jumps back a word; Alt+D kills the word after.
        editor.handle_key(mk(KeyModifiers::ALT, KeyCode::Left));
        assert_eq!(editor.cursor_position(), (0, 6));
        editor.handle_key(mk(KeyModifiers::ALT, KeyCode::Char('d')));
        assert_eq!(editor.get_text(), "alpha  ");

        // Ctrl+J inserts a newline.
        editor.set_text("a");
        editor.set_cursor(0, 1);
        editor.handle_key(mk(KeyModifiers::CONTROL, KeyCode::Char('j')));
        assert_eq!(
            editor.get_text(),
            "a
"
        );

        // Ctrl+D deletes the char forward (deleteCharForward).
        editor.set_text("abc");
        editor.set_cursor(0, 1);
        editor.handle_key(mk(KeyModifiers::CONTROL, KeyCode::Char('d')));
        assert_eq!(editor.get_text(), "ac");
    }
    #[test]
    fn test_selection_cut_copy_paste() {
        let editor = Editor::simple();
        editor.insert("hello world");
        // Caret to the start, then Shift+Right ×5 selects "hello".
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Home,
            crossterm::event::KeyModifiers::NONE,
        ));
        let shift_right = || {
            editor.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Right,
                crossterm::event::KeyModifiers::SHIFT,
            ))
        };
        for _ in 0..5 {
            shift_right();
        }
        assert!(editor.has_selection());
        assert_eq!(editor.selected_text().as_deref(), Some("hello"));
        // Ctrl+C copies (selection intact), then plain Right clears it.
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert_eq!(editor.get_text(), "hello world");
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Right,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(!editor.has_selection(), "plain movement clears selection");
        // Move to the end and yank the copied text.
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::End,
            crossterm::event::KeyModifiers::NONE,
        ));
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('v'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert_eq!(editor.get_text(), "hello worldhello");
    }

    #[test]
    fn test_selection_replace_and_cut() {
        let editor = Editor::simple();
        editor.insert("abcdef");
        // Caret to the start, then Shift+Right ×4 selects "abcd".
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Home,
            crossterm::event::KeyModifiers::NONE,
        ));
        for _ in 0..4 {
            editor.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Right,
                crossterm::event::KeyModifiers::SHIFT,
            ));
        }
        // Backspace deletes the selection ("abcd" → "ef").
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Backspace,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(editor.get_text(), "ef", "backspace deletes the selection");
        // Undo restores it.
        editor.undo();
        assert_eq!(editor.get_text(), "abcdef");
        // Cut: return to the start, select "ab", Ctrl+X removes + kills it.
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Home,
            crossterm::event::KeyModifiers::NONE,
        ));
        for _ in 0..2 {
            editor.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Right,
                crossterm::event::KeyModifiers::SHIFT,
            ));
        }
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('x'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert_eq!(editor.get_text(), "cdef");
        // Type over a selection replaces it.
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Right,
            crossterm::event::KeyModifiers::SHIFT,
        ));
        editor.insert("Z");
        assert_eq!(editor.get_text(), "Zdef", "typing replaces the selection");
    }
    #[test]
    fn test_undo_redo_roundtrip() {
        let editor = Editor::simple();
        editor.insert("hello");
        editor.insert(" world");
        // Consecutive inserts coalesce: one undo restores the empty state.
        assert_eq!(editor.get_text(), "hello world");
        editor.undo();
        assert_eq!(editor.get_text(), "", "coalesced inserts undo as one step");
        // Redo restores everything.
        editor.redo();
        assert_eq!(editor.get_text(), "hello world");
        // A fresh edit after undo clears redo.
        editor.undo();
        editor.insert("hi");
        editor.redo();
        assert_eq!(editor.get_text(), "hi", "new edit clears the redo stack");
    }

    #[test]
    fn test_kill_ring_yank() {
        let editor = Editor::simple();
        editor.insert("alpha beta gamma");
        // Ctrl+K kills to end of line.
        editor.set_cursor(0, 6); // after "alpha "
        let key = |m, c| crossterm::event::KeyEvent::new(c, m);
        editor.handle_key(key(
            crossterm::event::KeyModifiers::CONTROL,
            crossterm::event::KeyCode::Char('k'),
        ));
        assert_eq!(editor.get_text(), "alpha ");
        // Ctrl+Y yanks it back.
        editor.handle_key(key(
            crossterm::event::KeyModifiers::CONTROL,
            crossterm::event::KeyCode::Char('y'),
        ));
        assert_eq!(editor.get_text(), "alpha beta gamma");
    }

    #[test]
    fn test_undo_after_kill() {
        let editor = Editor::simple();
        editor.insert("hello world");
        editor.set_cursor(0, 5);
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('k'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert_eq!(editor.get_text(), "hello");
        editor.undo();
        assert_eq!(editor.get_text(), "hello world", "kill is undoable");
    }

    #[test]
    fn test_submit_clears_undo() {
        let editor = Editor::simple();
        editor.insert("committed");
        editor.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
        editor.undo();
        assert_eq!(
            editor.get_text(),
            "",
            "submit clears undo so committed text stays committed"
        );
    }
    #[test]
    fn test_editor_text() {
        let editor = Editor::simple();
        editor.set_text("Hello");
        assert_eq!(editor.get_text(), "Hello");
    }

    #[test]
    fn test_editor_insert() {
        let editor = Editor::simple();
        editor.insert("Hello");
        assert_eq!(editor.get_text(), "Hello");
        editor.insert(" World");
        assert_eq!(editor.get_text(), "Hello World");
    }

    #[test]
    fn test_editor_clear() {
        let editor = Editor::simple();
        editor.set_text("Test");
        editor.clear();
        assert_eq!(editor.get_text(), "");
    }

    #[test]
    fn test_editor_render_has_borders() {
        // The editor renders a native-pi bordered box: a full-width `─` line
        // above and below the content, with no `> ` prompt prefix on the text
        // lines (mirrors TS editor.ts:525-588).
        let editor = Editor::new(
            EditorOptions {
                padding_x: 1,
                ..Default::default()
            },
            EditorStyle::default(),
            Arc::new(Keybindings::new()),
        );
        editor.set_text("hi");
        let lines = editor.render(20);
        assert_eq!(lines.len(), 3, "expected [border, content, border]");
        // Both borders are a 20-wide `─` run (visible width, ANSI stripped).
        let top = crate::ansi::strip_ansi(&lines[0]);
        let bottom = crate::ansi::strip_ansi(lines.last().unwrap());
        assert_eq!(
            top.chars().filter(|c| *c == '─').count(),
            20,
            "top border not full width: {top:?}"
        );
        assert_eq!(
            bottom.chars().filter(|c| *c == '─').count(),
            20,
            "bottom border not full width: {bottom:?}"
        );
        // The content line carries the text with no `> ` prompt.
        let content = crate::ansi::strip_ansi(&lines[1]);
        assert!(
            content.contains("hi"),
            "content line missing text: {content:?}"
        );
        assert!(
            !content.contains("> "),
            "content line should not have a prompt prefix: {content:?}"
        );
    }

    #[test]
    fn test_editor_multibyte_no_panic() {
        // Inserting a multibyte char (读 = 3 bytes) and then a second char
        // used to panic: `cursor_col += 1` left the caret at byte 1, inside
        // '读', so the next `String::insert(1, …)` hit a char boundary. This
        // exercises the byte-cursor fix across insert + render + a second
        // keystroke.
        let editor = Editor::simple();
        editor.set_focused(true);
        editor.insert("读");
        editor.insert("a");
        editor.insert("书");
        assert_eq!(editor.get_text(), "读a书");
        // cursor_col should be at the end (byte offset 7: 读=3 + a=1 + 书=3).
        assert_eq!(editor.cursor_position(), (0, 7));
        // Render must not panic when placing the cursor marker on the row.
        let lines = editor.render(20);
        assert_eq!(
            lines.len(),
            3,
            "bordered box keeps [border, content, border]"
        );

        // Backspace / left/right must also stay on boundaries.
        editor.backspace(); // remove 书
        assert_eq!(editor.get_text(), "读a");
        editor.cursor_left(); // left of 'a'
        assert_eq!(editor.cursor_position(), (0, 3));
        editor.insert("b"); // insert between 读 and a
        assert_eq!(editor.get_text(), "读ba");
    }

    #[test]
    fn test_autocomplete_cursor_origin() {
        // Origin of the autocomplete panic (autocomplete.rs:279, before the
        // fix): the editor handed back a byte `cursor_col` for multibyte
        // text, and the slash-command provider sliced `&input[..cursor]`
        // mid-character. `refresh_autocomplete` in the CLI now passes the
        // byte col directly (providers snap to a boundary), so a multibyte
        // editor line must round-trip through `get_suggestions` without
        // panicking.
        use crate::autocomplete::AutocompleteManager;

        let editor = Editor::simple();
        editor.set_text("读");
        editor.set_cursor(0, 3); // byte end of line
                                 // Emulate refresh_autocomplete: col (byte) min text len.
        let text = editor.get_text();
        let (_r, col) = editor.cursor_position();
        let cursor = col.min(text.len());
        let mgr = AutocompleteManager::new();

        // No slash prefix → None; must not panic on the multibyte slice.
        assert!(mgr.get_suggestions(&text, cursor).is_none());
        // And the slash path with a char-count overshoot also must not panic.
        editor.set_text("/读");
        editor.set_cursor(0, 4);
        let text2 = editor.get_text();
        let mgr2 = AutocompleteManager::new();
        let _ = mgr2.get_suggestions(&text2, 9); // overshoot past end
    }
}
