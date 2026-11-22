use super::*;
use crate::host::{AgentId, AgentMode, AgentState, ExecutionMode};
use futures::FutureExt;
use rpi_harness::{
    agent_harness::{AgentHarness, AgentLane},
    session::{
        jsonl::JsonlSessionStorage, memory::SystemClock, session::DefaultIdGenerator,
        types::SessionStorage,
    },
    types::DrivingMode,
};
use serde::Serialize;
use std::{
    collections::BTreeSet,
    io::Write,
    panic::AssertUnwindSafe,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct Active {
    id: RunId,
    cancel: CancellationToken,
    completion: watch::Receiver<Completion>,
}

pub(super) struct Control {
    record: AgentRecord,
    active: Option<Active>,
    deferred: Option<String>,
    closing: bool,
    cleaned: bool,
    uncertain: bool,
    fault: Option<BackendError>,
    last_report: Option<Arc<BackendReport>>,
}

pub(super) struct Backend {
    pub control: Arc<Mutex<Control>>,
    close_gate: Arc<Mutex<()>>,
    catalog: HostCatalog,
    harness: AgentHarness,
    storage: JsonlSessionStorage,
    limits: OrdinaryLimits,
    retiring: AtomicBool,
    closed: AtomicBool,
}

pub(super) async fn restore(
    catalog: HostCatalog,
    id: AgentId,
    guard: CatalogGuard,
    limits: OrdinaryLimits,
    binder: OrdinaryBinder,
) -> BackendResult<OrdinaryAgent> {
    let record = catalog.get(id).await?;
    if record.spec.mode != AgentMode::Persistent
        || record.spec.execution != ExecutionMode::InProcess
    {
        return Err(BackendError::Binding(
            "only ordinary in-process backends are supported",
        ));
    }
    if record.spec.launch.model.is_none() {
        return Err(BackendError::Binding(
            "a resolved model reference is required",
        ));
    }
    let record = catalog.transition(id, guard, AgentState::Starting).await?;
    match build(&catalog, &record, limits, binder).await {
        Ok(backend) => Ok(OrdinaryAgent { inner: backend }),
        Err(error) => {
            // build has drained/released any acquired writer before this rollback.
            // A failed rollback retains the reservation rather than claiming free capacity.
            catalog
                .transition(id, catalog.guard(&record), AgentState::Closed)
                .await?;
            Err(error)
        }
    }
}

async fn build(
    catalog: &HostCatalog,
    record: &AgentRecord,
    limits: OrdinaryLimits,
    binder: OrdinaryBinder,
) -> BackendResult<Arc<Backend>> {
    let cwd = tokio::fs::canonicalize(&record.spec.launch.cwd).await?;
    let env = Arc::new(OsExecutionEnv::with_cwd(cwd.clone()));
    let reference = record
        .spec
        .session
        .as_ref()
        .ok_or(BackendError::Binding("missing Session reference"))?;
    let path = tokio::fs::canonicalize(&reference.path).await?;
    let path = path
        .to_str()
        .ok_or(BackendError::Binding("Session path must be UTF-8"))?;
    let prepared = JsonlSessionStorage::prepare_restore(
        env.clone(),
        path,
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await?;
    let facts = prepared.facts().await?;
    if facts.metadata.id != reference.id {
        return Err(BackendError::Binding("Session header identity mismatch"));
    }
    if tokio::fs::canonicalize(&facts.metadata.cwd).await? != cwd {
        return Err(BackendError::Binding("saved workspace identity mismatch"));
    }
    // The mandatory resolver sees exact saved facts before any repair or lease.
    let mut options = std::panic::catch_unwind(AssertUnwindSafe(|| {
        binder(&record.spec, env.clone(), &facts)
    }))
    .map_err(|_| BackendError::TaskStopped)??;
    validate_bindings(record, &options)?;
    let storage = prepared.commit_strict().await?;
    if !storage.writer_lease_held() {
        return Err(BackendError::Binding(
            "ordinary backend requires a real writer lease",
        ));
    }
    options.session = Session::new(Arc::new(storage.clone()), None);
    options.require_native_durable = true;
    options.allow_existing_session = true;
    options.active_tool_names = record.spec.launch.tools.clone();
    let created = AssertUnwindSafe(AgentHarness::create(options))
        .catch_unwind()
        .await;
    let harness = match created {
        Ok(Ok(harness)) => harness,
        other => {
            storage.drain().await;
            storage.release_writer_lease();
            return Err(match other {
                Ok(Err(error)) => error.into(),
                _ => BackendError::TaskStopped,
            });
        }
    };
    harness.set_skill_execution_env(env);
    if let Err(error) = harness.seal_resources(record.spec.launch.tools.clone()) {
        harness.wait_for_idle().await?;
        harness.drain_session().await;
        harness.close().await?;
        storage.release_writer_lease();
        return Err(error.into());
    }
    let deferred = match harness.pending_deferred().await {
        Ok(handle) => handle.map(|handle| handle.id),
        Err(error) => {
            harness.wait_for_idle().await?;
            harness.drain_session().await;
            harness.close().await?;
            storage.release_writer_lease();
            return Err(error.into());
        }
    };
    let state = if deferred.is_some() {
        AgentState::Running
    } else {
        AgentState::Idle
    };
    let idle = catalog
        .transition(record.spec.id, catalog.guard(record), state)
        .await;
    let idle = match idle {
        Ok(idle) => idle,
        Err(error) => {
            harness.wait_for_idle().await?;
            harness.drain_session().await;
            harness.close().await?;
            storage.release_writer_lease();
            return Err(error.into());
        }
    };
    Ok(Arc::new(Backend {
        control: Arc::new(Mutex::new(Control {
            record: idle,
            active: None,
            deferred,
            closing: false,
            cleaned: false,
            uncertain: false,
            fault: None,
            last_report: None,
        })),
        close_gate: Arc::new(Mutex::new(())),
        catalog: catalog.clone(),
        harness,
        storage,
        limits,
        retiring: AtomicBool::new(false),
        closed: AtomicBool::new(false),
    }))
}

fn validate_bindings(record: &AgentRecord, options: &AgentHarnessOptions) -> BackendResult<()> {
    let model = record
        .spec
        .launch
        .model
        .as_ref()
        .ok_or(BackendError::Binding("missing model"))?;
    if model.provider != options.model.provider || model.model != options.model.id {
        return Err(BackendError::Binding(
            "resolved model differs from registry",
        ));
    }
    let provider_ids: BTreeSet<_> = options
        .models
        .iter()
        .map(|provider| provider.id())
        .collect();
    if provider_ids.len() != options.models.len()
        || !provider_ids.contains(options.model.provider.as_str())
    {
        return Err(BackendError::Binding(
            "provider bindings are missing or ambiguous",
        ));
    }
    let tools: BTreeSet<_> = options
        .tools
        .iter()
        .map(|tool| tool.tool.schema().name.as_str())
        .collect();
    let expected: BTreeSet<_> = record
        .spec
        .launch
        .tools
        .iter()
        .map(String::as_str)
        .collect();
    if tools.len() != options.tools.len()
        || expected.len() != record.spec.launch.tools.len()
        || tools != expected
    {
        return Err(BackendError::Binding("tool bindings differ from registry"));
    }
    if !options.active_tool_names.is_empty()
        && options
            .active_tool_names
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            != expected
    {
        return Err(BackendError::Binding("active tools differ from registry"));
    }
    if options.drive != DrivingMode::Automatic {
        return Err(BackendError::Binding("manual stepping is not integrated"));
    }
    Ok(())
}

impl Backend {
    pub fn retire(&self) {
        self.retiring.store(true, Ordering::Release);
    }
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn ready(&self, control: &Control) -> BackendResult<()> {
        if self.retiring.load(Ordering::Acquire) || control.closing || self.is_closed() {
            return Err(BackendError::Closed);
        }
        if control.fault.is_some() {
            return Err(BackendError::Faulted);
        }
        Ok(())
    }

    pub fn validate_message(&self, message: &AgentMessage) -> BackendResult<()> {
        bounded_json(message, self.limits.max_input_bytes)
    }

    pub fn validate_operation(&self, operation: &OrdinaryOperation) -> BackendResult<()> {
        match operation {
            OrdinaryOperation::Prompt(messages) => {
                bounded_json(messages, self.limits.max_input_bytes)
            }
            OrdinaryOperation::ResumeDeferred(id) => bounded_json(id, self.limits.max_input_bytes),
            OrdinaryOperation::Compact(instructions) => {
                bounded_json(instructions, self.limits.max_input_bytes)
            }
            OrdinaryOperation::ResumePending => Ok(()),
        }
    }

    pub async fn submit(
        self: Arc<Self>,
        mut control: OwnedMutexGuard<Control>,
        operation: OrdinaryOperation,
    ) -> BackendResult<RunTicket> {
        self.ready(&control)?;
        if control.active.is_some()
            || (control.deferred.is_some()
                && !matches!(operation, OrdinaryOperation::ResumeDeferred(_)))
        {
            return Err(BackendError::Busy);
        }
        if control.record.state != AgentState::Running {
            let running = self
                .catalog
                .transition(
                    control.record.spec.id,
                    self.catalog.guard(&control.record),
                    AgentState::Running,
                )
                .await?;
            control.record = running;
        }
        let id = RunId::new();
        let cancel = CancellationToken::new();
        let (done, completion) = watch::channel(None);
        control.active = Some(Active {
            id,
            cancel: cancel.clone(),
            completion: completion.clone(),
        });
        drop(control);
        let inner = self.clone();
        tokio::spawn(async move {
            let result = AssertUnwindSafe(inner.run(id, cancel, operation))
                .catch_unwind()
                .await;
            let result = match result {
                Ok(report) => Ok(report),
                Err(_) => {
                    // Never publish Idle/release a slot after an unknown panic boundary.
                    let mut control = inner.control.lock().await;
                    control.fault = Some(BackendError::TaskStopped);
                    control.uncertain = true;
                    Err(BackendError::TaskStopped)
                }
            };
            done.send_replace(Some(result));
        });
        Ok(RunTicket { id, completion })
    }

    async fn run(
        &self,
        id: RunId,
        cancel: CancellationToken,
        operation: OrdinaryOperation,
    ) -> Arc<BackendReport> {
        let outcome = if cancel.is_cancelled() {
            Err(BackendError::CancelledBeforeAdmission)
        } else {
            // Keep polling the real Harness future after signalling cancellation.
            // Do not abort/drop it, even while image preparation/recovery precedes RunStart.
            let execute = async {
                match operation {
                    OrdinaryOperation::Prompt(messages) => self
                        .harness
                        .prompt_messages(messages)
                        .await
                        .map(OrdinaryOutput::Run),
                    OrdinaryOperation::ResumePending => {
                        self.harness.resume_pending().await.map(|result| {
                            result
                                .map(OrdinaryOutput::Run)
                                .unwrap_or(OrdinaryOutput::NoPendingResume)
                        })
                    }
                    OrdinaryOperation::ResumeDeferred(id) => {
                        self.harness.resume(&id).await.map(OrdinaryOutput::Run)
                    }
                    OrdinaryOperation::Compact(instructions) => self
                        .harness
                        .compact(instructions.as_deref())
                        .await
                        .map(OrdinaryOutput::Compaction),
                }
                .map_err(BackendError::from)
            };
            let execute = AssertUnwindSafe(execute).catch_unwind();
            tokio::pin!(execute);
            let request_cancel = async {
                cancel.cancelled().await;
                loop {
                    if let Some(active) = self.harness.active_operation_snapshot() {
                        if self
                            .harness
                            .request_abort(&active.run_id)
                            .await
                            .unwrap_or(false)
                        {
                            break;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            };
            tokio::pin!(request_cancel);
            let outcome = tokio::select! {
                result = &mut execute => result,
                _ = &mut request_cancel => execute.await,
            };
            outcome.unwrap_or(Err(BackendError::TaskStopped))
        };
        // Covers delayed callbacks, write drains and admission leases, not merely
        // a terminal provider response or a RunEnd observation.
        let settled = self.harness.wait_for_idle().await;
        self.harness.drain_session().await;
        let outcome = settled.map_err(BackendError::from).and(outcome);
        let failures = self.harness.take_write_failures();
        let unsaved = self.harness.unsaved_messages().len();
        let mut control = self.control.lock().await;
        let persistence_uncertain = control.uncertain
            || !failures.is_empty()
            || unsaved != 0
            || match &outcome {
                Err(BackendError::Harness(error)) => matches!(
                    error.as_ref(),
                    rpi_harness::result::HarnessError::Io { .. }
                        | rpi_harness::result::HarnessError::SessionStorage { .. }
                ),
                Err(BackendError::TaskStopped) => true,
                _ => false,
            };
        if persistence_uncertain {
            control.uncertain = true;
            control.fault = Some(
                outcome
                    .as_ref()
                    .err()
                    .cloned()
                    .unwrap_or(BackendError::Unsaved),
            );
        }
        let mut catalog_error = None;
        if let Ok(OrdinaryOutput::Run(result)) = &outcome {
            control.deferred = match &result.outcome {
                rpi_harness::agent_harness::HarnessRunOutcome::Suspended { deferred, .. } => {
                    Some(deferred.id.clone())
                }
                _ => None,
            };
        }
        if !control.closing
            && !self.retiring.load(Ordering::Acquire)
            && control.fault.is_none()
            && control.deferred.is_none()
        {
            match self
                .catalog
                .transition(
                    control.record.spec.id,
                    self.catalog.guard(&control.record),
                    AgentState::Idle,
                )
                .await
            {
                Ok(record) => control.record = record,
                Err(error) => {
                    let error = BackendError::from(error);
                    control.fault = Some(error.clone());
                    catalog_error = Some(error);
                }
            }
        }
        if control
            .active
            .as_ref()
            .is_some_and(|active| active.id == id)
        {
            control.active = None;
        }
        let report = Arc::new(BackendReport {
            outcome,
            write_failures: failures,
            unsaved_messages: unsaved,
            persistence_uncertain,
            catalog_error,
        });
        control.last_report = Some(report.clone());
        report
    }

    pub async fn deliver(
        &self,
        mut control: OwnedMutexGuard<Control>,
        message: AgentMessage,
        delivery: Delivery,
    ) -> BackendResult<QueueResult> {
        self.ready(&control)?;
        let result = AssertUnwindSafe(async {
            match delivery {
                Delivery::Steer => self.harness.steer(message).await,
                Delivery::FollowUp => self.harness.follow_up(message).await,
                Delivery::NextRun => self.harness.next_run(message).await,
            }
            .map_err(BackendError::from)
        })
        .catch_unwind()
        .await
        .unwrap_or(Err(BackendError::TaskStopped));
        if let Err(error) = &result {
            let uncertain = match error {
                BackendError::Harness(error) => matches!(
                    error.as_ref(),
                    rpi_harness::result::HarnessError::Io { .. }
                        | rpi_harness::result::HarnessError::SessionStorage { .. }
                ),
                BackendError::TaskStopped => true,
                _ => false,
            };
            if uncertain {
                control.uncertain = true;
                control.fault = Some(error.clone());
                if let Some(active) = &control.active {
                    active.cancel.cancel();
                }
            }
        }
        // Keep the control guard until the cancellation-safe Harness queue write
        // finishes, even when the frontend has dropped its waiting JoinHandle.
        drop(control);
        result
    }

    pub fn cancel(&self, ticket: RunId) -> BackendResult<bool> {
        let control = self.control.try_lock().map_err(|_| BackendError::Busy)?;
        if let Some(active) = control.active.as_ref().filter(|active| active.id == ticket) {
            active.cancel.cancel();
            return Ok(true);
        }
        Ok(false)
    }

    pub fn snapshot(&self) -> BackendResult<BackendSnapshot> {
        let control = self.control.try_lock().map_err(|_| BackendError::Busy)?;
        Ok(BackendSnapshot {
            record: control.record.clone(),
            ticket: control.active.as_ref().map(|active| active.id),
            active: self.harness.active_operation_snapshot(),
            pending_resume: self.harness.pending_resume().is_some(),
            deferred: control.deferred.clone(),
            retiring: self.retiring.load(Ordering::Acquire),
            closed: self.is_closed(),
            fault: control.fault.clone(),
            last_report: control.last_report.clone(),
        })
    }

    pub fn unsaved_messages(&self) -> Vec<UnsavedMessage> {
        self.harness.unsaved_messages()
    }

    pub async fn close(&self) -> BackendResult<()> {
        let _closing = self.close_gate.try_lock().map_err(|_| BackendError::Busy)?;
        if self.is_closed() {
            return Ok(());
        }
        self.retire();
        let mut control = self.control.lock().await;
        control.closing = true;
        // Signal cancellation regardless of a failed metadata Closing commit.
        let active = control.active.clone();
        if let Some(active) = &active {
            active.cancel.cancel();
        }
        let transition_error = if control.record.state != AgentState::Closing {
            match self
                .catalog
                .transition(
                    control.record.spec.id,
                    self.catalog.guard(&control.record),
                    AgentState::Closing,
                )
                .await
            {
                Ok(record) => {
                    control.record = record;
                    None
                }
                Err(error) => Some(BackendError::from(error)),
            }
        } else {
            None
        };
        let cleaned = control.cleaned;
        drop(control);
        if let Some(active) = active {
            let ticket = RunTicket {
                id: active.id,
                completion: active.completion,
            };
            // Even a task failure is not proof of cleanup; always wait below.
            let _ = ticket.wait().await;
        }
        if !cleaned {
            self.harness.wait_for_idle().await?;
            self.harness.drain_session().await;
        }
        let mut control = self.control.lock().await;
        if control.uncertain || !self.harness.unsaved_messages().is_empty() {
            return Err(BackendError::Unsaved);
        }
        if let Some(error) = transition_error {
            control.fault = Some(error.clone());
            return Err(error);
        }
        // Actual cleanup precedes the Closed commit and its capacity release.
        // If that commit fails, keep a conservative reservation; retries skip the
        // already-closed Harness and only finish publishing the catalog fact.
        if !control.cleaned {
            self.harness.close().await?;
            self.storage.drain().await;
            self.storage.release_writer_lease();
            control.cleaned = true;
        }
        let closed = match self
            .catalog
            .transition(
                control.record.spec.id,
                self.catalog.guard(&control.record),
                AgentState::Closed,
            )
            .await
        {
            Ok(record) => record,
            Err(error) => {
                let error = BackendError::from(error);
                control.fault = Some(error.clone());
                return Err(error);
            }
        };
        control.record = closed;
        control.active = None;
        self.closed.store(true, Ordering::Release);
        Ok(())
    }
}

struct ByteCounter {
    bytes: usize,
    limit: usize,
}
impl Write for ByteCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        if self.bytes > self.limit {
            return Err(io::Error::other("input budget"));
        }
        Ok(buffer.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn bounded_json(value: &impl Serialize, limit: usize) -> BackendResult<()> {
    let mut counter = ByteCounter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value).map_err(|_| BackendError::InputTooLarge)
}
