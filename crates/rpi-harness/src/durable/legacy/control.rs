//! Conservative analysis only: never a native import permit or live state.
//! The private decoder is the MIT upstream source prefix; see adjacent LICENSE
//! and provenance.json. It is not the native journal reducer.
#[rustfmt::skip]
#[allow(dead_code)]
#[path = "decoder.rs"]
mod decoder;

use super::{
    LegacyBranchScope, LegacyBranchSource, MAX_LEGACY_BRANCH_BYTES,
    MAX_LEGACY_BRANCH_CUSTOM_ENTRIES,
};
use rpi_durable::control::{ControlState, QueueStatus};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashSet;

pub const LEGACY_CONTROL_POLICY: &str = "anchored_control_only_v1";
/// Explicit revision-one origin analysis; not an automatic fallback or permit.
pub const LEGACY_CONTROL_REVISION_ONE_POLICY: &str = "anchored_revision_one_control_only_v1";

/// A value-only inspection result. Public/serializable fields can be forged;
/// consumers MUST NOT treat it as authorization, provenance or a native receipt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LegacyControlInspection {
    pub policy: String,
    pub scope: LegacyBranchScope,
    pub canonical_entry_ids: Vec<String>,
    pub source_anchor_id: String,
    /// Preserves source revision/counter/stamps; not committed native state.
    pub projection: ControlState,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LegacyControlInspectionError {
    #[error("legacy control inspection: source bounds exceeded")]
    LimitExceeded,
    #[error("legacy control inspection: invalid typed source metadata or envelope")]
    InvalidSource,
    #[error("legacy control inspection: no canonical legacy history selected")]
    NoLegacyHistory,
    #[error("legacy control inspection: runtime or native mixed history is excluded")]
    MixedHistory,
    #[error("legacy control inspection: unknown custom history is excluded")]
    OpaqueHistory,
    #[error("legacy control inspection: unsupported fields, format or record shape")]
    UnsupportedRecord,
    #[error("legacy control inspection: execution history is excluded")]
    ExecutionHistory,
    #[error("legacy control inspection: selected origin or contiguous branch anchors are absent")]
    UnanchoredHistory,
    #[error("legacy control inspection: legacy decoder rejected a history prefix")]
    InvalidLegacyHistory,
    #[error("legacy control inspection: source control state is not native-valid")]
    NativeStateRejected,
    #[error("legacy control inspection: current claimed item has no trusted owner mapping")]
    UnownedClaim,
}
type Result<T> = std::result::Result<T, LegacyControlInspectionError>;
use LegacyControlInspectionError as Error;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Record {
    format: String,
    version: u64,
    kind: String,
    base_revision: u64,
    revision: u64,
    #[serde(default)]
    base_entry_id: Option<String>,
    #[serde(default)]
    state: Option<Value>,
    #[serde(default)]
    changes: Option<Vec<Value>>,
}
fn object<'a>(value: &'a Value, allowed: &[&str]) -> Result<&'a Map<String, Value>> {
    let map = value.as_object().ok_or(Error::UnsupportedRecord)?;
    if map.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(Error::UnsupportedRecord);
    }
    Ok(map)
}
fn workflow(value: &Value) -> Result<()> {
    if value.is_null() {
        return Ok(());
    }
    let map = object(value, &["name", "status", "steps", "reason"])?;
    for step in map
        .get("steps")
        .and_then(Value::as_array)
        .ok_or(Error::UnsupportedRecord)?
    {
        object(step, &["id", "title", "status", "note"])?;
    }
    Ok(())
}
fn queue_item(value: &Value) -> Result<()> {
    object(
        value,
        &[
            "id",
            "text",
            "status",
            "createdRevision",
            "updatedRevision",
            "note",
        ],
    )?;
    Ok(())
}
fn queue(value: &Value) -> Result<()> {
    for item in value.as_array().ok_or(Error::UnsupportedRecord)? {
        queue_item(item)?;
    }
    Ok(())
}
fn state(value: &Value) -> Result<()> {
    let map = object(
        value,
        &[
            "revision",
            "nextRunId",
            "nextQueueId",
            "workflow",
            "run",
            "queue",
        ],
    )?;
    if map
        .get("nextRunId")
        .is_some_and(|value| value.as_u64() != Some(0))
        || map.get("run").is_some_and(|value| !value.is_null())
    {
        return Err(Error::ExecutionHistory);
    }
    if let Some(value) = map.get("workflow") {
        workflow(value)?;
    }
    if let Some(value) = map.get("queue") {
        queue(value)?;
    }
    Ok(())
}
fn change(value: &Value) -> Result<()> {
    let op = value
        .get("op")
        .and_then(Value::as_str)
        .ok_or(Error::UnsupportedRecord)?;
    let fields: &[&str] = match op {
        "setWorkflow" => &["op", "workflow"],
        "updateStep" => &["op", "step_id", "status", "note"],
        "setWorkflowMeta" => &["op", "name", "status", "reason"],
        "setCounters" => &["op", "next_run_id", "next_queue_id"],
        "appendQueue" | "updateQueue" => &["op", "item"],
        "setQueue" => &["op", "queue"],
        "setRun" | "setRunMeta" | "appendTool" | "updateTool" | "setTools" => {
            return Err(Error::ExecutionHistory)
        }
        _ => return Err(Error::UnsupportedRecord),
    };
    let map = object(value, fields)?;
    match op {
        "setWorkflow" => workflow(map.get("workflow").ok_or(Error::UnsupportedRecord)?)?,
        "setCounters" => {
            if map.get("next_run_id").and_then(Value::as_u64) != Some(0) {
                return Err(Error::ExecutionHistory);
            }
        }
        "appendQueue" | "updateQueue" => {
            queue_item(map.get("item").ok_or(Error::UnsupportedRecord)?)?
        }
        "setQueue" => queue(map.get("queue").ok_or(Error::UnsupportedRecord)?)?,
        _ => (),
    }
    Ok(())
}

