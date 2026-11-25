//! Session-free native owner. Results are bounded, volatile and incarnation
//! scoped; no admission journal, Harness, Session, writer or replay exists here.
//! Only HostService owns this backend. A client is a weak observer, never an
//! abort handle for the supervised prompt future or a second owner registry.

use super::{
    bindings::{NativeBindingProfile, NativeMiniAgent},
    ordinary::{BackendError, BackendResult},
    AgentId, AgentMode, AgentRecord, AgentState, CatalogGuard, HostCatalog, HostEpoch,
    NativeExtensionCustody, RequestId, RunId,
};
use futures::FutureExt;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
};
use tokio::sync::{watch, Mutex};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
pub struct MiniLimits {
    pub max_input_bytes: usize,
    pub max_results: usize,
    pub max_output_bytes: usize,
}
impl Default for MiniLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 256 * 1024,
            max_results: 64,
            max_output_bytes: 1024 * 1024,
        }
    }
}

/// A volatile identity, NOT a durable admission/receipt or execution success.
/// The random run ID also fences reuse of an Agent identity within one epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MiniReceipt {
    pub epoch: HostEpoch,
    pub agent_id: AgentId,
    pub request_id: RequestId,
    pub run_id: RunId,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MiniOutcome {
    Running,
    Succeeded,
    Failed,
    Aborted,
    OutputLimit,
    Faulted,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MiniResult {
    pub receipt: MiniReceipt,
    pub outcome: MiniOutcome,
    /// Exact final assistant text. Oversized results are explicitly OutputLimit,
    /// never truncated or silently evicted. Provider diagnostics stay private.
    pub answer: String,
}
struct Entry {
    digest: [u8; 32],
    result: MiniResult,
}
struct Active {
    receipt: MiniReceipt,
    cancel: CancellationToken,
}
struct Control {
    record: AgentRecord,
    results: BTreeMap<RequestId, Entry>,
    active: Option<Active>,
    output_bytes: usize,
    closing: bool,
    faulted: bool,
    closed: bool,
}
struct Backend {
    native: NativeMiniAgent,
    catalog: HostCatalog,
    limits: MiniLimits,
    stopping: Arc<AtomicBool>,
    control: Mutex<Control>,
    close_gate: Mutex<()>,
    changed: watch::Sender<()>,
}
pub(super) struct MiniAgent {
    inner: Arc<Backend>,
}
#[derive(Clone)]
pub struct MiniClient {
    inner: Weak<Backend>,
}

impl MiniAgent {
    pub(super) async fn start(
        catalog: HostCatalog,
        id: AgentId,
        guard: CatalogGuard,
        profile: NativeBindingProfile,
        limits: MiniLimits,
        custody: Arc<NativeExtensionCustody>,
        stopping: Arc<AtomicBool>,
    ) -> BackendResult<Self> {
        if limits.max_input_bytes == 0
            || limits.max_input_bytes > 8 * 1024 * 1024
            || limits.max_results == 0
            || limits.max_results > 64
            || limits.max_output_bytes == 0
            || limits.max_output_bytes > 1024 * 1024
        {
            return Err(BackendError::Binding("invalid Mini budgets"));
        }
        let record = catalog.get(id).await?;
        if record.spec.mode != AgentMode::Mini || record.spec.session.is_some() {
            return Err(BackendError::Binding(
                "Mini requires explicit Session-free metadata",
            ));
        }
        let record = catalog.transition(id, guard, AgentState::Starting).await?;
        // Service has already reserved Starting and retained this exact custody.
        // Any error/panic leaves reconciliation with that same service entry.
        let native = profile
            .bind_mini_agent(record.spec.clone(), catalog.clone(), custody)
            .await?;
        let record = catalog
            .transition(id, catalog.guard(&record), AgentState::Idle)
            .await?;
        let (changed, _) = watch::channel(());
        Ok(Self {
            inner: Arc::new(Backend {
                native,
                catalog,
                limits,
                stopping,
                control: Mutex::new(Control {
                    record,
                    results: BTreeMap::new(),
                    active: None,
                    output_bytes: 0,
                    closing: false,
                    faulted: false,
                    closed: false,
                }),
                close_gate: Mutex::new(()),
                changed,
            }),
        })
    }
    pub(super) fn client(&self) -> MiniClient {
        MiniClient {
            inner: Arc::downgrade(&self.inner),
        }
    }
    pub(super) async fn close(&self) -> BackendResult<()> {
        self.inner.close().await
    }
}
impl MiniClient {
    fn upgrade(&self) -> BackendResult<Arc<Backend>> {
        self.inner.upgrade().ok_or(BackendError::Closed)
    }
    pub async fn submit(
        &self,
        epoch: HostEpoch,
        request_id: RequestId,
        prompt: String,
    ) -> BackendResult<MiniReceipt> {
        let inner = self.upgrade()?;
        owned(async move { inner.submit(epoch, request_id, prompt).await }).await
    }
    /// Lookup only within this already-owned incarnation; never loads or
    /// replays input when an admission reply was lost.
    pub async fn request(
        &self,
        epoch: HostEpoch,
        agent_id: AgentId,
        request_id: RequestId,
    ) -> BackendResult<Option<MiniResult>> {
        let inner = self.upgrade()?;
        let control = inner.control.lock().await;
        inner.validate(&control, epoch, agent_id)?;
        Ok(control
            .results
            .get(&request_id)
            .map(|entry| entry.result.clone()))
    }
    pub async fn result(&self, receipt: MiniReceipt) -> BackendResult<Option<MiniResult>> {
        let inner = self.upgrade()?;
        let control = inner.control.lock().await;
        inner.validate(&control, receipt.epoch, receipt.agent_id)?;
        Ok(control
            .results
            .get(&receipt.request_id)
            .filter(|entry| entry.result.receipt == receipt)
            .map(|entry| entry.result.clone()))
    }
    /// Only signal this exact volatile run. Neither true nor cancellation of a
    /// query proves settlement; late callbacks still belong to the owner.
    pub async fn cancel(&self, receipt: MiniReceipt) -> BackendResult<bool> {
        let inner = self.upgrade()?;
        let control = inner.control.lock().await;
        inner.validate(&control, receipt.epoch, receipt.agent_id)?;
        match &control.active {
            Some(active) if active.receipt == receipt => {
                active.cancel.cancel();
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}
impl Backend {
    fn validate(&self, control: &Control, epoch: HostEpoch, id: AgentId) -> BackendResult<()> {
        if epoch != self.catalog.epoch() || id != control.record.spec.id {
            return Err(BackendError::Binding("Mini owner identity mismatch"));
        }
        Ok(())
    }
    async fn submit(
        self: Arc<Self>,
        epoch: HostEpoch,
        request_id: RequestId,
        prompt: String,
    ) -> BackendResult<MiniReceipt> {
        let mut control = self.control.lock().await;
        self.validate(&control, epoch, control.record.spec.id)?;
        if control.closing || control.closed || self.stopping.load(Ordering::Acquire) {
            return Err(BackendError::Closed);
        }
        if control.faulted {
            return Err(BackendError::Faulted);
        }
        if prompt.trim().is_empty() || prompt.len() > self.limits.max_input_bytes {
            return Err(BackendError::InputTooLarge);
        }
        let digest: [u8; 32] = Sha256::digest(prompt.as_bytes()).into();
        if let Some(entry) = control.results.get(&request_id) {
            return if entry.digest == digest {
                Ok(entry.result.receipt)
            } else {
                Err(BackendError::Binding("Mini request intent conflict"))
            };
        }
        if control.active.is_some() {
            return Err(BackendError::Busy);
        }
        if control.results.len() >= self.limits.max_results {
            return Err(BackendError::Binding("Mini result capacity exhausted"));
        }
        control.record = self
            .catalog
            .transition(
                control.record.spec.id,
                self.catalog.guard(&control.record),
                AgentState::Running,
            )
            .await?;
        let receipt = MiniReceipt {
            epoch,
            agent_id: control.record.spec.id,
            request_id,
            run_id: RunId::new(),
        };
        let cancel = CancellationToken::new();
        control.active = Some(Active {
            receipt,
            cancel: cancel.clone(),
        });
        control.results.insert(
            request_id,
            Entry {
                digest,
                result: MiniResult {
                    receipt,
                    outcome: MiniOutcome::Running,
                    answer: String::new(),
                },
            },
        );
        // No await between publishing Active and transferring the entire prompt
        // future to its owner. Caller cancellation can only lose the response.
        let owner = self.clone();
        tokio::spawn(async move {
            owner.run(receipt, prompt, cancel).await;
        });
        Ok(receipt)
    }
    async fn run(self: Arc<Self>, receipt: MiniReceipt, prompt: String, cancel: CancellationToken) {
        let completed = AssertUnwindSafe(async {
            let first_message = self.native.agent().state().messages.len();
            let prompt = self.native.agent().prompt(prompt);
            tokio::pin!(prompt);
            let result = tokio::select! {
                // Poll native start BEFORE observing a pre-existing cancel. An
                // abort before start_active_run would miss the new run entirely.
                biased;
                result = &mut prompt => result,
                _ = cancel.cancelled() => { self.native.cancel(); prompt.await }
            };
            let state = self.native.agent().state();
            let last = state.messages[first_message..].iter().rev().find_map(|m| {
                if let rpi_agent::AgentMessage::Assistant(m) = m {
                    Some(m)
                } else {
                    None
                }
            });
            let outcome = match (&result, last.map(|m| m.stop_reason)) {
                (Err(rpi_agent::AgentError::Abort), _) | (_, Some(rpi_ai::StopReason::Aborted)) => {
                    MiniOutcome::Aborted
                }
                (Err(_), _) | (_, Some(rpi_ai::StopReason::Error)) => MiniOutcome::Failed,
                (Ok(()), Some(rpi_ai::StopReason::Stop | rpi_ai::StopReason::Length))
                    if state.error_message.is_none() =>
                {
                    MiniOutcome::Succeeded
                }
                _ => MiniOutcome::Failed,
            };
            let mut answer = String::new();
            if let Some(last) = last {
                for block in &last.content {
                    if let rpi_ai::Content::Text(text) = block {
                        if answer.len().saturating_add(text.text.len())
                            > self.limits.max_output_bytes
                        {
                            return (MiniOutcome::OutputLimit, String::new());
                        }
                        answer.push_str(&text.text);
                    }
                }
            }
            (outcome, answer)
        })
        .catch_unwind()
        .await;
        let mut control = self.control.lock().await;
        let (mut outcome, mut answer) = completed.unwrap_or_else(|_| {
            control.faulted = true;
            (MiniOutcome::Faulted, String::new())
        });
        if !control.faulted {
            match self
                .catalog
                .transition(
                    receipt.agent_id,
                    self.catalog.guard(&control.record),
                    AgentState::Idle,
                )
                .await
            {
                Ok(record) => control.record = record,
                Err(_) => {
                    control.faulted = true;
                    outcome = MiniOutcome::Faulted;
                    answer.clear();
                }
            }
        }
        if control.output_bytes.saturating_add(answer.len()) > self.limits.max_output_bytes {
            outcome = MiniOutcome::OutputLimit;
            answer.clear();
        }
        control.output_bytes += answer.len();
        if let Some(entry) = control.results.get_mut(&receipt.request_id) {
            entry.result.outcome = outcome;
            entry.result.answer = answer;
        }
        control.active = None;
        self.changed.send_replace(());
    }
    async fn close(&self) -> BackendResult<()> {
        let _close = self.close_gate.lock().await;
        let mut changed = self.changed.subscribe();
        loop {
            let mut control = self.control.lock().await;
            control.closing = true;
            if control.closed {
                return Ok(());
            }
            if let Some(active) = &control.active {
                active.cancel.cancel();
                drop(control);
                changed
                    .changed()
                    .await
                    .map_err(|_| BackendError::TaskStopped)?;
                continue;
            }
            // A panic is not native settlement, even if the request task exited.
            if control.faulted {
                return Err(BackendError::Faulted);
            }
            if control.record.state != AgentState::Closing {
                control.record = self
                    .catalog
                    .transition(
                        control.record.spec.id,
                        self.catalog.guard(&control.record),
                        AgentState::Closing,
                    )
                    .await?;
            }
            drop(control);
            self.native.close().await?;
            let mut control = self.control.lock().await;
            control.record = self
                .catalog
                .transition(
                    control.record.spec.id,
                    self.catalog.guard(&control.record),
                    AgentState::Lost,
                )
                .await?;
            control.closed = true;
            return Ok(());
        }
    }
}
async fn owned<T: Send + 'static>(
    future: impl Future<Output = BackendResult<T>> + Send + 'static,
) -> BackendResult<T> {
    tokio::spawn(AssertUnwindSafe(future).catch_unwind())
        .await
        .map_err(|_| BackendError::TaskStopped)?
        .map_err(|_| BackendError::TaskStopped)?
}
