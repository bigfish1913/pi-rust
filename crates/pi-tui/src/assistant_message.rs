//! Assistant message component for rendering AI responses.
//!
//! Based on TypeScript implementation:
//! packages/coding-agent/src/modes/interactive/components/assistant-message.ts
//!
//! Renders an assistant message's content blocks **in order** — text blocks
//! as markdown, thinking/reasoning blocks as dim italic markdown, with a
//! spacing rule that mirrors the TS `updateContent` (a blank line between a
//! thinking run and a following visible block). The block model
//! ([`AssistantBlock`]) is a small, provider-free projection so this library
//! crate never depends on `rpi-ai` (project constraint); the host glue in
//! `rpi-cli` maps an `AssistantMessage` into `Vec<AssistantBlock>` and feeds
//! it to [`AssistantMessageComponent::update_blocks`].

use std::any::Any;
use std::sync::{Arc, Mutex};

use super::component::Component;
use super::container::Container;
use super::markdown::Markdown;
use super::spacer::Spacer;
use super::text::Text;
use crate::ansi::italic;
use crate::theme::theme;

/// A single visible block of an assistant message, in document order. This is
/// the provider-free projection the component renders from — the host maps
/// `rpi_ai::Content::{Text, Thinking}` into these (dropping tool-call / image
/// blocks, which are rendered by their own components).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantBlock {
    /// Plain assistant text (rendered as markdown).
    Text(String),
    /// Reasoning / thinking text (rendered as dim italic markdown).
    Thinking(String),
}

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
/// Mirrors TypeScript AssistantMessageComponent class. Content is rebuilt from
/// [`AssistantBlock`]s on every [`update_blocks`] (and the legacy
/// [`update_text`]), keeping thinking + text in order with TS-faithful spacing.
///
/// [`update_blocks`]: AssistantMessageComponent::update_blocks
/// [`update_text`]: AssistantMessageComponent::update_text
pub struct AssistantMessageComponent {
    /// Content container for message parts (rebuilt per update).
    content_container: Arc<Container>,
    /// Rendering options
    options: Mutex<AssistantMessageOptions>,
    /// Whether this message has tool calls
    has_tool_calls: Mutex<bool>,
    /// Whether currently streaming
    is_streaming: Mutex<bool>,
    /// The last blocks rendered (kept so `invalidate`/option changes can rebuild).
    last_blocks: Mutex<Vec<AssistantBlock>>,
    /// B5e: an optional markdown transformer applied to the raw assistant text
    /// BEFORE it reaches the [`Markdown`] renderer. The host (`rpi-cli`) injects
    /// a closure wrapping any plugin `register_markdown_transformer` handlers
    /// (called as a sync request/response across the cdylib FFI). `None` is the
    /// no-op identity transform, so this render crate gains **no** `rpi-extensions`
    /// dep — only a `Fn(&str) -> String` trait object. The transform fires on
    /// every `rebuild_content` (i.e. once per streaming delta + once on
    /// finalize), so the plugin always sees the latest full markdown; the FFI
    /// call is cheap (a bare fn-pointer invocation + one JSON round-trip).
    markdown_transformer:
        Mutex<Option<Arc<dyn Fn(&str) -> String + Send + Sync>>>,
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
            last_blocks: Mutex::new(Vec::new()),
            markdown_transformer: Mutex::new(None),
        }
    }

    /// B5e: install a markdown transformer applied to raw assistant text before
    /// the [`Markdown`] renderer styles it. The closure is `Send + Sync` so it
    /// is safe across the streaming-drain + render threads. Idempotent against
    /// `rebuild_content`: callers may set it before the first `update_blocks` or
    /// swap it on a `/reload`; either way the next rebuild re-applies it. A
    /// `None` value clears the transform (identity) — used on a reload that
    /// unregisters all markdown transformers.
    ///
    /// On a transform CHANGE, the last blocks are rebuilt so the new transform
    /// is reflected immediately (mirrors `set_hide_thinking`/`set_output_pad`).
    pub fn set_markdown_transformer(
        &self,
        transformer: Option<Arc<dyn Fn(&str) -> String + Send + Sync>>,
    ) {
        let blocks = {
            let mut cur = self.markdown_transformer.lock().unwrap();
            // `Arc<dyn Fn>` has no PartialEq, so we can't short-circuit on
            // "unchanged" like the option setters. Always rebuild: the call is
            // cheap and the rebuild only fires on an explicit set/swap.
            *cur = transformer;
            self.last_blocks.lock().unwrap().clone()
        };
        if !blocks.is_empty() {
            self.rebuild_content(&blocks);
        }
    }

    /// Apply the installed transformer to a raw markdown string (identity when
    /// none is installed). Both the text arm and the thinking arm of
    /// `rebuild_content` route through here so a single install point covers
    /// both. Thinking is treated like text — the spec transforms "assistant
    /// markdown text" and thinking is rendered as markdown too; a plugin that
    /// wants to skip thinking can inspect the input it receives (no flag is
    /// carried in the `{"markdown": …}` envelope).
    fn transform_markdown(&self, raw: &str) -> String {
        match self.markdown_transformer.lock().unwrap().clone() {
            Some(f) => f(raw),
            None => raw.to_string(),
        }
    }

    /// Create with default options.
    pub fn default() -> Self {
        Self::new(AssistantMessageOptions::default())
    }

    /// Update the content from a flat block list. Mirrors the TS
    /// `updateContent` ordering + spacing: text blocks render as markdown,
    /// **runs** of consecutive thinking blocks coalesce into one dim-italic
    /// markdown section (joined with `\n\n`), and a blank line separates a
    /// thinking run from a following visible block. When `hide_thinking` is
    /// set, each thinking run collapses to a single dim label line.
    pub fn update_blocks(&self, blocks: &[AssistantBlock]) {
        *self.last_blocks.lock().unwrap() = blocks.to_vec();
        self.rebuild_content(blocks);
    }

    /// Update the content with assistant message text.
    ///
    /// Convenience for callers that have only text (the legacy path). The
    /// block-aware [`update_blocks`] supersedes this where thinking blocks
    /// are available; both share the rebuild path.
    pub fn update_text(&self, text: &str) {
        if text.trim().is_empty() {
            self.update_blocks(&[]);
        } else {
            self.update_blocks(&[AssistantBlock::Text(text.to_string())]);
        }
    }

    /// Rebuild the content container from a block list (the shared render path).
    ///
    /// Mirrors the TS `updateContent` loop, simplified to the text/thinking
    /// block pair (tool-call / image / error rendering is owned by other
    /// components: tool-exec blocks render via `ToolExecutionComponent`,
    /// and stop-reason error lines surface through `add_error_message` in the
    /// host). Empty content renders nothing.
    fn rebuild_content(&self, blocks: &[AssistantBlock]) {
        self.content_container.clear();

        let opts = self.options.lock().unwrap().clone();
        let has_visible = blocks
            .iter()
            .any(|b| matches!(b, AssistantBlock::Text(t) if !t.trim().is_empty())
                || matches!(b, AssistantBlock::Thinking(t) if !t.trim().is_empty()));

        if !has_visible {
            // Empty / whitespace-only content renders nothing (matches the TS
            // early return when no visible block is present).
            return;
        }

        // Leading spacer, matching the TS `Spacer(1)` before the first block.
        self.content_container.add_child(Arc::new(Spacer::new(1)));

        let mut i = 0;
        while i < blocks.len() {
            let block = &blocks[i];
            match block {
                AssistantBlock::Text(t) if !t.trim().is_empty() => {
                    // B5e: apply the installed markdown transformer (identity
                    // when none) BEFORE styling, so a plugin's
                    // `register_markdown_transformer` sees the raw assistant
                    // markdown and `Markdown::render_markdown` styles the
                    // transformed text.
                    let transformed = self.transform_markdown(t.trim());
                    let md = Arc::new(Markdown::new(transformed, opts.output_pad, 0));
                    self.content_container.add_child(md);
                    i += 1;
                }
                AssistantBlock::Text(_) => {
                    // Whitespace-only text block — skip (TS trims + skips empty).
                    i += 1;
                }
                AssistantBlock::Thinking(_) => {
                    // Coalesce a run of consecutive thinking blocks into one
                    // section, exactly like the TS inner loop.
                    let mut joined: Vec<String> = Vec::new();
                    while i < blocks.len() {
                        match &blocks[i] {
                            AssistantBlock::Thinking(t) if !t.trim().is_empty() => {
                                joined.push(t.trim().to_string());
                                i += 1;
                            }
                            AssistantBlock::Thinking(_) => {
                                i += 1; // whitespace-only thinking, skip
                            }
                            _ => break,
                        }
                    }
                    if joined.is_empty() {
                        continue;
                    }

                    if opts.hide_thinking {
                        // One static dim label per thinking run (TS path).
                        self.content_container.add_child(Arc::new(Text::new(
                            italic(&theme().colors.thinking_text.fg(&opts.hidden_thinking_label)),
                            opts.output_pad,
                            0,
                        )));
                    } else {
                        // Render the joined thinking as one dim italic markdown
                        // section — the TS `color: thinkingText, italic: true`
                        // styling, applied via the thinking-text color + italic
                        // wrapper around the markdown lines.
                        let body = joined.join("\n\n");
                        // B5e: transform the PLAIN thinking markdown first (the
                        // plugin must see markdown, not ANSI), then apply the
                        // thinking-text color + italic wrap around the
                        // transformed text — same order as the text block
                        // (transform → style → render).
                        let transformed = self.transform_markdown(&body);
                        let wrapped = italic(&theme().colors.thinking_text.fg(&transformed));
                        let md = Arc::new(Markdown::new(wrapped, opts.output_pad, 0));
                        self.content_container.add_child(md);
                    }

                    // Spacer before a following visible block (TS
                    // `hasVisibleContentAfter` → Spacer(1)).
                    let has_after = blocks[i..]
                        .iter()
                        .any(|b| matches!(b, AssistantBlock::Text(t) if !t.trim().is_empty())
                            || matches!(b, AssistantBlock::Thinking(t) if !t.trim().is_empty()));
                    if has_after {
                        self.content_container.add_child(Arc::new(Spacer::new(1)));
                    }
                }
            }
        }
    }

    /// Update with error message.
    pub fn update_error(&self, error: &str, stop_reason: &str) {
        // Clear content container
        self.content_container.clear();
        *self.last_blocks.lock().unwrap() = Vec::new();

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

    /// Set hide thinking option. Rebuilds the last content so the toggle is
    /// reflected immediately (mirrors the TS setter).
    pub fn set_hide_thinking(&self, hide: bool) {
        let blocks = {
            let mut opts = self.options.lock().unwrap();
            if opts.hide_thinking == hide {
                return;
            }
            opts.hide_thinking = hide;
            self.last_blocks.lock().unwrap().clone()
        };
        self.rebuild_content(&blocks);
    }

    /// Set hidden thinking label. Rebuilds the last content (mirrors TS).
    pub fn set_hidden_thinking_label(&self, label: &str) {
        let blocks = {
            let mut opts = self.options.lock().unwrap();
            opts.hidden_thinking_label = label.to_string();
            self.last_blocks.lock().unwrap().clone()
        };
        self.rebuild_content(&blocks);
    }

    /// Set output padding. Rebuilds the last content (mirrors TS).
    pub fn set_output_pad(&self, pad: usize) {
        let blocks = {
            let mut opts = self.options.lock().unwrap();
            opts.output_pad = pad;
            self.last_blocks.lock().unwrap().clone()
        };
        self.rebuild_content(&blocks);
    }

    /// Check if has tool calls.
    pub fn has_tool_calls(&self) -> bool {
        *self.has_tool_calls.lock().unwrap()
    }

    /// The currently installed markdown transformer (B5e), if any. The host
    /// reads this to carry an existing transform into a fresh component (e.g.
    /// after a `/reload`) so the new component renders with the same plugin
    /// transformer without the host re-querying the registry.
    pub fn markdown_transformer(
        &self,
    ) -> Option<Arc<dyn Fn(&str) -> String + Send + Sync>> {
        self.markdown_transformer.lock().unwrap().clone()
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
        // Rebuild from the last blocks (mirrors the TS invalidate path that
        // re-runs updateContent on the cached lastMessage).
        let blocks = self.last_blocks.lock().unwrap().clone();
        if !blocks.is_empty() {
            self.rebuild_content(&blocks);
        }
        self.content_container.invalidate();
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
        assert!(strip_ansi(&lines.join("\n")).contains("Error:"));
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

    #[test]
    fn test_thinking_and_text_render_in_order() {
        // A thinking block followed by a text block should render BOTH, in
        // order, with a blank separator between them.
        let msg = AssistantMessageComponent::default();
        msg.update_blocks(&[
            AssistantBlock::Thinking("Let me consider the options.".to_string()),
            AssistantBlock::Text("Here is the answer.".to_string()),
        ]);
        let lines = msg.render(80);
        let joined = strip_ansi(&lines.join("\n"));
        // Ordering: the thinking content appears before the answer text.
        let think_pos = joined.find("consider the options");
        let text_pos = joined.find("Here is the answer");
        assert!(think_pos.is_some(), "thinking not rendered: {joined}");
        assert!(text_pos.is_some(), "text not rendered: {joined}");
        assert!(think_pos < text_pos, "thinking should precede text: {joined}");
    }

    #[test]
    fn test_hide_thinking_shows_label_only() {
        let msg = AssistantMessageComponent::new(AssistantMessageOptions {
            hide_thinking: true,
            hidden_thinking_label: "Thinking...".to_string(),
            ..Default::default()
        });
        msg.update_blocks(&[
            AssistantBlock::Thinking("secret reasoning".to_string()),
            AssistantBlock::Text("Public answer.".to_string()),
        ]);
        let joined = strip_ansi(&msg.render(80).join("\n"));
        assert!(joined.contains("Thinking..."), "label missing: {joined}");
        assert!(
            !joined.contains("secret reasoning"),
            "hidden thinking leaked: {joined}"
        );
        assert!(joined.contains("Public answer."), "text missing: {joined}");
    }

    #[test]
    fn test_consecutive_thinking_blocks_coalesce() {
        // Two adjacent thinking blocks coalesce into one section (no trailing
        // blank between them — the spacer goes between the run and the next
        // visible block, not between thinking blocks).
        let msg = AssistantMessageComponent::default();
        msg.update_blocks(&[
            AssistantBlock::Thinking("part one".to_string()),
            AssistantBlock::Thinking("part two".to_string()),
            AssistantBlock::Text("Done.".to_string()),
        ]);
        let joined = strip_ansi(&msg.render(80).join("\n"));
        assert!(joined.contains("part one"));
        assert!(joined.contains("part two"));
        assert!(joined.contains("Done."));
    }

    #[test]
    fn test_set_hide_thinking_rebuilds() {
        // Toggling hide_thinking after an update should rebuild the render.
        let msg = AssistantMessageComponent::default();
        msg.update_blocks(&[
            AssistantBlock::Thinking("internal".to_string()),
            AssistantBlock::Text("visible".to_string()),
        ]);
        let before = strip_ansi(&msg.render(80).join("\n"));
        assert!(before.contains("internal"));
        msg.set_hide_thinking(true);
        let after = strip_ansi(&msg.render(80).join("\n"));
        assert!(!after.contains("internal"), "not rebuilt after toggle: {after}");
        assert!(after.contains("Thinking..."));
    }

    #[test]
    fn test_markdown_transformer_applied_to_text_and_thinking() {
        // B5e: an installed transformer rewrites the raw markdown BEFORE styling.
        // A trivial uppercasing transformer proves the text arm + the thinking
        // arm both route through `transform_markdown` (and that the thinking arm
        // transforms the PLAIN body, not the ANSI-wrapped output).
        let msg = AssistantMessageComponent::default();
        let transformer: Arc<dyn Fn(&str) -> String + Send + Sync> =
            Arc::new(|raw: &str| raw.to_uppercase());
        msg.set_markdown_transformer(Some(transformer));
        msg.update_blocks(&[
            AssistantBlock::Thinking("quiet reasoning".to_string()),
            AssistantBlock::Text("hello world".to_string()),
        ]);
        let joined = strip_ansi(&msg.render(80).join("\n"));
        assert!(
            joined.contains("QUIET REASONING"),
            "thinking not transformed: {joined}"
        );
        assert!(
            joined.contains("HELLO WORLD"),
            "text not transformed: {joined}"
        );
    }

    #[test]
    fn test_markdown_transformer_swap_rebuilds() {
        // B5e: swapping the transformer after an update rebuilds the last blocks
        // so the new transform is reflected immediately (mirrors the option
        // setters). A None clears the transform (identity).
        let msg = AssistantMessageComponent::default();
        msg.update_blocks(&[AssistantBlock::Text("hello".to_string())]);
        let upper: Arc<dyn Fn(&str) -> String + Send + Sync> =
            Arc::new(|raw: &str| raw.to_uppercase());
        msg.set_markdown_transformer(Some(upper));
        assert!(strip_ansi(&msg.render(80).join("\n")).contains("HELLO"));
        // Clear → identity.
        msg.set_markdown_transformer(None);
        assert!(strip_ansi(&msg.render(80).join("\n")).contains("hello"));
    }

    #[test]
    fn test_no_transformer_is_identity() {
        // B5e: with no transformer installed, raw text reaches the renderer
        // unchanged (the default path — every existing test relies on this).
        let msg = AssistantMessageComponent::default();
        msg.update_blocks(&[AssistantBlock::Text("plain text".to_string())]);
        assert!(msg.markdown_transformer().is_none());
        assert!(strip_ansi(&msg.render(80).join("\n")).contains("plain text"));
    }
}