/// Analyze ONLY a fully anchored, understood control-only history. This is more
/// conservative than the old decoder: no obsolete invalid prefixes, execution
/// history, unknown fields, nonzero/truncated roots, active unowned claims or
/// unanchored snapshots are accepted. All source values remain untouched.
///
/// No Session access/writes or current-scope revalidation occurs here. Even Ok
/// is merely a candidate comparison: backup, all-branch provenance, trusted
/// ownership/receipt mapping, atomic staging/cutover and authorization are absent.
/// Runtime cost can be quadratic in the bounded count (every prefix is replayed);
/// byte/count caps are not allocation, depth, latency or CPU guarantees.
pub fn inspect_legacy_control(source: &LegacyBranchSource) -> Result<LegacyControlInspection> {
    inspect_at_origin(source, 0, LEGACY_CONTROL_POLICY)
}

/// Explicitly analyze a control-only branch whose first record is a revision-one
/// snapshot with base revision zero and no prior canonical anchor. The original
/// zero-root API is unchanged and NEVER falls back to this policy.
///
/// This recognizes a record shape emitted by the pinned old runtime, not proof
/// that the runtime produced this source or that no prior history was omitted.
/// All other bounds, prefix validation, execution exclusions and unowned-current-
/// claim rejection are shared with `inspect_legacy_control`. Source revision,
/// counters, stamps and business values are preserved, never rebased or repaired.
/// Ok remains a forgeable, non-authorizing comparison; it is NOT permission to
/// import, invent ownership/receipts, bypass native guards or change defaults.
pub fn inspect_legacy_control_revision_one(
    source: &LegacyBranchSource,
) -> Result<LegacyControlInspection> {
    inspect_at_origin(source, 1, LEGACY_CONTROL_REVISION_ONE_POLICY)
}

