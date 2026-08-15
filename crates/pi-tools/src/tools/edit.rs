//! Mirrors `packages/agent/src/harness/tools/edit.ts` — the built-in `edit`
//! `AgentTool`. Performs exact-text replacements (with fuzzy fallback) on a
//! single file, serialized per canonical path via the file-mutation-queue.
//!
//! `prepare_arguments` folds legacy top-level `oldText`/`newText` into the
//! `edits[]` array and parses a JSON-string `edits`, mirroring
//! `prepareEditArguments`. The execute path runs:
//! `fileInfo` → `readTextFile` → `stripBom` → `detectLineEnding` →
//! `normalizeToLF` → `applyEditsToNormalizedContent` → `restoreLineEndings` →
//! `writeFile`, with `generateDiffString` + `generateUnifiedPatch` in the
//! details.

use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::FutureExt;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use rpi_ai::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::env::FileContent;
use crate::error::{FileError, FileErrorCode};
use crate::file_mutation_queue::{with_file_mutation_queue, MutatingEnv};
use crate::path_utils::resolve_tool_path;
use crate::tools::edit_diff::{
    apply_edits_to_normalized_content, detect_line_ending, generate_diff_string,
    generate_unified_patch, normalize_to_lf, restore_line_endings, strip_bom, ApplyResult,
    ReplaceEdit,
};
use crate::tools::tool_context::ExecutionToolContext;

/// Input for the edit tool. Mirrors TS `EditToolInput`.
#[derive(Debug, Clone, JsonSchema, Deserialize)]
pub struct EditInput {
    /// Path to the file to edit (relative or absolute).
    pub path: String,
    /// One or more targeted replacements.
    pub edits: Vec<ReplaceEditJson>,
}

/// JSON-schema-facing edit shape (camelCase to match the TS wire contract).
#[derive(Debug, Clone, JsonSchema, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplaceEditJson {
    /// Exact text for one targeted replacement. Must be unique in the original
    /// file and must not overlap with any other edit's `oldText` in the same call.
    pub old_text: String,
    /// Replacement text for this targeted edit.
    pub new_text: String,
}

/// Structured details for the edit tool. Mirrors TS `EditToolDetails`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EditToolDetails {
    pub diff: String,
    pub patch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_changed_line: Option<usize>,
}

/// The built-in `edit` tool.
pub struct EditTool {
    schema: Tool,
    env: Arc<dyn MutatingEnv>,
}

impl EditTool {
    fn schema() -> Tool {
        let params = schemars::schema_for!(EditInput);
        Tool {
            name: "edit".to_string(),
            description: "Edit a single file using exact text replacement. Every edits[].oldText \
                          must match a unique, non-overlapping region of the original file. If two \
                          changes affect the same block or nearby lines, merge them into one edit \
                          instead of emitting overlapping edits. Do not include large unchanged \
                          regions just to connect distant changes."
                .to_string(),
            parameters: rpi_ai::types::Schema::new(serde_json::to_value(params).unwrap_or_default()),
            constrained_sampling: None,
        }
    }
}

/// Build the `edit` tool from a context. Requires a mutation-capable env.
/// Mirrors TS `createEditTool`.
pub fn create_edit_tool(context: &ExecutionToolContext) -> Arc<dyn AgentTool> {
    let env = context
        .mutating_env()
        .expect("edit tool requires a mutation-capable ExecutionToolContext")
        .clone();
    Arc::new(EditTool {
        schema: EditTool::schema(),
        env,
    })
}

