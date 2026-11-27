//! Frozen instruction-only resources. Not an extension authority or loader.
use super::ordinary::{BackendError, BackendResult};
use crate::resource_selection::{self, ResolvedSelection, ResourceSelection, ToolSelection};
use rpi_harness::{
    session::{Session, SessionValues},
    types::{AgentHarnessResources, HarnessTool},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// Immutable, bounded explicit instruction snapshot. Construction performs only
/// reads. This type cannot contain DLLs, package discovery or skill execution.
/// A profile identifier is evidence of binding identity, never an authority grant.
pub struct NativeInstructionResources {
    selected: ResolvedSelection,
    identity: String,
}
impl NativeInstructionResources {
    pub fn freeze(selection: ResourceSelection) -> BackendResult<Self> {
        let selected = ResolvedSelection::host_instructions(selection)
            .map_err(|_| BackendError::Binding("invalid explicit instruction resources"))?;
        if let ToolSelection::Only { names } = &selected.selection.tools {
            let unique: BTreeSet<_> = names.iter().collect();
            if unique.len() != names.len() || names.iter().any(|name| name.is_empty()) {
                return Err(BackendError::Binding("invalid resource tool selection"));
            }
        }
        let identity =
            format!("instructions-v1:{:x}", Sha256::digest(serde_json::to_vec(
            &json!({"selection":selected.selection,"instructions":selected.provenance()})
        ).map_err(|_| BackendError::Binding("invalid resource identity"))?));
        Ok(Self { selected, identity })
    }
    pub fn identity(&self) -> &str {
        &self.identity
    }
    pub(super) fn validate_tools(&self, tools: &[String]) -> BackendResult<()> {
        let matches = match &self.selected.selection.tools {
            ToolSelection::Only { names } => names == tools,
            ToolSelection::None {} => tools.is_empty(),
            ToolSelection::Default {} => false,
        };
        if !matches {
            return Err(BackendError::Binding(
                "resource tools differ from exact Agent tools",
            ));
        }
        Ok(())
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
        let schemas: Vec<_> = tools.iter().map(|tool| json!(tool.tool.schema())).collect();
        json!({"skills":[],"extensions":[],"toolSchemasSha256":format!("{:x}",
            Sha256::digest(serde_json::to_vec(&schemas).expect("tool schemas serialize")))})
    }
    pub(super) fn validate(
        &self,
        values: &SessionValues,
        tools: &[HarnessTool],
    ) -> BackendResult<()> {
        resource_selection::validate_exact_inventory(values, &self.selected, &self.inventory(tools))
            .map_err(|_| {
                BackendError::Binding("saved instruction resources or tool inventory changed")
            })
    }
    /// Only called inside owned build, with the exact strict-committed Session,
    /// before creating any Harness or publishing Live. Always sync on reopen,
    /// including a binding left visible after a previous failed sync.
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
}
