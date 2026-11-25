//! One-shot text execution using the agent loop directly.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rpi_agent::agent_loop::run_agent_loop;
use rpi_agent::hooks::{
    default_convert_to_llm_fn, default_convert_to_llm_incremental_fn, AgentLoopConfig,
};
use rpi_agent::types::AgentContext;
use rpi_agent::{AgentEmitter, AgentEvent, AgentMessage};
use rpi_ai::types::{Content, ImageContent, StopReason, UserContent, UserMessage};
use rpi_tools::{ExecutionToolContext, OsExecutionEnv};
use tokio_util::sync::CancellationToken;

use crate::app::{resolve_error_message, EXIT_RUNTIME, EXIT_USAGE};
use crate::args::Args;
use crate::config::AgentHome;
use crate::provider::ResolveError;

struct DiscardEvents;

impl AgentEmitter for DiscardEvents {
    fn emit(&self, _event: AgentEvent) -> futures::future::BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

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

pub(crate) async fn run(args: &Args, cwd: &Path, input: Input) -> i32 {
    match execute(
        &AgentHome::process(),
        args,
        cwd,
        input,
        CancellationToken::new(),
        |message| eprintln!("{message}"),
    )
    .await
    {
        Ok(Some(text)) => {
            print_text(text);
            0
        }
        Ok(None) => 0,
        Err(code) => code,
    }
}

pub(crate) async fn execute(
    home: &AgentHome,
    args: &Args,
    cwd: &Path,
    input: Input,
    signal: CancellationToken,
    mut report: impl FnMut(String),
) -> Result<Option<String>, i32> {
    let mut prompts = input.prompts.into_iter().peekable();
    if prompts.peek().is_none() {
        return Ok(None);
    }

    let (provider, model, thinking_level) = {
        let resolved = match crate::provider::resolve_for_cwd(
            home,
            args.provider.as_deref(),
            args.model.as_deref(),
            args.thinking,
            args.api_key.as_deref(),
            args.base_url.as_deref(),
            cwd,
            crate::session::resolve_project_trust_for(home, args, cwd),
        ) {
            Ok(resolved) => resolved,
            Err(error) => {
                report(resolve_error_message(&error));
                return Err(match error {
                    ResolveError::NoApiKey { .. } | ResolveError::Config(_) => EXIT_USAGE,
                    _ => EXIT_RUNTIME,
                });
            }
        };
        for warning in &resolved.compat_warnings {
            report(format!("warning: models.json: {warning}"));
        }
        (resolved.provider, resolved.model, resolved.thinking_level)
    };

    let tools = if args.tools.as_ref().is_some_and(|names| !names.is_empty())
        && !args.no_tools
        && !args.no_builtin_tools
    {
        let env = Arc::new(OsExecutionEnv::with_cwd(cwd.to_path_buf()));
        let context = ExecutionToolContext::new(env.clone(), Some(env));
        crate::session::build_tools_bound(home, &context, args, None)
            .into_iter()
            .map(|tool| tool.tool)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mut system_prompt = args.system_prompt.clone().unwrap_or_else(|| {
        let names = tools
            .iter()
            .map(|tool| tool.schema().name.clone())
            .collect::<Vec<_>>();
        crate::session::default_system_prompt_with_tools(
            &cwd.to_string_lossy(),
            rpi_ai::model::prior_reasoning_is_replayed(&model),
            &names,
        )
    });
    for extra in &args.append_system_prompt {
        system_prompt.push_str("\n\n");
        let path = cwd.join(extra);
        let file_text = path
            .is_file()
            .then(|| std::fs::read_to_string(path).ok())
            .flatten();
        system_prompt.push_str(file_text.as_deref().unwrap_or(extra));
    }
    if args.debug_system_prompt {
        report(system_prompt.clone());
    }

    let settings = home.settings().unwrap_or_default();
    let thinking_budgets = settings.thinking_budgets;
    let config = AgentLoopConfig {
        model,
        convert_to_llm: default_convert_to_llm_fn(),
        convert_to_llm_incremental: Some(default_convert_to_llm_incremental_fn()),
        transform_context: None,
        get_api_key: None,
        should_stop_after_turn: None,
        prepare_next_turn: None,
        after_tool_results: None,
        get_steering_messages: None,
        get_follow_up_messages: None,
        pending_message_count: None,
        before_tool_call: crate::approval_gate::handler_installed()
            .then(|| crate::approval_gate::hook(Arc::default())),
        after_tool_call: None,
        provider_hooks: None,
        tool_execution: Default::default(),
        thinking_level,
        api_key: None,
        timeout: args.timeout,
        stream_idle_timeout: args
            .stream_idle_timeout
            .or_else(|| settings.stream_idle_timeout.map(Duration::from_secs)),
        max_retries: None,
        max_retry_delay: None,
        cache_retention: Default::default(),
        session_id: None,
        signal,
    };
    let stream = rpi_agent::stream_fn(move |model, context, options| {
        let mut options = options.clone();
        options.thinking_budgets = thinking_budgets;
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(provider.stream_simple(model, context, &options))
        })
    });
    let emit: Arc<dyn AgentEmitter> = Arc::new(DiscardEvents);
    let mut context = AgentContext {
        system_prompt,
        tools,
        messages: Vec::new(),
    };
    let mut images = input.images;
    while let Some(prompt) = prompts.next() {
        let content = if images.is_empty() {
            UserContent::Text(prompt)
        } else {
            let blocks = (!prompt.is_empty())
                .then(|| Content::text(prompt))
                .into_iter()
                .chain(std::mem::take(&mut images).into_iter().map(Content::Image))
                .collect();
            UserContent::Blocks(blocks)
        };
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as i64);
        let message = AgentMessage::User(UserMessage::new(content, timestamp));
        let is_last = prompts.peek().is_none();
        let turn = if is_last {
            std::mem::take(&mut context)
        } else {
            context.clone()
        };
        let new_messages = match run_agent_loop(
            vec![message],
            turn,
            config.clone(),
            emit.clone(),
            stream.clone(),
        )
        .await
        {
            Ok(messages) => messages,
            Err(error) => {
                report(format!("run failed: {error}"));
                return Err(EXIT_RUNTIME);
            }
        };
        let Some(reply) = new_messages
            .iter()
            .rev()
            .find_map(AgentMessage::as_assistant)
        else {
            report("run finished without an assistant message".into());
            return Err(EXIT_RUNTIME);
        };
        match reply.stop_reason {
            StopReason::Error | StopReason::Aborted => {
                report(
                    reply
                        .error_message
                        .clone()
                        .unwrap_or_else(|| reply.stop_reason.as_str().into()),
                );
                return Err(EXIT_RUNTIME);
            }
            StopReason::Deferred => {
                report("deferred responses require a session".into());
                return Err(EXIT_RUNTIME);
            }
            _ => {}
        }
        if is_last {
            return Ok(Some(crate::modes::assistant_text(reply)));
        }
        context.messages.extend(new_messages);
    }
    Ok(None)
}
