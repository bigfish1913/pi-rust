//! Input component - single-line text input with horizontal scrolling.
//!
//! Provides a text input field with cursor positioning, selection,
//! undo/redo, and Emacs-style editing.

use std::any::Any;
use std::sync::{Arc, Mutex};

use super::component::{Component, Focusable};
use super::keybindings::Keybindings;
use super::kill_ring::KillRing;
use super::undo_stack::UndoStack;
use super::word_navigation::{find_word_backward, find_word_forward};
use crate::ansi::CURSOR_MARKER;
use crate::utils::{visible_width, slice_by_column};

/// Input state for undo.
#[derive(Debug, Clone)]
struct InputState {
    value: String,
    cursor: usize,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            value: String::new(),
            cursor: 0,
        }
    }
}

/// Last action type for kill ring coalescing.
#[derive(Debug, Clone, Copy, PartialEq)]
enum LastAction {
    None,
    Kill,
    Yank,
    TypeWord,
}

/// Input component - single-line text input.
pub struct Input {
    state: Mutex<InputState>,
    prompt: String,
    placeholder: Option<String>,
    keybindings: Arc<Keybindings>,
    kill_ring: Mutex<KillRing>,
    undo_stack: Mutex<UndoStack<InputState>>,
    last_action: Mutex<LastAction>,
    focused: Mutex<bool>,
    on_submit: Mutex<Option<Arc<dyn Fn(&str) + Send + Sync>>>,
    on_escape: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    on_change: Mutex<Option<Arc<dyn Fn(&str) + Send + Sync>>>,
}

