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
        topic: "authoring",
        description: "Rust cdylib 扩展开发最佳实践：ABI v3/v2 入口、工具生命周期、事件处理器、资源发现、rpi dev 开发与发布检查清单",
        content: include_str!("../embedded-docs/extension-authoring.md"),
        aliases: &["package-authoring", "extension-authoring", "create-package", "create-extension"],
    },
    DocPage {
        topic: "guide",
        description: "完整使用手册：安装、模型、CLI、.rpi 资源、Rust 扩展、SDK、排错和发布",
        content: include_str!("../embedded-docs/user-guide.md"),
        aliases: &["manual", "user-guide", "cli", "quickstart"],
    },
    DocPage {
        topic: "overview",
        description: "rpi installation, built-in tools, configuration, extensions, and release basics",
        content: include_str!("../embedded-docs/README.md"),
        aliases: &["readme", "getting-started", "start", "usage"],
    },
    DocPage {
        topic: "agent",
        description: "用 rpi SDK 创建自己的 Agent 项目的默认结构与规范：目录布局、内嵌 Agent、cdylib 适配、工具单一登记点、prompts/skills、配置与开发循环",
        content: include_str!("../embedded-docs/agent-project.md"),
        aliases: &[
            "agent-project",
            "project-layout",
            "create-agent",
            "scaffold",
            "project-structure",
        ],
    },
    DocPage {
        topic: "architecture",
        description: "rpi crate layering, agent loop, provider, harness, sessions, and extension boundaries",
        content: include_str!("../embedded-docs/architecture.md"),
        aliases: &["design", "crates", "sdk"],
    },
    DocPage {
        topic: "debugging",
        description: "Rust cdylib 扩展编写（ABI v3/v2、工具生命周期、事件处理器）与 agent/扩展调试（事件日志、rpi dev-local、常见失败定位）",
        content: include_str!("../embedded-docs/rust-debugging.md"),
        aliases: &["rust", "rust-debug", "extension-debug", "troubleshoot", "debug"],
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
                description: "Look up rpi usage documentation. Omit topic (or use topic=list) to list topics; pass a topic such as guide, authoring, agent, debugging, architecture, or overview. Add query to find relevant sections. Use this before guessing rpi commands, creating an agent project or extension, extension APIs, or .rpi configuration.".to_string(),
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
    out.push_str("\nUse docs with {\"topic\": \"agent\"} or add {\"query\": \"AgentBuilder\"} for a focused lookup.");
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
        assert!(output.contains("authoring"));
        assert!(output.contains("- agent:"));
        assert!(output.contains("debugging"));
        assert!(!output.contains("- extensions:"));
        assert!(!output.contains("- compatibility:"));
    }

    #[tokio::test]
    async fn returns_rust_debugging_guide() {
        let output = execute(serde_json::json!({"topic": "debugging", "query": "rpi dev"})).await;
        assert!(output.contains("rpi dev"));
        assert!(output.contains("cdylib"));
    }

    #[tokio::test]
    async fn returns_complete_user_guide() {
        let output = execute(serde_json::json!({"topic": "guide", "query": "cdylib"})).await;
        assert!(output.contains("cdylib"));
        assert!(output.contains("Rust"));
    }

    #[tokio::test]
    async fn returns_extension_authoring_practices() {
        let output = execute(serde_json::json!({"topic": "authoring", "query": "rpi dev"})).await;
        assert!(output.contains("rpi dev"));

        // The cdylib build contract is a separate section from the `rpi dev`
        // workflow text, so query for it directly rather than relying on both
        // terms co-occurring in one excerpt window.
        let cdylib = execute(serde_json::json!({"topic": "authoring", "query": "cdylib"})).await;
        assert!(cdylib.contains("cdylib"));
    }

    #[tokio::test]
    async fn returns_agent_project_guide() {
        let output = execute(serde_json::json!({
            "topic": "agent",
            "query": "AgentBuilder"
        }))
        .await;
        assert!(output.contains("AgentBuilder"));
        assert!(output.contains("Matches for"));
    }

    #[tokio::test]
    async fn agent_aliases_resolve() {
        let output = execute(serde_json::json!({"topic": "create-agent"})).await;
        assert!(output.contains("rpi Agent 项目结构创建指南"));
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
