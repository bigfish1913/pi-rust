//! Native-only production assembly, using real runtime Providers and builtin
//! tools. Inputs are supplied by a trusted Host owner, never model arguments.
//! No CLI Args, settings discovery, process env mutation or subprocess Agent
//! fallback occurs here. Only an explicit private nativeHooks resource binding
//! can load trusted code, through owned deferred assembly and strict custody.

use super::{
    binding_resources::NativeHookCallbacks,
    ordinary::{BackendError, BackendResult, OrdinaryBinding},
    AgentMode, AgentSpec, ExecutionMode,
};
use rpi_harness::types::{AgentHarnessOptions, AgentHarnessStreamOptions, HarnessTool, ToolReplay};
use rpi_runtime::{ModelRequest, ProviderRegistry, RuntimeConfig};
use rpi_tools::{tools::*, OsExecutionEnv};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};

/// A trusted host's explicit, noninteractive grant. Default is no tools. This
/// does not represent a user's interactive approval or a filesystem sandbox.
/// IPC/model-facing callers must never be able to mint this from their input.
#[derive(Default)]
pub enum NativeToolPolicy {
    #[default]
    DenyAll,
    Preapproved(BTreeSet<String>),
}
impl NativeToolPolicy {
    fn allows(&self, name: &str) -> bool {
        matches!(self, Self::Preapproved(names) if names.contains(name))
    }
}

/// Per-Agent private inputs. Deliberately not Debug/Serialize: runtime and
/// value maps can contain secrets. Provider values and tool environment are
/// separate; provider credentials are never copied into a shell baseline.
pub struct NativeBindingProfile {
    pub runtime: RuntimeConfig,
    /// Closed value source, including any proxy/header/key variables. Missing
    /// values and secret commands do not fall back to process env or shell.
    pub provider_values: BTreeMap<String, String>,
    /// Complete tool baseline, including an absolute HOME (USERPROFILE on
    /// Windows). The trusted host chooses what a granted local shell may see.
    pub tool_environment: BTreeMap<String, String>,
    pub shell_path: PathBuf,
    pub tools: NativeToolPolicy,
    pub thinking: rpi_ai::ThinkingLevel,
    pub stream_options: AgentHarnessStreamOptions,
    /// Closed frozen resources. The historical field name also covers explicitly
    /// approved native hook resources; it never enables ambient discovery.
    pub instruction_resources: Option<Arc<super::NativeBindingResources>>,
}
impl NativeBindingProfile {
    /// Prepare one fresh execution environment per Agent; never share process
    /// cleanup ownership across Agents, even if they use the same workspace.
    /// The immutable trusted inputs remain reusable. Resolve closed Provider
    /// inputs here, before callers may create a Session or register metadata;
    /// persisted resource authority is checked later against readonly facts.
    pub async fn bind(&self, expected: AgentSpec) -> BackendResult<OrdinaryBinding> {
        let (resolved, env) = self
            .resolve_binding(&expected, AgentMode::Persistent)
            .await?;
        let stream_options = self.stream_options.clone();
        let resources = self.instruction_resources.clone();
        let configured_resources = resources.clone();
        let binder: super::ordinary::OrdinaryBinder = Box::new(move |spec, env, facts| {
            if spec != &expected {
                return Err(BackendError::Binding(
                    "prepared Agent identity or launch changed",
                ));
            }
            // Do not silently restore an immutable exact resource declaration
            // using a native-only prompt/tool set. This also validates corrupt
            // persisted declarations before any repair or writer acquisition.
            if configured_resources.is_none() {
                crate::resource_selection::validate_session_binding(&facts.values, None).map_err(
                    |_| {
                        BackendError::Binding("saved resources require their exact resource binder")
                    },
                )?;
            }
            if facts.values.iter().any(|(key, _)| {
                key.starts_with("rpi.resource-inventory.")
                    || (key.starts_with("rpi.resource-selection.")
                        && key != "rpi.resource-selection.v1")
            }) {
                return Err(BackendError::Binding(
                    "saved resource inventory or version is unsupported by native binding",
                ));
            }
            let tools = native_tools(spec, &env)?;
            let granted: BTreeSet<_> = spec.launch.tools.iter().cloned().collect();
            let before_tool_call: rpi_agent::BeforeToolCall = Arc::new(move |context, signal| {
                let allowed = granted.contains(&context.tool_call.name) && !signal.is_cancelled();
                Box::pin(async move {
                    (!allowed).then(|| rpi_agent::BeforeToolCallResult {
                        block: true,
                        reason: Some("tool not granted by Host profile".into()),
                        ..Default::default()
                    })
                })
            });
            let mut options = AgentHarnessOptions {
                system_prompt: Some(crate::session::default_system_prompt_with_tools(
                    &spec.launch.cwd.to_string_lossy(),
                    rpi_ai::model::prior_reasoning_is_replayed(&resolved.model),
                    &spec.launch.tools,
                )),
                model: resolved.model,
                thinking_level: resolved.thinking_level,
                models: vec![resolved.provider],
                tools,
                active_tool_names: spec.launch.tools.clone(),
                stream_options,
                before_tool_call: Some(before_tool_call),
                ..Default::default()
            };
            if let Some(resources) = &configured_resources {
                resources.configure_parts(&mut options.system_prompt, &mut options.resources);
            }
            Ok(options)
        });
        let mut binding =
            if let Some(resources) = resources.as_ref().filter(|resources| resources.has_hooks()) {
                let frozen = resources.clone();
                let owner = super::NativeExtensionCustody::new();
                let assembly_owner = owner.clone();
                OrdinaryBinding::new_deferred(
                    env,
                    Box::new(move |spec, env, facts| {
                        let options = binder(spec, env, facts)?;
                        // Pure saved-fact validation must precede staging/registration.
                        frozen.validate(&facts.values, &options.tools)?;
                        let spec = spec.clone();
                        Ok(Box::pin(async move {
                            frozen.assemble(&spec, assembly_owner, options).await
                        }))
                    }),
                )?
                .with_extensions(owner)
            } else {
                OrdinaryBinding::new(env, binder)?
            };
        binding.instruction_resources = resources;
        Ok(binding)
    }

