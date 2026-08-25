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

        // Render each line with horizontal padding, WORD-WRAPPING to the
        // content width (pi text.ts: wrapTextWithAnsi) instead of truncating —
        // long model output flows onto continuation lines rather than being
        // cut. Each wrapped line carries the padding prefix (matching pi's
        // per-line padding).
        let pad = " ".repeat(self.padding_x);
        let content_width = width.saturating_sub(self.padding_x).max(1);
        for line in content.lines() {
            let wrapped = crate::utils::wrap_text_with_ansi(line, content_width);
            for wl in wrapped {
                if self.padding_x > 0 {
                    lines.push(format!("{pad}{wl}"));
                } else {
                    lines.push(wl);
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::Component;

    /// Regression: rendering a line wider than the layout column used to
    /// byte-slice `padded[..width]` and panic mid-character on multi-byte
    /// chars — "end byte index 120 is not a char boundary; it is inside
    /// '⏎' (bytes 119..122)". Truncation must never split a char: it keeps
    /// whole chars whose visible width fits the column.
    #[test]
    fn render_truncates_multibyte_at_char_boundary() {
        let t = Text::new("x⏎y", 0, 0);
        // Width 1: only 'x' fits (the 3-byte ⏎ is dropped whole, never split).
        let out = t.render(1);
        assert_eq!(out[0], "x");
        // Width 2: 'x' + '⏎' (⏎ is visible-width 1) — whole chars only.
        let out = t.render(2);
        assert_eq!(out[0], "x⏎");
        // Width 3: everything fits unchanged.
        let out = t.render(3);
        assert_eq!(out[0], "x⏎y");
    }

    #[test]
    fn render_keeps_ansi_and_truncates() {
        let styled = "\x1b[31mred\x1b[0m text";
        let t = Text::new(styled, 0, 0);
        let out = t.render(3);
        // No panic on ANSI content; visible width respected.
        assert!(crate::ansi::visible_width(&out[0]) <= 3);
    }
}
