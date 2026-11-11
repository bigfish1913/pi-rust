//! Receipts and verified durable journal views.

use super::admission::{validate_execution_target, validate_identity, DurableError};
use super::scope::{DurableScope, DurableSnapshot};
use crate::domain::control::{ControlAction, ControlState};
use crate::domain::execution::ExecutionControl;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableReceipt {
    pub request_id: String,
    pub actor: String,
    pub expected: DurableScope,
    pub action: ControlAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub entry_id: String,
    pub control_revision: u64,
    pub changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableOutcome {
    pub receipt: DurableReceipt,
    pub snapshot: DurableSnapshot,
    pub replayed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControlRecord {
    pub format: String,
    pub version: u32,
    pub previous_entry_id: Option<String>,
    pub state: ControlState,
    #[serde(default)]
    pub execution_control: ExecutionControl,
    pub receipts: Vec<DurableReceipt>,
}

/// An immutable, bounded proof assembled only after the complete canonical
/// control branch has been read and checked. This is a local journal view: it
/// is not a writer capability, a cross-process fence, a cryptographic root, or
/// a power-loss/crash guarantee. In particular, callers must not manufacture
/// one from a snapshot or from a single newest record.
#[derive(Clone, Debug)]
pub struct VerifiedJournal {
    snapshot: DurableSnapshot,
    record: ControlRecord,
}

impl VerifiedJournal {
    /// Build the view after the branch reader has checked predecessor ordering
    /// and cumulative-record continuity. The constructor repeats the
    /// current-record checks so future callers cannot accidentally omit the
    /// final binding checks when introducing another reader.
    pub fn from_validated(
        snapshot: DurableSnapshot,
        record: ControlRecord,
    ) -> Result<Self, DurableError> {
        let invalid = || DurableError::InvalidJournal("invalid verified journal boundary".into());
        if record.format != super::CONTROL_FORMAT
            || record.version != 1
            || record.receipts.len() > super::MAX_RECEIPTS
            || record.state != snapshot.state
            || record.execution_control != snapshot.execution_control
            || record.state.revision != snapshot.scope.control_revision
        {
            return Err(invalid());
        }
        match (snapshot.scope.entry_id.as_deref(), record.receipts.last()) {
            (None, None) => {
                if record.previous_entry_id.is_some()
                    || record.state.revision != 0
                    || record.execution_control != ExecutionControl::default()
                {
                    return Err(invalid());
                }
            }
            (Some(entry_id), Some(last)) if entry_id == last.entry_id => {}
            _ => return Err(invalid()),
        }

        let mut requests = std::collections::HashSet::new();
        let mut entries = std::collections::HashSet::new();
        for (index, receipt) in record.receipts.iter().enumerate() {
            validate_identity(&receipt.request_id).map_err(|_| invalid())?;
            validate_identity(&receipt.actor).map_err(|_| invalid())?;
            validate_execution_target(&receipt.action, receipt.run_id.as_deref())
                .map_err(|_| invalid())?;
            if receipt.expected.session_id != snapshot.scope.session_id
                || receipt.expected.lane != snapshot.scope.lane
                || receipt.expected.history_revision > snapshot.scope.history_revision
                || receipt.expected.history_generation != snapshot.scope.history_generation
                || receipt.entry_id.is_empty()
                || !requests.insert(&receipt.request_id)
                || !entries.insert(&receipt.entry_id)
                || receipt.control_revision
                    != receipt
                        .expected
                        .control_revision
                        .checked_add(u64::from(receipt.changed))
                        .ok_or_else(invalid)?
            {
                return Err(invalid());
            }
            if index == 0 {
                if receipt.expected.entry_id.is_some() || receipt.expected.control_revision != 0 {
                    return Err(invalid());
                }
            } else {
                let previous = &record.receipts[index - 1];
                if receipt.expected.entry_id.as_deref() != Some(previous.entry_id.as_str())
                    || receipt.expected.control_revision != previous.control_revision
                {
                    return Err(invalid());
                }
            }
        }
        if let Some(last) = record.receipts.last() {
            if last.control_revision != record.state.revision
                || last.expected.entry_id != record.previous_entry_id
            {
                return Err(invalid());
            }
        }
        Ok(Self { snapshot, record })
    }

    pub fn snapshot(&self) -> &DurableSnapshot {
        &self.snapshot
    }

    pub fn receipts(&self) -> &[DurableReceipt] {
        &self.record.receipts
    }

    pub fn receipt(&self, request_id: &str) -> Option<&DurableReceipt> {
        self.record
            .receipts
            .iter()
            .find(|receipt| receipt.request_id == request_id)
    }

    pub fn into_parts(self) -> (DurableSnapshot, ControlRecord) {
        (self.snapshot, self.record)
    }
}
