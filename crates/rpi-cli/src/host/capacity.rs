use super::{AgentId, HostError, HostResult};
use serde::Serialize;
use std::{
    collections::HashSet,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

#[derive(Debug)]
struct State {
    limit: Option<NonZeroUsize>,
    resident: HashSet<AgentId>,
}

/// One short synchronous critical section covers duplicate checks, capacity
/// checks and reservation. No I/O or awaits occur while holding this lock.
#[derive(Clone, Debug)]
pub struct Capacity(Arc<Mutex<State>>);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct CapacitySnapshot {
    pub limit: Option<usize>,
    pub resident: usize,
    pub available: Option<usize>,
}

impl Capacity {
    pub fn new(limit: Option<NonZeroUsize>) -> Self {
        Self(Arc::new(Mutex::new(State {
            limit,
            resident: HashSet::new(),
        })))
    }

    pub fn try_reserve(&self, agent_id: AgentId) -> HostResult<ResidentPermit> {
        let mut state = self.0.lock().map_err(|_| HostError::Poisoned)?;
        if state.resident.contains(&agent_id) {
            return Err(HostError::AlreadyResident);
        }
        if let Some(limit) = state.limit {
            if state.resident.len() >= limit.get() {
                return Err(HostError::AtCapacity { limit: limit.get() });
            }
        }
        state.resident.insert(agent_id);
        Ok(ResidentPermit {
            capacity: self.clone(),
            agent_id,
        })
    }

    /// Lowering a limit blocks new reservations; it never evicts existing Agents.
    pub fn set_limit(&self, limit: Option<NonZeroUsize>) -> HostResult<()> {
        self.0.lock().map_err(|_| HostError::Poisoned)?.limit = limit;
        Ok(())
    }

    pub fn snapshot(&self) -> HostResult<CapacitySnapshot> {
        let state = self.0.lock().map_err(|_| HostError::Poisoned)?;
        let limit = state.limit.map(NonZeroUsize::get);
        Ok(CapacitySnapshot {
            limit,
            resident: state.resident.len(),
            available: limit.map(|limit| limit.saturating_sub(state.resident.len())),
        })
    }
}

/// Held by the supervised backend owner through startup, execution, cancellation
/// and final cleanup, NOT by a transient client request or foreground run future.
/// Dropping it frees a catalog slot; it does not cancel or stop any actual backend.
#[derive(Debug)]
#[must_use = "retain the permit for the complete backend residency"]
pub struct ResidentPermit {
    capacity: Capacity,
    agent_id: AgentId,
}

impl ResidentPermit {
    pub fn agent_id(&self) -> AgentId {
        self.agent_id
    }
}

impl Drop for ResidentPermit {
    fn drop(&mut self) {
        // Fail-closed public operations still report poisoning. Drop must not
        // panic during another unwind or leave this guard's entry behind.
        self.capacity
            .0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .resident
            .remove(&self.agent_id);
    }
}
