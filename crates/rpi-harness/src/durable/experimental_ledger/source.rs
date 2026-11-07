//! Explicit bounded branch source, not a verified journal or live admission path.
//!
//! Follows every parent entry (including non-custom entries) from an explicit
//! leaf. None means empty, never current/latest. Storage is a trusted resolver;
//! observed fences are not atomic generation/ABA fencing. Local codec results
//! do not prove AVL transitions, business transitions, actor permission or runs.
//! Session payloads are already Values: original raw duplicates/whitespace are
//! irrecoverable here. The raw codec's duplicate checks cannot restore them.
use super::{decode, CodecError, DecodedCandidate, ReadPolicy, CUSTOM_TYPE};
use crate::durable::validate_identity;
use crate::session::{
    types::{CustomEntry, Entry},
    Session,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    io::{self, Write},
    sync::Arc,
};

pub const MAX_PATH_ENTRIES: usize = 4096;
pub const MAX_CUSTOM_ENTRIES: usize = 256;
pub const MAX_BRANCH_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceScope {
    pub session_id: String,
    pub lane: String,
    pub binding_token: String,
    pub selected_leaf_id: Option<String>,
    pub current_leaf_id: Option<String>,
    pub history_revision: u64,
    #[serde(default)]
    pub history_generation: u64,
}

#[derive(Clone, Debug)]
pub struct SourceRecord {
    pub entry: CustomEntry,
    pub candidate: DecodedCandidate,
}

/// Read-only source candidate, not an authorization token. Public fields do not
/// confer trust; future chain validation must establish its own invariants.
#[derive(Clone, Debug)]
pub struct SourceCandidate {
    pub scope: SourceScope,
    /// Oldest first. Only local codec and source-chain shape were checked.
    pub records: Vec<SourceRecord>,
    pub inspected_entries: usize,
    pub inspected_custom_entries: usize,
    pub other_custom_entries: usize,
    pub encoded_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    #[error("experimental ledger source budget exceeded")]
    Limit,
    #[error("invalid experimental ledger source identity")]
    Identity,
    #[error("experimental ledger source policy mismatch")]
    Policy,
    #[error("experimental ledger source read failed")]
    Read,
    #[error("experimental ledger source changed")]
    Conflict,
    #[error("incompatible durable history on selected branch")]
    Mixed,
    #[error("invalid experimental ledger source metadata")]
    Metadata,
    #[error("invalid experimental ledger source chain")]
    Chain,
    #[error(transparent)]
    Codec(#[from] CodecError),
}

type Result<T> = std::result::Result<T, SourceError>;
fn identity(id: &str) -> Result<()> {
    validate_identity(id).map_err(|_| SourceError::Identity)
}

struct Counter {
    bytes: usize,
    limit: usize,
}
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes) {
            return Err(io::Error::other("source serialization budget exceeded"));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn encoded_len(value: &impl Serialize, limit: usize) -> Result<usize> {
    let mut counter = Counter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value).map_err(|_| SourceError::Limit)?;
    Ok(counter.bytes)
}

/// No opening/repair/import, lane movement, recovery, provider or writer calls.
/// Errors from the traversal are fenced too; no successful prefix is returned.
pub async fn read_branch(
    source: &Session,
    lane: &str,
    selected_leaf_id: Option<&str>,
    policy: &ReadPolicy,
) -> Result<SourceCandidate> {
    identity(lane)?;
    if let Some(id) = selected_leaf_id {
        identity(id)?;
    }
    let session = source.pin(Arc::clone(source.id_generator()));
    let binding_token = session.binding_token();
    let storage = session.storage();
    let session_id = storage.metadata().id;
    identity(&session_id)?;
    if policy.epoch.session_id != session_id || policy.epoch.lane != lane {
        return Err(SourceError::Policy);
    }
    let history_identity = storage
        .history_identity()
        .await
        .map_err(|_| SourceError::Read)?;
    let revision = history_identity.revision;
    let view = session.view(lane);
    let current_leaf_id = view.get_leaf_id().await.map_err(|_| SourceError::Read)?;
    let scope = SourceScope {
        session_id,
        lane: lane.to_owned(),
        binding_token: binding_token.clone(),
        selected_leaf_id: selected_leaf_id.map(str::to_owned),
        current_leaf_id: current_leaf_id.clone(),
        history_revision: revision,
        history_generation: history_identity.generation,
    };
    let result = collect(&session, selected_leaf_id, policy, scope).await;
    // Deliberately observe every fence read, including when collect failed.
    let leaf_after = view.get_leaf_id().await.map_err(|_| SourceError::Read);
    let history_identity_after = storage
        .history_identity()
        .await
        .map_err(|_| SourceError::Read);
    let binding_after = source.binding_token();
    if leaf_after? != current_leaf_id
        || history_identity_after? != history_identity
        || binding_after != binding_token
    {
        return Err(SourceError::Conflict);
    }
    result
}

