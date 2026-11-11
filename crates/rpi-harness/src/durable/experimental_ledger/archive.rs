//! Bounded receipt archive payload for explicit history checkpoints.
//!
//! This is an integrity-checked archive image, not an authorization token or a
//! cross-process journal. Callers must validate the archive before using it and
//! must still compare an archived receipt's scope with the current scope.

use super::{receipt_canonical_bytes, DurableReceipt};
use crate::durable::DurableSnapshot;
use crate::session::types::HistoryIdentity;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub const FORMAT: &str = "rpi-native-durable-receipt-archive";
pub const VERSION: u32 = 1;
pub const MAX_ENTRIES: usize = 256;
pub const MAX_ARCHIVE_BYTES: usize = 4 * 1024 * 1024;
pub const CHECKPOINT_FORMAT: &str = "rpi-native-durable-ledger-checkpoint";
pub const CHECKPOINT_VERSION: u32 = 1;
const ENTRY_DOMAIN: &[u8] = b"rpi-durable-receipt-archive-entry-1\0";
const ARCHIVE_DOMAIN: &[u8] = b"rpi-durable-receipt-archive-1\0";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArchivedReceipt {
    pub request_id: String,
    pub history_revision: u64,
    pub history_generation: u64,
    pub entry_id: String,
    pub ordinal: u64,
    pub receipt: DurableReceipt,
    pub digest: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiptArchive {
    pub format: String,
    pub version: u32,
    pub entries: Vec<ArchivedReceipt>,
    pub digest: [u8; 32],
}

/// The bounded durable projection that a future physical compactor may publish.
/// It binds the current native snapshot to the historical receipt archive; it is
/// not itself a writer capability or a cross-process publication fence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CheckpointImage {
    pub format: String,
    pub version: u32,
    pub history_identity: HistoryIdentity,
    pub snapshot: DurableSnapshot,
    pub archive: ReceiptArchive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ArchiveError {
    #[error("receipt archive format is unsupported")]
    Format,
    #[error("receipt archive limit exceeded")]
    Limit,
    #[error("receipt archive identity is invalid")]
    Identity,
    #[error("receipt archive contains a duplicate request")]
    Duplicate,
    #[error("receipt archive receipt is invalid")]
    Receipt,
    #[error("receipt archive digest is invalid")]
    Digest,
    #[error("receipt archive serialization failed")]
    Serialization,
    #[error("checkpoint identity does not match its durable snapshot")]
    CheckpointIdentity,
    #[error("checkpoint snapshot is invalid")]
    CheckpointSnapshot,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UnsignedEntry<'a> {
    request_id: &'a str,
    history_revision: u64,
    history_generation: u64,
    entry_id: &'a str,
    ordinal: u64,
    receipt: &'a DurableReceipt,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UnsignedArchive<'a> {
    format: &'a str,
    version: u32,
    entries: &'a [ArchivedReceipt],
}

fn identity(value: &str) -> Result<(), ArchiveError> {
    crate::durable::validate_identity(value).map_err(|_| ArchiveError::Identity)
}

fn entry_bytes(entry: &ArchivedReceipt) -> Result<Vec<u8>, ArchiveError> {
    let body = serde_json::to_vec(&UnsignedEntry {
        request_id: &entry.request_id,
        history_revision: entry.history_revision,
        history_generation: entry.history_generation,
        entry_id: &entry.entry_id,
        ordinal: entry.ordinal,
        receipt: &entry.receipt,
    })
    .map_err(|_| ArchiveError::Serialization)?;
    let length = u32::try_from(body.len()).map_err(|_| ArchiveError::Limit)?;
    let mut bytes = Vec::with_capacity(ENTRY_DOMAIN.len() + 4 + body.len());
    bytes.extend_from_slice(ENTRY_DOMAIN);
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(&body);
    Ok(bytes)
}

fn archive_bytes(archive: &ReceiptArchive) -> Result<Vec<u8>, ArchiveError> {
    let body = serde_json::to_vec(&UnsignedArchive {
        format: &archive.format,
        version: archive.version,
        entries: &archive.entries,
    })
    .map_err(|_| ArchiveError::Serialization)?;
    let length = u32::try_from(body.len()).map_err(|_| ArchiveError::Limit)?;
    let mut bytes = Vec::with_capacity(ARCHIVE_DOMAIN.len() + 4 + body.len());
    bytes.extend_from_slice(ARCHIVE_DOMAIN);
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(&body);
    Ok(bytes)
}

impl ArchivedReceipt {
    /// Build a sealed archive entry from the receipt observed before a
    /// replacement. The receipt remains historical and is not a current proof.
    pub fn new(
        receipt: DurableReceipt,
        history: HistoryIdentity,
        ordinal: u64,
    ) -> Result<Self, ArchiveError> {
        if ordinal == 0 {
            return Err(ArchiveError::Identity);
        }
        identity(&receipt.request_id)?;
        identity(&receipt.entry_id)?;
        if receipt.expected.history_revision != history.revision
            || receipt.expected.history_generation != history.generation
        {
            return Err(ArchiveError::Receipt);
        }
        receipt_canonical_bytes(&receipt).map_err(|_| ArchiveError::Receipt)?;
        let mut entry = Self {
            request_id: receipt.request_id.clone(),
            history_revision: history.revision,
            history_generation: history.generation,
            entry_id: receipt.entry_id.clone(),
            ordinal,
            receipt,
            digest: [0; 32],
        };
        entry.digest = Sha256::digest(entry_bytes(&entry)?).into();
        Ok(entry)
    }

    pub fn validate(&self) -> Result<(), ArchiveError> {
        identity(&self.request_id)?;
        identity(&self.entry_id)?;
        if self.ordinal == 0
            || self.receipt.request_id != self.request_id
            || self.receipt.entry_id != self.entry_id
            || self.receipt.expected.history_revision != self.history_revision
            || self.receipt.expected.history_generation != self.history_generation
        {
            return Err(ArchiveError::Receipt);
        }
        receipt_canonical_bytes(&self.receipt).map_err(|_| ArchiveError::Receipt)?;
        if Sha256::digest(entry_bytes(self)?).as_slice() != self.digest {
            return Err(ArchiveError::Digest);
        }
        Ok(())
    }
}

impl CheckpointImage {
    pub fn new(
        history_identity: HistoryIdentity,
        snapshot: DurableSnapshot,
        archive: ReceiptArchive,
    ) -> Result<Self, ArchiveError> {
        let image = Self {
            format: CHECKPOINT_FORMAT.to_owned(),
            version: CHECKPOINT_VERSION,
            history_identity,
            snapshot,
            archive,
        };
        image.validate()?;
        Ok(image)
    }

    pub fn validate(&self) -> Result<(), ArchiveError> {
        if self.format != CHECKPOINT_FORMAT || self.version != CHECKPOINT_VERSION {
            return Err(ArchiveError::Format);
        }
        if self.snapshot.scope.history_revision != self.history_identity.revision
            || self.snapshot.scope.history_generation != self.history_identity.generation
        {
            return Err(ArchiveError::CheckpointIdentity);
        }
        self.snapshot
            .state
            .validate()
            .map_err(|_| ArchiveError::CheckpointSnapshot)?;
        self.snapshot
            .execution_control
            .validate()
            .map_err(|_| ArchiveError::CheckpointSnapshot)?;
        self.archive.validate()
    }
}

impl ReceiptArchive {
    pub fn empty() -> Self {
        let mut archive = Self {
            format: FORMAT.to_owned(),
            version: VERSION,
            entries: Vec::new(),
            digest: [0; 32],
        };
        archive.seal().expect("empty archive is valid");
        archive
    }

    pub fn from_entries(entries: Vec<ArchivedReceipt>) -> Result<Self, ArchiveError> {
        let mut archive = Self {
            format: FORMAT.to_owned(),
            version: VERSION,
            entries,
            digest: [0; 32],
        };
        archive.seal()?;
        Ok(archive)
    }

    pub fn lookup(&self, request_id: &str) -> Result<Option<&ArchivedReceipt>, ArchiveError> {
        identity(request_id)?;
        self.validate()?;
        Ok(self
            .entries
            .iter()
            .find(|entry| entry.request_id == request_id))
    }

    pub fn encoded_len(&self) -> Result<usize, ArchiveError> {
        self.validate()?;
        serde_json::to_vec(self)
            .map(|bytes| bytes.len())
            .map_err(|_| ArchiveError::Serialization)
    }

    pub fn validate(&self) -> Result<(), ArchiveError> {
        if self.format != FORMAT || self.version != VERSION || self.entries.len() > MAX_ENTRIES {
            return Err(ArchiveError::Format);
        }
        let mut request_ids = std::collections::HashSet::with_capacity(self.entries.len());
        for entry in &self.entries {
            entry.validate()?;
            if !request_ids.insert(entry.request_id.as_str()) {
                return Err(ArchiveError::Duplicate);
            }
        }
        let bytes = archive_bytes(self)?;
        if bytes.len() > MAX_ARCHIVE_BYTES {
            return Err(ArchiveError::Limit);
        }
        if Sha256::digest(bytes).as_slice() != self.digest {
            return Err(ArchiveError::Digest);
        }
        Ok(())
    }

    fn seal(&mut self) -> Result<(), ArchiveError> {
        if self.format != FORMAT || self.version != VERSION || self.entries.len() > MAX_ENTRIES {
            return Err(ArchiveError::Format);
        }
        let mut request_ids = std::collections::HashSet::with_capacity(self.entries.len());
        for entry in &self.entries {
            entry.validate()?;
            if !request_ids.insert(entry.request_id.as_str()) {
                return Err(ArchiveError::Duplicate);
            }
        }
        let bytes = archive_bytes(self)?;
        if bytes.len() > MAX_ARCHIVE_BYTES {
            return Err(ArchiveError::Limit);
        }
        self.digest = Sha256::digest(bytes).into();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable::{DurableReceipt, DurableScope, DurableSnapshot};
    use rpi_durable::control::ControlAction;
    use rpi_durable::execution::ExecutionControl;

    fn receipt() -> DurableReceipt {
        DurableReceipt {
            request_id: "request".into(),
            actor: "actor".into(),
            expected: DurableScope {
                session_id: "session".into(),
                lane: "main".into(),
                binding_token: "binding".into(),
                leaf_id: None,
                history_revision: 3,
                history_generation: 4,
                entry_id: None,
                control_revision: 0,
            },
            action: ControlAction::Reset,
            run_id: None,
            entry_id: "entry".into(),
            control_revision: 1,
            changed: true,
        }
    }

    #[test]
    fn seals_and_finds_historical_receipt() {
        let entry = ArchivedReceipt::new(
            receipt(),
            HistoryIdentity {
                revision: 3,
                generation: 4,
            },
            2,
        )
        .unwrap();
        let archive = ReceiptArchive::from_entries(vec![entry]).unwrap();
        assert!(archive.validate().is_ok());
        assert!(archive.lookup("request").unwrap().is_some());
        assert_eq!(
            archive.encoded_len().unwrap(),
            serde_json::to_vec(&archive).unwrap().len()
        );
    }

    #[test]
    fn checkpoint_binds_snapshot_identity_and_archive() {
        let history = HistoryIdentity {
            revision: 0,
            generation: 4,
        };
        let snapshot = DurableSnapshot {
            scope: DurableScope {
                session_id: "session".into(),
                lane: "main".into(),
                binding_token: "binding".into(),
                leaf_id: None,
                history_revision: 0,
                history_generation: 4,
                entry_id: None,
                control_revision: 0,
            },
            state: Default::default(),
            execution_control: ExecutionControl::default(),
        };
        let image = CheckpointImage::new(history, snapshot, ReceiptArchive::empty()).unwrap();
        let encoded = serde_json::to_value(&image).unwrap();
        let restored: CheckpointImage = serde_json::from_value(encoded).unwrap();
        restored.validate().unwrap();
        assert_eq!(restored.history_identity, history);
    }

    #[test]
    fn checkpoint_rejects_snapshot_from_another_generation() {
        let snapshot = DurableSnapshot {
            scope: DurableScope {
                session_id: "session".into(),
                lane: "main".into(),
                binding_token: "binding".into(),
                leaf_id: None,
                history_revision: 0,
                history_generation: 3,
                entry_id: None,
                control_revision: 0,
            },
            state: Default::default(),
            execution_control: ExecutionControl::default(),
        };
        assert_eq!(
            CheckpointImage::new(
                HistoryIdentity {
                    revision: 0,
                    generation: 4,
                },
                snapshot,
                ReceiptArchive::empty(),
            ),
            Err(ArchiveError::CheckpointIdentity)
        );
    }

    #[test]
    fn tampering_is_rejected() {
        let entry = ArchivedReceipt::new(
            receipt(),
            HistoryIdentity {
                revision: 3,
                generation: 4,
            },
            2,
        )
        .unwrap();
        let mut archive = ReceiptArchive::from_entries(vec![entry]).unwrap();
        archive.entries[0].receipt.changed = false;
        assert_eq!(archive.validate(), Err(ArchiveError::Digest));
    }

    #[test]
    fn duplicate_requests_are_rejected() {
        let history = HistoryIdentity {
            revision: 3,
            generation: 4,
        };
        let first = ArchivedReceipt::new(receipt(), history, 1).unwrap();
        assert_eq!(
            ReceiptArchive::from_entries(vec![first.clone(), first]),
            Err(ArchiveError::Duplicate)
        );
    }
}
