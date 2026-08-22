//! Footer component for TUI.
//!
//! Based on TypeScript implementation:
//! packages/coding-agent/src/modes/interactive/components/footer.ts

use std::any::Any;
use std::sync::Mutex;

use super::component::Component;
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
}

impl FooterComponent {
    /// Create a new footer component.
    pub fn new() -> Self {
        Self {
            status: Mutex::new(String::new()),
            model: Mutex::new("claude-sonnet-5".to_string()),
            hints: Mutex::new("Ctrl+C: Exit | Enter: Send | Shift+Enter: New line".to_string()),
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
        let status = self.status.lock().unwrap();
        let model = self.model.lock().unwrap();
        let hints = self.hints.lock().unwrap();

        // Build footer line
        let mut line = String::new();
        
        // Add model name if present
        if !model.is_empty() {
            line.push_str(&format!("[{}] ", model));
        }
        
        // Add status if present
        if !status.is_empty() {
            line.push_str(&format!("{} | ", status));
        }
        
        // Add hints
        line.push_str(&hints);

        // Truncate to width — ANSI-safe and multibyte-safe. The previous
        // `line[..width]` byte-slice panicked on CJK/emoji and leaked ANSI
        // mid-sequence (same bug class as the markdown fix in 600b595).
        if visible_width(&line) > width {
            line = truncate_to_width(&line, width, "…");
        }

        vec![line]
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

    #[test]
    fn test_footer_basic() {
        let footer = FooterComponent::new();
        let lines = footer.render(80);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("Ctrl+C"));
    }

    #[test]
    fn test_footer_with_status() {
        let footer = FooterComponent::new();
        footer.set_status("Working...");
        footer.set_model("claude-sonnet-5");
        
        let lines = footer.render(80);
        assert!(lines[0].contains("Working..."));
        assert!(lines[0].contains("claude-sonnet-5"));
    }

    #[test]
    fn test_footer_truncate() {
        let footer = FooterComponent::new();
        footer.set_status("This is a very long status message that should be truncated");
        footer.set_model("claude-opus-4");
        footer.set_hints("Ctrl+C: Exit | Shift+Enter: Send | Ctrl+L: Clear | More hints here");
        
        let lines = footer.render(40);
        assert!(visible_width(&lines[0]) <= 40);
    }
}