#[async_trait]
impl AgentTool for EditTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "edit"
    }

    /// Fold legacy `oldText`/`newText` and parse a JSON-string `edits`. Mirrors
    /// `prepareEditArguments`.
    fn prepare_arguments(&self, args: serde_json::Value) -> Result<serde_json::Value, AgentError> {
        Ok(prepare_edit_arguments(args))
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: EditInput = serde_json::from_value(params)
            .map_err(|e| AgentError::Validation(format!("edit input invalid: {e}")))?;
        // Validate edits non-empty.
        if input.edits.is_empty() {
            return Err(AgentError::Tool(
                "Edit tool input is invalid. edits must contain at least one replacement.".into(),
            ));
        }
        let edits: Vec<ReplaceEdit> = input
            .edits
            .into_iter()
            .map(|e| ReplaceEdit {
                old_text: e.old_text,
                new_text: e.new_text,
            })
            .collect();
        let path = input.path.clone();
        let path_for_msg = path.clone();
        let edits_count = edits.len();

        let abs = resolve_tool_path(self.env.as_env(), &path, Some(&signal))
            .await
            .map_err(file_err_to_agent)?;
        let abs_str = abs.clone();

        let env = self.env.clone();
        let env_ref = env.clone();
        let abs_for_closure = abs_str.clone();
        let apply_result: Result<ApplyResult, FileError> = with_file_mutation_queue(
            &*env_ref,
            &abs_str,
            &signal,
            move |cancel: &CancellationToken| -> BoxFuture<'_, Result<ApplyResult, FileError>> {
                let env = env.clone();
                let edits = edits.clone();
                let path = path.clone();
                let abs_str = abs_for_closure.clone();
                async move {
                    check_abort(cancel)?;
                    // FileInfo: must be a file or symlink.
                    let info = env
                        .as_env()
                        .file_info(&abs_str, Some(cancel))
                        .await?;
                    if !matches!(info.kind, crate::env::FileKind::File | crate::env::FileKind::Symlink) {
                        return Err(FileError::new(
                            FileErrorCode::Invalid,
                            format!("Could not edit file: {path}. Path is not a file."),
                        ));
                    }
                    let read = env.as_env().read_text_file(&abs_str, Some(cancel)).await?;
                    check_abort(cancel)?;

                    let bom_res = strip_bom(&read);
                    let bom = bom_res.bom;
                    let content = bom_res.text;
                    let original_ending = detect_line_ending(content);
                    let normalized = normalize_to_lf(content);
                    let apply = apply_edits_to_normalized_content(&normalized, &edits, &path)?;
                    check_abort(cancel)?;

                    let final_content =
                        format!("{bom}{}", restore_line_endings(&apply.new_content, original_ending));
                    env.as_env()
                        .write_file(&abs_str, FileContent::Text(final_content), Some(cancel))
                        .await?;
                    check_abort(cancel)?;
                    Ok(apply)
                }
                .boxed()
            },
        )
        .await;

        let apply = apply_result.map_err(file_err_to_agent)?;
        let diff = generate_diff_string(&apply.base_content, &apply.new_content, 4);
        let patch = generate_unified_patch(&path_for_msg, &apply.base_content, &apply.new_content);

        let details = serde_json::to_value(EditToolDetails {
            diff: diff.diff,
            patch,
            first_changed_line: diff.first_changed_line,
        })
        .unwrap_or(serde_json::Value::Null);

        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(format!(
                "Successfully replaced {edits_count} block(s) in {path_for_msg}."
            ))],
            details,
            ..Default::default()
        })
    }
}

/// Mirrors `prepareEditArguments`: if `edits` is a JSON string, parse it into an
/// array; if legacy `oldText`/`newText` are present, append them to `edits` and
/// strip them. Returns the (possibly rewritten) args value.
fn prepare_edit_arguments(input: serde_json::Value) -> serde_json::Value {
    let mut args = match input {
        serde_json::Value::Object(m) => m,
        other => return other,
    };
    // Parse a JSON-string `edits`.
    if let Some(edits_val) = args.get("edits").cloned() {
        if let serde_json::Value::String(s) = edits_val {
            if let Ok(serde_json::Value::Array(arr)) = serde_json::from_str::<serde_json::Value>(&s)
            {
                args.insert("edits".to_string(), serde_json::Value::Array(arr));
            }
        }
    }
    // Legacy oldText/newText.
    let has_legacy = matches!(args.get("oldText"), Some(serde_json::Value::String(_)))
        && matches!(args.get("newText"), Some(serde_json::Value::String(_)));
    if !has_legacy {
        return serde_json::Value::Object(args);
    }
    let old_text = args.get("oldText").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let new_text = args.get("newText").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut edits_arr = match args.get("edits").cloned() {
        Some(serde_json::Value::Array(a)) => a,
        _ => Vec::new(),
    };
    edits_arr.push(serde_json::json!({ "oldText": old_text, "newText": new_text }));
    args.remove("oldText");
    args.remove("newText");
    args.insert("edits".to_string(), serde_json::Value::Array(edits_arr));
    serde_json::Value::Object(args)
}

fn check_abort(cancel: &CancellationToken) -> Result<(), FileError> {
    if cancel.is_cancelled() {
        return Err(FileError::new(FileErrorCode::Aborted, "Operation aborted"));
    }
    Ok(())
}

fn file_err_to_agent(e: FileError) -> AgentError {
    AgentError::Tool(e.to_string())
}
