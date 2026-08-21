//! Keybinding system for TUI.
//!
//! Provides configurable keyboard shortcuts for TUI actions.

use std::collections::HashMap;
use std::sync::Mutex;

use crossterm::event::{KeyCode, KeyModifiers};

/// A keybinding identifier.
pub type KeybindingId = &'static str;

/// Predefined keybinding IDs.
pub mod keys {
    use super::KeybindingId;
    
    pub const PAGE_UP: KeybindingId = "tui.altScreen.pageUp";
    pub const PAGE_DOWN: KeybindingId = "tui.altScreen.pageDown";
    pub const HALF_PAGE_UP: KeybindingId = "tui.altScreen.halfPageUp";
    pub const HALF_PAGE_DOWN: KeybindingId = "tui.altScreen.halfPageDown";
    pub const LINE_UP: KeybindingId = "tui.altScreen.lineUp";
    pub const LINE_DOWN: KeybindingId = "tui.altScreen.lineDown";
    pub const TOP: KeybindingId = "tui.altScreen.top";
    pub const BOTTOM: KeybindingId = "tui.altScreen.bottom";
    pub const SEARCH: KeybindingId = "tui.altScreen.search";
    pub const SEARCH_NEXT: KeybindingId = "tui.altScreen.searchNext";
    pub const SEARCH_PREVIOUS: KeybindingId = "tui.altScreen.searchPrevious";
    pub const SEARCH_CLOSE: KeybindingId = "tui.altScreen.searchClose";
    pub const PREVIOUS_PROMPT: KeybindingId = "tui.altScreen.previousPrompt";
    pub const NEXT_PROMPT: KeybindingId = "tui.altScreen.nextPrompt";
}

/// A key combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyCombo {
    /// Create a new key combination.
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    /// Create from just a key code (no modifiers).
    pub fn from_code(code: KeyCode) -> Self {
        Self {
            code,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Add Ctrl modifier.
    pub fn ctrl(self) -> Self {
        Self {
            modifiers: self.modifiers | KeyModifiers::CONTROL,
            ..self
        }
    }

    /// Add Shift modifier.
    pub fn shift(self) -> Self {
        Self {
            modifiers: self.modifiers | KeyModifiers::SHIFT,
            ..self
        }
    }

    /// Add Alt modifier.
    pub fn alt(self) -> Self {
        Self {
            modifiers: self.modifiers | KeyModifiers::ALT,
            ..self
        }
    }
}

/// Keybindings manager.
pub struct Keybindings {
    bindings: Mutex<HashMap<KeybindingId, Vec<KeyCombo>>>,
    reverse: Mutex<HashMap<KeyCombo, KeybindingId>>,
}

impl Keybindings {
    /// Create a new keybindings manager with default bindings.
    pub fn new() -> Self {
        let bindings = Self::default_bindings();
        let reverse = Self::build_reverse(&bindings);
        Self {
            bindings: Mutex::new(bindings),
            reverse: Mutex::new(reverse),
        }
    }

    /// Get default keybindings.
    fn default_bindings() -> HashMap<KeybindingId, Vec<KeyCombo>> {
        use KeyCode::*;
        use KeyModifiers as M;

        let mut map = HashMap::new();

        // Navigation
        map.insert(keys::PAGE_UP, vec![KeyCombo::new(PageUp, M::SHIFT)]);
        map.insert(keys::PAGE_DOWN, vec![KeyCombo::new(PageDown, M::SHIFT)]);
        map.insert(keys::HALF_PAGE_UP, vec![]);
        map.insert(keys::HALF_PAGE_DOWN, vec![]);
        map.insert(keys::LINE_UP, vec![]);
        map.insert(keys::LINE_DOWN, vec![]);
        map.insert(keys::TOP, vec![KeyCombo::new(Home, M::CONTROL)]);
        map.insert(keys::BOTTOM, vec![KeyCombo::new(End, M::CONTROL)]);

        // Search
        map.insert(keys::SEARCH, vec![KeyCombo::from_code(Char('/')).ctrl()]);
        map.insert(keys::SEARCH_NEXT, vec![KeyCombo::from_code(Char('n'))]);
        map.insert(keys::SEARCH_PREVIOUS, vec![KeyCombo::from_code(Char('N')).shift()]);
        map.insert(keys::SEARCH_CLOSE, vec![KeyCombo::from_code(Esc)]);

        // Prompt navigation
        map.insert(keys::PREVIOUS_PROMPT, vec![KeyCombo::from_code(Char('p')).alt()]);
        map.insert(keys::NEXT_PROMPT, vec![KeyCombo::from_code(Char('n')).alt()]);

        map
    }

    /// Build reverse mapping from key combo to binding ID.
    fn build_reverse(bindings: &HashMap<KeybindingId, Vec<KeyCombo>>) -> HashMap<KeyCombo, KeybindingId> {
        let mut reverse = HashMap::new();
        for (id, combos) in bindings {
            for combo in combos {
                reverse.insert(*combo, *id);
            }
        }
        reverse
    }

    /// Check if a key event matches a keybinding.
    pub fn matches(&self, event: &crossterm::event::KeyEvent, binding_id: KeybindingId) -> bool {
        if let Ok(bindings) = self.bindings.lock() {
            if let Some(combos) = bindings.get(binding_id) {
                let combo = KeyCombo::new(event.code, event.modifiers);
                return combos.contains(&combo);
            }
        }
        false
    }

    /// Get the keybinding ID for a key event.
    pub fn get_binding(&self, event: &crossterm::event::KeyEvent) -> Option<KeybindingId> {
        let combo = KeyCombo::new(event.code, event.modifiers);
        self.reverse.lock().ok()?.get(&combo).copied()
    }

    /// Set a keybinding.
    pub fn set(&self, binding_id: KeybindingId, combos: Vec<KeyCombo>) {
        if let Ok(mut bindings) = self.bindings.lock() {
            bindings.insert(binding_id, combos);
        }
        // Rebuild reverse mapping
        if let Ok(bindings) = self.bindings.lock() {
            if let Ok(mut reverse) = self.reverse.lock() {
                *reverse = Self::build_reverse(&bindings);
            }
        }
    }
}

impl Default for Keybindings {
    fn default() -> Self {
        Self::new()
    }
}