impl Input {
    /// Create a new input component.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(InputState::default()),
            prompt: "> ".to_string(),
            placeholder: None,
            keybindings: Arc::new(Keybindings::new()),
            kill_ring: Mutex::new(KillRing::new()),
            undo_stack: Mutex::new(UndoStack::new()),
            last_action: Mutex::new(LastAction::None),
            focused: Mutex::new(false),
            on_submit: Mutex::new(None),
            on_escape: Mutex::new(None),
            on_change: Mutex::new(None),
        }
    }

    /// Create an input with a custom prompt.
    pub fn with_prompt(prompt: &str) -> Self {
        let mut input = Self::new();
        input.prompt = prompt.to_string();
        input
    }

    /// Create an input with a placeholder.
    pub fn with_placeholder(placeholder: &str) -> Self {
        let mut input = Self::new();
        input.placeholder = Some(placeholder.to_string());
        input
    }

    /// Get the current value.
    pub fn get_value(&self) -> String {
        self.state.lock()
            .map(|s| s.value.clone())
            .unwrap_or_default()
    }

    /// Set the value.
    pub fn set_value(&self, value: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.value = value.to_string();
            state.cursor = value.len();
        }
    }

    /// Clear the input.
    pub fn clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.value.clear();
            state.cursor = 0;
        }
    }

    /// Set callback for when input is submitted (Enter pressed).
    pub fn on_submit(&self, callback: Arc<dyn Fn(&str) + Send + Sync>) {
        if let Ok(mut cb) = self.on_submit.lock() {
            *cb = Some(callback);
        }
    }

    /// Set callback for when escape is pressed.
    pub fn on_escape(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(mut cb) = self.on_escape.lock() {
            *cb = Some(callback);
        }
    }

    /// Set callback for when value changes.
    pub fn on_change(&self, callback: Arc<dyn Fn(&str) + Send + Sync>) {
        if let Ok(mut cb) = self.on_change.lock() {
            *cb = Some(callback);
        }
    }

    /// Handle bracketed paste.
    fn handle_paste(&self, pasted_text: &str) {
        self.set_last_action(LastAction::None);
        self.push_undo();

        // Clean the pasted text - remove newlines and tabs
        let clean_text = pasted_text
            .replace("\r\n", "")
            .replace("\r", "")
            .replace("\n", "")
            .replace("\t", "    ");

        if let Ok(mut state) = self.state.lock() {
            let cursor = state.cursor;
            state.value = format!(
                "{}{}{}",
                &state.value[..cursor],
                clean_text,
                &state.value[cursor..]
            );
            state.cursor += clean_text.len();
        }

        self.notify_change();
    }

    /// Insert a character at the cursor.
    fn insert_character(&self, char: &str) {
        // Undo coalescing: consecutive word chars coalesce into one undo unit
        if char.chars().next().map(|c| c.is_whitespace()).unwrap_or(true)
            || self.get_last_action() != LastAction::TypeWord
        {
            self.push_undo();
        }
        self.set_last_action(LastAction::TypeWord);

        if let Ok(mut state) = self.state.lock() {
            let cursor = state.cursor;
            state.value = format!(
                "{}{}{}",
                &state.value[..cursor],
                char,
                &state.value[cursor..]
            );
            state.cursor += char.len();
        }

        self.notify_change();
    }

    /// Handle backspace (delete character before cursor).
    fn handle_backspace(&self) {
        self.set_last_action(LastAction::None);
        if let Ok(mut state) = self.state.lock() {
            if state.cursor > 0 {
                self.push_undo();
                // Move cursor back and delete the character
                let cursor = state.cursor;
                state.value = format!(
                    "{}{}",
                    &state.value[..cursor - 1],
                    &state.value[cursor..]
                );
                state.cursor -= 1;
                self.notify_change();
            }
        }
    }

    /// Handle forward delete (delete character after cursor).
    fn handle_forward_delete(&self) {
        self.set_last_action(LastAction::None);
        if let Ok(mut state) = self.state.lock() {
            let cursor = state.cursor;
            if cursor < state.value.len() {
                self.push_undo();
                state.value = format!(
                    "{}{}",
                    &state.value[..cursor],
                    &state.value[cursor + 1..]
                );
                self.notify_change();
            }
        }
    }

    /// Delete to the start of the line.
    fn delete_to_line_start(&self) {
        if let Ok(mut state) = self.state.lock() {
            if state.cursor == 0 {
                return;
            }
            self.push_undo();
            let deleted = &state.value[..state.cursor];
            if let Ok(mut kill_ring) = self.kill_ring.lock() {
                use super::kill_ring::PushOptions;
                let accumulate = self.get_last_action() == LastAction::Kill;
                kill_ring.push(deleted, PushOptions { prepend: true, accumulate });
            }
            self.set_last_action(LastAction::Kill);
            state.value = state.value[state.cursor..].to_string();
            state.cursor = 0;
            self.notify_change();
        }
    }

    /// Delete to the end of the line.
    fn delete_to_line_end(&self) {
        if let Ok(mut state) = self.state.lock() {
            if state.cursor >= state.value.len() {
                return;
            }
            self.push_undo();
            let deleted = &state.value[state.cursor..];
            if let Ok(mut kill_ring) = self.kill_ring.lock() {
                use super::kill_ring::PushOptions;
                let accumulate = self.get_last_action() == LastAction::Kill;
                kill_ring.push(deleted, PushOptions { prepend: false, accumulate });
            }
            self.set_last_action(LastAction::Kill);
            state.value = state.value[..state.cursor].to_string();
            self.notify_change();
        }
    }

    /// Delete word backwards.
    fn delete_word_backwards(&self) {
        if let Ok(mut state) = self.state.lock() {
            if state.cursor == 0 {
                return;
            }
            let was_kill = self.get_last_action() == LastAction::Kill;
            self.push_undo();
            let old_cursor = state.cursor;
            let new_cursor = find_word_backward(&state.value, old_cursor);
            let deleted = &state.value[new_cursor..old_cursor];
            if let Ok(mut kill_ring) = self.kill_ring.lock() {
                use super::kill_ring::PushOptions;
                kill_ring.push(deleted, PushOptions { prepend: true, accumulate: was_kill });
            }
            self.set_last_action(LastAction::Kill);
            state.value = format!(
                "{}{}",
                &state.value[..new_cursor],
                &state.value[old_cursor..]
            );
            state.cursor = new_cursor;
            self.notify_change();
        }
    }

    /// Delete word forward.
    fn delete_word_forward(&self) {
        if let Ok(mut state) = self.state.lock() {
            if state.cursor >= state.value.len() {
                return;
            }
            let was_kill = self.get_last_action() == LastAction::Kill;
            self.push_undo();
            let old_cursor = state.cursor;
            let new_cursor = find_word_forward(&state.value, old_cursor);
            let deleted = &state.value[old_cursor..new_cursor];
            if let Ok(mut kill_ring) = self.kill_ring.lock() {
                use super::kill_ring::PushOptions;
                kill_ring.push(deleted, PushOptions { prepend: false, accumulate: was_kill });
            }
            self.set_last_action(LastAction::Kill);
            state.value = format!(
                "{}{}",
                &state.value[..old_cursor],
                &state.value[new_cursor..]
            );
            state.cursor = old_cursor;
            self.notify_change();
        }
    }

    /// Yank from kill ring.
    fn yank(&self) {
        let text = if let Ok(kill_ring) = self.kill_ring.lock() {
            kill_ring.peek().map(|s| s.to_string())
        } else {
            None
        };
        
        if let Some(text) = text {
            self.push_undo();
            if let Ok(mut state) = self.state.lock() {
                let cursor = state.cursor;
                state.value = format!(
                    "{}{}{}",
                    &state.value[..cursor],
                    text,
                    &state.value[cursor..]
                );
                state.cursor += text.len();
            }
            self.set_last_action(LastAction::Yank);
            self.notify_change();
        }
    }

    /// Yank pop (cycle through kill ring).
    fn yank_pop(&self) {
        if self.get_last_action() != LastAction::Yank {
            return;
        }
        if let Ok(kill_ring) = self.kill_ring.lock() {
            if kill_ring.len() <= 1 {
                return;
            }
            drop(kill_ring);
            
            self.push_undo();
            
            if let Ok(mut kill_ring) = self.kill_ring.lock() {
                let prev_text = kill_ring.peek().unwrap_or("").to_string();
                kill_ring.rotate();
                let new_text = kill_ring.peek().unwrap_or("").to_string();
                
                if let Ok(mut state) = self.state.lock() {
                    // Remove previously yanked text
                    let cursor = state.cursor;
                    if cursor >= prev_text.len() {
                        state.value = format!(
                            "{}{}",
                            &state.value[..cursor - prev_text.len()],
                            &state.value[cursor..]
                        );
                        state.cursor -= prev_text.len();
                        
                        // Insert new text
                        let cursor = state.cursor;
                        state.value = format!(
                            "{}{}{}",
                            &state.value[..cursor],
                            new_text,
                            &state.value[cursor..]
                        );
                        state.cursor += new_text.len();
                    }
                }
            }
            self.set_last_action(LastAction::Yank);
            self.notify_change();
        }
    }

    /// Move cursor left.
    fn move_cursor_left(&self) {
        self.set_last_action(LastAction::None);
        if let Ok(mut state) = self.state.lock() {
            if state.cursor > 0 {
                state.cursor -= 1;
            }
        }
    }

    /// Move cursor right.
    fn move_cursor_right(&self) {
        self.set_last_action(LastAction::None);
        if let Ok(mut state) = self.state.lock() {
            if state.cursor < state.value.len() {
                state.cursor += 1;
            }
        }
    }

    /// Move cursor to line start.
    fn move_to_line_start(&self) {
        self.set_last_action(LastAction::None);
        if let Ok(mut state) = self.state.lock() {
            state.cursor = 0;
        }
    }

    /// Move cursor to line end.
    fn move_to_line_end(&self) {
        self.set_last_action(LastAction::None);
        if let Ok(mut state) = self.state.lock() {
            state.cursor = state.value.len();
        }
    }

    /// Move cursor word left.
    fn move_word_left(&self) {
        self.set_last_action(LastAction::None);
        if let Ok(mut state) = self.state.lock() {
            state.cursor = find_word_backward(&state.value, state.cursor);
        }
    }

    /// Move cursor word right.
    fn move_word_right(&self) {
        self.set_last_action(LastAction::None);
        if let Ok(mut state) = self.state.lock() {
            state.cursor = find_word_forward(&state.value, state.cursor);
        }
    }

    /// Undo.
    fn undo(&self) {
        if let Ok(mut undo_stack) = self.undo_stack.lock() {
            if let Some(snapshot) = undo_stack.pop() {
                drop(undo_stack);
                if let Ok(mut state) = self.state.lock() {
                    *state = snapshot;
                }
                self.set_last_action(LastAction::None);
                self.notify_change();
            }
        }
    }

    /// Push current state to undo stack.
    fn push_undo(&self) {
        if let Ok(state) = self.state.lock() {
            if let Ok(mut undo_stack) = self.undo_stack.lock() {
                undo_stack.push(InputState {
                    value: state.value.clone(),
                    cursor: state.cursor,
                });
            }
        }
    }

    /// Get the last action.
    fn get_last_action(&self) -> LastAction {
        self.last_action.lock()
            .map(|a| *a)
            .unwrap_or(LastAction::None)
    }

    /// Set the last action.
    fn set_last_action(&self, action: LastAction) {
        if let Ok(mut a) = self.last_action.lock() {
            *a = action;
        }
    }

    /// Notify change callback.
    fn notify_change(&self) {
        if let Ok(cb) = self.on_change.lock() {
            if let Some(callback) = cb.as_ref() {
                let value = self.get_value();
                callback(&value);
            }
        }
    }

    /// Handle key input.
    pub fn handle_key(&self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};

        match (key.code, key.modifiers) {
            // Submit
            (KeyCode::Enter, _) | (KeyCode::Char('\n'), _) => {
                if let Ok(cb) = self.on_submit.lock() {
                    if let Some(callback) = cb.as_ref() {
                        let value = self.get_value();
                        callback(&value);
                    }
                }
            }
            // Escape
            (KeyCode::Esc, _) => {
                if let Ok(cb) = self.on_escape.lock() {
                    if let Some(callback) = cb.as_ref() {
                        callback();
                    }
                }
            }
            // Navigation
            (KeyCode::Left, KeyModifiers::CONTROL) => self.move_word_left(),
            (KeyCode::Right, KeyModifiers::CONTROL) => self.move_word_right(),
            (KeyCode::Left, _) => self.move_cursor_left(),
            (KeyCode::Right, _) => self.move_cursor_right(),
            (KeyCode::Home, _) => self.move_to_line_start(),
            (KeyCode::End, _) => self.move_to_line_end(),
            // Deletion
            (KeyCode::Backspace, KeyModifiers::CONTROL) => self.delete_word_backwards(),
            (KeyCode::Delete, KeyModifiers::CONTROL) => self.delete_word_forward(),
            (KeyCode::Backspace, _) => self.handle_backspace(),
            (KeyCode::Delete, _) => self.handle_forward_delete(),
            // Character input
            (KeyCode::Char(c), KeyModifiers::CONTROL) => {
                match c {
                    'a' => self.move_to_line_start(),
                    'e' => self.move_to_line_end(),
                    'b' => self.move_cursor_left(),
                    'f' => self.move_cursor_right(),
                    'w' => self.delete_word_backwards(),
                    'k' => self.delete_to_line_end(),
                    'u' => self.delete_to_line_start(),
                    'y' => self.yank(),
                    '_' | '/' => self.undo(),
                    _ => {}
                }
            }
            (KeyCode::Char(c), KeyModifiers::ALT) => {
                match c {
                    'b' => self.move_word_left(),
                    'f' => self.move_word_right(),
                    'd' => self.delete_word_forward(),
                    _ => {}
                }
            }
            (KeyCode::Char(c), _) => {
                self.insert_character(&c.to_string());
            }
            _ => {}
        }
    }
}

