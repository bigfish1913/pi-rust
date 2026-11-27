//! Run a one-shot Mini prompt through the local Host's Agent harness.

use std::path::Path;
use std::sync::Arc;

use rpi_ai::types::ImageContent;
use rpi_harness::agent_harness::HarnessRunOutcome;
use tokio_util::sync::CancellationToken;

use crate::app::{resolve_error_message, EXIT_RUNTIME, EXIT_USAGE};
use crate::args::Args;
use crate::config::AgentHome;
use crate::provider::ResolveError;

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub prompts: Vec<String>,
    pub images: Vec<ImageContent>,
}

pub(crate) fn print_text(text: String) {
    if text.ends_with('\n') {
        print!("{text}");
    } else {
        println!("{text}");
    }
}

pub(crate) async fn execute(
    home: &AgentHome,
    mut args: Args,
    cwd: &Path,
    plugin_images: Arc<rpi_extensions::SharedPluginImages>,
    input: Input,
    signal: CancellationToken,
    mut report: impl FnMut(String),
) -> Result<Option<String>, i32> {
    if input.prompts.is_empty() {
        return Ok(None);
    }
    let trusted = crate::session::resolve_project_trust_for(home, &args, cwd);
    let resolved = crate::provider::resolve_for_cwd(
        home,
        args.provider.as_deref(),
        args.model.as_deref(),
        args.thinking,
        args.api_key.as_deref(),
        args.base_url.as_deref(),
        cwd,
        trusted,
    )
    .map_err(|error| {
        report(resolve_error_message(&error));
        match error {
            ResolveError::NoApiKey { .. } | ResolveError::Config(_) => EXIT_USAGE,
            _ => EXIT_RUNTIME,
        }
    })?;
    for warning in &resolved.compat_warnings {
        report(format!("warning: models.json: {warning}"));
    }
    let debug_system_prompt = std::mem::take(&mut args.debug_system_prompt);
    let prepared =
        crate::session::prepare_host_extensions(home, &args, cwd, trusted, plugin_images).map_err(
            |error| {
                report(format!("error: {error}"));
                EXIT_RUNTIME
            },
        )?;
    let (harness, events, _context) =
        crate::session::build_prepared(home, &resolved, &args, cwd, trusted, prepared)
            .await
            .map_err(|error| {
                report(format!("error: {error}"));
                EXIT_RUNTIME
            })?;
    drop(events);
    drop(resolved);
    if debug_system_prompt {
        if let Ok(Some(prompt)) = harness.get_system_prompt().await {
            report(prompt);
        }
    }

    let lane = harness.lane("main");
    let mut images = input.images;
    let mut last_reply = None;
    for prompt in input.prompts {
        let run = lane.prompt_text(&prompt, std::mem::take(&mut images));
        tokio::pin!(run);
        let result = tokio::select! {
            biased;
            _ = signal.cancelled() => {
                // A registered run needs to settle before the Host returns its slot.
                if lane.abort().await.is_ok() {
                    let _ = run.await;
                }
                return Err(EXIT_RUNTIME);
            }
            result = &mut run => result,
        }
        .map_err(|error| {
            report(format!("prompt rejected: {error}"));
            EXIT_RUNTIME
        })?;
        match result.outcome {
            HarnessRunOutcome::Completed { final_message, .. } => {
                last_reply = Some(final_message);
            }
            HarnessRunOutcome::Aborted { final_message, .. } => {
                report(
                    final_message
                        .error_message
                        .unwrap_or_else(|| "aborted".into()),
                );
                return Err(EXIT_RUNTIME);
            }
            HarnessRunOutcome::Failed {
                error,
                final_message,
                ..
            } => {
                let detail = final_message.and_then(|message| message.error_message);
                report(detail.unwrap_or_else(|| format!("run failed: {error:?}")));
                return Err(EXIT_RUNTIME);
            }
            HarnessRunOutcome::Suspended { .. } => {
                report("deferred responses require a session".into());
                return Err(EXIT_RUNTIME);
            }
        }
    }
    Ok(last_reply.as_ref().map(crate::modes::assistant_text))
}