    /// Build the existing raw Agent loop with no Harness/Session construction.
    /// Caller must already retain `custody` in the owning Host entry before
    /// awaiting this method; dropping the request waiter is not cleanup.
    pub async fn bind_mini_agent(
        &self,
        expected: AgentSpec,
        catalog: super::HostCatalog,
        custody: Arc<super::NativeExtensionCustody>,
    ) -> BackendResult<NativeMiniAgent> {
        let (resolved, env) = self.resolve_binding(&expected, AgentMode::Mini).await?;
        let tools = native_tools(&expected, &env)?;
        let granted: BTreeSet<_> = expected.launch.tools.iter().cloned().collect();
        let before_tool_call: rpi_agent::BeforeToolCall = Arc::new(move |context, signal| {
            let allowed = granted.contains(&context.tool_call.name) && !signal.is_cancelled();
            Box::pin(async move {
                (!allowed).then(|| rpi_agent::BeforeToolCallResult {
                    block: true,
                    reason: Some("tool not granted by Host profile".into()),
                    ..Default::default()
                })
            })
        });
        let mut prompt = Some(crate::session::default_system_prompt_with_tools(
            &expected.launch.cwd.to_string_lossy(),
            rpi_ai::model::prior_reasoning_is_replayed(&resolved.model),
            &expected.launch.tools,
        ));
        let mut provenance = rpi_harness::types::AgentHarnessResources::empty();
        if let Some(resources) = &self.instruction_resources {
            resources.configure_parts(&mut prompt, &mut provenance);
        }
        let callbacks = match self.instruction_resources.as_ref() {
            Some(resources) if resources.has_hooks() => {
                custody.claim(catalog)?;
                resources
                    .assemble_callbacks(&expected, custody.clone())
                    .await?
            }
            _ => NativeHookCallbacks::default(),
        };
        let provider = resolved.provider;
        let provider_hooks = callbacks.provider_hooks.clone();
        let stream_options = self.stream_options.clone();
        let stream = rpi_agent::async_stream_fn(move |model, context, options| {
            let provider = provider.clone();
            let hooks = provider_hooks.clone();
            let pinned = stream_options.clone();
            Box::pin(async move {
                let mut context = std::borrow::Cow::Borrowed(context);
                let mut options = pinned.merge_into(options.clone());
                if let Some(hooks) = &hooks {
                    if let Some(patch) = hooks.transform_context(model, &context) {
                        if patch != rpi_ai::ContextPatch::default() {
                            patch.apply(context.to_mut());
                        }
                    }
                    if let Some(patch) = hooks.before_request_async(model, &context, &options).await
                    {
                        patch.apply(&mut options);
                    }
                }
                provider.stream_simple(model, &context, &options).await
            })
        });
        let mut builder = rpi_agent::AgentBuilder::new()
            .model(resolved.model)
            .thinking_level(resolved.thinking_level)
            .system_prompt(prompt.unwrap_or_default())
            .tools(tools.into_iter().map(|tool| tool.tool).collect())
            .async_stream_fn(stream)
            .before_tool_call(before_tool_call);
        if let Some(after_tool_call) = callbacks.after_tool_call {
            builder = builder.after_tool_call(after_tool_call);
        }
        if let Some(provider_hooks) = callbacks.provider_hooks {
            builder = builder.provider_hooks(provider_hooks);
        }
        let agent = builder
            .build()
            .map_err(|_| BackendError::Binding("native mini Agent construction failed"))?;
        Ok(NativeMiniAgent {
            agent,
            custody: self
                .instruction_resources
                .as_ref()
                .is_some_and(|resources| resources.has_hooks())
                .then_some(custody),
        })
    }

