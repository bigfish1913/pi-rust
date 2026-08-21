//! Overlay system for modal dialogs and selectors.
//!
//! Provides overlay components that can be shown on top of the main content.

use std::any::Any;
use std::sync::{Arc, Mutex};

use super::component::Component;
use super::tui::OverlayOptions;
use crate::ansi::{bold, dim, visible_width};

/// Overlay manager for managing multiple overlays.
pub struct OverlayManager {
    overlays: Mutex<Vec<OverlayEntry>>,
}

struct OverlayEntry {
    component: Arc<dyn Component>,
    options: OverlayOptions,
    visible: bool,
    z_index: usize,
}

// Clone implementation for OverlayEntry (partial - we don't clone the component)
impl Clone for OverlayEntry {
    fn clone(&self) -> Self {
        Self {
            component: self.component.clone(),
            options: self.options.clone(),
            visible: self.visible,
            z_index: self.z_index,
        }
    }
}

impl OverlayManager {
    /// Create a new overlay manager.
    pub fn new() -> Self {
        Self {
            overlays: Mutex::new(Vec::new()),
        }
    }

    /// Add an overlay.
    pub fn add(&self, component: Arc<dyn Component>, options: OverlayOptions) -> OverlayHandle {
        let entry = OverlayEntry {
            component,
            options,
            visible: true,
            z_index: self.overlays.lock().map(|o| o.len()).unwrap_or(0),
        };

        let handle = OverlayHandle {
            id: entry.z_index,
            manager: Arc::new(self.clone_manager()),
        };

        if let Ok(mut overlays) = self.overlays.lock() {
            overlays.push(entry);
        }

        handle
    }

    /// Remove an overlay by ID.
    pub fn remove(&self, id: usize) {
        if let Ok(mut overlays) = self.overlays.lock() {
            overlays.retain(|e| e.z_index != id);
        }
    }

    /// Set overlay visibility.
    pub fn set_visible(&self, id: usize, visible: bool) {
        if let Ok(mut overlays) = self.overlays.lock() {
            if let Some(entry) = overlays.iter_mut().find(|e| e.z_index == id) {
                entry.visible = visible;
            }
        }
    }

