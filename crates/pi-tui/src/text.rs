//! Text component for simple text rendering.

use std::sync::Mutex;

use std::any::Any;

use super::component::Component;

/// Simple text component with optional padding.
pub struct Text {
    content: Mutex<String>,
    padding_x: usize,
    padding_y: usize,
}

impl Text {
    /// Create a new text component.
    pub fn new(content: impl Into<String>, padding_x: usize, padding_y: usize) -> Self {
        Self {
            content: Mutex::new(content.into()),
            padding_x,
            padding_y,
        }
    }

    /// Set the text content.
    pub fn set_text(&self, text: impl Into<String>) {
        if let Ok(mut content) = self.content.lock() {
            *content = text.into();
        }
    }

    /// Get the text content.
    pub fn get_text(&self) -> String {
        self.content.lock().map(|c| c.clone()).unwrap_or_default()
    }
}

impl Component for Text {
    fn render(&self, width: usize) -> Vec<String> {
        let content = self.content.lock().map(|c| c.clone()).unwrap_or_default();
        let mut lines = Vec::new();

        // Add top padding
        for _ in 0..self.padding_y {
            lines.push(String::new());
        }

        // Render each line with horizontal padding
        for line in content.lines() {
            let padded = if self.padding_x > 0 {
                format!("{}{}", " ".repeat(self.padding_x), line)
            } else {
                line.to_string()
            };
            // Truncate to width
            let truncated = if padded.len() > width {
                padded[..width].to_string()
            } else {
                padded
            };
            lines.push(truncated);
        }

        // Add bottom padding
        for _ in 0..self.padding_y {
            lines.push(String::new());
        }

        if lines.is_empty() {
            lines.push(String::new());
        }

        lines
    }

    fn invalidate(&self) {
        // Text has no cached state to invalidate
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}