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
                    state.cursor_col += 1;
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
                let col = state.cursor_col - 1;
                state.lines[row].remove(col);
                state.cursor_col -= 1;
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
                state.cursor_col -= 1;
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
            if state.cursor_col < state.lines[row].len() {
                state.cursor_col += 1;
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
                state.cursor_col = state.cursor_col.min(state.lines[new_row].len());
            }
        }
    }

    /// Move cursor down.
    fn cursor_down(&self) {
        if let Ok(mut state) = self.state.lock() {
            if state.cursor_row < state.lines.len() - 1 {
                state.cursor_row += 1;
                let new_row = state.cursor_row;
                state.cursor_col = state.cursor_col.min(state.lines[new_row].len());
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
                // Multi-line: insert newline
                self.insert("\n");
            }
            (KeyModifiers::SHIFT, KeyCode::Enter) => self.submit(),
            
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
        let mut lines = Vec::new();

        let prompt_width = crate::ansi::visible_width(&self.style.prompt);
        let _content_width = width.saturating_sub(self.options.padding_x * 2).saturating_sub(prompt_width);

        for (row_idx, line) in state.lines.iter().enumerate() {
            let mut rendered = String::new();
            
            // Add padding
            if self.options.padding_x > 0 {
                rendered.push_str(&" ".repeat(self.options.padding_x));
            }

            // Add prompt on first line or continuation indicator
            if row_idx == 0 {
                rendered.push_str(&self.style.prompt);
            } else {
                rendered.push_str(&" ".repeat(prompt_width));
            }

            // Add line content
            if line.is_empty() && row_idx == state.cursor_row && state.lines.len() == 1 {
                if let Some(placeholder) = &self.options.placeholder {
                    rendered.push_str(&format!("\x1b[2m{}\x1b[22m", placeholder));
                }
            } else {
                rendered.push_str(line);
            }

            // Add cursor marker if focused
            if state.focused && row_idx == state.cursor_row {
                // Insert cursor marker at cursor position
                let cursor_pos = rendered.len().min(state.cursor_col + prompt_width + self.options.padding_x);
                let before: String = rendered.chars().take(cursor_pos).collect();
                let after: String = rendered.chars().skip(cursor_pos).collect();
                rendered = format!("{}{}{}", before, CURSOR_MARKER, after);
            }

            lines.push(rendered);
        }

        if lines.is_empty() {
            lines.push(format!("{}{}", self.style.prompt, CURSOR_MARKER));
        }

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
}