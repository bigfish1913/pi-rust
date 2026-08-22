//! User message component — port of `user-message.ts`.
//!
//! Renders the user's submitted text as markdown inside a padded [`Box`] with
//! a subtle background, wrapped in [OSC 133] prompt zone markers so capable
//! terminals can detect user/assistant boundaries. Where OSC 133 is
//! unsupported the sequences are harmless no-ops.
//!
//! Replaces the plain `> text` echo in the transcript.
//!
//! [OSC 133]: https://terminalguide.namepad.de/seq/osc-133/

use std::any::Any;

use super::component::Component;
use crate::box_component::Box;
use crate::markdown::Markdown;
use crate::theme::theme;

const OSC133_ZONE_START: &str = "\x1b]133;A\x07";
const OSC133_ZONE_END: &str = "\x1b]133;B\x07";
const OSC133_ZONE_FINAL: &str = "\x1b]133;C\x07";

/// Component that renders a user message with a background and OSC133 markers.
pub struct UserMessageComponent {
    text: String,
    output_pad: usize,
}

impl UserMessageComponent {
    /// Create a new user message component.
    ///
    /// `output_pad` is the horizontal+vertical padding inside the background
    /// box (mirrors the TS `outputPad`, default 1).
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            output_pad: 1,
        }
    }

    /// Override the inner padding.
    pub fn with_pad(mut self, pad: usize) -> Self {
        self.output_pad = pad;
        self
    }

    fn build_box(&self) -> Box {
        let bg = theme().colors.surface;
        let content_box = Box::new(self.output_pad, 1);
        // Background function: apply the surface color as bg to each line.
        content_box.set_bg_fn(Some(std::sync::Arc::new(move |s: &str| bg.bg(s))));
        let md = Markdown::new(self.text.clone(), 0, 0);
        content_box.add_child(std::sync::Arc::new(md));
        content_box
    }
}

impl Component for UserMessageComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let inner = self.build_box();
        let mut lines = inner.render(width);
        if lines.is_empty() {
            return lines;
        }
        // Prepend zone-start to the first line, append zone-end+final to the
        // last line. These are zero-width escapes so visible layout is unchanged.
        let first = lines.first_mut().unwrap();
        first.insert_str(0, OSC133_ZONE_START);
        let last = lines.last_mut().unwrap();
        last.insert_str(0, OSC133_ZONE_END);
        last.insert_str(OSC133_ZONE_END.len(), OSC133_ZONE_FINAL);
        lines
    }

    fn invalidate(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::strip_ansi;

    #[test]
    fn test_renders_non_empty() {
        let c = UserMessageComponent::new("Hello world");
        let lines = c.render(40);
        assert!(!lines.is_empty());
        // Should contain the text (after stripping ANSI/OSC).
        let joined = strip_ansi(&lines.join("\n"));
        assert!(joined.contains("Hello"));
    }

    #[test]
    fn test_osc133_markers_present() {
        let c = UserMessageComponent::new("Hi");
        let lines = c.render(40);
        let joined = lines.join("\n");
        assert!(joined.contains(OSC133_ZONE_START), "missing zone-start A");
        assert!(joined.contains(OSC133_ZONE_END), "missing zone-end B");
        assert!(joined.contains(OSC133_ZONE_FINAL), "missing zone-final C");
    }

    #[test]
    fn test_cjk_no_panic() {
        let c = UserMessageComponent::new("你好世界");
        let lines = c.render(6);
        assert!(!lines.is_empty());
    }
}
