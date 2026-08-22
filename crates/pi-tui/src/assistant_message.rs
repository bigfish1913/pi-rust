//! Assistant message component for rendering AI responses.
//!
//! Based on TypeScript implementation:
//! packages/coding-agent/src/modes/interactive/components/assistant-message.ts

use std::any::Any;
use std::sync::{Arc, Mutex};

use super::component::Component;
use super::container::Container;
use super::markdown::Markdown;
use super::spacer::Spacer;
use super::text::Text;

/// Configuration for assistant message rendering.
#[derive(Debug, Clone)]
pub struct AssistantMessageOptions {
    /// Hide thinking blocks (show placeholder instead)
    pub hide_thinking: bool,
    /// Label to show when thinking is hidden
    pub hidden_thinking_label: String,
    /// Horizontal padding
    pub output_pad: usize,
}

impl Default for AssistantMessageOptions {
    fn default() -> Self {
        Self {
            hide_thinking: false,
            hidden_thinking_label: "Thinking...".to_string(),
            output_pad: 1,
        }
    }
}

/// Component that renders a complete assistant message.
/// 
/// Mirrors TypeScript AssistantMessageComponent class.
pub struct AssistantMessageComponent {
    /// Content container for message parts
    content_container: Arc<Container>,
    /// Rendering options
    options: Mutex<AssistantMessageOptions>,
    /// Whether this message has tool calls
    has_tool_calls: Mutex<bool>,
    /// Whether currently streaming
    is_streaming: Mutex<bool>,
}

impl AssistantMessageComponent {
    /// Create a new assistant message component.
    pub fn new(options: AssistantMessageOptions) -> Self {
        let content_container = Arc::new(Container::new());
        
        Self {
            content_container,
            options: Mutex::new(options),
            has_tool_calls: Mutex::new(false),
            is_streaming: Mutex::new(false),
        }
    }

    /// Create with default options.
    pub fn default() -> Self {
        Self::new(AssistantMessageOptions::default())
    }

    /// Update the content with assistant message text.
    /// 
    /// For simplicity, this version takes a plain text string.
    /// A more complete implementation would take an AssistantMessage struct.
    pub fn update_text(&self, text: &str) {
        // Clear content container
        self.content_container.clear();
        
        if text.trim().is_empty() {
            return;
        }

        // Add spacing
        self.content_container.add_child(Arc::new(Spacer::new(1)));

        // Add markdown content
        let markdown = Arc::new(Markdown::new(text.to_string(), self.options.lock().unwrap().output_pad, 0));
        self.content_container.add_child(markdown);
    }

    /// Update with error message.
    pub fn update_error(&self, error: &str, stop_reason: &str) {
        // Clear content container
        self.content_container.clear();
        
        // Add spacing
        self.content_container.add_child(Arc::new(Spacer::new(1)));

        // Add error message
        let error_text = match stop_reason {
            "aborted" => format!("❌ Operation aborted: {}", error),
            "error" => format!("❌ Error: {}", error),
            "length" => "⚠️ Response was truncated before completion.".to_string(),
            _ => format!("❌ {}", error),
        };
        
        let error_component = Arc::new(Text::new(error_text, 1, 0));
        self.content_container.add_child(error_component);
    }

    /// Set hide thinking option.
    pub fn set_hide_thinking(&self, hide: bool) {
        if let Ok(mut opts) = self.options.lock() {
            opts.hide_thinking = hide;
        }
    }

    /// Set hidden thinking label.
    pub fn set_hidden_thinking_label(&self, label: &str) {
        if let Ok(mut opts) = self.options.lock() {
            opts.hidden_thinking_label = label.to_string();
        }
    }

    /// Set output padding.
    pub fn set_output_pad(&self, pad: usize) {
        if let Ok(mut opts) = self.options.lock() {
            opts.output_pad = pad;
        }
    }

    /// Check if has tool calls.
    pub fn has_tool_calls(&self) -> bool {
        *self.has_tool_calls.lock().unwrap()
    }

    /// Set streaming state.
    pub fn set_streaming(&self, streaming: bool) {
        if let Ok(mut s) = self.is_streaming.lock() {
            *s = streaming;
        }
    }
}

impl Component for AssistantMessageComponent {
    fn render(&self, width: usize) -> Vec<String> {
        // Render content container
        self.content_container.render(width)
    }

    fn invalidate(&self) {
        self.content_container.invalidate();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_assistant_message_basic() {
        let msg = AssistantMessageComponent::default();
        msg.update_text("Hello, world!");
        
        let lines = msg.render(80);
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_assistant_message_empty() {
        let msg = AssistantMessageComponent::default();
        msg.update_text("");
        
        let lines = msg.render(80);
        assert!(lines.is_empty() || lines.iter().all(|l| l.trim().is_empty()));
    }

    #[test]
    fn test_assistant_message_error() {
        let msg = AssistantMessageComponent::default();
        msg.update_error("Something went wrong", "error");
        
        let lines = msg.render(80);
        assert!(!lines.is_empty());
        assert!(lines.join("\n").contains("Error:"));
    }

    #[test]
    fn test_assistant_message_options() {
        let msg = AssistantMessageComponent::new(AssistantMessageOptions {
            hide_thinking: true,
            hidden_thinking_label: "Processing...".to_string(),
            output_pad: 2,
        });
        
        msg.update_text("Test");
        let lines = msg.render(80);
        assert!(!lines.is_empty());
    }
}