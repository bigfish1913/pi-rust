//! The write tool now returns a real diff in `details` (same shape as edit):
//! `diff` (display-oriented, line-number prefixed), `patch` (unified), and
//! `first_changed_line`. Hosts render collapsed write rows from it instead of
//! guessing `+N` from the content length.
//!
//! Also pins the bash/powershell `description` schema extension: hosts show it
//! in collapsed tool rows, so the property must stay in the tool schema.

use std::sync::Arc;

use rpi_agent::agent_tool::AgentTool;
use rpi_tools::{
    create_bash_tool, create_powershell_tool, create_write_tool, ExecutionEnv,
    ExecutionToolContext, FileSystem, InMemoryExecutionEnv,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn fresh_context() -> (Arc<InMemoryExecutionEnv>, ExecutionToolContext) {
    let env = Arc::new(InMemoryExecutionEnv::with_cwd("/tmp/work".into()));
    let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
    let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
    let ctx = ExecutionToolContext::new(env_dyn, Some(mut_env));
    (env, ctx)
}

async fn run_write(
    tool: Arc<dyn AgentTool>,
    path: &str,
    content: &str,
) -> rpi_agent::types::AgentToolResult {
    tool.execute(
        "write-x",
        json!({ "path": path, "content": content }),
        CancellationToken::new(),
        Arc::new(|_p| ()),
    )
    .await
    .expect("write succeeds")
}

#[tokio::test]
async fn create_diff_lists_every_line_as_added() {
    let (_env, ctx) = fresh_context();
    let tool = create_write_tool(&ctx);
    let result = run_write(tool, "notes.txt", "alpha\nbeta\n").await;

    let diff = result.details["diff"].as_str().expect("details.diff");
    let added = diff.lines().filter(|l| l.starts_with('+')).count();
    let removed = diff.lines().filter(|l| l.starts_with('-')).count();
    assert_eq!((added, removed), (2, 0), "diff was: {diff}");
    assert!(diff.contains("alpha"));
    assert!(diff.contains("beta"));

    let patch = result.details["patch"].as_str().expect("details.patch");
    assert!(patch.contains("--- notes.txt"), "patch was: {patch}");
    assert!(patch.contains("+++ notes.txt"));
    assert_eq!(result.details["diff_skipped"], serde_json::Value::Null);
}

#[tokio::test]
async fn overwrite_diff_reports_real_adds_and_removes() {
    let (env, ctx) = fresh_context();
    let abs = env.absolute_path("notes.txt", None).await.unwrap();
    env.seed_file(&abs.to_string_lossy(), b"alpha\nbeta\ngamma\n".to_vec())
        .await;

    let tool = create_write_tool(&ctx);
    let result = run_write(tool, "notes.txt", "alpha\nBETA\ngamma\ndelta\n").await;

    let diff = result.details["diff"].as_str().expect("details.diff");
    let added = diff.lines().filter(|l| l.starts_with('+')).count();
    let removed = diff.lines().filter(|l| l.starts_with('-')).count();
    // beta → BETA is a replace (1 removed + 1 added); delta is an insertion.
    assert_eq!((added, removed), (2, 1), "diff was: {diff}");
    assert_eq!(result.details["first_changed_line"], json!(2));
}

#[tokio::test]
async fn unchanged_overwrite_has_empty_diff() {
    let (env, ctx) = fresh_context();
    let abs = env.absolute_path("same.txt", None).await.unwrap();
    env.seed_file(&abs.to_string_lossy(), b"same\n".to_vec()).await;

    let tool = create_write_tool(&ctx);
    let result = run_write(tool, "same.txt", "same\n").await;

    assert_eq!(result.details["diff"], json!(""));
    assert_eq!(result.details["first_changed_line"], serde_json::Value::Null);
}

#[tokio::test]
async fn bash_and_powershell_schemas_expose_description() {
    let (_env, ctx) = fresh_context();

    for tool in [
        create_bash_tool(&ctx, None),
        create_powershell_tool(&ctx),
    ] {
        let schema = &tool.schema().parameters.0;
        let prop = &schema["properties"]["description"];
        assert!(
            prop.is_object(),
            "{} schema is missing the description property: {schema}",
            tool.schema().name
        );
        assert!(
            prop["description"]
                .as_str()
                .is_some_and(|d| !d.is_empty()),
            "{} description property needs a schema description",
            tool.schema().name
        );
    }
}
