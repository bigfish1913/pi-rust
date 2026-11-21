//! Optional, loss-detecting observation of execution leases, separate from
//! durable RunEnd callbacks and per-attempt AgentEvents. No transport types.
use crate::agent_harness::{CompactionOutcome, HarnessRunOutcome};
use rpi_agent::{AgentEmitter, AgentEvent};
use serde_json::Value;
use std::{
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct ExecutionIdentity {
    pub execution_id: String,
    pub request_id: Option<Value>,
    pub lane: String,
}

/// A caller-scoped correlation, captured at admission, never inferred from a
/// session's most recently accepted request. Spawned plugin tasks do not inherit it.
#[derive(Clone)]
pub struct ExecutionRequest {
    id: Value,
    last: Arc<Mutex<Option<ExecutionIdentity>>>,
}
tokio::task_local! { static REQUEST: ExecutionRequest; }
impl ExecutionRequest {
    pub fn new(id: Value) -> Self {
        Self {
            id,
            last: Default::default(),
        }
    }
    pub async fn scope<F: Future>(&self, future: F) -> F::Output {
        REQUEST.scope(self.clone(), future).await
    }
    pub fn last_execution(&self) -> Option<ExecutionIdentity> {
        self.last.lock().unwrap().clone()
    }
}

#[derive(Clone, Debug)]
pub enum ExecutionOutcome {
    Run(HarnessRunOutcome),
    Compact(CompactionOutcome),
    Skipped,
}
pub enum ExecutionEvent {
    // Admission precedes durable recovery/poll lookup; the durable run may not
    // be known yet. Never advertise the provisional lease ID as that run.
    Started {
        identity: ExecutionIdentity,
    },
    Progress {
        identity: ExecutionIdentity,
        run_id: String,
        event: AgentEvent,
    },
    Released {
        identity: ExecutionIdentity,
        run_id: String,
        outcome: Option<ExecutionOutcome>,
        recovery_required: bool,
        pending_resume: bool,
    },
    SessionProgress(AgentEvent),
    Barrier(oneshot::Sender<()>),
}

// Progress holds a separate budget until it leaves the queue. Two slots remain
// available for Started/Released even when a synchronous tool floods updates.
struct QueuedEvent {
    event: ExecutionEvent,
    _progress: Option<OwnedSemaphorePermit>,
}

pub struct ExecutionReceiver(mpsc::Receiver<QueuedEvent>);
impl ExecutionReceiver {
    pub async fn recv(&mut self) -> Option<ExecutionEvent> {
        self.0.recv().await.map(|queued| queued.event)
    }
    pub fn try_recv(&mut self) -> Result<ExecutionEvent, mpsc::error::TryRecvError> {
        self.0.try_recv().map(|queued| queued.event)
    }
}

#[derive(Clone)]
pub struct ExecutionObserver {
    sender: mpsc::Sender<QueuedEvent>,
    progress: Arc<Semaphore>,
    dropped_progress: Arc<AtomicU64>,
    failed: CancellationToken,
}
impl ExecutionObserver {
    pub fn channel(capacity: usize, failed: CancellationToken) -> (Self, ExecutionReceiver) {
        assert!(
            capacity >= 3,
            "execution queue needs two lifecycle slots plus progress"
        );
        let (sender, receiver) = mpsc::channel(capacity);
        (
            Self {
                sender,
                progress: Arc::new(Semaphore::new(capacity.saturating_sub(2))),
                dropped_progress: Default::default(),
                failed,
            },
            ExecutionReceiver(receiver),
        )
    }
    /// Only synchronous tool partials can be dropped: ToolExecutionEnd carries
    /// their complete result. Assistant deltas and message boundaries are not
    /// disposable (compact-delta clients cannot reconstruct a missing delta).
    pub fn dropped_progress(&self) -> u64 {
        self.dropped_progress.load(Ordering::Relaxed)
    }
    fn is_progress(event: &ExecutionEvent) -> bool {
        matches!(
            event,
            ExecutionEvent::Progress { .. } | ExecutionEvent::SessionProgress(_)
        )
    }
    fn is_tool_partial(event: &ExecutionEvent) -> bool {
        matches!(
            event,
            ExecutionEvent::Progress {
                event: AgentEvent::ToolExecutionUpdate { .. },
                ..
            } | ExecutionEvent::SessionProgress(AgentEvent::ToolExecutionUpdate { .. })
        )
    }
    // Admission/release and tool on_update cannot await. Lifecycle loss and
    // reader loss are transport failures; tool-partial congestion is not.
    pub(crate) fn publish(&self, event: ExecutionEvent) {
        let progress = if Self::is_progress(&event) {
            match self.progress.clone().try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) if Self::is_tool_partial(&event) && !self.sender.is_closed() => {
                    self.dropped_progress.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Err(_) => {
                    self.failed.cancel();
                    return;
                }
            }
        } else {
            None
        };
        if self
            .sender
            .try_send(QueuedEvent {
                event,
                _progress: progress,
            })
            .is_err()
        {
            self.failed.cancel();
        }
    }
    async fn send(&self, event: ExecutionEvent) {
        let progress = if Self::is_progress(&event) {
            tokio::select! {
                biased;
                _ = self.failed.cancelled() => return,
                _ = self.sender.closed() => { self.failed.cancel(); return; },
                permit = self.progress.clone().acquire_owned() => Some(permit.expect("progress budget stays open")),
            }
        } else {
            None
        };
        tokio::select! {
            biased;
            _ = self.failed.cancelled() => {},
            sent = self.sender.send(QueuedEvent { event, _progress: progress }) => {
                if sent.is_err() { self.failed.cancel(); }
            }
        }
    }
    pub async fn flush(&self) {
        let (done, drained) = oneshot::channel();
        self.send(ExecutionEvent::Barrier(done)).await;
        tokio::select! {
            _ = self.failed.cancelled() => {},
            result = drained => { if result.is_err() { self.failed.cancel(); } }
        }
    }
    pub(crate) fn session_emitter(&self, inner: Arc<dyn AgentEmitter>) -> Arc<dyn AgentEmitter> {
        Arc::new(ObservedEmitter {
            inner,
            observer: self.clone(),
            identity: None,
            run_id: String::new(),
        })
    }
}

