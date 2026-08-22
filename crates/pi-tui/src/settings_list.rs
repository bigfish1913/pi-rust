//! Settings list component.
//!
//! A list of settings with navigation and modification support.

use std::any::Any;
use std::sync::{Arc, Mutex};

use super::component::Component;
use super::fuzzy::fuzzy_filter;
use crate::utils::{truncate_to_width, visible_width};

/// A setting item.
#[derive(Debug, Clone)]
pub struct SettingItem {
    /// Setting key.
    pub key: String,
    /// Display label.
    pub label: String,
    /// Setting value.
    pub value: String,
    /// Description.
    pub description: Option<String>,
    /// Setting type.
    pub setting_type: SettingType,
}

impl SettingItem {
    /// Create a new setting item.
    pub fn new(key: &str, label: &str, value: &str) -> Self {
        Self {
            key: key.to_string(),
            label: label.to_string(),
            value: value.to_string(),
            description: None,
            setting_type: SettingType::String,
        }
    }

    /// Add description.
    pub fn with_description(mut self, description: &str) -> Self {
        self.description = Some(description.to_string());
        self
    }

    /// Set setting type.
    pub fn with_type(mut self, setting_type: SettingType) -> Self {
        self.setting_type = setting_type;
        self
    }
}

/// Setting type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SettingType {
    /// String setting.
    String,
    /// Boolean setting.
    Boolean,
    /// Number setting.
    Number,
    /// Enum setting.
    Enum,
}

/// Settings list theme.
#[derive(Debug, Clone)]
pub struct SettingsListTheme {
    pub key: fn(&str) -> String,
    pub value: fn(&str) -> String,
    pub description: fn(&str) -> String,
    pub selected: fn(&str) -> String,
    pub selected_prefix: fn(&str) -> String,
}

impl Default for SettingsListTheme {
    fn default() -> Self {
        Self {
            key: |s| format!("\x1b[1m{}\x1b[0m", s),
            value: |s| format!("\x1b[36m{}\x1b[0m", s),
            description: |s| format!("\x1b[90m{}\x1b[0m", s),
            selected: |s| format!("\x1b[7m{}\x1b[0m", s),
            selected_prefix: |s| format!("\x1b[32m{}\x1b[0m", s),
        }
    }
}

/// Settings list component.
pub struct SettingsList {
    settings: Mutex<Vec<SettingItem>>,
    filtered: Mutex<Vec<SettingItem>>,
    selected_index: Mutex<usize>,
    max_visible: usize,
    theme: SettingsListTheme,
    filter: Mutex<String>,
    on_change: Mutex<Option<Arc<dyn Fn(&str, &str) + Send + Sync>>>,
    on_select: Mutex<Option<Arc<dyn Fn(&SettingItem) + Send + Sync>>>,
}

impl SettingsList {
    /// Create a new settings list.
    pub fn new(settings: Vec<SettingItem>) -> Self {
        Self {
            filtered: Mutex::new(settings.clone()),
            settings: Mutex::new(settings),
            selected_index: Mutex::new(0),
            max_visible: 10,
            theme: SettingsListTheme::default(),
            filter: Mutex::new(String::new()),
            on_change: Mutex::new(None),
            on_select: Mutex::new(None),
        }
    }

    /// Create with theme.
    pub fn with_theme(settings: Vec<SettingItem>, theme: SettingsListTheme) -> Self {
        Self {
            filtered: Mutex::new(settings.clone()),
            settings: Mutex::new(settings),
            selected_index: Mutex::new(0),
            max_visible: 10,
            theme,
            filter: Mutex::new(String::new()),
            on_change: Mutex::new(None),
            on_select: Mutex::new(None),
        }
    }

    /// Set filter.
    pub fn set_filter(&self, filter: &str) {
        if let Ok(mut f) = self.filter.lock() {
            *f = filter.to_string();
        }

        if let Ok(settings) = self.settings.lock() {
            let filtered = fuzzy_filter(&*settings, filter, |item| item.key.as_str());
            if let Ok(mut f) = self.filtered.lock() {
                *f = filtered;
            }
        }

        // Reset selection
        if let Ok(mut idx) = self.selected_index.lock() {
            *idx = 0;
        }
    }

    /// Get selected item.
    pub fn get_selected(&self) -> Option<SettingItem> {
        let idx = *self.selected_index.lock().ok()?;
        let filtered = self.filtered.lock().ok()?;
        filtered.get(idx).cloned()
    }

    /// Set selected index.
    pub fn set_selected(&self, index: usize) {
        if let Ok(mut idx) = self.selected_index.lock() {
            if let Ok(filtered) = self.filtered.lock() {
                *idx = index.min(filtered.len().saturating_sub(1));
            }
        }
    }

    /// Move selection up.
    pub fn move_up(&self) {
        if let Ok(mut idx) = self.selected_index.lock() {
            if let Ok(filtered) = self.filtered.lock() {
                if !filtered.is_empty() {
                    if *idx == 0 {
                        *idx = filtered.len() - 1;
                    } else {
                        *idx -= 1;
                    }
                }
            }
        }
    }