    async fn resolve_binding(
        &self,
        expected: &AgentSpec,
        mode: AgentMode,
    ) -> BackendResult<(rpi_runtime::ResolvedModel, Arc<OsExecutionEnv>)> {
        expected
            .validate()
            .map_err(|_| BackendError::Binding("invalid Agent metadata"))?;
        if expected.mode != mode || expected.execution != ExecutionMode::InProcess {
            return Err(BackendError::Binding("native binding mode is unsupported"));
        }
        if expected.launch.resource_profile.as_deref()
            != self
                .instruction_resources
                .as_ref()
                .map(|resources| resources.identity())
        {
            return Err(BackendError::Binding(
                "resource profile requires its exact frozen instruction binding",
            ));
        }
        if let Some(resources) = &self.instruction_resources {
            resources.validate_tools(&expected.launch.tools)?;
        }
        if expected
            .launch
            .tools
            .iter()
            .any(|name| !self.tools.allows(name))
        {
            return Err(BackendError::Binding(
                "requested tool is not preapproved by Host policy",
            ));
        }
        if expected
            .launch
            .tools
            .iter()
            .any(|name| !crate::session::BUILTIN_TOOL_NAMES.contains(&name.as_str()))
        {
            return Err(BackendError::Binding(
                "native binding does not implement requested tool",
            ));
        }
        let home_key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        if !self.shell_path.is_absolute()
            || self
                .tool_environment
                .get(home_key)
                .is_none_or(|home| !std::path::Path::new(home).is_absolute())
            || self.tool_environment.iter().any(|(name, value)| {
                name.is_empty()
                    || name.contains(['=', '\0'])
                    || value.contains('\0')
                    || rpi_tools::is_host_private_env(name)
            })
        {
            return Err(BackendError::Binding(
                "invalid explicit tool environment or shell path",
            ));
        }
        let unresolved_header = self.runtime.models.providers.values().any(|provider| {
            provider
                .headers
                .iter()
                .chain(
                    provider
                        .models
                        .iter()
                        .filter_map(|model| model.headers.as_ref()),
                )
                .flat_map(|headers| headers.values())
                .any(|raw| {
                    rpi_runtime::values::resolve_config_value(
                        raw,
                        None,
                        &self.provider_values,
                        false,
                    )
                    .is_none()
                })
        });
        let command_key = self
            .runtime
            .models
            .providers
            .values()
            .filter_map(|provider| provider.api_key.as_deref())
            .any(|key| key.starts_with('!'))
            || self.runtime.credentials.values().any(|credential| {
                matches!(credential,
                    rpi_runtime::config::Credential::ApiKey { key: Some(key), .. } if key.starts_with('!'))
            });
        if unresolved_header || command_key {
            return Err(BackendError::Binding(
                "native binding requires fully resolved headers and non-command credentials",
            ));
        }
        let model = expected
            .launch
            .model
            .as_ref()
            .ok_or(BackendError::Binding("model reference required"))?;
        let resolved = self
            .runtime
            .resolve(
                &ModelRequest {
                    provider: Some(&model.provider),
                    model: Some(&model.model),
                    thinking: Some(self.thinking),
                    ..Default::default()
                },
                &ProviderRegistry::default(),
                &self.provider_values,
            )
            .map_err(|_| {
                BackendError::Binding("explicit Provider configuration could not be resolved")
            })?;
        if resolved.model.provider != model.provider || resolved.model.id != model.model {
            return Err(BackendError::Binding(
                "Provider resolution must match the exact model reference",
            ));
        }
        let cwd = tokio::fs::canonicalize(&expected.launch.cwd).await?;
        let shell = tokio::fs::canonicalize(&self.shell_path).await?;
        if !tokio::fs::metadata(&shell).await?.is_file() {
            return Err(BackendError::Binding("explicit shell must be a file"));
        }
        let env = Arc::new(
            OsExecutionEnv::with_cwd(cwd)
                .with_environment(self.tool_environment.clone().into_iter().collect())
                .with_shell_path(shell),
        );
        Ok((resolved, env))
    }
}

