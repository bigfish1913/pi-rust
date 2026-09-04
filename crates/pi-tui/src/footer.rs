//! Footer component for TUI.
//!
//! Based on TypeScript implementation:
//! packages/coding-agent/src/modes/interactive/components/footer.ts
//!
//! The TS footer is a two-line status bar: a dim `pwd (branch)` line then a
//! stats line with the model right-aligned. Our Rust host carries less session
//! state (no pwd/token/cost aggregation here), so this is a **styled
//! single-row** version: a thin theme-colored separator border, then
//! `[model]` in accent brackets, a status word colored by run state, and the
//! keybinding hints dimmed on the right. The model is right-aligned to mirror
//! pi's layout; an optional thinking-level suffix (pi shows
//! `model • thinking off`) follows the model when set.

use std::any::Any;
use std::sync::Mutex;

use super::component::Component;
use crate::theme::theme;
use crate::utils::{truncate_to_width, visible_width};

/// Footer component that displays status information.
///
/// Mirrors TypeScript FooterComponent class (simplified version).
pub struct FooterComponent {
    /// Current status text
    status: Mutex<String>,
    /// Model name
    model: Mutex<String>,
    /// Keybinding hints
    hints: Mutex<String>,
    /// Optional thinking-level suffix shown after the model (pi parity:
    /// `model • thinking off` / `model • medium`). `None` → not shown.
    thinking_level: Mutex<Option<String>>,
}

impl FooterComponent {
    /// Create a new footer component.
    pub fn new() -> Self {
        Self {
            status: Mutex::new(String::new()),
            model: Mutex::new("claude-sonnet-5".to_string()),
            // Compact default so the right-aligned hints actually fit
            // alongside the model on an 80-col terminal (the render path
            // drops hints when `left + right + 2 > width`; the old 50-char
            // default was silently dropped on every common width).
            hints: Mutex::new("Enter send · Ctrl+C abort · /help".to_string()),
            thinking_level: Mutex::new(None),
        }
    }

    /// Set the status text.
    pub fn set_status(&self, status: &str) {
        if let Ok(mut s) = self.status.lock() {
            *s = status.to_string();
        }
    }

    /// Set the model name.
    pub fn set_model(&self, model: &str) {
        if let Ok(mut m) = self.model.lock() {
            *m = model.to_string();
        }
    }

    /// Set the keybinding hints.
    pub fn set_hints(&self, hints: &str) {
        if let Ok(mut h) = self.hints.lock() {
            *h = hints.to_string();
        }
    }

    /// Set the thinking-level suffix shown after the model name (pi parity).
    /// Pass `None` (or [`Self::clear_thinking_level`]) to hide it.
    pub fn set_thinking_level(&self, level: Option<&str>) {
        if let Ok(mut t) = self.thinking_level.lock() {
            *t = level.map(|s| s.to_string());
        }
    }

    /// Clear the thinking-level suffix.
    pub fn clear_thinking_level(&self) {
        if let Ok(mut t) = self.thinking_level.lock() {
            *t = None;
        }
    }

    /// Get the status text.
    pub fn get_status(&self) -> String {
        self.status.lock().unwrap().clone()
    }

    /// Get the model name.
    pub fn get_model(&self) -> String {
        self.model.lock().unwrap().clone()
    }
}

impl Default for FooterComponent {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for FooterComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let colors = theme().colors;
        let status = self.status.lock().unwrap();
        let model = self.model.lock().unwrap();
        let hints = self.hints.lock().unwrap();
        let thinking_level = self.thinking_level.lock().unwrap();

        // Line 1 — a thin themed separator border (visual separation from the
        // editor/input area above, matching pi's footer weight).
        let sep = colors.border_muted.fg(&"─".repeat(width.max(1)));

        // Line 2 — the status row. Left side: `[model]` in accent brackets
        // (dim model name inside) + a status word colored by run state.
        // Right side: the keybinding hints, dimmed. Right-aligned to mirror
        // pi's model-on-the-right layout.
        let model_label = if !model.is_empty() {
            // pi renders the model dim; the brackets are the accent cue.
            let inner = if let Some(ref lvl) = *thinking_level {
                format!("{} • {}", model, lvl)
            } else {
                model.clone()
            };
            format!(
                "{}{} {}",
                colors.accent.fg("["),
                colors.dim.fg(&inner),
                colors.accent.fg("]")
            )
        } else {
            String::new()
        };

        // Status word colored by content: working/aborting → accent, error →
        // error, idle/blank → muted. Anything else stays as-is.
        let status_word = if status.is_empty() {
            String::new()
        } else {
            let lower = status.to_ascii_lowercase();
            let color = if lower.contains("abort") {
                colors.error
            } else if lower.contains("work") || lower.contains("running") {
                colors.accent
            } else {
                colors.muted
            };
            format!(" {}", color.fg(&status))
        };

        let left = format!("{}{}", model_label, status_word);
        let right = colors.dim.fg(&hints);

        let left_w = visible_width(&left);
        let right_w = visible_width(&right);

        let row = if left_w + right_w + 2 <= width {
            // Both fit — pad to right-align the hints.
            let pad = " ".repeat(width.saturating_sub(left_w + right_w));
            format!("{}{}{}", left, pad, right)
        } else {
            // Too tight — drop the right-aligned hints, show left side,
            // truncate if needed.
            let combined = format!("{}", left);
            if visible_width(&combined) > width {
                truncate_to_width(&combined, width, "…")
            } else {
                combined
            }
        };

        vec![sep, row]
    }

    fn invalidate(&self) {
        // Footer has no cached state
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::strip_ansi;

    #[test]
    fn test_footer_basic() {
        let footer = FooterComponent::new();
        let lines = footer.render(80);
        // Separator + status row.
        assert_eq!(lines.len(), 2);
        assert!(strip_ansi(&lines[1]).contains("Ctrl+C"));
    }

    #[test]
    fn test_footer_with_status() {
        let footer = FooterComponent::new();
        footer.set_status("Working…");
        footer.set_model("claude-sonnet-5");

        let lines = footer.render(80);
        let row = strip_ansi(&lines[1]);
        assert!(row.contains("Working…"));
        assert!(row.contains("claude-sonnet-5"));
    }

    #[test]
    fn test_footer_truncate() {
        let footer = FooterComponent::new();
        footer.set_status("This is a very long status message that should be truncated");
        footer.set_model("claude-opus-4");
        footer.set_hints("Ctrl+C: Exit | Shift+Enter: Send | Ctrl+L: Clear | More hints here");

        let lines = footer.render(40);
        // The status row (index 1) must respect the width budget; the
        // separator (index 0) is exactly width — both <= 40.
        assert!(
            visible_width(&lines[1]) <= 40,
            "row too wide: {}",
            visible_width(&lines[1])
        );
    }

    #[test]
    fn test_footer_thinking_level_suffix() {
        let footer = FooterComponent::new();
        footer.set_model("test-model");
        footer.set_thinking_level(Some("medium"));
        let row = strip_ansi(&footer.render(80)[1]);
        assert!(row.contains("test-model • medium"), "suffix missing: {row}");
    }
}
