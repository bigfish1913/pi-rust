//! Approved Host hook bridge over the existing owned SDK loader and custody.
use super::{BackendError, BackendResult, NativeHookCallbacks, NativeHookConfig};
use crate::{
    host::{AgentSpec, NativeExtensionArtifacts, NativeExtensionCustody},
    resource_selection::{self, ResolvedSelection, ResourceSelection, ToolSelection},
};
use rpi_extensions::RegistrationInventory;
use rpi_harness::{
    session::{Session, SessionValues},
    types::{AgentHarnessResources, HarnessTool},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fs, os::unix::fs::MetadataExt, path::PathBuf, sync::Arc};

#[path = "native_provider_hooks.rs"]
mod provider_hooks;

pub struct NativeHookResources {
    selected: ResolvedSelection,
    artifacts: Arc<NativeExtensionArtifacts>,
    expected: Vec<RegistrationInventory>,
    staging_root: PathBuf,
    identity: String,
}
impl NativeHookResources {
    pub(super) fn freeze(
        selection: ResourceSelection,
        config: NativeHookConfig,
    ) -> BackendResult<Self> {
        if selection.extensions.is_empty()
            || selection.extensions.len() != config.registrations.len()
            || selection.extensions.len() > crate::host::MAX_NATIVE_EXTENSION_ARTIFACTS
        {
            return Err(BackendError::Binding(
                "DLL selection and exact hook approvals must match",
            ));
        }
        let mut instructions = selection.clone();
        instructions.extensions.clear();
        let mut selected = ResolvedSelection::host_instructions(instructions)
            .map_err(|_| BackendError::Binding("unsupported Host hook resources"))?;
        if let ToolSelection::Only { names } = &selection.tools {
            if names.iter().any(|name| name.is_empty())
                || names.iter().collect::<BTreeSet<_>>().len() != names.len()
            {
                return Err(BackendError::Binding("invalid exact resource tools"));
            }
        }
        let mut expected = Vec::with_capacity(config.registrations.len());
        for registration in config.registrations {
            if registration.tool_result > 16 {
                return Err(BackendError::Binding("hook registration budget exceeded"));
            }
            let mut event_handlers = Vec::new();
            let mut tags = BTreeSet::new();
            for approval in registration.provider_events {
                let tag = approval.event.tag() as u32;
                if approval.count == 0 || approval.count > 16 || !tags.insert(tag) {
                    return Err(BackendError::Binding(
                        "invalid exact Provider event approval",
                    ));
                }
                event_handlers.push(rpi_extensions::EventRegistration {
                    tag,
                    count: approval.count,
                });
            }
            // SDK inventories enumerate tags numerically, not in profile order.
            event_handlers.sort_unstable_by_key(|event| event.tag);
            // Only exact tool-result and the three implemented Provider observer
            // events are supported. All other registrations/metadata still fail
            // equality before any registry merge can publish them.
            expected.push(RegistrationInventory {
                tool_result: registration.tool_result,
                event_handlers,
                ..Default::default()
            });
        }
        // Pure validation only: no directory creation, dlopen or worker here.
        if !config.staging_root.is_absolute() {
            return Err(BackendError::Binding("DLL staging root must be absolute"));
        }
        let root: PathBuf = config.staging_root.components().collect();
        let metadata = fs::symlink_metadata(&root)?;
        if !metadata.is_dir()
            || metadata.uid() != nix::unistd::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
        {
            return Err(BackendError::Binding(
                "DLL staging root must be a private owned directory",
            ));
        }
        let staging_root = fs::canonicalize(root)?;
        let artifacts = Arc::new(NativeExtensionArtifacts::freeze(&selection.extensions)?);
        selected.extensions = artifacts
            .provenance()
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        selected.selection = selection;
        let identity = format!("native-hooks-v1:{:x}", Sha256::digest(serde_json::to_vec(&json!({
            "selection":selected.selection,"instructions":selected.provenance(),
            "artifacts":artifacts.identity(),"registrations":expected,"stagingRoot":staging_root
        })).map_err(|_| BackendError::Binding("invalid hook resource identity"))?));
        Ok(Self {
            selected,
            artifacts,
            expected,
            staging_root,
            identity,
        })
    }
    pub(super) fn identity(&self) -> &str {
        &self.identity
    }
    pub(super) fn validate_tools(&self, tools: &[String]) -> BackendResult<()> {
        let exact = match &self.selected.selection.tools {
            ToolSelection::Only { names } => names == tools,
            ToolSelection::None {} => tools.is_empty(),
            ToolSelection::Default {} => false,
        };
        if exact {
            Ok(())
        } else {
            Err(BackendError::Binding(
                "resource tools differ from exact Agent tools",
            ))
        }
    }
    pub(super) fn configure_parts(
        &self,
        prompt: &mut Option<String>,
        provenance: &mut AgentHarnessResources,
    ) {
        let prompt = prompt.get_or_insert_with(String::new);
        for (path, _, text) in &self.selected.instructions {
            prompt.push_str("\n\n");
            prompt.push_str(text);
            provenance
                .context_files
                .push(path.to_string_lossy().into_owned());
        }
    }
    fn inventory(&self, tools: &[HarnessTool]) -> Value {
        let extensions: Vec<_> = self
            .artifacts
            .provenance()
            .into_iter()
            .map(|(path, sha256)| json!({"path":path,"sha256":sha256}))
            .collect();
        let schemas: Vec<_> = tools.iter().map(|tool| json!(tool.tool.schema())).collect();
        json!({"skills":[],"extensions":extensions,"hookRegistrations":self.expected,
            "toolSchemasSha256":format!("{:x}", Sha256::digest(serde_json::to_vec(&schemas).expect("tool schemas serialize")))})
    }
    pub(super) fn validate(
        &self,
        values: &SessionValues,
        tools: &[HarnessTool],
    ) -> BackendResult<()> {
        resource_selection::validate_exact_inventory(values, &self.selected, &self.inventory(tools))
            .map_err(|_| BackendError::Binding("saved hook resources or inventory changed"))
    }
    pub(super) async fn persist(
        &self,
        session: &Session,
        tools: &[HarnessTool],
    ) -> BackendResult<()> {
        resource_selection::bind_inventory_to_session(
            session,
            &self.selected,
            self.inventory(tools),
        )
        .await
        .map_err(|_| BackendError::ResourcePersistence)?;
        session.sync_history().await?;
        Ok(())
    }
    pub(super) async fn assemble_callbacks(
        &self,
        spec: &AgentSpec,
        owner: Arc<NativeExtensionCustody>,
    ) -> BackendResult<NativeHookCallbacks> {
        let artifacts = self.artifacts.clone();
        let root = self.staging_root.clone();
        let agent = spec.id;
        let staged = tokio::task::spawn_blocking(move || artifacts.stage(&root, agent))
            .await
            .map_err(|_| BackendError::TaskStopped)??;
        let expected = self.expected.clone();
        owner
            .load_owned_checked(
                staged.paths().to_vec(),
                Arc::new(rpi_extensions::NullDiagnostics),
                Some(staged),
                move |index, actual| {
                    expected
                        .get(index)
                        .is_some_and(|expected| actual == expected)
                },
            )
            .await?;
        let mut callbacks = NativeHookCallbacks::default();
        if self
            .expected
            .iter()
            .any(|inventory| inventory.tool_result != 0)
        {
            callbacks.after_tool_call = Some(after_tool_call(&owner, spec));
        }
        if self
            .expected
            .iter()
            .any(|inventory| !inventory.event_handlers.is_empty())
        {
            callbacks.provider_hooks = Some(Arc::new(provider_hooks::NativeProviderHooks::new(
                &owner, spec,
            )));
        }
        Ok(callbacks)
    }
}

