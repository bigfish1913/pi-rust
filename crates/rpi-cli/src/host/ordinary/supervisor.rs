use super::*;
use crate::host::{AgentId, AgentMode, AgentState, ExecutionMode};
use futures::FutureExt;
use rpi_harness::{
    agent_harness::{AgentHarness, AgentLane},
    session::{
        jsonl::{JsonlSessionStorage, PreparedJsonlRestore},
        memory::SystemClock,
        session::DefaultIdGenerator,
        types::SessionStorage,
    },
    types::DrivingMode,
};
use rpi_tools::FileSystem;
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
    // Exact input identity, not this observer's volatile ticket. Duplicates
    // never replace it; it lives until the original task fully settles.
    submitted_request_id: Option<String>,
    cancel: CancellationToken,
    completion: watch::Receiver<Completion>,
}

pub(super) struct Control {
    record: AgentRecord,
    active: Option<Active>,
    deferred: Option<String>,
    // Volatile association carried only across explicit deferred polls in this
    // owner lifetime. Restoring history never recreates cancellation authority.
    deferred_request_id: Option<String>,
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
    extensions: Option<Arc<super::super::NativeExtensionCustody>>,
    limits: OrdinaryLimits,
    retiring: AtomicBool,
    closed: AtomicBool,
}

pub(super) async fn restore(
    catalog: HostCatalog,
    id: AgentId,
    guard: CatalogGuard,
    limits: OrdinaryLimits,
    binding: OrdinaryBinding,
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
    match build(&catalog, &record, limits, binding).await {
        Ok(backend) => Ok(OrdinaryAgent { inner: backend }),
        Err(error @ BackendError::ExtensionUnsettled(_)) => {
            // No release proof: Service must retain Starting/residency and
            // this error's concrete DLL/storage custody, never roll back Closed.
            Err(error)
        }
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

struct PreparedNativeBinding {
    env: Arc<OsExecutionEnv>,
    storage: PreparedJsonlRestore,
    // Keep the immediate path inline: no extra heap allocation for existing
    // native bindings. Exactly one slot is populated by prepare_binding.
    options: Option<AgentHarnessOptions>,
    assembly: Option<OrdinaryAssembly>,
    instruction_resources: Option<Arc<super::super::NativeBindingResources>>,
}

/// Pure readonly preflight: neither Starting/resident metadata nor a Session
/// writer is claimed. Actual restore repeats it to revalidate current facts.
pub(super) async fn preflight(
    spec: &crate::host::AgentSpec,
    binding: OrdinaryBinding,
) -> BackendResult<()> {
    drop(prepare_binding(spec, binding).await?);
    Ok(())
}

async fn prepare_binding(
    spec: &crate::host::AgentSpec,
    binding: OrdinaryBinding,
) -> BackendResult<PreparedNativeBinding> {
    let cwd = tokio::fs::canonicalize(&spec.launch.cwd).await?;
    let OrdinaryBinding {
        environment,
        binder,
        instruction_resources,
        extensions: _,
        #[cfg(test)]
            filesystem: test_filesystem,
    } = binding;
    if let Some(resources) = &instruction_resources {
        if spec.launch.resource_profile.as_deref() != Some(resources.identity()) {
            return Err(BackendError::Binding(
                "instruction resource identity mismatch",
            ));
        }
        resources.validate_tools(&spec.launch.tools)?;
    }
    let env = match environment {
        Some(env) if env.cwd() == cwd => env,
        Some(_) => {
            return Err(BackendError::Binding(
                "execution workspace differs from registry",
            ))
        }
        None => Arc::new(OsExecutionEnv::with_cwd(cwd.clone())),
    };
    let reference = spec
        .session
        .as_ref()
        .ok_or(BackendError::Binding("missing Session reference"))?;
    let path = tokio::fs::canonicalize(&reference.path).await?;
    let path = path
        .to_str()
        .ok_or(BackendError::Binding("Session path must be UTF-8"))?;
    let filesystem: Arc<dyn FileSystem> = env.clone();
    #[cfg(test)]
    let filesystem = test_filesystem.unwrap_or(filesystem);
    let prepared = JsonlSessionStorage::prepare_restore(
        filesystem,
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
    // Both resolver forms see exact saved facts before any repair or lease.
    // A deferred resolver must only validate and construct an unpolled future.
    let (options, assembly) = std::panic::catch_unwind(AssertUnwindSafe(|| -> BackendResult<_> {
        match binder {
            BindingResolver::Immediate(binder) => {
                let options = binder(spec, env.clone(), &facts)?;
                validate_bindings(spec, &options)?;
                if let Some(resources) = &instruction_resources {
                    resources.validate(&facts.values, &options.tools)?;
                }
                Ok((Some(options), None))
            }
            BindingResolver::Deferred(validator) => {
                Ok((None, Some(validator(spec, env.clone(), &facts)?)))
            }
        }
    }))
    .map_err(|_| BackendError::TaskStopped)??;
    Ok(PreparedNativeBinding {
        env,
        storage: prepared,
        options,
        assembly,
        instruction_resources,
    })
}

async fn build(
    catalog: &HostCatalog,
    record: &AgentRecord,
    limits: OrdinaryLimits,
    binding: OrdinaryBinding,
) -> BackendResult<Arc<Backend>> {
    let extensions = binding.extensions.clone();
    let Some(custody) = &extensions else {
        return build_inner(catalog, record, limits, binding, None).await;
    };
    // Reject reuse before entering cleanup: a rejected second binding must
    // never close the first Agent's still-live DLL owner or writer.
    custody.claim(catalog.clone())?;
    let result = AssertUnwindSafe(build_inner(
        catalog,
        record,
        limits,
        binding,
        extensions.clone(),
    ))
    .catch_unwind()
    .await
    .unwrap_or(Err(BackendError::TaskStopped));
    if result.is_err() {
        custody.close().await?;
        custody.release_storage().await;
    }
    result
}

async fn release_build_writer(
    storage: &JsonlSessionStorage,
    extensions: &Option<Arc<super::super::NativeExtensionCustody>>,
) -> BackendResult<()> {
    storage.drain().await;
    if let Some(custody) = extensions {
        custody.close().await?;
    }
    storage.release_writer_lease();
    Ok(())
}

async fn build_inner(
    catalog: &HostCatalog,
    record: &AgentRecord,
    limits: OrdinaryLimits,
    binding: OrdinaryBinding,
    extensions: Option<Arc<super::super::NativeExtensionCustody>>,
) -> BackendResult<Arc<Backend>> {
    let PreparedNativeBinding {
        env,
        storage: prepared,
        options,
        assembly,
        instruction_resources,
    } = prepare_binding(&record.spec, binding).await?;
    // build runs only in the owned restore, never the readonly preflight.
    // Do not detach/cancel assembly on waiter loss; final options still must
    // pass the same model/provider/tool/drive validation before writer/repair.
    let mut options = match (options, assembly) {
        (Some(options), None) => options, // Already checked by the immediate resolver.
        (None, Some(assembly)) => {
            let options = AssertUnwindSafe(assembly)
                .catch_unwind()
                .await
                .map_err(|_| BackendError::TaskStopped)??;
            validate_bindings(&record.spec, &options)?;
            if let Some(resources) = &instruction_resources {
                resources.validate(&prepared.facts().await?.values, &options.tools)?;
            }
            options
        }
        _ => return Err(BackendError::Binding("invalid prepared assembly")),
    };
    let storage = prepared.commit_strict().await?;
    if let Some(custody) = &extensions {
        custody.retain_storage(storage.clone());
    }
    if !storage.writer_lease_held() {
        return Err(BackendError::Binding(
            "ordinary backend requires a real writer lease",
        ));
    }
    options.session = Session::new(Arc::new(storage.clone()), None);
    if let Some(resources) = &instruction_resources {
        // The exact writer also covers DLL cleanup when this binding owns
        // executable resources; failed sync is not permission to unload early.
        let persisted = AssertUnwindSafe(resources.persist(&options.session, &options.tools))
            .catch_unwind()
            .await
            .unwrap_or(Err(BackendError::TaskStopped));
        if let Err(error) = persisted {
            release_build_writer(&storage, &extensions).await?;
            return Err(error);
        }
    }
    options.require_native_durable = true;
    options.allow_existing_session = true;
    options.active_tool_names = record.spec.launch.tools.clone();
    let created = AssertUnwindSafe(AgentHarness::create(options))
        .catch_unwind()
        .await;
    let harness = match created {
        Ok(Ok(harness)) => harness,
        other => {
            release_build_writer(&storage, &extensions).await?;
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
        release_build_writer(&storage, &extensions).await?;
        return Err(error.into());
    }
    let deferred = match harness.pending_deferred().await {
        Ok(handle) => handle.map(|handle| handle.id),
        Err(error) => {
            harness.wait_for_idle().await?;
            harness.drain_session().await;
            harness.close().await?;
            release_build_writer(&storage, &extensions).await?;
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
            release_build_writer(&storage, &extensions).await?;
            return Err(error.into());
        }
    };
    Ok(Arc::new(Backend {
        control: Arc::new(Mutex::new(Control {
            record: idle,
            active: None,
            deferred,
            deferred_request_id: None,
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
        extensions,
        limits,
        retiring: AtomicBool::new(false),
        closed: AtomicBool::new(false),
    }))
}

fn validate_bindings(
    spec: &crate::host::AgentSpec,
    options: &AgentHarnessOptions,
) -> BackendResult<()> {
    let model = spec
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
    let expected: BTreeSet<_> = spec.launch.tools.iter().map(String::as_str).collect();
    if tools.len() != options.tools.len()
        || expected.len() != spec.launch.tools.len()
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

    pub fn validate_messages(&self, messages: &[AgentMessage]) -> BackendResult<()> {
        bounded_json(&messages, self.limits.max_input_bytes)
    }

    /// Native admission observation for one request, independent of execution.
    pub async fn admission(
        &self,
        request_id: &str,
    ) -> BackendResult<Option<rpi_harness::agent_harness::HostAdmissionStatus>> {
        self.harness
            .host_admission(request_id)
            .await
            .map_err(super::admission_error)
    }

    pub async fn get_result(
        &self,
        run_id: &str,
        history: HistoryIdentity,
    ) -> BackendResult<Option<(RunResult, HistoryIdentity)>> {
        let Some(result) = self.harness.get_result(run_id).await? else {
            return Ok(None);
        };
        // This Backend owns a fixed Harness/Session. Cover the terminal record
        // and all referenced entries, not only the earlier input admission.
        let covered = self
            .harness
            .session()
            .sync_history()
            .await
            .map_err(|_| BackendError::AdmissionUncertain)?;
        if covered.generation != history.generation || covered.revision < history.revision {
            return Err(BackendError::AdmissionUncertain);
        }
        Ok(Some((result, covered)))
    }

    pub async fn admissions(
        &self,
    ) -> BackendResult<Vec<rpi_harness::agent_harness::HostAdmissionStatus>> {
        self.harness
            .host_admissions()
            .await
            .map_err(super::admission_error)
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

    /// In-memory supervision only: a ticket is not proof that the input reached
    /// the native journal. Use [`Self::submit_admitted`] for request-linked
    /// admission.
    pub async fn submit(
        self: Arc<Self>,
        control: OwnedMutexGuard<Control>,
        operation: OrdinaryOperation,
    ) -> BackendResult<RunTicket> {
        let launched = self.launch(control, &operation, None).await?;
        let ticket = RunTicket {
            id: launched.id,
            completion: launched.completion.clone(),
        };
        launched.spawn(operation, None);
        Ok(ticket)
    }

    /// Admit the exact prompt in the native journal, then run it. The returned
    /// ticket exposes the durable admission separately from execution outcome.
    pub async fn submit_admitted(
        self: Arc<Self>,
        control: OwnedMutexGuard<Control>,
        host_id: String,
        request_id: String,
        messages: Vec<AgentMessage>,
    ) -> BackendResult<AdmittedRunTicket> {
        if messages.is_empty() {
            return Err(BackendError::Admission(
                "at least one admitted message is required",
            ));
        }
        let request = rpi_harness::agent_harness::HostAdmissionRequest {
            request_id,
            host_id,
            agent_id: control.record.spec.id.to_string(),
            kind: rpi_harness::agent_harness::HostAdmissionKind::Prompt {},
            messages: messages.clone(),
        };
        rpi_harness::agent_harness::validate_host_admission(&request)
            .map_err(super::admission_error)?;
        // Full request comparison and a new strict sync are native Harness
        // responsibilities. Entry visibility alone is never a durable retry ACK.
        self.ready(&control)?;
        if let Some(outcome) = self
            .harness
            .resync_host_admission(request.clone())
            .await
            .map_err(super::admission_error)?
        {
            return Ok(replayed_ticket(outcome.receipt));
        }
        let launched = self
            .launch(
                control,
                &OrdinaryOperation::Prompt(messages.clone()),
                Some(request.clone()),
            )
            .await?;
        let admission = launched
            .admission
            .clone()
            .ok_or(BackendError::TaskStopped)?;
        let ticket = AdmittedRunTicket {
            id: launched.id,
            admission,
            completion: launched.completion.clone(),
        };
        launched.spawn(OrdinaryOperation::Prompt(messages), Some(request));
        Ok(ticket)
    }

    async fn launch(
        self: &Arc<Self>,
        mut control: OwnedMutexGuard<Control>,
        operation: &OrdinaryOperation,
        plan: Option<rpi_harness::agent_harness::HostAdmissionRequest>,
    ) -> BackendResult<Launch> {
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
        let submitted_request_id = plan
            .as_ref()
            .map(|request| request.request_id.clone())
            .or_else(|| match operation {
                OrdinaryOperation::ResumeDeferred(_) => control.deferred_request_id.clone(),
                _ => None,
            });
        control.active = Some(Active {
            id,
            submitted_request_id,
            cancel: cancel.clone(),
            completion: completion.clone(),
        });
        drop(control);
        // `Pending` is published before the task starts so a ticket waiting
        // for admission can never observe `NotApplicable` for a plan.
        let (admission_tx, admission) = match &plan {
            Some(_) => {
                let (tx, rx) = watch::channel(super::AdmissionState::Pending);
                (Some(tx), Some(rx))
            }
            None => (None, None),
        };
        Ok(Launch {
            inner: self.clone(),
            id,
            cancel,
            completion,
            admission,
            admission_tx,
            done,
        })
    }

    async fn run(
        &self,
        id: RunId,
        cancel: CancellationToken,
        operation: OrdinaryOperation,
        plan: Option<rpi_harness::agent_harness::HostAdmissionRequest>,
        admission: Option<watch::Sender<AdmissionState>>,
    ) -> Arc<BackendReport> {
        // Admission precedes any provider/tool work. A rejection is a definite
        // non-execution, never a durable input claim.
        let mut replayed = false;
        let admission_failure = if cancel.is_cancelled() {
            let error = BackendError::CancelledBeforeAdmission;
            publish_admission(&admission, AdmissionState::Rejected(error.clone()));
            Some(error)
        } else if let Some(request) = plan {
            match self.harness.admit_host_request(request).await {
                Ok(outcome) => {
                    replayed = outcome.replayed;
                    publish_admission(
                        &admission,
                        AdmissionState::Admitted(Arc::new(outcome.receipt)),
                    );
                    None
                }
                Err(error) => {
                    let error = super::admission_error(error);
                    publish_admission(&admission, AdmissionState::Rejected(error.clone()));
                    Some(error)
                }
            }
        } else {
            None
        };
        let outcome = if let Some(error) = admission_failure {
            Err(error)
        } else if replayed {
            // Exactly-once admission: the same request id never executes twice.
            Ok(OrdinaryOutput::AlreadyAdmitted)
        } else {
            // Keep polling the real Harness future after signalling cancellation.
            // Do not abort/drop it, even while image preparation/recovery precedes RunStart.
            let execute = async {
                match operation {
                    OrdinaryOperation::Prompt(messages) => self
                        .harness
                        .prompt_messages(messages)
                        .await
                        .map(Box::new)
                        .map(OrdinaryOutput::Run),
                    OrdinaryOperation::ResumePending => {
                        self.harness.resume_pending().await.map(|result| {
                            result
                                .map(Box::new)
                                .map(OrdinaryOutput::Run)
                                .unwrap_or(OrdinaryOutput::NoPendingResume)
                        })
                    }
                    OrdinaryOperation::ResumeDeferred(id) => self
                        .harness
                        .resume(&id)
                        .await
                        .map(Box::new)
                        .map(OrdinaryOutput::Run),
                    OrdinaryOperation::Compact(instructions) => self
                        .harness
                        .compact(instructions.as_deref())
                        .await
                        .map(Box::new)
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
            control.deferred_request_id = if control.deferred.is_some() {
                control
                    .active
                    .as_ref()
                    .filter(|active| active.id == id)
                    .and_then(|active| active.submitted_request_id.clone())
            } else {
                None
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

    fn validate_guard(&self, control: &Control, guard: CatalogGuard) -> BackendResult<()> {
        let error = if guard.epoch != self.catalog.epoch() {
            Some(CatalogError::EpochConflict)
        } else if guard.revision != control.record.revision {
            Some(CatalogError::RevisionConflict {
                expected: guard.revision,
                actual: control.record.revision,
            })
        } else {
            None
        };
        match error {
            Some(error) => Err(BackendError::GuardRejected(Arc::new(error))),
            None => Ok(()),
        }
    }

    pub fn cancel_guarded(&self, guard: CatalogGuard, ticket: RunId) -> BackendResult<bool> {
        let control = self.control.try_lock().map_err(|_| BackendError::Busy)?;
        self.validate_guard(&control, guard)?;
        if let Some(active) = control.active.as_ref().filter(|active| active.id == ticket) {
            active.cancel.cancel();
            return Ok(true);
        }
        Ok(false)
    }

    /// Exact submitted input, checked under the same gate as the signal. A
    /// historical receipt, a duplicate observer and a parked deferred handle
    /// never redirect this signal to another active operation.
    pub fn cancel_submitted_guarded(
        &self,
        guard: CatalogGuard,
        request_id: &str,
    ) -> BackendResult<bool> {
        let control = self.control.try_lock().map_err(|_| BackendError::Busy)?;
        self.validate_guard(&control, guard)?;
        if let Some(active) = control
            .active
            .as_ref()
            .filter(|active| active.submitted_request_id.as_deref() == Some(request_id))
        {
            active.cancel.cancel();
            return Ok(true);
        }
        Ok(false)
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
            submitted_request_id: control
                .active
                .as_ref()
                .and_then(|active| active.submitted_request_id.clone())
                .or_else(|| control.deferred_request_id.clone()),
            cancel_requested: control
                .active
                .as_ref()
                .is_some_and(|active| active.cancel.is_cancelled()),
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
        self.close_inner(None).await
    }

    pub async fn close_guarded(&self, guard: CatalogGuard) -> BackendResult<()> {
        self.close_inner(Some(guard)).await
    }

    async fn close_inner(&self, guard: Option<CatalogGuard>) -> BackendResult<()> {
        let _closing = self.close_gate.try_lock().map_err(|_| BackendError::Busy)?;
        if self.is_closed() && guard.is_none() {
            return Ok(());
        }
        let mut control = self.control.lock().await;
        if let Some(guard) = guard {
            self.validate_guard(&control, guard)?;
            if self.is_closed() || control.record.state == AgentState::Closing {
                // No new Closing transition is needed on this retry, but an
                // old retained handle must not acknowledge a reloaded owner.
                let current = self.catalog.get(control.record.spec.id).await?;
                if current.revision != guard.revision {
                    return Err(BackendError::GuardRejected(Arc::new(
                        CatalogError::RevisionConflict {
                            expected: guard.revision,
                            actual: current.revision,
                        },
                    )));
                }
                if self.is_closed() {
                    return Ok(());
                }
            } else {
                // Administrative admission must win the catalog CAS BEFORE any
                // retire/cancel flag changes. An out-of-band catalog transition
                // must not turn a stale request into unconditional shutdown.
                let record = self
                    .catalog
                    .transition(control.record.spec.id, guard, AgentState::Closing)
                    .await
                    .map_err(|error| match error {
                        CatalogError::EpochConflict | CatalogError::RevisionConflict { .. } => {
                            BackendError::GuardRejected(Arc::new(error))
                        }
                        other => BackendError::from(other),
                    })?;
                control.record = record;
            }
        }
        self.retire();
        control.closing = true;
        // Unconditional shutdown signals despite a failed Closing commit;
        // guarded administration already won that commit before reaching here.
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
            if let Some(custody) = &self.extensions {
                custody.close().await?;
            }
            self.storage.drain().await;
            self.storage.release_writer_lease();
            if let Some(custody) = &self.extensions {
                custody.release_storage().await;
            }
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
        control.deferred_request_id = None;
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
/// Ticket for an already-durable request id: admission is resolved, execution
/// was settled as `AlreadyAdmitted` without launching anything.
fn replayed_ticket(receipt: rpi_harness::agent_harness::HostAdmissionReceipt) -> AdmittedRunTicket {
    let (_, admission) = watch::channel(AdmissionState::Admitted(Arc::new(receipt)));
    let report = Arc::new(BackendReport {
        outcome: Ok(OrdinaryOutput::AlreadyAdmitted),
        write_failures: Vec::new(),
        unsaved_messages: 0,
        persistence_uncertain: false,
        catalog_error: None,
    });
    let (_, completion) = watch::channel(Some(Ok(report)));
    AdmittedRunTicket {
        id: RunId::new(),
        admission,
        completion,
    }
}

fn publish_admission(admission: &Option<watch::Sender<AdmissionState>>, state: AdmissionState) {
    if let Some(sender) = admission {
        sender.send_replace(state);
    }
}

/// A launched supervisor task plus its admission/execution observers.
struct Launch {
    inner: Arc<Backend>,
    id: RunId,
    cancel: CancellationToken,
    completion: watch::Receiver<Completion>,
    admission: Option<watch::Receiver<AdmissionState>>,
    admission_tx: Option<watch::Sender<AdmissionState>>,
    done: watch::Sender<Completion>,
}
impl Launch {
    fn spawn(
        self,
        operation: OrdinaryOperation,
        plan: Option<rpi_harness::agent_harness::HostAdmissionRequest>,
    ) {
        let Launch {
            inner,
            id,
            cancel,
            completion: _,
            admission: _,
            admission_tx,
            done,
        } = self;
        tokio::spawn(async move {
            let result = AssertUnwindSafe(inner.run(id, cancel, operation, plan, admission_tx))
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
    }
}

fn bounded_json(value: &impl Serialize, limit: usize) -> BackendResult<()> {
    let mut counter = ByteCounter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value).map_err(|_| BackendError::InputTooLarge)
}
