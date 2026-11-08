//! Experimental pure receipt-map lookup prerequisite, NOT a journal or writer.
//!
//! The caller supplies a trusted root and resolves only requested nodes. A valid
//! empty child means Absent; missing/corrupt proofs poison the lookup. Validation
//! covers the visited path and child reference summaries, not unvisited subtrees,
//! receipt contents, session/epoch identity or arbitrary forged history.
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

pub mod insert;

pub const MAX_ID_BYTES: usize = 128;
pub const MAX_PATH_NODES: usize = 128;
pub const MAX_NODE_BYTES: usize = 2048;
const DOMAIN: &[u8] = b"rpi-durable-ledger-node-experimental-1\0";

pub type Digest = [u8; 32];

/// Trusted embedding-provided algorithm. This crate does not select a production
/// digest or authenticate roots; a test hash must never be treated as security.
pub trait NodeHasher {
    fn digest(&self, canonical: &[u8]) -> Digest;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Address {
    pub entry_id: String,
    pub ordinal: u64,
    pub slot: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiptRef {
    pub entry_id: String,
    pub ordinal: u64,
    pub digest: Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NodeRef {
    pub address: Address,
    pub digest: Digest,
    pub height: u8,
    pub count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Node {
    pub address: Address,
    pub key: String,
    pub value: ReceiptRef,
    pub left: Option<NodeRef>,
    pub right: Option<NodeRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LedgerError {
    #[error("invalid ledger identity")]
    Identity,
    #[error("invalid ledger reference order")]
    ReferenceOrder,
    #[error("invalid ledger AVL shape or count")]
    Shape,
    #[error("ledger arithmetic overflow")]
    Overflow,
    #[error("ledger bounded budget exceeded")]
    Limit,
    #[error("ledger proof does not match requested reference")]
    Proof,
    #[error("ledger key violates search bounds")]
    KeyOrder,
    #[error("requested ledger node unavailable")]
    MissingNode,
    #[error("ledger lookup is terminal")]
    Terminal,
}
type Result<T> = std::result::Result<T, LedgerError>;

fn identity(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > MAX_ID_BYTES
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(LedgerError::Identity);
    }
    Ok(())
}
impl Address {
    fn validate(&self) -> Result<()> {
        identity(&self.entry_id)?;
        if self.ordinal == 0 {
            return Err(LedgerError::ReferenceOrder);
        }
        Ok(())
    }
    fn precedes(&self, parent: &Self) -> bool {
        if self.entry_id == parent.entry_id {
            self.ordinal == parent.ordinal && self.slot < parent.slot
        } else {
            self.ordinal < parent.ordinal
        }
    }
}
impl NodeRef {
    fn validate(&self) -> Result<()> {
        self.address.validate()?;
        if self.height == 0 || self.count == 0 {
            return Err(LedgerError::Shape);
        }
        if usize::from(self.height) > MAX_PATH_NODES {
            return Err(LedgerError::Limit);
        }
        // Minimum AVL nodes: n(0)=0, n(1)=1, n(h)=1+n(h-1)+n(h-2).
        // Reject impossible tall summaries even before asking for their bodies.
        let (mut a, mut b) = (0u64, 1u64);
        for _ in 2..=self.height {
            let next = a
                .checked_add(b)
                .and_then(|v| v.checked_add(1))
                .ok_or(LedgerError::Overflow)?;
            a = b;
            b = next;
        }
        let maximum = if self.height < 64 {
            (1u64 << self.height) - 1
        } else {
            u64::MAX
        };
        if self.count < b || self.count > maximum {
            return Err(LedgerError::Shape);
        }
        Ok(())
    }
}

impl Node {
    fn shape(&self) -> Result<(u8, u64)> {
        self.address.validate()?;
        identity(&self.key)?;
        identity(&self.value.entry_id)?;
        if self.value.ordinal == 0
            || (if self.value.entry_id == self.address.entry_id {
                self.value.ordinal != self.address.ordinal
            } else {
                self.value.ordinal >= self.address.ordinal
            })
        {
            return Err(LedgerError::ReferenceOrder);
        }
        for child in [&self.left, &self.right].into_iter().flatten() {
            child.validate()?;
            if !child.address.precedes(&self.address) {
                return Err(LedgerError::ReferenceOrder);
            }
        }
        if self
            .left
            .as_ref()
            .zip(self.right.as_ref())
            .is_some_and(|(a, b)| a.address == b.address)
        {
            return Err(LedgerError::Shape);
        }
        let lh = self.left.as_ref().map_or(0, |v| v.height);
        let rh = self.right.as_ref().map_or(0, |v| v.height);
        if lh.abs_diff(rh) > 1 {
            return Err(LedgerError::Shape);
        }
        let height = lh.max(rh).checked_add(1).ok_or(LedgerError::Overflow)?;
        if usize::from(height) > MAX_PATH_NODES {
            return Err(LedgerError::Limit);
        }
        let count = self
            .left
            .as_ref()
            .map_or(0, |v| v.count)
            .checked_add(self.right.as_ref().map_or(0, |v| v.count))
            .and_then(|v| v.checked_add(1))
            .ok_or(LedgerError::Overflow)?;
        Ok((height, count))
    }

    /// Fixed binary field order, big-endian integers, u16 byte-length strings,
    /// 0/1 option tags and a domain/version prefix. No input JSON ordering.
    /// Input decode/allocation budgets are the caller's responsibility.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.shape()?;
        fn text(out: &mut Vec<u8>, text: &str) {
            out.extend_from_slice(&(text.len() as u16).to_be_bytes());
            out.extend_from_slice(text.as_bytes());
        }
        fn address(out: &mut Vec<u8>, a: &Address) {
            text(out, &a.entry_id);
            out.extend_from_slice(&a.ordinal.to_be_bytes());
            out.extend_from_slice(&a.slot.to_be_bytes());
        }
        let mut out = DOMAIN.to_vec();
        address(&mut out, &self.address);
        text(&mut out, &self.key);
        text(&mut out, &self.value.entry_id);
        out.extend_from_slice(&self.value.ordinal.to_be_bytes());
        out.extend_from_slice(&self.value.digest);
        for child in [&self.left, &self.right] {
            if let Some(child) = child {
                out.push(1);
                address(&mut out, &child.address);
                out.extend_from_slice(&child.digest);
                out.push(child.height);
                out.extend_from_slice(&child.count.to_be_bytes());
            } else {
                out.push(0);
            }
        }
        if out.len() > MAX_NODE_BYTES {
            return Err(LedgerError::Limit);
        }
        Ok(out)
    }

    /// Construct the exact reference to this locally valid node. This does not
    /// recursively validate child bodies or turn an untrusted root into proof.
    pub fn reference(&self, hasher: &dyn NodeHasher) -> Result<NodeRef> {
        let (height, count) = self.shape()?;
        let reference = NodeRef {
            address: self.address.clone(),
            digest: hasher.digest(&self.canonical_bytes()?),
            height,
            count,
        };
        reference.validate()?;
        Ok(reference)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Need(NodeRef),
    Found(ReceiptRef),
    Absent,
    Poisoned,
}

/// No path Vec, visited set or node cache: only a requested reference, key,
/// two exclusive search bounds and visit counter. Does not own supplied nodes.
#[derive(Debug)]
pub struct Lookup {
    key: String,
    lower: Option<String>,
    upper: Option<String>,
    visited: usize,
    state: State,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LookupStep<'a> {
    NeedNode(&'a NodeRef),
    Found(&'a ReceiptRef),
    Absent,
    Poisoned,
}

impl Lookup {
    /// An empty root is Absent only when the embedding already proved this is
    /// the correct root/epoch. This type cannot authorize initial-root selection.
    pub fn new(key: &str, root: Option<NodeRef>) -> Result<Self> {
        identity(key)?;
        if let Some(root) = &root {
            root.validate()?;
        }
        Ok(Self {
            key: key.to_owned(),
            lower: None,
            upper: None,
            visited: 0,
            state: root.map_or(State::Absent, State::Need),
        })
    }
    pub fn step(&self) -> LookupStep<'_> {
        match &self.state {
            State::Need(v) => LookupStep::NeedNode(v),
            State::Found(v) => LookupStep::Found(v),
            State::Absent => LookupStep::Absent,
            State::Poisoned => LookupStep::Poisoned,
        }
    }
    pub fn visited_nodes(&self) -> usize {
        self.visited
    }

    /// Report resolver failure, never substitute None for the requested node.
    pub fn unavailable(&mut self) -> Result<()> {
        if !matches!(self.state, State::Need(_)) {
            return Err(LedgerError::Terminal);
        }
        self.state = State::Poisoned;
        Err(LedgerError::MissingNode)
    }

    /// Any failed supplied proof permanently poisons this cursor. The caller
    /// may explicitly start a fresh lookup, but cannot continue a partial proof.
    pub fn provide(&mut self, node: &Node, hasher: &dyn NodeHasher) -> Result<()> {
        if !matches!(self.state, State::Need(_)) {
            return Err(LedgerError::Terminal);
        }
        let State::Need(expected) = std::mem::replace(&mut self.state, State::Poisoned) else {
            unreachable!()
        };
        if self.visited >= MAX_PATH_NODES {
            return Err(LedgerError::Limit);
        }
        if node.reference(hasher)? != expected {
            return Err(LedgerError::Proof);
        }
        if self.lower.as_ref().is_some_and(|lo| node.key <= *lo)
            || self.upper.as_ref().is_some_and(|hi| node.key >= *hi)
        {
            return Err(LedgerError::KeyOrder);
        }
        self.visited += 1;
        self.state = match self.key.cmp(&node.key) {
            Ordering::Equal => State::Found(node.value.clone()),
            Ordering::Less => {
                self.upper = Some(node.key.clone());
                node.left.clone().map_or(State::Absent, State::Need)
            }
            Ordering::Greater => {
                self.lower = Some(node.key.clone());
                node.right.clone().map_or(State::Absent, State::Need)
            }
        };
        Ok(())
    }
}