// Only weak owner refs are installed into Harness options: no owner/Harness/DLL
// cycle. Each actual callback retains its own session and physical call scope.
fn after_tool_call(
    owner: &Arc<NativeExtensionCustody>,
    spec: &AgentSpec,
) -> rpi_agent::AfterToolCall {
    let weak = Arc::downgrade(owner);
    let context = rpi_extensions::ToolCallContext::for_agent(
        spec.launch.cwd.to_string_lossy(),
        spec.id.to_string(),
        spec.session.as_ref().map(|session| session.id.clone()),
    );
    Arc::new(move |event, _signal| {
        let owner = weak.upgrade().expect("Host tool-result hook owner lost");
        let context = context.clone();
        let (id, name, input, result, is_error) = (
            event.tool_call.id.clone(),
            event.tool_call.name.clone(),
            event.args.clone(),
            event.result.clone(),
            event.is_error,
        );
        Box::pin(async move {
            // Acquire the call scope IN the actual worker, never just its waiter.
            // Abort/cancel cannot unload code while the callback/free is running.
            tokio::task::spawn_blocking(move || {
                let call = owner.call().expect("Host tool-result custody closed");
                let snapshot = call.session().snapshot().expect("Host hook snapshot lost");
                rpi_extensions::emit_tool_result_with_context(
                    &id,
                    &name,
                    &input,
                    &result,
                    is_error,
                    snapshot,
                    Some(&context),
                )
                .expect("Host hook snapshot became stale")
                .map(|result| rpi_agent::AfterToolCallResult {
                    content: Some(result.content),
                    details: Some(result.details),
                    ..Default::default()
                })
            })
            .await
            .expect("Host tool-result worker failed")
        })
    })
}
