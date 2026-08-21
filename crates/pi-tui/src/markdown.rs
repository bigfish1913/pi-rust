//! Markdown component for rendering markdown text.
//!
//! Provides basic markdown rendering with syntax highlighting support.

use std::any::Any;
use std::sync::Mutex;

use super::component::Component;
use crate::ansi::{bold, dim, italic, underline, fg_256};

/// Markdown rendering options.
#[derive(Debug, Clone)]
pub struct MarkdownOptions {
    /// Indentation for code blocks.
    pub code_block_indent: usize,
    /// Maximum width for wrapping.
    pub max_width: Option<usize>,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            code_block_indent: 2,
            max_width: None,
        }
    }
}

/// Markdown - A component that renders markdown text.
pub struct Markdown {
    content: Mutex<String>,
    options: MarkdownOptions,
    padding_x: usize,
    padding_y: usize,
}

impl Markdown {
    /// Create a new markdown component.
    pub fn new(content: impl Into<String>, padding_x: usize, padding_y: usize) -> Self {
        Self {
            content: Mutex::new(content.into()),
            options: MarkdownOptions::default(),
            padding_x,
            padding_y,
        }
    }

    /// Create with options.
    pub fn with_options(content: impl Into<String>, options: MarkdownOptions, padding_x: usize, padding_y: usize) -> Self {
        Self {
            content: Mutex::new(content.into()),
            options,
            padding_x,
            padding_y,
        }
    }

    /// Set the content.
    pub fn set_content(&self, content: impl Into<String>) {
        if let Ok(mut c) = self.content.lock() {
            *c = content.into();
        }
    }

    /// Get the content.
    pub fn get_content(&self) -> String {
        self.content.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Render markdown to lines.
    fn render_markdown(&self, width: usize) -> Vec<String> {
        let content = self.content.lock().map(|c| c.clone()).unwrap_or_default();
        let mut lines = Vec::new();
        let indent = " ".repeat(self.options.code_block_indent);

        // Add top padding
        for _ in 0..self.padding_y {
            lines.push(String::new());
        }

        let mut in_code_block = false;

        for line in content.lines() {
            let mut rendered = if self.padding_x > 0 {
                " ".repeat(self.padding_x)
            } else {
                String::new()
            };

            // Handle code blocks
            if line.trim().starts_with("```") {
                in_code_block = !in_code_block;
                rendered.push_str(&dim(&format!("{}┌─", indent)));
                lines.push(rendered);
                continue;
            }

            if in_code_block {
                rendered.push_str(&format!("{}│ {}", indent, line));
                lines.push(rendered);
                continue;
            }

            // Handle headers
            if line.starts_with("### ") {
                rendered.push_str(&bold(&dim("### ")));
                rendered.push_str(&bold(&self.render_inline(line.trim_start_matches("### "))));
            } else if line.starts_with("## ") {
                rendered.push_str(&bold(&dim("## ")));
                rendered.push_str(&bold(&underline(&self.render_inline(line.trim_start_matches("## ")))));
            } else if line.starts_with("# ") {
                rendered.push_str(&bold(&dim("# ")));
                rendered.push_str(&bold(&underline(&bold(&self.render_inline(line.trim_start_matches("# "))))));
            } else if line.starts_with("- ") || line.starts_with("* ") {
                // Unordered list
                rendered.push_str("  • ");
                rendered.push_str(&self.render_inline(line.trim_start_matches(|c| c == '-' || c == '*').trim()));
            } else if line.starts_with(|c: char| c.is_ascii_digit()) && line.contains(". ") {
                // Ordered list
                rendered.push_str(&format!("  {}", line));
            } else if line.starts_with("> ") {
                // Blockquote
                rendered.push_str(&dim("  │ "));
                rendered.push_str(&italic(&self.render_inline(line.trim_start_matches("> "))));
            } else if line.trim().starts_with("---") || line.trim().starts_with("***") {
                // Horizontal rule
                rendered.push_str(&dim(&"─".repeat(width.saturating_sub(self.padding_x * 2))));
            } else if line.trim().is_empty() {
                // Empty line
            } else {
                // Regular text with inline formatting
                rendered.push_str(&self.render_inline(line));
            }

            // Truncate to width
            if rendered.len() > width {
                rendered = rendered[..width].to_string();
            }

            lines.push(rendered);
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

    /// Render inline markdown (bold, italic, code, links).
    fn render_inline(&self, text: &str) -> String {
        let mut result = String::new();
        let mut chars = text.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '*' {
                if chars.peek() == Some(&'*') {
                    chars.next(); // consume second *
                    // Bold
                    let bold_text = self.consume_until(&mut chars, "**");
                    result.push_str(&bold(&bold_text));
                } else {
                    // Italic
                    let italic_text = self.consume_until(&mut chars, "*");
                    result.push_str(&italic(&italic_text));
                }
            } else if c == '_' {
                if chars.peek() == Some(&'_') {
                    chars.next();
                    let bold_text = self.consume_until(&mut chars, "__");
                    result.push_str(&underline(&bold_text));
                } else {
                    let italic_text = self.consume_until(&mut chars, "_");
                    result.push_str(&italic(&italic_text));
                }
            } else if c == '`' {
                // Inline code
                let code_text = self.consume_until(&mut chars, "`");
                result.push_str(&fg_256(14, &code_text)); // Cyan
            } else if c == '[' {
                // Link
                let link_text = self.consume_until(&mut chars, "]");
                if chars.next() == Some('(') {
                    let _url = self.consume_until(&mut chars, ")");
                    result.push_str(&underline(&link_text));
                } else {
                    result.push('[');
                    result.push_str(&link_text);
                }
            } else {
                result.push(c);
            }
        }

        result
    }

    /// Consume characters until the delimiter.
    fn consume_until(&self, chars: &mut std::iter::Peekable<std::str::Chars<'_>>, delimiter: &str) -> String {
        let mut result = String::new();
        let delim_chars: Vec<char> = delimiter.chars().collect();

        while let Some(c) = chars.peek() {
            if *c == delim_chars[0] {
                // Check if this is the delimiter
                let mut matches = true;
                let mut lookahead: Vec<char> = Vec::new();
                
                for (_i, dc) in delim_chars.iter().enumerate() {
                    if let Some(&next) = chars.peek() {
                        if next == *dc {
                            lookahead.push(next);
                            chars.next();
                        } else {
                            matches = false;
                            break;
                        }
                    } else {
                        matches = false;
                        break;
                    }
                }

                if matches {
                    break;
                } else {
                    // Put back consumed characters
                    result.extend(lookahead);
                }
            } else {
                result.push(*c);
                chars.next();
            }
        }

        result
    }
}

impl Component for Markdown {
    fn render(&self, width: usize) -> Vec<String> {
        self.render_markdown(width)
    }

    fn invalidate(&self) {
        // Markdown caches content, no external cache to invalidate
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_markdown_headers() {
        let md = Markdown::new("# Title\n## Subtitle\n### Heading", 0, 0);
        let lines = md.render(80);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn test_markdown_inline() {
        let md = Markdown::new("This is **bold** and *italic* text", 0, 0);
        let lines = md.render(80);
        assert!(!lines[0].is_empty());
    }

    #[test]
    fn test_markdown_code() {
        let md = Markdown::new("Use `code` here", 0, 0);
        let lines = md.render(80);
        assert!(!lines[0].is_empty());
    }
}