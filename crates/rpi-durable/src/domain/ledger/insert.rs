//! Pure copy-on-write AVL insertion prerequisite; not a writer or journal.
//!
//! The embedding reserves `value.entry_id`/`value.ordinal` as a globally unique
//! new transaction identity, supplies a trusted root and hash algorithm, and
//! resolves the nodes requested by `step`. Only the search path is proved by
//! the unchanged lookup validator; unvisited bodies, receipts and root/epoch
//! selection remain the caller's responsibility. There is no I/O or node cache.
//!
//! A batch contains every staged node, including intermediate nodes made
//! unreachable by rotations. Slots are never renumbered: there is deliberately
//! no minimal-batch guarantee. Publication, persistence and reservation of the
//! transaction identity are outside this module.

use super::{
    identity, Address, LedgerError, Lookup, LookupStep, Node, NodeHasher, NodeRef, ReceiptRef,
    MAX_PATH_NODES,
};
use std::cmp::Ordering;

pub const MAX_CREATED_NODES: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    pub root: NodeRef,
    pub nodes: Vec<Node>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InsertError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("ledger insertion key already exists")]
    Duplicate,
    #[error("ledger insertion is terminal")]
    Terminal,
}

type Result<T> = std::result::Result<T, InsertError>;

#[derive(Debug)]
enum State {
    Searching,
    Ready(Batch),
    Poisoned,
    Taken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertStep<'a> {
    NeedNode(&'a NodeRef),
    Ready(&'a Batch),
    Poisoned,
    Taken,
}

/// Retains at most `MAX_PATH_NODES` validated path bodies. Newly created nodes
/// are private until the entire rebuild succeeds; any proof or rebuild error
/// permanently poisons the insertion and cannot expose a partial batch.
#[derive(Debug)]
pub struct Insert {
    key: String,
    value: ReceiptRef,
    lookup: Lookup,
    path: Vec<Node>,
    expected_count: u64,
    state: State,
}

impl Insert {
    /// `value` is both the new receipt reference and the reserved transaction
    /// identity for all new node addresses. Even an empty tree requires a valid
    /// identity and positive ordinal. A nonempty root must belong to a different
    /// entry and have a strictly smaller ordinal.
    pub fn new(
        key: &str,
        value: ReceiptRef,
        root: Option<NodeRef>,
        hasher: &dyn NodeHasher,
    ) -> Result<Self> {
        // Validate bounded input before copying strings or retaining path nodes.
        identity(key)?;
        identity(&value.entry_id)?;
        if value.ordinal == 0 {
            return Err(LedgerError::ReferenceOrder.into());
        }
        if let Some(root) = &root {
            root.validate()?;
            if root.address.entry_id == value.entry_id || root.address.ordinal >= value.ordinal {
                return Err(LedgerError::ReferenceOrder.into());
            }
        }
        let expected_count = root
            .as_ref()
            .map_or(0, |root| root.count)
            .checked_add(1)
            .ok_or(LedgerError::Overflow)?;
        let lookup = Lookup::new(key, root)?;
        let empty = matches!(lookup.step(), LookupStep::Absent);
        let mut insert = Self {
            key: key.to_owned(),
            value,
            lookup,
            path: Vec::new(),
            expected_count,
            state: State::Searching,
        };
        if empty {
            insert.state = State::Poisoned;
            let batch = insert.rebuild(hasher)?;
            insert.state = State::Ready(batch);
        }
        Ok(insert)
    }

    pub fn step(&self) -> InsertStep<'_> {
        match &self.state {
            State::Searching => match self.lookup.step() {
                LookupStep::NeedNode(reference) => InsertStep::NeedNode(reference),
                // Searching is restored only when Lookup requests another node.
                _ => unreachable!("insertion search state invariant"),
            },
            State::Ready(batch) => InsertStep::Ready(batch),
            State::Poisoned => InsertStep::Poisoned,
            State::Taken => InsertStep::Taken,
        }
    }

    pub fn retained_path_nodes(&self) -> usize {
        self.path.len()
    }

    /// Use the same trusted hash algorithm as in `new` for every supplied body.
    /// Duplicate keys are errors even when their receipt references are equal.
    /// Wrong-state calls return Terminal without destroying a ready batch.
    pub fn provide(&mut self, node: &Node, hasher: &dyn NodeHasher) -> Result<()> {
        if !matches!(self.state, State::Searching) {
            return Err(InsertError::Terminal);
        }
        self.state = State::Poisoned;
        if self.path.len() >= MAX_PATH_NODES {
            return Err(LedgerError::Limit.into());
        }
        // Lookup validates identities, sizes, local shape, reference equality
        // and search bounds before we clone any part of the supplied body.
        self.lookup.provide(node, hasher)?;
        match self.lookup.step() {
            LookupStep::Found(_) => Err(InsertError::Duplicate),
            LookupStep::NeedNode(_) => {
                self.path.push(node.clone());
                self.state = State::Searching;
                Ok(())
            }
            LookupStep::Absent => {
                self.path.push(node.clone());
                let batch = self.rebuild(hasher)?;
                self.state = State::Ready(batch);
                Ok(())
            }
            LookupStep::Poisoned => Err(LedgerError::Proof.into()),
        }
    }

    /// Resolver failure is never absence. Only a pending request may be marked
    /// unavailable; terminal calls leave Ready/Taken unchanged.
    pub fn unavailable(&mut self) -> Result<()> {
        if !matches!(self.state, State::Searching) {
            return Err(InsertError::Terminal);
        }
        self.state = State::Poisoned;
        self.lookup.unavailable()?;
        unreachable!("lookup unavailable always fails for a pending request")
    }

    /// Transfer a completed batch exactly once, without altering other states.
    pub fn take_batch(&mut self) -> Result<Batch> {
        if !matches!(self.state, State::Ready(_)) {
            return Err(InsertError::Terminal);
        }
        let State::Ready(batch) = std::mem::replace(&mut self.state, State::Taken) else {
            unreachable!()
        };
        Ok(batch)
    }

    fn rebuild(&mut self, hasher: &dyn NodeHasher) -> Result<Batch> {
        let mut builder = Builder {
            entry_id: &self.value.entry_id,
            ordinal: self.value.ordinal,
            hasher,
            nodes: Vec::new(),
        };
        let mut root = builder.stage(self.key.clone(), self.value.clone(), None, None)?;
        while let Some(mut old) = self.path.pop() {
            match self.key.cmp(&old.key) {
                Ordering::Less => old.left = Some(root),
                Ordering::Greater => old.right = Some(root),
                Ordering::Equal => return Err(InsertError::Duplicate),
            }
            // This temporary child arrangement may have balance +/-2. Never
            // hash or encode it; rebalance stages only valid AVL nodes.
            root = builder.rebalance(old)?;
        }
        if root.count != self.expected_count {
            return Err(LedgerError::Shape.into());
        }
        Ok(Batch {
            root,
            nodes: builder.nodes,
        })
    }
}

struct Builder<'a> {
    entry_id: &'a str,
    ordinal: u64,
    hasher: &'a dyn NodeHasher,
    nodes: Vec<Node>,
}

