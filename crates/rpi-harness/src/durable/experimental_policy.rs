//! Explicit, immutable opt-in policy for the experimental v2 harness mode.
//! Epochs and owner authority come only from host configuration, never receipts.
use super::{
    experimental_ledger::{control::TransitionPolicy, ReadPolicy},
    validate_identity, DurableActor, DurableError,
};
use std::collections::HashSet;

#[derive(Clone, Debug)]
pub struct ExperimentalDurablePolicy {
    epoch_id: String,
    owners: Vec<String>,
}

impl ExperimentalDurablePolicy {
    pub fn new(epoch_id: impl Into<String>, owners: Vec<String>) -> Result<Self, DurableError> {
        let epoch_id = epoch_id.into();
        validate_identity(&epoch_id)?;
        if owners.is_empty() || owners.len() > 256 {
            return Err(DurableError::InvalidRequest(
                "experimental policy requires 1..256 distinct owners".into(),
            ));
        }
        let mut seen = HashSet::new();
        for owner in &owners {
            validate_identity(owner)?;
            if !seen.insert(owner.as_str()) {
                return Err(DurableError::InvalidRequest(
                    "experimental policy requires distinct owners".into(),
                ));
            }
        }
        Ok(Self { epoch_id, owners })
    }

    pub fn epoch_id(&self) -> &str {
        &self.epoch_id
    }

    pub(crate) fn read_policy(
        &self,
        session_id: &str,
        lane: &str,
    ) -> Result<ReadPolicy, DurableError> {
        ReadPolicy::new_epoch(session_id, lane, &self.epoch_id)
            .map_err(|_| DurableError::InvalidJournal("invalid experimental policy binding".into()))
    }

    fn is_owner(&self, id: &str) -> bool {
        self.owners.iter().any(|owner| owner == id)
    }

    /// The trusted live actor must agree EXACTLY with configured membership.
    /// In particular, supplying a configured owner as a child is not accepted.
    pub(crate) fn check_actor(&self, actor: &DurableActor) -> Result<(), DurableError> {
        validate_identity(actor.id())?;
        if actor.may_replace_workflow() != self.is_owner(actor.id()) {
            return Err(DurableError::PermissionDenied);
        }
        Ok(())
    }

    /// Call only with IDs obtained from the actual, locally read source receipts
    /// (plus the prospective live actor for append preflight). Unconfigured IDs
    /// get child authority, regardless of historical actions or claimed_by IDs.
    /// Do not union every configured owner into the bounded replay roster.
    pub(crate) fn transition_policy<'a>(
        &self,
        read: ReadPolicy,
        receipt_actors: impl IntoIterator<Item = &'a str>,
    ) -> Result<TransitionPolicy, DurableError> {
        let mut seen = HashSet::new();
        let mut actors = Vec::new();
        for id in receipt_actors {
            validate_identity(id).map_err(|_| {
                DurableError::InvalidJournal("invalid experimental receipt actor".into())
            })?;
            if seen.insert(id) {
                if actors.len() == 256 {
                    return Err(DurableError::LimitExceeded);
                }
                actors.push(if self.is_owner(id) {
                    DurableActor::owner(id)?
                } else {
                    DurableActor::child(id)?
                });
            }
        }
        if actors.is_empty() {
            // TransitionPolicy requires a nonempty roster even for empty input.
            actors.push(DurableActor::owner(self.owners[0].clone())?);
        }
        TransitionPolicy::new(read, actors).map_err(|_| {
            DurableError::InvalidJournal("invalid experimental transition policy".into())
        })
    }
}
