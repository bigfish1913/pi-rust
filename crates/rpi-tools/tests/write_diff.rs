//! The write tool now returns a real diff in `details` (same shape as edit):
//! `diff` (display-oriented, line-number prefixed), `patch` (unified), and
//! `first_changed_line`. Hosts render collapsed write rows from it instead of
//! guessing `+N` from the content length.
//!
//! Also pins the bash/powershell `description` schema extension: hosts show it
//! in collapsed tool rows, so the property must stay in the tool schema.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rpi_agent::agent_tool::AgentTool;
use rpi_tools::tools::edit_diff::{generate_diff_string_budgeted, generate_unified_patch_budgeted};
use rpi_tools::{
    create_bash_tool, create_powershell_tool, create_write_tool, ExecutionToolContext, FileSystem,
    InMemoryExecutionEnv,
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
    env.seed_file(&abs.to_string_lossy(), b"same\n".to_vec())
        .await;

    let tool = create_write_tool(&ctx);
    let result = run_write(tool, "same.txt", "same\n").await;

    assert_eq!(result.details["diff"], json!(""));
    assert_eq!(
        result.details["first_changed_line"],
        serde_json::Value::Null
    );
}

/// 20k lines of completely different content per side (~1 MB total, well
/// under the 2 MiB cap). This is the size class the PR review measured at
/// multiple seconds for two Myers passes.
fn wide_replacement(lines: usize) -> (String, String) {
    let old = (0..lines)
        .map(|i| format!("old line {i:05} lorem ipsum dolor sit amet"))
        .collect::<Vec<_>>()
        .join("\n");
    let new = (0..lines)
        .map(|i| format!("new line {i:05} consectetur adipiscing elit"))
        .collect::<Vec<_>>()
        .join("\n");
    (old, new)
}

#[test]
fn budgeted_diffs_bail_out_on_exhausted_budget() {
    let (old, new) = wide_replacement(20_000);
    assert!(
        old.len() + new.len() < 2 * 1024 * 1024,
        "fixture must stay under MAX_DIFF_INPUT_BYTES"
    );

    let started = Instant::now();
    assert!(
        generate_diff_string_budgeted(&old, &new, 4, Duration::from_millis(1)).is_none(),
        "display diff must be skipped when the budget is exhausted"
    );
    assert!(
        generate_unified_patch_budgeted("big.txt", &old, &new, Duration::from_millis(1)).is_none(),
        "unified patch must be skipped when the budget is exhausted"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "budgeted diffs must return promptly, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn write_large_full_replacement_skips_diff() {
    let (env, ctx) = fresh_context();
    let abs = env.absolute_path("big.txt", None).await.unwrap();
    let (old, new) = wide_replacement(20_000);
    env.seed_file(&abs.to_string_lossy(), old.into_bytes())
        .await;

    let tool = create_write_tool(&ctx);
    let started = Instant::now();
    let result = run_write(tool, "big.txt", &new).await;

    assert_eq!(
        result.details["diff_skipped"],
        json!("timeout"),
        "large replacement must report diff_skipped=timeout; details: {}",
        result.details
    );
    assert!(result.details["diff"].is_null());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "write must not spend unbounded time on the diff, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn bash_and_powershell_schemas_expose_description() {
    let (_env, ctx) = fresh_context();

    for tool in [create_bash_tool(&ctx, None), create_powershell_tool(&ctx)] {
        let schema = &tool.schema().parameters.0;
        let prop = &schema["properties"]["description"];
        assert!(
            prop.is_object(),
            "{} schema is missing the description property: {schema}",
            tool.schema().name
        );
        assert!(
            prop["description"].as_str().is_some_and(|d| !d.is_empty()),
            "{} description property needs a schema description",
            tool.schema().name
        );
    }
}
