//! Mirrors `packages/agent/src/harness/tools/read.ts` — the built-in `read`
//! `AgentTool`. Reads a file (text or image), applies head-truncation with a
//! continuation hint, and detects supported image MIME types.
//!
//! Divergences from TS (documented): the TS supports an injected `imageProcessor`
//! (resize/convert). The v1 Rust port ships no processor — BMP images yield an
//! "omitted" text notice (matching the TS no-processor path), and other supported
//! images are base64-encoded inline. The `ReadImageProcessor` trait seam is kept
//! for a future impl.

use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use rpi_ai::types::{ImageContent, ImageContentType, Tool};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::env::ExecutionEnv;
use crate::image::{detect_supported_image_mime_type, encode_base64};
use crate::path_utils::resolve_read_tool_path;
use crate::tools::tool_context::ExecutionToolContext;
use crate::truncate::{
    format_size, truncate_head, TruncationOptions, TruncationResult, DEFAULT_MAX_BYTES,
    DEFAULT_MAX_LINES,
};
/// Input for the read tool. Mirrors TS `ReadToolInput`. `offset` is 1-indexed.
#[derive(Debug, Clone, JsonSchema, Deserialize)]
pub struct ReadInput {
    /// Path to the file to read (relative or absolute).
    pub path: String,
    /// Line number to start reading from (1-indexed).
    #[serde(default)]
    pub offset: Option<u32>,
    /// Maximum number of lines to read.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Structured details for the read tool. Mirrors TS `ReadToolDetails`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReadToolDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationResult>,
}

/// Options for `create_read_tool`. Mirrors TS `ReadToolOptions`. The Rust port
/// keeps the `auto_resize_images` flag for forward-compat but has no processor.
#[derive(Debug, Clone, Default)]
pub struct ReadToolOptions {
    /// Whether an injected image processor should resize images. Default true.
    pub auto_resize_images: bool,
}

/// Optional image-conversion seam (v1 ships no impl). Mirrors TS
/// `ReadImageProcessor`.
pub trait ReadImageProcessor: Send + Sync {
    fn process(
        &self,
        bytes: &[u8],
        mime_type: &str,
        auto_resize: bool,
    ) -> Result<ProcessedImage, String>;
}

/// Result of a successful [`ReadImageProcessor::process`].
pub struct ProcessedImage {
    pub data: String,
    pub mime_type: String,
    pub hints: Vec<String>,
}

/// The built-in `read` tool. Holds an `Arc<dyn ExecutionEnv>` (read-only view)
/// and an optional image processor.
pub struct ReadTool {
    schema: Tool,
    env: Arc<dyn ExecutionEnv>,
    options: ReadToolOptions,
    image_processor: Option<Arc<dyn ReadImageProcessor>>,
}

impl ReadTool {
    fn schema() -> Tool {
        let params = schemars::schema_for!(ReadInput);
        Tool {
            name: "read".to_string(),
            description: format!(
                "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). \
                 Images are sent as attachments. For text files, output is truncated to {DEFAULT_MAX_LINES} \
                 lines or {kb}KB (whichever is hit first). Use offset/limit for large files. When you need \
                 the full file, continue with offset until complete.",
                kb = DEFAULT_MAX_BYTES / 1024
            ),
            parameters: rpi_ai::types::Schema::new(serde_json::to_value(params).unwrap_or_default()),
            constrained_sampling: None,
        }
    }
}

/// Build the `read` tool from a context. Mirrors TS `createReadTool`.
pub fn create_read_tool(
    context: &ExecutionToolContext,
    options: Option<ReadToolOptions>,
) -> Arc<dyn AgentTool> {
    Arc::new(ReadTool {
        schema: ReadTool::schema(),
        env: context.env().clone(),
        options: options.unwrap_or_default(),
        image_processor: None,
    })
}