impl Default for Input {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Input {
    fn render(&self, width: usize) -> Vec<String> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let focused = *self.focused.lock().unwrap_or_else(|e| e.into_inner());
        
        let prompt_len = visible_width(&self.prompt);
        let available_width = width.saturating_sub(prompt_len);

        if available_width == 0 {
            return vec![self.prompt.clone()];
        }

        let value = &state.value;
        let cursor = state.cursor;

        // Show placeholder if empty and not focused
        if value.is_empty() && !focused {
            if let Some(ref placeholder) = self.placeholder {
                let dimmed = format!("\x1b[90m{}\x1b[0m", placeholder);
                return vec![format!("{}{}", self.prompt, dimmed)];
            }
        }

        // Calculate horizontal scrolling
        let total_width = visible_width(value);
        let cursor_col = visible_width(&value[..cursor.min(value.len())]);

        let visible_text = if total_width < available_width {
            value.clone()
        } else {
            // Need to scroll
            let scroll_width = if cursor == value.len() {
                available_width.saturating_sub(1)
            } else {
                available_width
            };

            if scroll_width == 0 {
                String::new()
            } else {
                let half_width = scroll_width / 2;
                let start_col = if cursor_col < half_width {
                    0
                } else if cursor_col > total_width.saturating_sub(half_width) {
                    total_width.saturating_sub(scroll_width)
                } else {
                    cursor_col.saturating_sub(half_width)
                };
                slice_by_column(value, start_col, scroll_width, true)
            }
        };