    /// Move selection down.
    pub fn move_down(&self) {
        if let Ok(mut idx) = self.selected_index.lock() {
            if let Ok(filtered) = self.filtered.lock() {
                if !filtered.is_empty() {
                    if *idx >= filtered.len() - 1 {
                        *idx = 0;
                    } else {
                        *idx += 1;
                    }
                }
            }
        }
    }

    /// Toggle boolean setting.
    pub fn toggle(&self) {
        if let Some(item) = self.get_selected() {
            if item.setting_type == SettingType::Boolean {
                let new_value = if item.value == "true" { "false" } else { "true" };
                self.update_value(&item.key, new_value);

                if let Ok(cb) = self.on_change.lock() {
                    if let Some(callback) = cb.as_ref() {
                        callback(&item.key, new_value);
                    }
                }
            }
        }
    }

    /// Update a setting value.
    pub fn update_value(&self, key: &str, value: &str) {
        if let Ok(mut settings) = self.settings.lock() {
            for setting in settings.iter_mut() {
                if setting.key == key {
                    setting.value = value.to_string();
                    break;
                }
            }
        }

        // Update filtered list
        if let Ok(filter) = self.filter.lock() {
            self.set_filter(&filter);
        }
    }

    /// Set change callback.
    pub fn on_change(&self, callback: Arc<dyn Fn(&str, &str) + Send + Sync>) {
        if let Ok(mut cb) = self.on_change.lock() {
            *cb = Some(callback);
        }
    }

    /// Set select callback.
    pub fn on_select(&self, callback: Arc<dyn Fn(&SettingItem) + Send + Sync>) {
        if let Ok(mut cb) = self.on_select.lock() {
            *cb = Some(callback);
        }
    }

    /// Confirm selection.
    pub fn confirm(&self) {
        if let Some(item) = self.get_selected() {
            if let Ok(cb) = self.on_select.lock() {
                if let Some(callback) = cb.as_ref() {
                    callback(&item);
                }
            }
        }
    }

    /// Render a single setting item.
    fn render_item(&self, item: &SettingItem, is_selected: bool, width: usize) -> String {
        let prefix = if is_selected { "→ " } else { "  " };

        let key_width = 20;
        let value_width = 15;

        let key_display = truncate_to_width(&item.label, key_width, "");
        let value_display = truncate_to_width(&item.value, value_width, "");

        let main_line = if is_selected {
            format!(
                "{}{} {} {}",
                (self.theme.selected_prefix)(prefix),
                (self.theme.key)(&key_display),
                (self.theme.selected)(&value_display),
                ""
            )
        } else {
            format!(
                "{}{} {}",
                prefix,
                (self.theme.key)(&key_display),
                (self.theme.value)(&value_display)
            )
        };

        // Add description if available
        if let Some(ref desc) = item.description {
            let desc_width = width.saturating_sub(visible_width(&main_line) + 2);
            if desc_width > 10 {
                let desc_display = truncate_to_width(desc, desc_width, "");
                format!("{} {}", main_line, (self.theme.description)(&desc_display))
            } else {
                main_line
            }
        } else {
            main_line
        }
    }
}

impl Component for SettingsList {
    fn render(&self, width: usize) -> Vec<String> {
        let filtered = if let Ok(f) = self.filtered.lock() {
            f.clone()
        } else {
            return Vec::new();
        };

        let selected = *self.selected_index.lock().unwrap();

        if filtered.is_empty() {
            return vec![(self.theme.description)("  No settings found")];
        }

        // Calculate visible range
        let start = if filtered.len() <= self.max_visible {
            0
        } else {
            selected.saturating_sub(self.max_visible / 2)
                .min(filtered.len().saturating_sub(self.max_visible))
        };
        let end = (start + self.max_visible).min(filtered.len());

        let mut lines = Vec::new();

        for i in start..end {
            if let Some(item) = filtered.get(i) {
                lines.push(self.render_item(item, i == selected, width));
            }
        }

        // Add scroll indicator
        if start > 0 || end < filtered.len() {
            let info = format!("  ({}/{})", selected + 1, filtered.len());
            lines.push((self.theme.description)(&info));
        }

        lines
    }

    fn invalidate(&self) {
        // No cached state
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_settings_list_new() {
        let settings = vec![
            SettingItem::new("theme", "Theme", "dark"),
            SettingItem::new("font", "Font Size", "14"),
        ];
        let list = SettingsList::new(settings);

        let lines = list.render(60);
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_settings_navigation() {
        let settings = vec![
            SettingItem::new("a", "A", "1"),
            SettingItem::new("b", "B", "2"),
        ];
        let list = SettingsList::new(settings);

        assert_eq!(list.get_selected().unwrap().key, "a");
        list.move_down();
        assert_eq!(list.get_selected().unwrap().key, "b");
        list.move_down(); // Wrap around
        assert_eq!(list.get_selected().unwrap().key, "a");
    }

    #[test]
    fn test_settings_filter() {
        let settings = vec![
            SettingItem::new("theme", "Theme", "dark"),
            SettingItem::new("font", "Font Size", "14"),
        ];
        let list = SettingsList::new(settings);

        list.set_filter("theme");
        let selected = list.get_selected();
        assert!(selected.is_some());
        assert_eq!(selected.unwrap().key, "theme");
    }
}