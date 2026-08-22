//! Editor component for multi-line text editing.
//!
//! Provides a text editor with cursor positioning, selection, and keyboard input.

use std::any::Any;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyModifiers};

use super::component::{Component, Focusable};
use super::keybindings::Keybindings;
use crate::ansi::CURSOR_MARKER;

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
    s.char_indices()
        .take_while(|(b, _)| *b <= idx)
        .last()
        .map(|(b, _)| b)
        .unwrap_or(0)
}

/// Editor - A multi-line text editor component.
pub struct Editor {
    state: Mutex<EditorState>,
    options: EditorOptions,
    style: EditorStyle,
    keybindings: Arc<Keybindings>,
    on_submit: Mutex<Option<Arc<dyn Fn(&str) + Send + Sync>>>,
    on_change: Mutex<Option<Arc<dyn Fn(&str) + Send + Sync>>>,
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
        }
    }

    /// Create a new editor with default options.
    pub fn simple() -> Self {
        Self::new(EditorOptions::default(), EditorStyle::default(), Arc::new(Keybindings::new()))
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
        self.state.lock()
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
        self.state.lock()
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

    /// Insert text at cursor position.
    pub fn insert(&self, text: &str) {
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

    /// Delete character before cursor (Backspace).
    fn backspace(&self) {
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

    /// Delete character at cursor (Delete).
    fn delete(&self) {
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

    /// Submit current text (Enter).
    fn submit(&self) {
        let text = self.get_text();
        if let Ok(mut state) = self.state.lock() {
            if !text.is_empty() {
                state.history.push(text.clone());
                state.history_index = state.history.len();
            }
        }
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
            // Navigation
            (KeyModifiers::NONE, KeyCode::Left) => self.cursor_left(),
            (KeyModifiers::NONE, KeyCode::Right) => self.cursor_right(),
            (KeyModifiers::NONE, KeyCode::Up) => self.cursor_up(),
            (KeyModifiers::NONE, KeyCode::Down) => self.cursor_down(),
            (KeyModifiers::NONE, KeyCode::Home) => self.cursor_home(),
            (KeyModifiers::NONE, KeyCode::End) => self.cursor_end(),
            
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
                // Delete to end of line
                if let Ok(mut state) = self.state.lock() {
                    let row = state.cursor_row;
                    let col = state.cursor_col;
                    state.lines[row].truncate(col);
                }
                self.notify_change();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                // Delete to start of line
                if let Ok(mut state) = self.state.lock() {
                    let row = state.cursor_row;
                    let col = state.cursor_col;
                    let after: String = state.lines[row].drain(col..).collect();
                    state.lines[row] = after;
                    state.cursor_col = 0;
                }
                self.notify_change();
            }
            
            // Regular character input
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                self.insert(&c.to_string());
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
            let content = if line.is_empty()
                && row_idx == state.cursor_row
                && state.lines.len() == 1
            {
                if let Some(placeholder) = &self.options.placeholder {
                    format!("\x1b[2m{}\x1b[22m", placeholder)
                } else {
                    String::new()
                }
            } else {
                line.clone()
            };

            let visible_w = crate::ansi::visible_width(&content);
            // Pad the line out to the full width (left padding + content +
            // right padding) so the border box has a uniform interior width
            // (mirrors editor.ts:522-578).
            let right_pad = " ".repeat(
                width
                    .saturating_sub(self.options.padding_x)
                    .saturating_sub(visible_w),
            );
            let right_pad_cursor = if state.focused
                && row_idx == state.cursor_row
                && content.is_empty()
            {
                // Reserve room for the empty-cursor (`\x1b[7m \x1b[0m`) so
                // the trailing cursor doesn't wrap past the right border.
                right_pad.get(..right_pad.len().saturating_sub(1)).unwrap_or("").to_string()
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
            content_lines.push(format!("{pad}{}", " ".repeat(width.saturating_sub(self.options.padding_x))));
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
            EditorOptions { padding_x: 1, ..Default::default() },
            EditorStyle::default(),
            Arc::new(Keybindings::new()),
        );
        editor.set_text("hi");
        let lines = editor.render(20);
        assert_eq!(lines.len(), 3, "expected [border, content, border]");
        // Both borders are a 20-wide `─` run (visible width, ANSI stripped).
        let top = crate::ansi::strip_ansi(&lines[0]);
        let bottom = crate::ansi::strip_ansi(lines.last().unwrap());
        assert_eq!(top.chars().filter(|c| *c == '─').count(), 20, "top border not full width: {top:?}");
        assert_eq!(bottom.chars().filter(|c| *c == '─').count(), 20, "bottom border not full width: {bottom:?}");
        // The content line carries the text with no `> ` prompt.
        let content = crate::ansi::strip_ansi(&lines[1]);
        assert!(content.contains("hi"), "content line missing text: {content:?}");
        assert!(!content.contains("> "), "content line should not have a prompt prefix: {content:?}");
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
        assert_eq!(lines.len(), 3, "bordered box keeps [border, content, border]");

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
