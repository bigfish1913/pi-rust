//! Closed, frozen Host resource bindings. No ambient discovery.
use super::{
    ordinary::{BackendError, BackendResult},
    AgentSpec, NativeExtensionCustody, NativeInstructionResources,
};
use crate::resource_selection::ResourceSelection;
use rpi_harness::{
    session::{Session, SessionValues},
    types::{AgentHarnessOptions, AgentHarnessResources, HarnessTool},
};
use serde::Deserialize;
use std::{path::PathBuf, sync::Arc};

#[cfg(unix)]
#[path = "native_hooks.rs"]
mod native_hooks;

/// Session-independent callbacks assembled by the same explicitly owned DLL
/// loader. The caller retains NativeExtensionCustody through settlement.
#[derive(Default)]
pub(super) struct NativeHookCallbacks {
    pub after_tool_call: Option<rpi_agent::AfterToolCall>,
    pub provider_hooks: Option<Arc<dyn rpi_ai::ProviderHooks>>,
}

/// Private profile approval for an ordered, explicit DLL batch. It grants only
/// the implemented hook bridge, not a sandbox for arbitrary native code.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NativeHookConfig {
    staging_root: PathBuf,
    registrations: Vec<HookRegistration>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct HookRegistration {
    #[serde(default)]
    tool_result: usize,
    #[serde(default)]
    provider_events: Vec<ProviderEventApproval>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProviderEventApproval {
    event: ProviderEvent,
    count: usize,
}

/// Closed observer-only surface; no raw event numbers or ambient subscription.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum ProviderEvent {
    BeforeRequest,
    BeforeHeaders,
    AfterResponse,
}
impl ProviderEvent {
    #[cfg(unix)]
    fn tag(&self) -> rpi_plugin_sdk::EventTag {
        use rpi_plugin_sdk::EventTag;
        match self {
            Self::BeforeRequest => EventTag::BeforeProviderRequest,
            Self::BeforeHeaders => EventTag::BeforeProviderHeaders,
            Self::AfterResponse => EventTag::AfterProviderResponse,
        }
    }
}

/// Kept distinct from the instruction-only type: only the Hooks variant can
/// stage/load code, and that requires an independently approved private profile.
pub enum NativeBindingResources {
    Instructions(Arc<NativeInstructionResources>),
    #[cfg(unix)]
    Hooks(Box<native_hooks::NativeHookResources>),
}
impl NativeBindingResources {
    pub fn freeze(
        selection: ResourceSelection,
        hooks: Option<NativeHookConfig>,
    ) -> BackendResult<Self> {
        match hooks {
            None => Ok(Self::Instructions(Arc::new(
                NativeInstructionResources::freeze(selection)?,
            ))),
            Some(config) => {
                #[cfg(unix)]
                {
                    Ok(Self::Hooks(Box::new(
                        native_hooks::NativeHookResources::freeze(selection, config)?,
                    )))
                }
                #[cfg(not(unix))]
                {
                    let _ = (selection, config);
                    Err(BackendError::Binding("native Host hooks require Unix"))
                }
            }
        }
    }
    pub fn identity(&self) -> &str {
        match self {
            Self::Instructions(resources) => resources.identity(),
            #[cfg(unix)]
            Self::Hooks(resources) => resources.identity(),
        }
    }
    pub(super) fn has_hooks(&self) -> bool {
        match self {
            Self::Instructions(_) => false,
            #[cfg(unix)]
            Self::Hooks(_) => true,
        }
    }
    pub(super) fn validate_tools(&self, tools: &[String]) -> BackendResult<()> {
        match self {
            Self::Instructions(resources) => resources.validate_tools(tools),
            #[cfg(unix)]
            Self::Hooks(resources) => resources.validate_tools(tools),
        }
    }
    /// Frozen prompt/provenance data only: never construct default Harness
    /// options (and their implicit memory Session) to configure a raw Agent.
    pub(super) fn configure_parts(
        &self,
        prompt: &mut Option<String>,
        provenance: &mut AgentHarnessResources,
    ) {
        match self {
            Self::Instructions(resources) => resources.configure_parts(prompt, provenance),
            #[cfg(unix)]
            Self::Hooks(resources) => resources.configure_parts(prompt, provenance),
        }
    }
    pub(super) fn validate(
        &self,
        values: &SessionValues,
        tools: &[HarnessTool],
    ) -> BackendResult<()> {
        match self {
            Self::Instructions(resources) => resources.validate(values, tools),
            #[cfg(unix)]
            Self::Hooks(resources) => resources.validate(values, tools),
        }
    }
    pub(super) async fn persist(
        &self,
        session: &Session,
        tools: &[HarnessTool],
    ) -> BackendResult<()> {
        match self {
            Self::Instructions(resources) => resources.persist(session, tools).await,
            #[cfg(unix)]
            Self::Hooks(resources) => resources.persist(session, tools).await,
        }
    }
    pub(super) async fn assemble(
        &self,
        spec: &AgentSpec,
        owner: Arc<NativeExtensionCustody>,
        mut options: AgentHarnessOptions,
    ) -> BackendResult<AgentHarnessOptions> {
        let callbacks = self.assemble_callbacks(spec, owner).await?;
        if let Some(after_tool_call) = callbacks.after_tool_call {
            options.after_tool_call = Some(after_tool_call);
        }
        if let Some(provider_hooks) = callbacks.provider_hooks {
            options.provider_hooks = Some(provider_hooks);
        }
        Ok(options)
    }
    /// Only poll within owned assembly. The caller must keep the same custody
    /// throughout startup/late callbacks/close, including uncertain failure.
    pub(super) async fn assemble_callbacks(
        &self,
        spec: &AgentSpec,
        owner: Arc<NativeExtensionCustody>,
    ) -> BackendResult<NativeHookCallbacks> {
        match self {
            Self::Instructions(_) => {
                let _ = (spec, owner);
                Ok(NativeHookCallbacks::default())
            }
            #[cfg(unix)]
            Self::Hooks(resources) => resources.assemble_callbacks(spec, owner).await,
        }
    }
}
