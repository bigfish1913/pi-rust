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

use super::component::{Component, RenderedDocument};
use super::container::Container;
use super::image::{Image, ImageOptions};
use super::markdown::Markdown;
use super::spacer::Spacer;
use super::text::Text;
use super::thinking_region::marker as thinking_marker;
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
    /// Inline image bytes decoded from the provider's base64 image block.
    Image(Vec<u8>),
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
    show_images: Mutex<bool>,
    trailing_spacing: Mutex<usize>,
    /// Whether this message has tool calls
    has_tool_calls: Mutex<bool>,
    /// Whether currently streaming
    is_streaming: Mutex<bool>,
    /// The last blocks rendered (kept so `invalidate`/option changes can rebuild).
    last_blocks: Mutex<Vec<AssistantBlock>>,
    /// Per-message id for the thinking click region (opaque; assigned by the
    /// host). `None` renders without region markers — e.g. markdown-style
    /// extension components that reuse this component for plain text.
    message_id: Mutex<Option<String>>,
    /// Per-message thinking visibility. `None` follows the global
    /// `hide_thinking` option; `Some(true)` expands this message's thinking
    /// even while the global preference hides every other message's.
    expand_thinking: Mutex<Option<bool>>,
    /// B5e: an optional markdown transformer applied to the raw assistant text
    /// BEFORE it reaches the [`Markdown`] renderer. The host (`rpi-cli`) injects
    /// a closure wrapping any plugin `register_markdown_transformer` handlers
    /// (called as a sync request/response across the cdylib FFI). `None` is the
    /// no-op identity transform, so this render crate gains **no** `rpi-extensions`
    /// dep — only a `Fn(&str) -> String` trait object. The transform fires on
    /// every `rebuild_content` (i.e. once per streaming delta + once on
    /// finalize), so the plugin always sees the latest full markdown; the FFI
    /// call is cheap (a bare fn-pointer invocation + one JSON round-trip).
    markdown_transformer: Mutex<Option<Arc<dyn Fn(&str) -> String + Send + Sync>>>,
}

/// Wrap a thinking row's rendered output with a zero-width click-region
/// marker on every visible row. Keeping the marker out of the markdown/text
/// SOURCE preserves block parsing (`#`, lists, fences) and keeps plugin
/// markdown transformers seeing plain text; the marker only exists in the
/// frame the renderer scans for hit rectangles.
struct ThinkingRegionComponent {
    inner: Arc<dyn Component>,
    message_id: Option<String>,
}

impl ThinkingRegionComponent {
    fn new(inner: Arc<dyn Component>, message_id: Option<String>) -> Self {
        Self { inner, message_id }
    }

    fn with_markers(&self, mut lines: Vec<String>) -> Vec<String> {
        let Some(id) = self.message_id.as_deref() else {
            return lines;
        };
        for line in &mut lines {
            if line.is_empty()
                || line.contains(super::thinking_region::THINKING_REGION_MARKER_PREFIX)
            {
                continue;
            }
            let width = crate::ansi::visible_width(line);
            line.insert_str(0, &thinking_marker(id, width));
        }
        lines
    }
}

