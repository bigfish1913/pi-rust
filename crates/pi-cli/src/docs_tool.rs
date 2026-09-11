//! Built-in `docs` tool for looking up rpi usage documentation.
//!
//! Pi ships its coding-agent documentation with the CLI and lets the model
//! reach it through the normal read/documentation flow. rpi keeps the same
//! model-facing behavior with a small, read-only topic index. The selected
//! Markdown pages are embedded at compile time so an installed binary does
//! not depend on the source checkout being present.

use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, ToolExecutionMode, ToolResultPartial};
use rpi_ai::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

const MAX_SEARCH_RESULTS: usize = 8;
const MAX_RESULT_CHARS: usize = 18_000;

struct DocPage {
    topic: &'static str,
    description: &'static str,
    content: &'static str,
    aliases: &'static [&'static str],
}

static DOCS: &[DocPage] = &[
    DocPage {
        topic: "guide",
        description: "完整使用手册：安装、模型、CLI、.rpi 资源、Pi package、扩展、SDK、排错和发布",
        content: include_str!("../../../docs/user-guide.md"),
        aliases: &["manual", "user-guide", "cli", "quickstart"],
    },
    DocPage {
        topic: "overview",
        description: "rpi installation, built-in tools, configuration, packages, and release basics",
        content: include_str!("../../../README.md"),
        aliases: &["readme", "getting-started", "start", "usage"],
    },
    DocPage {
        topic: "extensions",
        description: "Rust plugins, Pi JavaScript/TypeScript extensions, runtime capabilities, and UI compatibility",
        content: include_str!("../../../docs/extension-backends.md"),
        aliases: &["plugin", "plugins", "extension", "js", "typescript", "ts"],
    },
    DocPage {
        topic: "architecture",
        description: "rpi crate layering, agent loop, provider, harness, sessions, and extension boundaries",
        content: include_str!("../../../docs/architecture.md"),
        aliases: &["design", "crates", "sdk"],
    },
    DocPage {
        topic: "compatibility",
        description: "known Pi parity decisions, resource precedence, and remaining compatibility notes",
        content: include_str!("../../../docs/m6-cli-open-questions.md"),
        aliases: &["pi", "parity", "migration"],
    },
];

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DocsInput {
    /// Topic to open. Omit it, or use `list`, to list available topics.
    #[serde(default)]
    pub topic: Option<String>,
    /// Optional case-insensitive text to find inside the selected topic.
    #[serde(default)]
    pub query: Option<String>,
}

pub struct DocsTool {
    schema: Tool,
}

impl DocsTool {
    fn new() -> Self {
        let params = schemars::schema_for!(DocsInput);
        Self {
            schema: Tool {
                name: "docs".to_string(),
                description: "Look up rpi usage documentation. Omit topic (or use topic=list) to list topics; pass a topic such as guide, overview, extensions, architecture, or compatibility. Add query to find relevant sections. Use this before guessing rpi commands, Pi package compatibility, extension APIs, or .rpi configuration.".to_string(),
                parameters: rpi_ai::types::Schema::new(
                    serde_json::to_value(params).unwrap_or_default(),
                ),
                constrained_sampling: None,
            },
        }
    }
}

pub fn create_docs_tool() -> Arc<dyn AgentTool> {
    Arc::new(DocsTool::new())
}

#[async_trait]
impl AgentTool for DocsTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        "docs"
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: DocsInput = serde_json::from_value(params)
            .map_err(|error| AgentError::Validation(format!("docs input invalid: {error}")))?;
        if signal.is_cancelled() {
            return Err(AgentError::Tool("docs lookup cancelled".into()));
        }

        let topic = input.topic.as_deref().unwrap_or("list").trim();
        if topic.is_empty() || topic.eq_ignore_ascii_case("list") || topic == "*" {
            return Ok(AgentToolResult::text(format_catalog()));
        }

        let page = DOCS
            .iter()
            .find(|page| {
                page.topic.eq_ignore_ascii_case(topic)
                    || page
                        .aliases
                        .iter()
                        .any(|alias| alias.eq_ignore_ascii_case(topic))
            })
            .ok_or_else(|| {
                AgentError::Tool(format!(
                    "Unknown rpi docs topic `{topic}`. Available topics: {}",
                    DOCS.iter()
                        .map(|page| page.topic)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;

        let query = input
            .query
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let body = match query {
            Some(query) => search_page(page, query),
            None => truncate_result(page.content),
        };
        Ok(AgentToolResult::text(format!(
            "# rpi docs: {}\n\n{}",
            page.topic, body
        )))
    }
}

fn format_catalog() -> String {
    let mut out = String::from("Available rpi documentation topics:\n");
    for page in DOCS {
        out.push_str(&format!("- {}: {}\n", page.topic, page.description));
    }
    out.push_str("\nUse docs with {\"topic\": \"extensions\"} or add {\"query\": \"install-pi\"} for a focused lookup.");
    out
}

fn search_page(page: &DocPage, query: &str) -> String {
    let needle = query.to_lowercase();
    let lines: Vec<&str> = page.content.lines().collect();
    let mut selected = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if !line.to_lowercase().contains(&needle) {
            continue;
        }
        let start = index.saturating_sub(2);
        let end = (index + 3).min(lines.len());
        for line_no in start..end {
            if !selected.contains(&line_no) {
                selected.push(line_no);
            }
        }
        if selected.len() >= MAX_SEARCH_RESULTS * 5 {
            break;
        }
    }
    if selected.is_empty() {
        return format!(
            "No matches for `{query}` in `{}`. Try docs with topic=list or a broader query.",
            page.topic
        );
    }
    let mut out = format!("Matches for `{query}` in `{}`:\n\n", page.topic);
    for line_no in selected {
        out.push_str(&format!("{:>5}: {}\n", line_no + 1, lines[line_no]));
        if out.len() >= MAX_RESULT_CHARS {
            out.push_str("\n[Result truncated; run another focused docs query.]\n");
            break;
        }
    }
    out
}

fn truncate_result(content: &str) -> String {
    if content.len() <= MAX_RESULT_CHARS {
        return content.to_string();
    }
    let mut end = MAX_RESULT_CHARS;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n[Document truncated; use docs with a query for a focused section.]",
        &content[..end]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn execute(input: serde_json::Value) -> String {
        let tool = DocsTool::new();
        let result = tool
            .execute("test", input, CancellationToken::new(), Arc::new(|_| {}))
            .await
            .unwrap();
        match &result.content[0] {
            rpi_agent::types::TextContentOrImage::Text(text) => text.text.clone(),
            _ => panic!("docs returned an image"),
        }
    }

    #[tokio::test]
    async fn lists_topics() {
        let output = execute(serde_json::json!({})).await;
        assert!(output.contains("overview"));
        assert!(output.contains("guide"));
        assert!(output.contains("extensions"));
    }

    #[tokio::test]
    async fn returns_complete_user_guide() {
        let output = execute(serde_json::json!({"topic": "guide", "query": "install-pi"})).await;
        assert!(output.contains("rpi install-pi"));
        assert!(output.contains("npm"));
    }

    #[tokio::test]
    async fn returns_focused_search_results() {
        let output = execute(serde_json::json!({
            "topic": "extensions",
            "query": "Node"
        }))
        .await;
        assert!(output.contains("Node"));
        assert!(output.contains("Matches for"));
    }

    #[tokio::test]
    async fn rejects_unknown_topic() {
        let error = DocsTool::new()
            .execute(
                "test",
                serde_json::json!({"topic": "missing"}),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("Unknown rpi docs topic"));
    }
}