        // Build line with cursor
        let cursor_display = if total_width < available_width {
            cursor
        } else if visible_text.is_empty() {
            0
        } else {
            // Find cursor position in visible text
            let scroll_start_col = if total_width < available_width {
                0
            } else {
                let scroll_width = if cursor == value.len() {
                    available_width.saturating_sub(1)
                } else {
                    available_width
                };
                let half_width = scroll_width / 2;
                if cursor_col < half_width {
                    0
                } else if cursor_col > total_width.saturating_sub(half_width) {
                    total_width.saturating_sub(scroll_width)
                } else {
                    cursor_col.saturating_sub(half_width)
                }
            };
            cursor.saturating_sub(
                value.chars().take_while(|c| {
                    visible_width(&value[..value.chars().position(|ch| ch == *c).unwrap_or(0)]) < scroll_start_col
                }).count()
            )
        };

        // Insert cursor marker and reverse video for cursor character
        let chars: Vec<char> = visible_text.chars().collect();
        let before_cursor: String = chars.iter().take(cursor_display).collect();
        let at_cursor = chars.get(cursor_display).copied().unwrap_or(' ');
        let after_cursor: String = chars.iter().skip(cursor_display + 1).collect();

        let marker = if focused { CURSOR_MARKER } else { "" };
        let cursor_char = if focused {
            format!("\x1b[7m{}\x1b[27m", at_cursor)
        } else {
            at_cursor.to_string()
        };

        let line = format!(
            "{}{}{}{}{}{}",
            self.prompt,
            marker,
            before_cursor,
            cursor_char,
            after_cursor,
            "\x1b[0m"
        );

        vec![line]
    }

    fn invalidate(&self) {
        // No cached state to invalidate
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Focusable for Input {
    fn set_focused(&self, focused: bool) {
        if let Ok(mut f) = self.focused.lock() {
            *f = focused;
        }
    }

    fn is_focused(&self) -> bool {
        *self.focused.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_input_value() {
        let input = Input::new();
        input.set_value("hello");
        assert_eq!(input.get_value(), "hello");
    }

    #[test]
    fn test_input_clear() {
        let input = Input::new();
        input.set_value("hello");
        input.clear();
        assert_eq!(input.get_value(), "");
    }

    #[test]
    fn test_input_render() {
        let input = Input::new();
        input.set_value("hello");
        let lines = input.render(20);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("hello"));
    }

    #[test]
    fn test_input_placeholder() {
        let input = Input::with_placeholder("Enter text...");
        let lines = input.render(20);
        assert!(lines[0].contains("Enter text..."));
    }
}