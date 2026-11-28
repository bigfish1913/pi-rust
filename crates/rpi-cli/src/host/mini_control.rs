//! Optional, private Mini authority and bounded EPOCH-LOCAL operation records.
//! This is not another Agent owner, execution loop or durable journal. All
//! native effects use the sole HostService table and lifecycle reservation.
use super::{
    mini::{MiniLimits, MiniOutcome, MiniReceipt},
    protocol::{self as wire, *},
    service::{HostService, ServiceError, ServiceReservation},
    AgentMode, HostStartupAgent,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

fn operations() -> usize {
    64
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MiniPermission {
    Submit,
    Cancel,
    Close,
}
/// Only the owner loads this from a private configuration file. A command's
/// scope/Agent identities are selectors, never grants. All rights default off.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiniControlConfig {
    pub scope: ControlScopeId,
    #[serde(default)]
    pub allowed_agents: Vec<super::AgentId>,
    #[serde(default)]
    pub permissions: Vec<MiniPermission>,
    #[serde(default = "operations")]
    pub max_operations: usize,
}
impl MiniControlConfig {
    pub(crate) fn valid(&self) -> bool {
        (1..=64).contains(&self.max_operations)
            && self.allowed_agents.len() <= super::MAX_TRUSTED_PROFILES
            && self
                .allowed_agents
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .len()
                == self.allowed_agents.len()
            && self
                .permissions
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .len()
                == self.permissions.len()
    }
    pub(crate) async fn preflight(
        &self,
        agents: &[HostStartupAgent],
    ) -> Result<(), super::daemon::DaemonError> {
        use super::daemon::DaemonError;
        if !self.valid() || agents.len() > super::MAX_TRUSTED_PROFILES {
            return Err(DaemonError::StartupAgent);
        }
        let mut ids = BTreeSet::new();
        let mut mini = BTreeSet::new();
        for agent in agents {
            if !ids.insert(agent.spec.id) {
                return Err(DaemonError::StartupAgent);
            }
            if agent.spec.mode == AgentMode::Mini {
                if agent.limits.max_input_bytes == 0
                    || agent.limits.max_input_bytes > 8 * 1024 * 1024
                {
                    return Err(DaemonError::StartupAgent);
                }
                agent
                    .profile
                    .preflight_mini(&agent.spec)
                    .await
                    .map_err(|_| DaemonError::StartupAgent)?;
                mini.insert(agent.spec.id);
            }
        }
        if !self.allowed_agents.iter().all(|id| mini.contains(id)) {
            return Err(DaemonError::StartupAgent);
        }
        Ok(())
    }
}
struct Job {
    request: MiniRequest,
    state: &'static str,
    effect: Option<Value>,
    code: Option<ErrorCode>,
}
struct Inner {
    service: HostService,
    config: MiniControlConfig,
    jobs: Mutex<BTreeMap<RequestId, Job>>,
}
#[derive(Clone)]
pub(crate) struct MiniControl {
    inner: Arc<Inner>,
}
impl MiniControl {
    pub(crate) fn new(service: HostService, config: MiniControlConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                service,
                config,
                jobs: Mutex::new(BTreeMap::new()),
            }),
        }
    }
    fn authorize(
        &self,
        scope: ControlScopeId,
        agent: wire::AgentId,
        permission: MiniPermission,
    ) -> Result<(), ErrorCode> {
        let agent: super::AgentId = parse(agent)?;
        if scope != self.inner.config.scope
            || !self.inner.config.allowed_agents.contains(&agent)
            || !self.inner.config.permissions.contains(&permission)
        {
            return Err(ErrorCode::Unauthorized);
        }
        Ok(())
    }
    fn authorize_request(&self, request: &MiniRequest) -> Result<(), ErrorCode> {
        self.authorize(
            request.scope,
            request.action.agent(),
            match request.action {
                MiniAction::Submit { .. } => MiniPermission::Submit,
                MiniAction::Cancel { .. } => MiniPermission::Cancel,
                MiniAction::Close { .. } => MiniPermission::Close,
            },
        )
    }
    fn epoch_matches(&self, epoch: Epoch) -> bool {
        epoch.to_string() == self.inner.service.catalog().epoch().to_string()
    }
    fn projection(job: &Job) -> Value {
        json!({"durable":false,"volatile":true,"epoch":job.request.epoch,
            "request_id":job.request.request_id,"agent_id":job.request.action.agent(),
            "found":true,"operation":{"state":job.state,"effect":job.effect,"code":job.code}})
    }
    fn unknown(&self, epoch: Epoch, request_id: RequestId, reason: &str) -> Value {
        json!({"durable":false,"volatile":true,"epoch":epoch,"owner_epoch":self.inner.service.catalog().epoch(),
            "request_id":request_id,"found":false,"operation":{"state":"unknown","reason":reason}})
    }
    fn publish(&self, id: RequestId, effect: Result<Value, ErrorCode>) {
        let mut jobs = self.inner.jobs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(job) = jobs.get_mut(&id) {
            match effect {
                Ok(value) => {
                    job.state = "succeeded";
                    job.effect = Some(value);
                }
                Err(code) => {
                    job.state = if code == ErrorCode::OutcomeUnknown {
                        "unknown"
                    } else {
                        "failed"
                    };
                    job.code = Some(code);
                }
            }
        }
    }
    pub(crate) async fn dispatch(&self, command: Command) -> Reply {
        let result = match command {
            Command::MiniExecute { request } => self.execute(request),
            Command::MiniReceipt { request } => self.receipt(request),
            Command::MiniResult {
                scope,
                epoch,
                agent_id,
                request_id,
                run_id,
                offset,
                max_bytes,
            } => {
                self.result(
                    scope,
                    epoch,
                    agent_id,
                    request_id,
                    run_id,
                    (offset, max_bytes),
                )
                .await
            }
            _ => Err(ErrorCode::Unsupported),
        };
        match result {
            Ok(data) => Reply::Data { data },
            Err(code) => Reply::Error { code },
        }
    }
    fn execute(&self, request: MiniRequest) -> Result<Value, ErrorCode> {
        self.authorize_request(&request)?;
        if !self.epoch_matches(request.epoch) {
            return Err(ErrorCode::StaleEpoch);
        }
        if self.inner.service.is_stopping() {
            return Err(ErrorCode::Busy);
        }
        if !request.valid() {
            return Err(ErrorCode::InvalidRequest);
        }
        let reply = {
            let mut jobs = self.inner.jobs.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(job) = jobs.get(&request.request_id) {
                return if job.request == request {
                    Ok(Self::projection(job))
                } else {
                    Err(ErrorCode::Conflict)
                };
            }
            if jobs.len() >= self.inner.config.max_operations {
                return Err(ErrorCode::Capacity);
            }
            let job = Job {
                request: request.clone(),
                state: "running",
                effect: None,
                code: None,
            };
            let data = Self::projection(&job);
            jobs.insert(request.request_id, job);
            data
        };
        let this = self.clone();
        let service = self.inner.service.clone();
        // No await between reservation and transfer to supervised ownership.
        // Native effect AND publication hold one lifecycle reader. Stop cannot
        // close storage halfway through; never reacquire the fair lock inside.
        tokio::spawn(async move {
            let publish = this.clone();
            let id = request.request_id;
            let result = service
                .with_administration(move |reservation| {
                    Box::pin(async move {
                        let effect = Self::apply(&reservation, &request).await;
                        publish.publish(id, effect);
                        Ok(())
                    })
                })
                .await;
            if let Err(error) = result {
                this.publish(id, Err(service_code(error)));
            }
        });
        Ok(reply)
    }
    fn receipt(&self, request: MiniRequest) -> Result<Value, ErrorCode> {
        self.authorize_request(&request)?;
        if !request.valid() {
            return Err(ErrorCode::InvalidRequest);
        }
        if !self.epoch_matches(request.epoch) {
            return Ok(self.unknown(request.epoch, request.request_id, "owner_epoch_changed"));
        }
        let jobs = self.inner.jobs.lock().unwrap_or_else(|p| p.into_inner());
        match jobs.get(&request.request_id) {
            Some(job) if job.request == request => Ok(Self::projection(job)),
            Some(_) => Err(ErrorCode::Conflict),
            None => Ok(self.unknown(request.epoch, request.request_id, "not_observed_no_replay")),
        }
    }
    async fn apply(
        reservation: &ServiceReservation<'_>,
        request: &MiniRequest,
    ) -> Result<Value, ErrorCode> {
        let id = parse(request.action.agent())?;
        match &request.action {
            MiniAction::Submit { prompt, .. } => {
                let receipt = reservation
                    .mini_client(id)
                    .map_err(service_code)?
                    .submit(
                        parse(request.epoch)?,
                        parse(request.request_id)?,
                        prompt.clone(),
                    )
                    .await
                    .map_err(backend_code)?;
                Ok(
                    json!({"state":"volatile_admitted","epoch":receipt.epoch,"agent_id":receipt.agent_id,
                    "submitted_request_id":receipt.request_id,"run_id":receipt.run_id,
                    "execution_complete":false}),
                )
            }
            MiniAction::Cancel {
                submitted_request_id,
                run_id,
                ..
            } => {
                let receipt = MiniReceipt {
                    epoch: parse(request.epoch)?,
                    agent_id: id,
                    request_id: parse(*submitted_request_id)?,
                    run_id: parse(*run_id)?,
                };
                let signalled = reservation
                    .mini_client(id)
                    .map_err(service_code)?
                    .cancel(receipt)
                    .await
                    .map_err(backend_code)?;
                Ok(json!({"cancel_signalled":signalled,"settlement_proven":false}))
            }
            MiniAction::Close { .. } => {
                reservation.close_mini(id).await.map_err(service_code)?;
                Ok(json!({"native_settled":true,"state":"lost"}))
            }
        }
    }
    async fn result(
        &self,
        scope: ControlScopeId,
        epoch: Epoch,
        agent: wire::AgentId,
        request: RequestId,
        run: Option<wire::RunId>,
        page: (usize, usize),
    ) -> Result<Value, ErrorCode> {
        self.authorize(scope, agent, MiniPermission::Submit)?;
        let (offset, max_bytes) = page;
        if !(4..=MAX_RESULT_TEXT_BYTES).contains(&max_bytes) || (offset != 0 && run.is_none()) {
            return Err(ErrorCode::InvalidRequest);
        }
        let missing = |reason| {
            json!({"durable":false,"volatile":true,"epoch":epoch,"agent_id":agent,
            "request_id":request,"found":false,"result":{"state":"unknown","reason":reason}})
        };
        if !self.epoch_matches(epoch) {
            return Ok(missing("owner_epoch_changed"));
        }
        let client = match self.inner.service.mini_client(parse(agent)?) {
            Ok(client) => client,
            Err(ServiceError::NotLoaded) => return Ok(missing("owner_not_loaded_no_replay")),
            Err(error) => return Err(service_code(error)),
        };
        let Some(result) = client
            .request(parse(epoch)?, parse(agent)?, parse(request)?)
            .await
            .map_err(backend_code)?
        else {
            return Ok(missing("input_not_observed_no_replay"));
        };
        if run.is_some_and(|run| run.to_string() != result.receipt.run_id.to_string()) {
            return Err(ErrorCode::InvalidRequest);
        }
        let state = match result.outcome {
            MiniOutcome::Running => "running",
            MiniOutcome::Succeeded => "completed",
            MiniOutcome::Failed => "failed",
            MiniOutcome::Aborted => "aborted",
            MiniOutcome::OutputLimit => "output_limit",
            MiniOutcome::Faulted => "faulted",
        };
        let text = &result.answer;
        if offset > text.len() || !text.is_char_boundary(offset) {
            return Err(ErrorCode::InvalidRequest);
        }
        let output = if result.outcome == MiniOutcome::Running {
            None
        } else {
            let mut end = text.len().min(offset.saturating_add(max_bytes));
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            Some(
                json!({"text":&text[offset..end],"offset":offset,"next_offset":if end < text.len() { Some(end) } else { None },
                "total_bytes":text.len(),"bytes":end-offset,"done":end == text.len()}),
            )
        };
        Ok(
            json!({"durable":false,"volatile":true,"epoch":epoch,"agent_id":agent,"request_id":request,"found":true,
            "result":{"state":state,"run_id":result.receipt.run_id,"output":output}}),
        )
    }
}
fn parse<T: TryFrom<String>>(value: impl ToString) -> Result<T, ErrorCode> {
    T::try_from(value.to_string()).map_err(|_| ErrorCode::InvalidRequest)
}
fn service_code(error: ServiceError) -> ErrorCode {
    match error {
        ServiceError::NotLoaded => ErrorCode::NotFound,
        ServiceError::Busy | ServiceError::Stopping => ErrorCode::Busy,
        ServiceError::AlreadyOwned => ErrorCode::Conflict,
        ServiceError::Backend(error) => backend_code(error),
        _ => ErrorCode::OutcomeUnknown,
    }
}
fn backend_code(error: super::ordinary::BackendError) -> ErrorCode {
    use super::ordinary::BackendError;
    match error {
        BackendError::Busy => ErrorCode::Busy,
        BackendError::Closed => ErrorCode::NotFound,
        BackendError::InputTooLarge | BackendError::Binding(_) => ErrorCode::InvalidRequest,
        _ => ErrorCode::OutcomeUnknown,
    }
}

pub(super) async fn start_minis(
    service: &HostService,
    agents: Vec<HostStartupAgent>,
) -> Result<(), super::daemon::DaemonError> {
    use super::daemon::DaemonError;
    for agent in agents {
        match service.catalog().get(agent.spec.id).await {
            // An existing Mini is never restored, even when explicitly named
            // in the same startup config after a crash/restart. Lost stays Lost.
            Ok(record) if record.spec == agent.spec && record.state == super::AgentState::Lost => {
                continue
            }
            Ok(_) => return Err(DaemonError::StartupAgent),
            Err(super::CatalogError::UnknownAgent) => {}
            Err(error) => return Err(DaemonError::Catalog(error)),
        }
        let record = service
            .catalog()
            .register(agent.spec)
            .await
            .map_err(DaemonError::Catalog)?;
        service
            .start_mini(
                record.spec.id,
                service.catalog().guard(&record),
                agent.profile,
                MiniLimits {
                    max_input_bytes: agent.limits.max_input_bytes,
                    ..Default::default()
                },
            )
            .await
            .map_err(|_| DaemonError::StartupAgent)?;
    }
    Ok(())
}