/// Raw, non-persistent Agent and its optional native DLL settlement owner.
/// It contains no Harness, AgentLane, Session, journal or writer.
pub struct NativeMiniAgent {
    agent: rpi_agent::Agent,
    custody: Option<Arc<super::NativeExtensionCustody>>,
}
impl NativeMiniAgent {
    pub fn agent(&self) -> &rpi_agent::Agent {
        &self.agent
    }
    pub fn cancel(&self) {
        self.agent.abort();
    }
    /// Close through the existing Agent and native DLL settlement barriers.
    /// On failure this value and its custody must remain owned for retry.
    pub async fn close(&self) -> BackendResult<()> {
        self.agent.abort();
        self.agent.wait_for_idle().await;
        if let Some(custody) = &self.custody {
            custody.close().await?;
        }
        Ok(())
    }
}

fn native_tools(spec: &AgentSpec, env: &Arc<OsExecutionEnv>) -> BackendResult<Vec<HarnessTool>> {
    let ctx = ExecutionToolContext::new(env.clone(), Some(env.clone()));
    spec.launch
        .tools
        .iter()
        .map(|name| {
            Ok(match name.as_str() {
                "read" => HarnessTool::new(create_read_tool(
                    &ctx,
                    Some(ReadToolOptions {
                        image_processor: Some(Arc::new(
                            rpi_tools::image_processing::InlineImageProcessor,
                        )),
                        ..Default::default()
                    }),
                ))
                .with_replay(ToolReplay::Safe),
                "bash" => HarnessTool::new(create_bash_tool(
                    &ctx,
                    Some(BashToolOptions {
                        default_timeout: Some(rpi_tools::tools::bash::DEFAULT_BASH_TIMEOUT_SECONDS),
                        ..Default::default()
                    }),
                )),
                "edit" => HarnessTool::new(create_edit_tool(&ctx)),
                "write" => HarnessTool::new(create_write_tool(&ctx)),
                "docs" => HarnessTool::new(crate::docs_tool::create_docs_tool())
                    .with_replay(ToolReplay::Safe),
                _ => return Err(BackendError::Binding("unsupported native tool")),
            })
        })
        .collect()
}

#[cfg(all(test, unix))]
#[path = "bindings_tests.rs"]
mod tests;