impl Component for ThinkingRegionComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.with_markers(self.inner.render(width))
    }

    fn render_document(&self, width: usize) -> RenderedDocument {
        if self.message_id.is_none() {
            return self.inner.render_document(width);
        }
        // `render_document` is the path the primary ScrollView uses. Materialize
        // the rows once (the wrapper is per thinking run, so this is small) and
        // re-share them so viewport slicing sees the same markers as `render`.
        let lines = self.inner.render(width);
        RenderedDocument::from_lines(self.with_markers(lines))
    }

    fn invalidate(&self) {
        self.inner.invalidate();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl AssistantMessageComponent {
    /// Create a new assistant message component.
    pub fn new(options: AssistantMessageOptions) -> Self {
        let content_container = Arc::new(Container::new());

        Self {
            content_container,
            options: Mutex::new(options),
            show_images: Mutex::new(true),
            trailing_spacing: Mutex::new(0),
            has_tool_calls: Mutex::new(false),
            is_streaming: Mutex::new(false),
            last_blocks: Mutex::new(Vec::new()),
            message_id: Mutex::new(None),
            expand_thinking: Mutex::new(None),
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
        let content = Container::new();

        let opts = self.options.lock().unwrap().clone();
        let has_visible = blocks.iter().any(|b| {
            matches!(b, AssistantBlock::Text(t) if !t.trim().is_empty())
                || matches!(b, AssistantBlock::Thinking(t) if !t.trim().is_empty())
                || matches!(b, AssistantBlock::Image(data) if !data.is_empty())
        });

        if !has_visible {
            // Empty / whitespace-only content renders nothing (matches the TS
            // early return when no visible block is present).
            self.content_container.replace_children(Vec::new());
            return;
        }

        // No leading spacer here — every transcript entry (including this
        // one) contributes one blank row AFTER itself (the host's trailing
        // Spacer / the panel's trailing spacer), and no block opens with a
        // leading one. A leading spacer stacked with the previous entry's
        // trailing one would produce a double blank between blocks.
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
                    content.add_child(md);
                    i += 1;
                }
                AssistantBlock::Text(_) => {
                    // Whitespace-only text block — skip (TS trims + skips empty).
                    i += 1;
                }
                AssistantBlock::Image(data) if !data.is_empty() => {
                    let image = Arc::new(Image::from_data(data.clone(), ImageOptions::default()));
                    image.set_inline_visible(*self.show_images.lock().unwrap());
                    content.add_child(image);
                    i += 1;
                }
                AssistantBlock::Image(_) => {
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

                    if !self.thinking_expanded() {
                        // One static dim label per thinking run (TS path). The
                        // label row carries the click-region marker so a click
                        // can expand THIS message's thinking.
                        let label =
                            italic(&theme().colors.thinking_text.fg(&opts.hidden_thinking_label));
                        content.add_child(Arc::new(ThinkingRegionComponent::new(
                            Arc::new(Text::new(label, opts.output_pad, 0)),
                            self.message_id.lock().unwrap().clone(),
                        )));
                    } else {
                        // Render the joined thinking as one dim italic markdown
                        // section — the TS `color: thinkingText, italic: true`
                        // styling. The base style must be applied per rendered
                        // line (not by wrapping the whole markdown source with
                        // ANSI): the renderer's inner spans (inline code,
                        // links, headings) emit `SGR 0` resets, which would
                        // otherwise drop the enclosing italic/color from the
                        // rest of the block.
                        let body = joined.join("\n\n");
                        // B5e: transform the PLAIN thinking markdown first (the
                        // plugin must see markdown, not ANSI).
                        let transformed = self.transform_markdown(&body);
                        let base_style = format!("\x1b[3m{}", theme().colors.thinking_text.to_fg());
                        let md = Arc::new(
                            Markdown::new(transformed, opts.output_pad, 0)
                                .with_base_style(base_style),
                        );
                        // The marker is injected into the RENDERED rows, not
                        // into the markdown source: a prefix on line 1 would
                        // turn `# heading` / `- item` / ``` fences into a
                        // plain paragraph (pulldown-cmark sees the APC bytes
                        // as inline text).
                        content.add_child(Arc::new(ThinkingRegionComponent::new(
                            md,
                            self.message_id.lock().unwrap().clone(),
                        )));
                    }

                    // Spacer before a following visible block (TS
                    // `hasVisibleContentAfter` → Spacer(1)).
                    let has_after = blocks[i..].iter().any(|b| {
                        matches!(b, AssistantBlock::Text(t) if !t.trim().is_empty())
                            || matches!(b, AssistantBlock::Thinking(t) if !t.trim().is_empty())
                            || matches!(b, AssistantBlock::Image(data) if !data.is_empty())
                    });
                    if has_after {
                        content.add_child(Arc::new(Spacer::new(1)));
                    }
                }
            }
        }
        self.content_container
            .replace_children(content.get_children());
    }

    /// Update with error message.
    pub fn update_error(&self, error: &str, stop_reason: &str) {
        // Keep the previous complete message visible while building its error.
        let content = Container::new();
        *self.last_blocks.lock().unwrap() = Vec::new();

        // Add spacing
        content.add_child(Arc::new(Spacer::new(1)));

        // Add error message
        let error_text = match stop_reason {
            "aborted" => format!("❌ Operation aborted: {}", error),
            "error" => format!("❌ Error: {}", error),
            "length" => "⚠️ Response was truncated before completion.".to_string(),
            _ => format!("❌ {}", error),
        };

        let error_component = Arc::new(Text::new(error_text, 1, 0));
        content.add_child(error_component);
        self.content_container
            .replace_children(content.get_children());
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

    /// Assign this message's opaque id. Once set, thinking rows carry the
    /// zero-width click-region marker the renderer resolves to hit rectangles.
    pub fn set_message_id(&self, id: &str) {
        let changed = {
            let mut current = self.message_id.lock().unwrap();
            let changed = current.as_deref() != Some(id);
            if changed {
                *current = Some(id.to_string());
            }
            changed
        };
        if changed {
            self.rebuild_if_any();
        }
    }

    /// This message's id, if the host assigned one.
    pub fn message_id(&self) -> Option<String> {
        self.message_id.lock().unwrap().clone()
    }

    /// Expand this message's thinking regardless of the global hide option
    /// (`Some(true)`), collapse it back to the global preference (`None`), or
    /// force-collapse it (`Some(false)`). Rebuilds the content immediately.
    pub fn set_expand_thinking(&self, expand: Option<bool>) {
        if *self.expand_thinking.lock().unwrap() == expand {
            return;
        }
        *self.expand_thinking.lock().unwrap() = expand;
        self.rebuild_if_any();
    }

    /// The per-message expansion override.
    pub fn expand_thinking(&self) -> Option<bool> {
        *self.expand_thinking.lock().unwrap()
    }

    /// Whether this message renders full thinking bodies right now: the
    /// global option shows them, or the per-message override expands them.
    fn thinking_expanded(&self) -> bool {
        match *self.expand_thinking.lock().unwrap() {
            Some(expanded) => expanded,
            None => !self.options.lock().unwrap().hide_thinking,
        }
    }

    /// Rebuild from the last blocks when there is anything to rebuild.
    fn rebuild_if_any(&self) {
        let blocks = self.last_blocks.lock().unwrap().clone();
        if !blocks.is_empty() {
            self.rebuild_content(&blocks);
        }
    }

    pub fn set_show_images(&self, show: bool) {
        *self.show_images.lock().unwrap() = show;
        for child in self.content_container.get_children() {
            if let Some(image) = child.as_any().downcast_ref::<Image>() {
                image.set_inline_visible(show);
            }
        }
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
    pub fn markdown_transformer(&self) -> Option<Arc<dyn Fn(&str) -> String + Send + Sync>> {
        self.markdown_transformer.lock().unwrap().clone()
    }

    /// Add trailing rows only when the message renders visible content.
    pub fn set_trailing_spacing(&self, rows: usize) {
        *self.trailing_spacing.lock().unwrap() = rows;
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
        let mut lines = self.content_container.render(width);
        // A provider can start with no content or whitespace-only deltas.
        // Such a message must not reserve even its inter-message spacing.
        if !lines.is_empty() {
            lines.extend((0..*self.trailing_spacing.lock().unwrap()).map(|_| String::new()));
        }
        lines
    }

    fn render_document(&self, width: usize) -> RenderedDocument {
        let mut document = self.content_container.render_document(width);
        if !document.is_empty() {
            let spacing = *self.trailing_spacing.lock().unwrap();
            if spacing > 0 {
                document.append(RenderedDocument::from_lines(vec![String::new(); spacing]));
            }
        }
        document
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
    fn trailing_spacing_does_not_render_for_empty_streaming_content() {
        let msg = AssistantMessageComponent::default();
        msg.set_trailing_spacing(1);
        msg.set_streaming(true);
        for blocks in [
            vec![],
            vec![AssistantBlock::Text(" \n\t".into())],
            vec![AssistantBlock::Thinking(" ".into())],
        ] {
            msg.update_blocks(&blocks);
            assert!(msg.render(80).is_empty());
        }
        msg.update_text("Hello");
        msg.set_trailing_spacing(0);
        let mut expected = msg.render(80);
        expected.push(String::new());
        msg.set_trailing_spacing(1);
        assert_eq!(msg.render(80), expected);
        msg.update_blocks(&[]);
        msg.set_streaming(false);
        assert!(msg.render(80).is_empty());
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
        assert!(
            think_pos < text_pos,
            "thinking should precede text: {joined}"
        );
    }

    #[test]
    fn test_thinking_style_survives_inline_code_reset() {
        // Regression: the thinking base style used to be applied by wrapping
        // the whole markdown source with ANSI. Inline code emits an `SGR 0`
        // reset, which dropped the italic + thinking color for the rest of the
        // block. The base style must be re-opened after that reset.
        let msg = AssistantMessageComponent::default();
        msg.update_blocks(&[AssistantBlock::Thinking(
            "consider `render_inline` then keep thinking".to_string(),
        )]);
        let rendered = msg.render(80).join("\n");
        let fg = theme().colors.thinking_text.to_fg();
        let after_code = rendered
            .split("render_inline")
            .nth(1)
            .expect("inline code not rendered");
        assert!(
            after_code.contains("keep thinking"),
            "text after inline code missing: {rendered:?}"
        );
        assert!(
            after_code.contains("\x1b[3m") && after_code.contains(&fg),
            "thinking italic/color lost after inline code: {rendered:?}"
        );
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
        assert!(
            !after.contains("internal"),
            "not rebuilt after toggle: {after}"
        );
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

    // ---- Click-to-expand thinking probes ----

    const TEST_ID: &str = "t42";

    fn hidden_message() -> AssistantMessageComponent {
        let msg = AssistantMessageComponent::new(AssistantMessageOptions {
            hide_thinking: true,
            ..Default::default()
        });
        msg.set_message_id(TEST_ID);
        msg.update_blocks(&[
            AssistantBlock::Thinking("secret reasoning".to_string()),
            AssistantBlock::Text("Public answer.".to_string()),
        ]);
        msg
    }

    #[test]
    fn per_message_expand_overrides_global_hide() {
        // Axis: per-message state vs global preference. A collapsed message
        // (global hide on) must show its body after the expand override, and
        // clearing the override must collapse back to the global preference.
        let msg = hidden_message();
        let collapsed = strip_ansi(&msg.render(80).join("\n"));
        assert!(collapsed.contains("Thinking..."));
        assert!(!collapsed.contains("secret reasoning"));

        msg.set_expand_thinking(Some(true));
        let expanded = strip_ansi(&msg.render(80).join("\n"));
        assert!(
            expanded.contains("secret reasoning"),
            "override did not expand: {expanded}"
        );
        assert!(expanded.contains("Public answer."));

        msg.set_expand_thinking(None);
        let back = strip_ansi(&msg.render(80).join("\n"));
        assert!(
            !back.contains("secret reasoning"),
            "None must collapse back to the global preference"
        );
    }

    #[test]
    fn thinking_rows_carry_click_region_marker() {
        // Axis: marker geometry. The marker must exist on the collapsed label
        // row AND on the expanded body rows, must be zero-width, and must
        // advertise a width covering the whole rendered line.
        let msg = hidden_message();
        let collapsed = msg.render(80);
        let label_line = collapsed
            .iter()
            .find(|l| strip_ansi(l).contains("Thinking..."))
            .expect("collapsed label row");
        assert!(label_line.contains(crate::thinking_region::THINKING_REGION_MARKER_PREFIX));
        assert_eq!(
            crate::ansi::visible_width(label_line),
            crate::ansi::visible_width(&strip_ansi(label_line)),
            "marker is not zero-width"
        );
        let prefix = crate::thinking_region::THINKING_REGION_MARKER_PREFIX;
        let marker_start = label_line.rfind(prefix).expect("marker on the label row");
        let (_, advertised, _) =
            crate::thinking_region::parse_marker_public(label_line, marker_start)
                .expect("wellformed marker on the label row");
        let label_width = crate::ansi::visible_width(&strip_ansi(label_line));
        assert!(
            advertised >= label_width,
            "marker width {advertised} < label width {label_width}"
        );

        // Expanded body rows carry the marker too.
        msg.set_expand_thinking(Some(true));
        let expanded = msg.render(80);
        let body_row = expanded
            .iter()
            .find(|l| strip_ansi(l).contains("secret reasoning"))
            .expect("expanded body row");
        assert!(body_row.contains(prefix));
    }

    #[test]
    fn global_toggle_rebuilds_but_keeps_per_message_override() {
        // Axis: interaction between the global Ctrl+T toggle and the override.
        // A globally-hidden message expanded by click stays expanded when the
        // host re-applies the global hide (the rebuild must not clear it).
        let msg = hidden_message();
        msg.set_expand_thinking(Some(true));
        msg.set_hide_thinking(true);
        let rendered = strip_ansi(&msg.render(80).join("\n"));
        assert!(
            rendered.contains("secret reasoning"),
            "global toggle wiped the per-message expansion: {rendered}"
        );
    }

    #[test]
    fn show_thinking_global_mode_collapses_on_click_too() {
        // Axis: the other side of the state space. With the global preference
        // SHOWING thinking, a click must collapse THIS message (Some(false)),
        // and clearing the override restores the global show.
        let msg = AssistantMessageComponent::default();
        msg.set_message_id("t42");
        msg.update_blocks(&[AssistantBlock::Thinking("loud thinking".to_string())]);
        assert!(strip_ansi(&msg.render(80).join("\n")).contains("loud thinking"));
        msg.set_expand_thinking(Some(false));
        assert!(
            !strip_ansi(&msg.render(80).join("\n")).contains("loud thinking"),
            "force-collapse did not hide the body"
        );
        msg.set_expand_thinking(None);
        assert!(strip_ansi(&msg.render(80).join("\n")).contains("loud thinking"));
    }

    #[test]
    fn message_without_id_renders_no_marker() {
        // Extension-reused components (markdown custom messages) never get an
        // id: their rows must stay marker-free so they can never resolve to a
        // click region.
        let msg = AssistantMessageComponent::new(AssistantMessageOptions {
            hide_thinking: true,
            ..Default::default()
        });
        msg.update_blocks(&[AssistantBlock::Thinking("quiet".to_string())]);
        let rendered = msg.render(80).join("\n");
        assert!(!rendered.contains(crate::thinking_region::THINKING_REGION_MARKER_PREFIX));
    }

    #[test]
    fn region_marker_does_not_break_thinking_markdown_blocks() {
        // Regression: prefixing the marker to the markdown SOURCE made a
        // leading `#` an ordinary paragraph. The marker must live in the
        // rendered rows only; the heading still renders as a heading.
        let msg = AssistantMessageComponent::default();
        msg.set_message_id(TEST_ID);
        msg.update_blocks(&[AssistantBlock::Thinking("# Heading\n\n- item".to_string())]);
        let rendered = msg.render(80).join("\n");
        let stripped = strip_ansi(&rendered);
        assert!(stripped.contains("Heading"), "heading lost: {stripped}");
        assert!(stripped.contains("item"), "list item lost: {stripped}");
        assert!(
            !stripped.contains("# Heading"),
            "heading was not parsed: {stripped}"
        );
        assert!(rendered.contains(crate::thinking_region::THINKING_REGION_MARKER_PREFIX));
    }
}