#[async_trait]
impl AgentTool for ReadTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "read"
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: ReadInput = serde_json::from_value(params)
            .map_err(|e| AgentError::Validation(format!("read input invalid: {e}")))?;

        let abs = resolve_read_tool_path(&*self.env, &input.path, Some(&signal))
            .await
            .map_err(file_err_to_agent)?;
        let bytes = self
            .env
            .read_binary_file(&abs, Some(&signal))
            .await
            .map_err(file_err_to_agent)?;

        // Image path.
        if let Some(mime) = detect_supported_image_mime_type(&bytes) {
            return Ok(image_result(
                mime,
                &bytes,
                &self.options,
                self.image_processor.as_deref(),
            ));
        }

        // Text path.
        let text_content = String::from_utf8(bytes).map_err(|e| {
            AgentError::Tool(format!("read: invalid utf-8 in {}: {e}", input.path))
        })?;
        let all_lines: Vec<&str> = text_content.split('\n').collect();
        // Mirror TS: `textContent.split("\n")` keeps a trailing "" when the
        // content ends with '\n'; the line-count + slice math below uses the raw
        // split (NOT `split_lines_for_counting`), so we keep it as-is.
        let total_file_lines = all_lines.len();
        let start_line = input
            .offset
            .map(|o| (o.saturating_sub(1)) as usize)
            .unwrap_or(0);
        let start_line_display = start_line + 1;
        if start_line >= all_lines.len() {
            return Err(AgentError::Tool(format!(
                "Offset {} is beyond end of file ({} lines total)",
                input.offset.unwrap_or(1),
                all_lines.len()
            )));
        }

        let (selected_content, user_limited_lines): (String, Option<usize>) =
            if let Some(limit) = input.limit {
                let end_line = std::cmp::min(start_line + limit as usize, all_lines.len());
                (
                    all_lines[start_line..end_line].join("\n"),
                    Some(end_line - start_line),
                )
            } else {
                (all_lines[start_line..].join("\n"), None)
            };

        let truncation = truncate_head(&selected_content, TruncationOptions::default());
        let (output_text, details): (String, Option<TruncationResult>) =
            if truncation.first_line_exceeds_limit {
                let first_line_size = format_size(all_lines[start_line].len());
                (
                    format!(
                        "[Line {start_line_display} is {first_line_size}, exceeds {} limit. \
                         Use bash: sed -n '{start_line_display}p' {} | head -c {DEFAULT_MAX_BYTES}]",
                        format_size(DEFAULT_MAX_BYTES),
                        input.path
                    ),
                    Some(truncation),
                )
            } else if truncation.truncated {
                let end_line_display = start_line_display + truncation.output_lines - 1;
                let next_offset = end_line_display + 1;
                let mut text = truncation.content.clone();
                match truncation.truncated_by {
                    Some(crate::truncate::TruncationLimit::Lines) => {
                        text.push_str(&format!(
                            "\n\n[Showing lines {start_line_display}-{end_line_display} of \
                             {total_file_lines}. Use offset={next_offset} to continue.]"
                        ));
                    }
                    _ => {
                        text.push_str(&format!(
                            "\n\n[Showing lines {start_line_display}-{end_line_display} of \
                             {total_file_lines} ({} limit). Use offset={next_offset} to continue.]",
                            format_size(DEFAULT_MAX_BYTES)
                        ));
                    }
                }
                (text, Some(truncation))
            } else if let Some(ull) = user_limited_lines {
                if start_line + ull < all_lines.len() {
                    let remaining = all_lines.len() - (start_line + ull);
                    let next_offset = start_line + ull + 1;
                    (
                        format!(
                            "{}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
                            truncation.content
                        ),
                        None,
                    )
                } else {
                    (truncation.content, None)
                }
            } else {
                (truncation.content, None)
            };

        let details_value = match details {
            Some(t) => serde_json::to_value(ReadToolDetails { truncation: Some(t) })
                .unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        };
        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(output_text)],
            details: details_value,
            ..Default::default()
        })
    }
}

/// Build the image-path result, honoring an optional processor. Mirrors the TS
/// image branch.
fn image_result(
    mime: &str,
    bytes: &[u8],
    options: &ReadToolOptions,
    processor: Option<&dyn ReadImageProcessor>,
) -> AgentToolResult {
    if let Some(proc) = processor {
        match proc.process(bytes, mime, options.auto_resize_images) {
            Ok(p) => {
                let hints = if p.hints.is_empty() {
                    String::new()
                } else {
                    format!("\n{}", p.hints.join("\n"))
                };
                return AgentToolResult {
                    content: vec![
                        TextContentOrImage::text(format!("Read image file [{}]{}", p.mime_type, hints)),
                        TextContentOrImage::Image(ImageContent {
                            kind: ImageContentType,
                            data: p.data,
                            mime_type: p.mime_type,
                        }),
                    ],
                    details: serde_json::Value::Null,
                    ..Default::default()
                };
            }
            Err(message) => {
                return AgentToolResult {
                    content: vec![TextContentOrImage::text(format!(
                        "Read image file [{mime}]\n{message}"
                    ))],
                    details: serde_json::Value::Null,
                    ..Default::default()
                };
            }
        }
    }
    // No processor — BMP is omitted; others are inlined.
    if mime == "image/bmp" {
        return AgentToolResult {
            content: vec![TextContentOrImage::text(
                "Read image file [image/bmp]\n[Image omitted: configure an imageProcessor to convert BMP images.]",
            )],
            details: serde_json::Value::Null,
            ..Default::default()
        };
    }
    AgentToolResult {
        content: vec![
            TextContentOrImage::text(format!("Read image file [{mime}]")),
            TextContentOrImage::Image(ImageContent {
                kind: ImageContentType,
                data: encode_base64(bytes),
                mime_type: mime.to_string(),
            }),
        ],
        details: serde_json::Value::Null,
        ..Default::default()
    }
}

/// Map a `FileError` into an `AgentError::Tool`, mirroring the TS `getOrThrow`
/// surface (the message carries the path + code).
fn file_err_to_agent(e: crate::error::FileError) -> AgentError {
    AgentError::Tool(e.to_string())
}