fn height(reference: &Option<NodeRef>) -> i16 {
    reference
        .as_ref()
        .map_or(0, |reference| i16::from(reference.height))
}

impl Builder<'_> {
    fn stage(
        &mut self,
        key: String,
        value: ReceiptRef,
        left: Option<NodeRef>,
        right: Option<NodeRef>,
    ) -> Result<NodeRef> {
        if self.nodes.len() >= MAX_CREATED_NODES {
            return Err(LedgerError::Limit.into());
        }
        let slot = u16::try_from(self.nodes.len()).map_err(|_| LedgerError::Limit)?;
        let node = Node {
            address: Address {
                entry_id: self.entry_id.to_owned(),
                ordinal: self.ordinal,
                slot,
            },
            key,
            value,
            left,
            right,
        };
        // Enforces AVL shape, count arithmetic, encoding bounds, and older
        // ordinals or strictly smaller same-transaction child slots.
        let reference = node.reference(self.hasher)?;
        self.nodes.push(node);
        Ok(reference)
    }

    /// Rotation pivots are newly rebuilt path nodes, never an old-body fetch.
    /// Slots index this bounded staging vector, not an independent authority.
    fn staged(&self, reference: &Option<NodeRef>) -> Result<Node> {
        let reference = reference.as_ref().ok_or(LedgerError::Shape)?;
        if reference.address.entry_id != self.entry_id || reference.address.ordinal != self.ordinal
        {
            return Err(LedgerError::ReferenceOrder.into());
        }
        let node = self
            .nodes
            .get(usize::from(reference.address.slot))
            .ok_or(LedgerError::Shape)?;
        if node.address != reference.address {
            return Err(LedgerError::ReferenceOrder.into());
        }
        Ok(node.clone())
    }

    fn rebalance(&mut self, node: Node) -> Result<NodeRef> {
        match height(&node.left) - height(&node.right) {
            -1..=1 => self.stage(node.key, node.value, node.left, node.right),
            2 => {
                let left = self.staged(&node.left)?;
                if height(&left.left) >= height(&left.right) {
                    // LL: the old parent becomes the new right child.
                    let right = self.stage(node.key, node.value, left.right, node.right)?;
                    self.stage(left.key, left.value, left.left, Some(right))
                } else {
                    // LR: both the heavy child and inner pivot are staged.
                    let middle = self.staged(&left.right)?;
                    let new_left = self.stage(left.key, left.value, left.left, middle.left)?;
                    let new_right = self.stage(node.key, node.value, middle.right, node.right)?;
                    self.stage(middle.key, middle.value, Some(new_left), Some(new_right))
                }
            }
            -2 => {
                let right = self.staged(&node.right)?;
                if height(&right.right) >= height(&right.left) {
                    // RR: the old parent becomes the new left child.
                    let left = self.stage(node.key, node.value, node.left, right.left)?;
                    self.stage(right.key, right.value, Some(left), right.right)
                } else {
                    // RL: both the heavy child and inner pivot are staged.
                    let middle = self.staged(&right.left)?;
                    let new_left = self.stage(node.key, node.value, node.left, middle.left)?;
                    let new_right =
                        self.stage(right.key, right.value, middle.right, right.right)?;
                    self.stage(middle.key, middle.value, Some(new_left), Some(new_right))
                }
            }
            _ => Err(LedgerError::Shape.into()),
        }
    }
}
