//! Explicit, read-only source extraction for future legacy inspection.
//!
//! Source extraction is NOT a decoder, eligibility decision, import, native state or an
//! authorization token. It is not used by normal restore or the native guard.
mod control;
pub use control::{
    inspect_legacy_control, inspect_legacy_control_revision_one, LegacyControlInspection,
    LegacyControlInspectionError, LEGACY_CONTROL_POLICY, LEGACY_CONTROL_REVISION_ONE_POLICY,
};

use super::{DurableError, CONTROL_TYPE};
use crate::session::{
    types::{BranchBounds, Entry, EntryOrder, EntryQuery},
    Session,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, sync::Arc};

/// Includes unrelated custom entries: no unbounded search for a matching type.
pub const MAX_LEGACY_BRANCH_CUSTOM_ENTRIES: usize = 256;
/// Serialized selected custom-entry envelopes, including unrelated custom data.
/// This is not a limit on backend scan IO, allocation or branch depth.
pub const MAX_LEGACY_BRANCH_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LegacyBranchScope {
    pub session_id: String,
    pub lane: String,
    pub binding_token: String,
    /// Explicit source leaf. None means the empty branch, NOT current/latest.
    pub selected_leaf_id: Option<String>,
    /// Current lane pointer observed without moving it to the selected leaf.
    pub current_leaf_id: Option<String>,
    pub history_revision: u64,
    #[serde(default)]
    pub history_generation: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LegacyBranchSource {
    pub scope: LegacyBranchScope,
    /// Exact typed native envelopes, oldest first, retaining opaque data.
    /// Only these entries are accepted by the pinned legacy decoder in tests.
    pub canonical_entries: Vec<Value>,
    /// Never silently discarded or passed to the canonical legacy decoder.
    pub runtime_entries: Vec<Value>,
    /// Signals mixed history; extraction does not validate its native payload.
    pub native_control_entries: Vec<Value>,
    pub other_custom_entries: usize,
    pub inspected_custom_entries: usize,
}

fn read_error(_: crate::error::SessionError) -> DurableError {
    // No payloads, paths or backend error text in the diagnostic.
    DurableError::ReadFailure("legacy branch source read failed".into())
}
fn invalid(message: &str) -> DurableError {
    DurableError::InvalidJournal(format!("legacy branch source: {message}"))
}

/// Read one explicitly selected branch without publishing state or moving lanes.
///
/// Caller must already own a trusted Session. No filesystem open/repair, provider,
/// receipts, gate worker or implicit current/latest selection is performed here.
/// Observed binding/leaf/revision checks are not atomic/generation/ABA fencing;
/// a dishonest backend can still omit entries consistently. None deliberately
/// bypasses BranchBounds' normal default-to-current semantics.
pub async fn read_legacy_branch(
    source: &Session,
    lane: &str,
    selected_leaf_id: Option<&str>,
) -> Result<LegacyBranchSource, DurableError> {
    if lane.is_empty() || selected_leaf_id.is_some_and(str::is_empty) {
        return Err(invalid("empty lane or leaf identity"));
    }
    let session = source.pin(Arc::clone(source.id_generator()));
    let binding_token = session.binding_token();
    let storage = session.storage();
    let history_identity = storage.history_identity().await.map_err(read_error)?;
    let revision = history_identity.revision;
    let view = session.view(lane);
    let current_leaf_id = view.get_leaf_id().await.map_err(read_error)?;
    let scope = LegacyBranchScope {
        session_id: storage.metadata().id,
        lane: lane.into(),
        binding_token: binding_token.clone(),
        selected_leaf_id: selected_leaf_id.map(str::to_owned),
        current_leaf_id: current_leaf_id.clone(),
        history_revision: revision,
        history_generation: history_identity.generation,
    };
    let result = async {
        let mut selected_custom = None;
        let entries = if let Some(leaf) = selected_leaf_id {
            let entry = session
                .get_entry(leaf)
                .await
                .map_err(read_error)?
                .ok_or_else(|| invalid("selected leaf not found"))?;
            if entry.base().id != leaf {
                return Err(invalid("selected leaf identity mismatch"));
            }
            if matches!(entry, Entry::Custom(_)) {
                selected_custom = Some(leaf);
            }
            view.find_entries_on_branch(
                &EntryQuery {
                    entry_type: Some("custom"),
                    order: Some(EntryOrder::OldestFirst),
                    limit: Some(MAX_LEGACY_BRANCH_CUSTOM_ENTRIES + 1),
                    ..Default::default()
                },
                &BranchBounds {
                    start: Some(leaf.into()),
                    ..Default::default()
                },
            )
            .await
            .map_err(read_error)?
        } else {
            Vec::new()
        };
        if entries.len() > MAX_LEGACY_BRANCH_CUSTOM_ENTRIES {
            return Err(DurableError::LimitExceeded);
        }
        if selected_custom
            .is_some_and(|id| entries.last().map(|entry| entry.base().id.as_str()) != Some(id))
        {
            return Err(invalid("selected custom leaf missing from query"));
        }
        let mut out = LegacyBranchSource {
            scope,
            canonical_entries: vec![],
            runtime_entries: vec![],
            native_control_entries: vec![],
            other_custom_entries: 0,
            inspected_custom_entries: entries.len(),
        };
        let mut previous_seq = 0;
        let mut ids = HashSet::new();
        let mut bytes = 0usize;
        for entry in entries {
            let Entry::Custom(custom) = &entry else {
                return Err(invalid("unexpected non-custom query result"));
            };
            let base = &custom.base;
            if base.entry_type != "custom"
                || base.seq == 0
                || base.seq <= previous_seq
                || base.seq > revision
                || base.id.is_empty()
                || !ids.insert(base.id.clone())
            {
                return Err(invalid("invalid entry identity or sequence order"));
            }
            previous_seq = base.seq;
            let encoded =
                serde_json::to_vec(&entry).map_err(|_| invalid("entry serialization failed"))?;
            bytes = bytes
                .checked_add(encoded.len())
                .ok_or(DurableError::LimitExceeded)?;
            if bytes > MAX_LEGACY_BRANCH_BYTES {
                return Err(DurableError::LimitExceeded);
            }
            // Preserve id/type/customType/data and all typed native base fields.
            // No legacy payload interpretation or native state conversion here.
            match custom.custom_type.as_str() {
                "durable-state" => out.canonical_entries.push(
                    serde_json::to_value(&entry)
                        .map_err(|_| invalid("entry serialization failed"))?,
                ),
                "rpi-durable-runtime" => out.runtime_entries.push(
                    serde_json::to_value(&entry)
                        .map_err(|_| invalid("entry serialization failed"))?,
                ),
                CONTROL_TYPE => out.native_control_entries.push(
                    serde_json::to_value(&entry)
                        .map_err(|_| invalid("entry serialization failed"))?,
                ),
                _ => out.other_custom_entries += 1,
            }
        }
        Ok(out)
    }
    .await;
    // Check errors too: never publish a plausible stale source after a race.
    if view.get_leaf_id().await.map_err(read_error)? != current_leaf_id
        || storage.history_identity().await.map_err(read_error)? != history_identity
        || source.binding_token() != binding_token
    {
        return Err(DurableError::Conflict);
    }
    result
}