pub(crate) struct ExecutionLease {
    pub identity: ExecutionIdentity,
    pub observer: ExecutionObserver,
    pub outcome: Option<ExecutionOutcome>,
    pub finalized: bool,
    // Set BEFORE attempting durable intent: a failed append may have landed.
    pub durable_intent: bool,
}
impl ExecutionLease {
    pub fn new(observer: ExecutionObserver, lane: &str) -> Self {
        let request = REQUEST.try_with(Clone::clone).ok();
        let identity = ExecutionIdentity {
            execution_id: uuid::Uuid::new_v4().to_string(),
            request_id: request.as_ref().map(|r| r.id.clone()),
            lane: lane.to_string(),
        };
        if let Some(request) = request {
            *request.last.lock().unwrap() = Some(identity.clone());
        }
        observer.publish(ExecutionEvent::Started {
            identity: identity.clone(),
        });
        Self {
            identity,
            observer,
            outcome: None,
            finalized: false,
            durable_intent: false,
        }
    }
    pub fn emitter(&self, inner: Arc<dyn AgentEmitter>, run_id: &str) -> Arc<dyn AgentEmitter> {
        Arc::new(ObservedEmitter {
            inner,
            observer: self.observer.clone(),
            identity: Some(self.identity.clone()),
            run_id: run_id.into(),
        })
    }
    pub fn release(self, run_id: String, pending_resume: bool) {
        self.observer.publish(ExecutionEvent::Released {
            identity: self.identity,
            run_id,
            outcome: self.outcome,
            recovery_required: self.durable_intent && !self.finalized,
            pending_resume,
        });
    }
}
struct ObservedEmitter {
    inner: Arc<dyn AgentEmitter>,
    observer: ExecutionObserver,
    identity: Option<ExecutionIdentity>,
    run_id: String,
}
impl ObservedEmitter {
    fn progress(&self, event: AgentEvent) -> ExecutionEvent {
        match &self.identity {
            Some(identity) => ExecutionEvent::Progress {
                identity: identity.clone(),
                run_id: self.run_id.clone(),
                event,
            },
            None => ExecutionEvent::SessionProgress(event),
        }
    }
}
impl AgentEmitter for ObservedEmitter {
    fn tool_program_observer(
        &self,
    ) -> Option<Arc<dyn rpi_agent::tool_program_observation::ToolProgramObserver>> {
        self.inner.tool_program_observer()
    }
    fn emit(&self, event: AgentEvent) -> futures::future::BoxFuture<'static, ()> {
        let observed = self.progress(event.clone());
        let inner = self.inner.emit(event);
        let observer = self.observer.clone();
        Box::pin(async move {
            inner.await;
            observer.send(observed).await;
        })
    }
    fn emit_injected(&self, event: AgentEvent) -> futures::future::BoxFuture<'static, ()> {
        let observed = self.progress(event.clone());
        let inner = self.inner.emit_injected(event);
        let observer = self.observer.clone();
        Box::pin(async move {
            inner.await;
            observer.send(observed).await;
        })
    }
    fn try_emit(&self, event: AgentEvent) {
        self.observer.publish(self.progress(event.clone()));
        self.inner.try_emit(event);
    }
}

#[cfg(test)]
mod tests;