// The only callers select one fixed origin/tag pair above. No wire option,
// guessing, automatic fallback, source mutation or second decoder is introduced.
fn inspect_at_origin(
    source: &LegacyBranchSource,
    origin_revision: u64,
    policy: &'static str,
) -> Result<LegacyControlInspection> {
    if !source.runtime_entries.is_empty() || !source.native_control_entries.is_empty() {
        return Err(Error::MixedHistory);
    }
    if source.other_custom_entries != 0 {
        return Err(Error::OpaqueHistory);
    }
    let entries = &source.canonical_entries;
    if entries.len() > MAX_LEGACY_BRANCH_CUSTOM_ENTRIES {
        return Err(Error::LimitExceeded);
    }
    if entries.len() != source.inspected_custom_entries {
        return Err(Error::InvalidSource);
    }
    if entries.is_empty() {
        return Err(Error::NoLegacyHistory);
    }
    if source.scope.session_id.is_empty()
        || source.scope.lane.is_empty()
        || source.scope.binding_token.is_empty()
        || source
            .scope
            .selected_leaf_id
            .as_deref()
            .is_none_or(str::is_empty)
        || source
            .scope
            .current_leaf_id
            .as_deref()
            .is_some_and(str::is_empty)
    {
        return Err(Error::InvalidSource);
    }
    let mut ids = HashSet::new();
    let mut canonical_ids = Vec::with_capacity(entries.len());
    let mut bytes = 0usize;
    let mut previous_seq = 0;
    let mut previous_revision = 0u64;
    let mut anchor: Option<String> = None;
    let mut final_projection = None;
    for (index, entry) in entries.iter().enumerate() {
        bytes = bytes
            .checked_add(
                serde_json::to_vec(entry)
                    .map_err(|_| Error::InvalidSource)?
                    .len(),
            )
            .ok_or(Error::LimitExceeded)?;
        if bytes > MAX_LEGACY_BRANCH_BYTES {
            return Err(Error::LimitExceeded);
        }
        let map = object(
            entry,
            &[
                "type",
                "id",
                "seq",
                "parentId",
                "timestamp",
                "customType",
                "data",
            ],
        )?;
        let id = map
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or(Error::InvalidSource)?;
        let seq = map
            .get("seq")
            .and_then(Value::as_u64)
            .ok_or(Error::InvalidSource)?;
        if map.get("type").and_then(Value::as_str) != Some("custom")
            || map.get("customType").and_then(Value::as_str) != Some("durable-state")
            || !map.get("parentId").is_some_and(|value| {
                value.is_null() || value.as_str().is_some_and(|id| !id.is_empty())
            })
            || map.get("timestamp").and_then(Value::as_i64).is_none()
            || seq == 0
            || seq <= previous_seq
            || seq > source.scope.history_revision
            || !ids.insert(id.to_owned())
        {
            return Err(Error::InvalidSource);
        }
        previous_seq = seq;
        let record: Record =
            serde_json::from_value(map.get("data").ok_or(Error::InvalidSource)?.clone())
                .map_err(|_| Error::UnsupportedRecord)?;
        if record.format != "rpi-durable-journal" || record.version != 1 {
            return Err(Error::UnsupportedRecord);
        }
        if index == 0 {
            if record.kind != "snapshot"
                || record.base_revision != 0
                || record.revision != origin_revision
                || record.base_entry_id.is_some()
            {
                return Err(Error::UnanchoredHistory);
            }
        } else if record.base_revision != previous_revision
            || record.base_entry_id != anchor
            || Some(record.revision) != previous_revision.checked_add(1)
        {
            return Err(Error::UnanchoredHistory);
        }
        match record.kind.as_str() {
            "snapshot" => {
                if record.changes.is_some() {
                    return Err(Error::UnsupportedRecord);
                }
                state(record.state.as_ref().ok_or(Error::UnsupportedRecord)?)?;
            }
            "delta" => {
                if record.state.is_some() {
                    return Err(Error::UnsupportedRecord);
                }
                let changes = record
                    .changes
                    .as_ref()
                    .filter(|changes| !changes.is_empty())
                    .ok_or(Error::UnsupportedRecord)?;
                for value in changes {
                    change(value)?;
                }
            }
            _ => return Err(Error::UnsupportedRecord),
        }
        let old = decoder::DurableState::try_from_branch(&entries[..=index])
            .map_err(|_| Error::InvalidLegacyHistory)?;
        if old.next_run_id != 0 || old.run.is_some() {
            return Err(Error::ExecutionHistory);
        }
        let projection:ControlState=serde_json::from_value(json!({
            "revision":old.revision,"nextQueueId":old.next_queue_id,"workflow":old.workflow,"queue":old.queue
        })).map_err(|_|Error::NativeStateRejected)?;
        projection
            .validate()
            .map_err(|_| Error::NativeStateRejected)?;
        previous_revision = record.revision;
        anchor = Some(id.to_owned());
        canonical_ids.push(id.to_owned());
        final_projection = Some(projection);
    }
    let projection = final_projection.ok_or(Error::NoLegacyHistory)?;
    if projection
        .queue
        .iter()
        .any(|item| item.status == QueueStatus::Claimed)
    {
        return Err(Error::UnownedClaim);
    }
    Ok(LegacyControlInspection {
        policy: policy.into(),
        scope: source.scope.clone(),
        canonical_entry_ids: canonical_ids,
        source_anchor_id: anchor.ok_or(Error::NoLegacyHistory)?,
        projection,
    })
}