    /// Get visible overlays sorted by z-index.
    pub fn get_visible(&self) -> Vec<(Arc<dyn Component>, OverlayOptions)> {
        self.overlays.lock()
            .map(|overlays| {
                overlays.iter()
                    .filter(|e| e.visible)
                    .map(|e| (e.component.clone(), e.options.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn clone_manager(&self) -> Self {
        Self {
            overlays: Mutex::new(self.overlays.lock().map(|o| o.clone()).unwrap_or_default()),
        }
    }
}

impl Default for OverlayManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle to control an overlay.
pub struct OverlayHandle {
    id: usize,
    manager: Arc<OverlayManager>,
}

impl OverlayHandle {
    /// Hide the overlay.
    pub fn hide(&self) {
        self.manager.remove(self.id);
    }

    /// Set visibility.
    pub fn set_visible(&self, visible: bool) {
        self.manager.set_visible(self.id, visible);
    }

    /// Check if visible.
    pub fn is_visible(&self) -> bool {
        self.manager.overlays.lock()
            .map(|o| o.iter().any(|e| e.z_index == self.id && e.visible))
            .unwrap_or(false)
    }
}

/// Selector item.
#[derive(Debug, Clone)]
pub struct SelectorItem {
    pub label: String,
    pub value: String,
    pub description: Option<String>,
}

/// Selector - A component for selecting from a list of items.
pub struct Selector {
    items: Mutex<Vec<SelectorItem>>,
    selected: Mutex<usize>,
    query: Mutex<String>,
    padding_x: usize,
    max_visible: usize,
    on_select: Mutex<Option<Arc<dyn Fn(&SelectorItem) + Send + Sync>>>,
}

impl Selector {
    /// Create a new selector.
    pub fn new(items: Vec<SelectorItem>, max_visible: usize) -> Self {
        Self {
            items: Mutex::new(items),
            selected: Mutex::new(0),
            query: Mutex::new(String::new()),
            padding_x: 1,
            max_visible,
            on_select: Mutex::new(None),
        }
    }

    /// Set items.
    pub fn set_items(&self, items: Vec<SelectorItem>) {
        if let Ok(mut i) = self.items.lock() {
            *i = items;
        }
    }

    /// Get selected index.
    pub fn selected(&self) -> usize {
        self.selected.lock().map(|s| *s).unwrap_or(0)
    }

    /// Get selected item.
    pub fn selected_item(&self) -> Option<SelectorItem> {
        let items = self.items.lock().ok()?;
        let selected = self.selected.lock().ok()?;
        items.get(*selected).cloned()
    }

    /// Move selection up.
    pub fn select_up(&self) {
        if let Ok(mut selected) = self.selected.lock() {
            if *selected > 0 {
                *selected -= 1;
            }
        }
    }

    /// Move selection down.
    pub fn select_down(&self) {
        if let Ok(mut selected) = self.selected.lock() {
            let items = self.items.lock();
            if let Ok(items) = items {
                if *selected < items.len().saturating_sub(1) {
                    *selected += 1;
                }
            }
        }
    }

    /// Set filter query.
    pub fn set_query(&self, query: &str) {
        if let Ok(mut q) = self.query.lock() {
            *q = query.to_string();
        }
        // Reset selection
        if let Ok(mut selected) = self.selected.lock() {
            *selected = 0;
        }
    }

    /// Set callback for when an item is selected.
    pub fn on_select(&self, callback: Arc<dyn Fn(&SelectorItem) + Send + Sync>) {
        if let Ok(mut cb) = self.on_select.lock() {
            *cb = Some(callback);
        }
    }

    /// Confirm selection.
    pub fn confirm(&self) {
        if let Some(item) = self.selected_item() {
            if let Ok(cb) = self.on_select.lock() {
                if let Some(callback) = cb.as_ref() {
                    callback(&item);
                }
            }
        }
    }

    /// Get filtered items based on query.
    fn filtered_items(&self) -> Vec<(usize, SelectorItem)> {
        let items = self.items.lock().map(|i| i.clone()).unwrap_or_default();
        let query = self.query.lock().map(|q| q.to_lowercase()).unwrap_or_default();

        if query.is_empty() {
            return items.into_iter().enumerate().collect();
        }

        items.into_iter()
            .enumerate()
            .filter(|(_, item)| {
                item.label.to_lowercase().contains(&query) ||
                item.value.to_lowercase().contains(&query) ||
                item.description.as_ref().map(|d| d.to_lowercase().contains(&query)).unwrap_or(false)
            })
            .collect()
    }
}

impl Component for Selector {
    fn render(&self, width: usize) -> Vec<String> {
        let filtered = self.filtered_items();
        let mut lines = Vec::new();

        let selected = self.selected.lock().map(|s| *s).unwrap_or(0);
        let query = self.query.lock().map(|q| q.clone()).unwrap_or_default();

        // Search box
        let mut search_line = " ".repeat(self.padding_x);
        search_line.push_str(&bold("Search: "));
        search_line.push_str(&query);
        lines.push(search_line);
        lines.push(String::new());

        // Items
        let visible_count = self.max_visible.min(filtered.len());
        let scroll_offset = selected.saturating_sub(self.max_visible / 2);

        for (i, (_original_idx, item)) in filtered.iter()
            .skip(scroll_offset)
            .take(visible_count)
            .enumerate()
        {
            let is_selected = scroll_offset + i == selected;
            let mut line = " ".repeat(self.padding_x);

            if is_selected {
                line.push_str(&format!("\x1b[7m> {}\x1b[0m", item.label));
                if let Some(desc) = &item.description {
                    line.push_str(&format!(" - {}", desc));
                }
            } else {
                line.push_str(&format!("  {}", item.label));
                if let Some(desc) = &item.description {
                    line.push_str(&dim(&format!(" - {}", desc)));
                }
            }

            // Truncate to width
            if line.len() > width {
                line = line[..width].to_string();
            }

            lines.push(line);
        }

        if filtered.is_empty() {
            let mut line = " ".repeat(self.padding_x);
            line.push_str(&dim("No results found"));
            lines.push(line);
        }

        // Footer
        lines.push(String::new());
        let mut footer = " ".repeat(self.padding_x);
        footer.push_str(&dim(&format!("{} items", filtered.len())));
        footer.push_str(&dim(" | Enter: Select | Esc: Cancel"));
        lines.push(footer);

        lines
    }

    fn invalidate(&self) {
        // No cache
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Dialog - A simple message dialog.
pub struct Dialog {
    title: Mutex<String>,
    message: Mutex<String>,
    buttons: Mutex<Vec<String>>,
    selected: Mutex<usize>,
}

impl Dialog {
    /// Create a new dialog.
    pub fn new(title: impl Into<String>, message: impl Into<String>, buttons: Vec<String>) -> Self {
        Self {
            title: Mutex::new(title.into()),
            message: Mutex::new(message.into()),
            buttons: Mutex::new(buttons),
            selected: Mutex::new(0),
        }
    }

    /// Create an info dialog.
    pub fn info(message: impl Into<String>) -> Self {
        Self::new("Info", message, vec!["OK".to_string()])
    }

    /// Create a confirm dialog.
    pub fn confirm(message: impl Into<String>) -> Self {
        Self::new("Confirm", message, vec!["Yes".to_string(), "No".to_string()])
    }

    /// Get selected button index.
    pub fn selected(&self) -> usize {
        self.selected.lock().map(|s| *s).unwrap_or(0)
    }

    /// Move selection left.
    pub fn select_left(&self) {
        if let Ok(mut selected) = self.selected.lock() {
            if *selected > 0 {
                *selected -= 1;
            }
        }
    }

    /// Move selection right.
    pub fn select_right(&self) {
        if let Ok(mut selected) = self.selected.lock() {
            let buttons = self.buttons.lock();
            if let Ok(buttons) = buttons {
                if *selected < buttons.len().saturating_sub(1) {
                    *selected += 1;
                }
            }
        }
    }
}

impl Component for Dialog {
    fn render(&self, width: usize) -> Vec<String> {
        let title = self.title.lock().map(|t| t.clone()).unwrap_or_default();
        let message = self.message.lock().map(|m| m.clone()).unwrap_or_default();
        let buttons = self.buttons.lock().map(|b| b.clone()).unwrap_or_default();
        let selected = self.selected.lock().map(|s| *s).unwrap_or(0);

        let mut lines = Vec::new();

        // Border top
        let border_width = width.min(60);
        lines.push(format!("┌{}┐", "─".repeat(border_width.saturating_sub(2))));

        // Title
        lines.push(format!("│ {} {}│", bold(&title), " ".repeat(border_width.saturating_sub(4) - title.len())));

        // Separator
        lines.push(format!("├{}┤", "─".repeat(border_width.saturating_sub(2))));

        // Message
        for line in message.lines() {
            let padded = format!("│ {}{}", line, " ".repeat(border_width.saturating_sub(3) - visible_width(line)));
            lines.push(padded);
        }

        // Separator
        lines.push(format!("├{}┤", "─".repeat(border_width.saturating_sub(2))));

        // Buttons
        let mut button_line = "│ ".to_string();
        for (i, button) in buttons.iter().enumerate() {
            if i == selected {
                button_line.push_str(&format!("[{}] ", bold(button)));
            } else {
                button_line.push_str(&format!("[{}] ", button));
            }
        }
        button_line.push_str(&" ".repeat(border_width.saturating_sub(3) - visible_width(&button_line)));
        button_line.push('│');
        lines.push(button_line);

        // Border bottom
        lines.push(format!("└{}┘", "─".repeat(border_width.saturating_sub(2))));

        lines
    }

    fn invalidate(&self) {
        // No cache
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_selector() {
        let items = vec![
            SelectorItem { label: "Item 1".into(), value: "1".into(), description: None },
            SelectorItem { label: "Item 2".into(), value: "2".into(), description: None },
        ];
        let selector = Selector::new(items, 5);
        assert_eq!(selector.selected(), 0);
        selector.select_down();
        assert_eq!(selector.selected(), 1);
    }

    #[test]
    fn test_dialog() {
        let dialog = Dialog::info("Hello, World!");
        let lines = dialog.render(40);
        assert!(!lines.is_empty());
    }
}