async fn collect(
    session: &Session,
    selected_leaf_id: Option<&str>,
    policy: &ReadPolicy,
    scope: SourceScope,
) -> Result<SourceCandidate> {
    let revision = scope.history_revision;
    let generation = scope.history_generation;
    let mut output = SourceCandidate {
        scope,
        records: Vec::new(),
        inspected_entries: 0,
        inspected_custom_entries: 0,
        other_custom_entries: 0,
        encoded_bytes: 0,
    };
    let mut next = selected_leaf_id.map(str::to_owned);
    let mut upper_seq: Option<u64> = None;
    // Bounded metadata map also checks duplicate IDs and expected leaf ancestry.
    let mut ancestors = HashMap::new();
    while let Some(id) = next {
        if output.inspected_entries >= MAX_PATH_ENTRIES {
            return Err(SourceError::Limit);
        }
        let entry = session
            .get_entry(&id)
            .await
            .map_err(|_| SourceError::Read)?
            .ok_or(SourceError::Metadata)?;
        let base = entry.base();
        identity(&base.id)?;
        if base.id != id
            || base.entry_type != entry.entry_type()
            || base.seq == 0
            || base.seq > revision
            || upper_seq.is_some_and(|seq| base.seq >= seq)
            || ancestors.insert(base.id.clone(), base.seq).is_some()
        {
            return Err(SourceError::Metadata);
        }
        if let Some(parent) = &base.parent_id {
            identity(parent)?;
            if parent == &base.id {
                return Err(SourceError::Metadata);
            }
        }
        next = base.parent_id.clone();
        upper_seq = Some(base.seq);
        output.inspected_entries += 1;
        output.encoded_bytes += encoded_len(&entry, MAX_BRANCH_BYTES - output.encoded_bytes)?;
        if let Entry::Custom(custom) = entry {
            output.inspected_custom_entries += 1;
            if output.inspected_custom_entries > MAX_CUSTOM_ENTRIES {
                return Err(SourceError::Limit);
            }
            match custom.custom_type.as_str() {
                CUSTOM_TYPE => {
                    let data = custom.data.as_ref().ok_or(SourceError::Metadata)?;
                    encoded_len(data, super::MAX_RECORD_BYTES)?;
                    let raw = serde_json::to_vec(data).map_err(|_| SourceError::Metadata)?;
                    let candidate = decode(&raw, &custom.base.id, CUSTOM_TYPE, policy)?;
                    output.records.push(SourceRecord {
                        entry: custom,
                        candidate,
                    });
                }
                "durable-state" | "rpi-durable-runtime" => return Err(SourceError::Mixed),
                ty if ty.starts_with("rpi.durable.") => return Err(SourceError::Mixed),
                _ => output.other_custom_entries += 1,
            }
        }
    }
    output.records.reverse();
    let mut requests = HashSet::new();
    let mut previous: Option<&SourceRecord> = None;
    for (index, item) in output.records.iter().enumerate() {
        let record = item.candidate.record();
        if record.ordinal != index as u64 + 1
            || !requests.insert(&record.receipt.request_id)
            || record.receipt.expected.history_revision >= item.entry.base.seq
            || record.receipt.expected.history_generation != generation
        {
            return Err(SourceError::Chain);
        }
        match (previous, &record.previous) {
            (None, None) => {}
            (Some(old), Some(link))
                if link.entry_id == old.entry.base.id
                    && link.ordinal == old.candidate.record().ordinal
                    && record.receipt.expected.control_revision
                        == old.candidate.record().state.revision
                    && record.receipt.expected.history_revision >= old.entry.base.seq => {}
            _ => return Err(SourceError::Chain),
        }
        // The captured pre-append leaf must be the actual native parent, not
        // merely some older ancestor. This is scope shape, not action replay.
        if record.receipt.expected.leaf_id != item.entry.base.parent_id {
            return Err(SourceError::Chain);
        }
        if let Some(leaf) = &record.receipt.expected.leaf_id {
            if !ancestors.get(leaf).is_some_and(|seq| {
                *seq < item.entry.base.seq && *seq <= record.receipt.expected.history_revision
            }) {
                return Err(SourceError::Chain);
            }
        }
        previous = Some(item);
    }
    Ok(output)
}
