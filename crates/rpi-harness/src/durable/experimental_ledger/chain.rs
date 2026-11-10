//! Pure canonical receipt-map chain inspection, NOT a verified durable journal.
//!
//! Starts with an empty tree and replays the existing pure AVL insertion for
//! every locally decoded record. Only exact batches are admitted as resolvers
//! for later records, including rotation intermediates. No caller-supplied root,
//! cache, SourceCandidate scope/counters, or missing-node-as-absence is trusted.
//!
//! This proves canonical receipt-map evolution relative to the supplied ordered
//! records and fixed epoch, NOT their native branch provenance/completeness,
//! business transitions, actor authorization, ExecutionControl or run facts.
//! It cannot be used as a snapshot, writer admission, or restore token. There is
//! no I/O, Session, provider, worker or publication. Limits bound logical retained
//! references/counts, not input allocation, physical RSS, CPU or backend I/O.
use super::{receipt_reference, DecodedCandidate, Epoch, ReadPolicy, Sha256NodeHasher};
use rpi_durable::ledger::{
    insert::{Insert, InsertStep},
    Node, NodeRef,
};
use std::collections::{HashMap, HashSet};

pub const MAX_CHAIN_RECORDS: usize = 256;
pub const MAX_CHAIN_NODES: usize = 16_384;

/// Private construction, read-only summary of this limited inspection only.
/// Not deserializable and no conversion to a durable snapshot or authority.
#[derive(Clone, Debug)]
pub struct LedgerChainInspection {
    epoch: Epoch,
    receipts: u64,
    root: Option<NodeRef>,
}
impl LedgerChainInspection {
    pub fn epoch(&self) -> &Epoch {
        &self.epoch
    }
    pub fn receipts(&self) -> u64 {
        self.receipts
    }
    pub fn root(&self) -> Option<&NodeRef> {
        self.root.as_ref()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ChainError {
    #[error("experimental ledger chain budget exceeded")]
    Limit,
    #[error("experimental ledger chain epoch mismatch")]
    Epoch,
    #[error("invalid experimental ledger predecessor chain")]
    Chain,
    #[error("noncanonical experimental ledger insertion")]
    Ledger,
}

/// Inspect the caller-supplied oldest-first local candidates from an empty root.
/// Empty input means an empty map, NOT proof that native history is empty. The
/// candidates' private records preserve codec checks; native source envelopes
/// and public SourceCandidate fields are deliberately not accepted or trusted.
/// A failure returns no prefix or resumable state; each call starts anew.
pub fn inspect_chain(
    records: &[DecodedCandidate],
    policy: &ReadPolicy,
) -> Result<LedgerChainInspection, ChainError> {
    if records.len() > MAX_CHAIN_RECORDS {
        return Err(ChainError::Limit);
    }
    // Preflight the entire node count before building any resolver. Decoded
    // inputs already satisfy the codec's per-record bounds. Borrow bodies rather
    // than cloning a full history; Insert retains only its bounded search path.
    let mut total_nodes = 0usize;
    for candidate in records {
        total_nodes = total_nodes
            .checked_add(candidate.record().nodes.len())
            .filter(|count| *count <= MAX_CHAIN_NODES)
            .ok_or(ChainError::Limit)?;
    }
    let mut nodes: HashMap<(&str, u16), &Node> = HashMap::new();
    let mut entries = HashSet::new();
    let mut requests = HashSet::new();
    let mut root = None;
    let mut previous: Option<&super::Record> = None;
    let hasher = Sha256NodeHasher;
    for (index, candidate) in records.iter().enumerate() {
        let record = candidate.record();
        let receipt = &record.receipt;
        if record.epoch != policy.epoch {
            return Err(ChainError::Epoch);
        }
        if record.ordinal != index as u64 + 1
            || !entries.insert(receipt.entry_id.as_str())
            || !requests.insert(receipt.request_id.as_str())
        {
            return Err(ChainError::Chain);
        }
        match (previous, &record.previous) {
            (None, None) => {}
            (Some(old), Some(link))
                if link.entry_id == old.receipt.entry_id
                    && link.ordinal == old.ordinal
                    && receipt.expected.control_revision == old.state.revision => {}
            _ => return Err(ChainError::Chain),
        }
        let value = receipt_reference(receipt, record.ordinal).map_err(|_| ChainError::Ledger)?;
        let mut insert = Insert::new(&receipt.request_id, value, root, &hasher)
            .map_err(|_| ChainError::Ledger)?;
        loop {
            match insert.step() {
                InsertStep::NeedNode(reference) => {
                    // Only already exact-matched predecessor batches are in the
                    // map. Missing/wrong old references never imply Absent.
                    let node = nodes
                        .get(&(reference.address.entry_id.as_str(), reference.address.slot))
                        .ok_or(ChainError::Ledger)?;
                    insert
                        .provide(node, &hasher)
                        .map_err(|_| ChainError::Ledger)?;
                }
                InsertStep::Ready(_) => break,
                InsertStep::Poisoned | InsertStep::Taken => return Err(ChainError::Ledger),
            }
        }
        let batch = insert.take_batch().map_err(|_| ChainError::Ledger)?;
        // Equality includes every slot/address, digest/summary, key/value and
        // child reference, not only current-request lookup or reachable nodes.
        if batch.root != record.root || batch.nodes != record.nodes {
            return Err(ChainError::Ledger);
        }
        for node in &record.nodes {
            if nodes
                .insert((node.address.entry_id.as_str(), node.address.slot), node)
                .is_some()
            {
                return Err(ChainError::Ledger);
            }
        }
        root = Some(batch.root);
        previous = Some(record);
    }
    Ok(LedgerChainInspection {
        epoch: policy.epoch.clone(),
        receipts: records.len() as u64,
        root,
    })
}
