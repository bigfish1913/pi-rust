//! Bounded, plugin-owned child runs. Execution bindings stay inside the host.
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use rpi_agent::{AgentEmitter, AgentEvent};
use rpi_ai::types::AssistantMessageEvent;
use rpi_harness::agent_harness::{AgentHarness, ChildHarness};
use rpi_harness::session::memory::{InMemorySessionStorage, SystemClock};
use rpi_harness::session::session::DefaultIdGenerator;
use rpi_harness::session::types::SessionMetadata;
use rpi_harness::session::Session;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// Host-owned asynchronous transport hook. Implementations must complete each emit;
/// the child owns ordering and draining, not the host transport's lifetime.
pub(crate) type ChildEventSink =
    Arc<dyn Fn(Value) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// Creator-only request: no model-provided path or relation fields.
pub(crate) struct ChildSessionRequest {
    pub id: String,
    pub parent_storage: Arc<dyn rpi_harness::session::types::SessionStorage>,
    pub parent_tool_call_id: Option<rpi_agent::tool_execution_scope::InvocationToolCallId>,
}
pub(crate) type ChildSessionFactory = Arc<dyn Fn(ChildSessionRequest) -> futures::future::BoxFuture<'static, Result<Session, String>> + Send + Sync>;

pub(crate) fn memory_child_session(request: &ChildSessionRequest) -> Session {
    Session::new(Arc::new(InMemorySessionStorage::new(SessionMetadata {
        id: request.id.clone(), created_at: 0,
        parent_session_id: Some(request.parent_storage.metadata().id),
    }, Arc::new(SystemClock), Arc::new(DefaultIdGenerator::new()))), None).with_volatile_progress()
}

// Cancelling the admission caller revokes publication, but the creator-owned
// task still finishes storage creation and drains before releasing its slot.
struct AdmissionGuard(Option<Arc<Job>>);
impl Drop for AdmissionGuard {
    fn drop(&mut self) { if let Some(job) = &self.0 { job.cancel.cancel(); } }
}

const MAX_JOBS: usize = 16;
const MAX_OUTPUT: usize = 4 * 1024 * 1024;
const MAX_EVENTS: usize = 8192;

#[derive(Default)]
pub(crate) struct ChildAgents {
    jobs: Arc<Mutex<HashMap<String, Arc<Job>>>>,
    revoked: Mutex<HashSet<u64>>,
    active: Arc<AtomicUsize>,
    sink: Mutex<Option<ChildEventSink>>,
    factory: Mutex<Option<ChildSessionFactory>>,
    tasks: Mutex<tokio::task::JoinSet<()>>,
}

struct Permit(Arc<AtomicUsize>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Job {
    owner: u64,
    cancel: CancellationToken,
    invocation_signal: Option<CancellationToken>,
    parent_tool_call_id: Option<rpi_agent::tool_execution_scope::InvocationToolCallId>,
    state: Mutex<State>,
    forwarding: Mutex<Option<Forwarding>>,
}

struct Forwarding {
    sender: Option<tokio::sync::mpsc::Sender<Value>>,
    task: tokio::task::JoinHandle<()>,
    envelope: Value,
}

#[derive(Default)]
struct State {
    deltas: VecDeque<String>,
    bytes: usize,
    events: usize,
    turns: u32,
    result: Option<Value>,
    settled: bool,
    error: Option<String>,
}

impl Job {
    // Called before the runner is spawned. The forwarding task announces the
    // association before releasing the runner, and owns the single terminal.
    fn attach(
        self: &Arc<Self>,
        sink: ChildEventSink,
        mut envelope: Value,
    ) -> tokio::sync::oneshot::Receiver<()> {
        if let Some(id) = &self.parent_tool_call_id {
            envelope["parentToolCallId"] = json!(id.as_str());
        }
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Value>(256);
        let (ready, started) = tokio::sync::oneshot::channel();
        let weak = Arc::downgrade(self);
        let mut announcement = envelope.clone();
        announcement["type"] = json!("child_start");
        let mut terminal = envelope.clone();
        let task = tokio::spawn(async move {
            sink(announcement).await;
            let _ = ready.send(());
            while let Some(event) = receiver.recv().await {
                sink(event).await;
            }
            terminal["type"] = json!("child_end");
            if let Some(job) = weak.upgrade() {
                let state = job.state.lock().unwrap_or_else(|e| e.into_inner());
                terminal["status"] = json!(if state.error.is_some() {
                    "failed"
                } else {
                    "done"
                });
                terminal["error"] = json!(state.error);
                terminal["result"] = json!(state.result);
            }
            sink(terminal).await;
        });
        *self.forwarding.lock().unwrap_or_else(|e| e.into_inner()) = Some(Forwarding {
            sender: Some(sender),
            task,
            envelope,
        });
        started
    }

    async fn finish_events(&self) {
        let forwarding = self
            .forwarding
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(mut forwarding) = forwarding {
            forwarding.sender.take();
            let _ = forwarding.task.await;
        }
    }

    fn fail(&self, error: impl Into<String>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.result.is_none() && state.error.is_none() {
            state.error = Some(error.into());
        }
        self.cancel.cancel();
    }

    fn observe(&self, event: AgentEvent) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.result.is_some() || state.error.is_some() {
            return;
        }
        let forwarding = self.forwarding.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(forwarding) = forwarding.as_ref() {
            let mut envelope = forwarding.envelope.clone();
            envelope["type"] = json!("child_event");
            envelope["event"] = serde_json::to_value(
                crate::remote::protocol::RemoteEvent::from_agent_event(&event),
            )
            .expect("native event projection is serializable");
            // RemoteEvent intentionally projects message payloads; retain the
            // native canonical identity as well where the source supplies it.
            match &event {
                AgentEvent::MessageStart { message_id, .. }
                | AgentEvent::MessageUpdate { message_id, .. }
                | AgentEvent::MessageEnd { message_id, .. } => {
                    if let Some(id) = message_id {
                        envelope["event"]["messageId"] = json!(id);
                    }
                }
                _ => {}
            }
            if forwarding
                .sender
                .as_ref()
                .is_none_or(|tx| tx.try_send(envelope).is_err())
            {
                state.error = Some("child event forwarding overflow or closed sink".into());
                self.cancel.cancel();
                return;
            }
        }
        drop(forwarding);
        match event {
            AgentEvent::TurnEnd { .. } => state.turns += 1,
            AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
                ..
            } => {
                if state.bytes.saturating_add(delta.len()) > MAX_OUTPUT
                    || state.events >= MAX_EVENTS
                {
                    state.error = Some("child output limit exceeded".into());
                    self.cancel.cancel();
                } else {
                    state.bytes += delta.len();
                    state.events += 1;
                    state.deltas.push_back(delta);
                }
            }
            _ => {}
        }
    }
}

struct Emitter(Arc<Job>);
impl AgentEmitter for Emitter {
    fn emit(&self, event: AgentEvent) -> futures::future::BoxFuture<'static, ()> {
        self.0.observe(event);
        Box::pin(async {})
    }
    fn try_emit(&self, event: AgentEvent) {
        self.0.observe(event);
    }
}

impl Drop for ChildAgents {
    fn drop(&mut self) {
        self.cancel_all();
        // Rust Drop cannot await. Transfer the bounded set to one cleanup owner,
        // which joins (never detaches) every creation/runner future. In-flight
        // creation finishes without publication, then drains and releases slots.
        let mut tasks = std::mem::take(self.tasks.get_mut().unwrap_or_else(|e| e.into_inner()));
        if !tasks.is_empty() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move { while tasks.join_next().await.is_some() {} });
            }
        }
    }
}

impl ChildAgents {
    pub(crate) fn set_session_factory(&self, factory: ChildSessionFactory) {
        *self.factory.lock().unwrap_or_else(|e| e.into_inner()) = Some(factory);
    }

    /// Applies to future children only; existing jobs retain their original sink.
    pub(crate) fn set_event_sink(&self, sink: Option<ChildEventSink>) {
        *self.sink.lock().unwrap_or_else(|e| e.into_inner()) = sink;
    }

    pub(crate) fn cancel_owner(&self, owner: u64) {
        self.revoked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(owner);
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, job| {
                if job.owner == owner {
                    job.fail("owning plugin unloaded");
                    false
                } else {
                    true
                }
            });
    }

    pub(crate) fn cancel_all(&self) {
        for job in self.jobs.lock().unwrap_or_else(|e| e.into_inner()).values() {
            job.fail("parent session closed or changed");
        }
    }

    pub(crate) async fn handle(
        &self,
        parent: &AgentHarness,
        owner: u64,
        args: Value,
    ) -> Result<Value, String> {
        let op = args
            .get("op")
            .and_then(Value::as_str)
            .ok_or("missing child op")?;
        if op == "start" {
            return self.start(parent, owner, &args).await;
        }
        self.handle_existing(owner, args)
    }

    pub(crate) fn handle_existing(&self, owner: u64, args: Value) -> Result<Value, String> {
        let op = args
            .get("op")
            .and_then(Value::as_str)
            .ok_or("missing child op")?;
        let id = args
            .get("childId")
            .and_then(Value::as_str)
            .ok_or("missing childId")?;
        let job = {
            let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
            jobs.get(id)
                .filter(|job| job.owner == owner)
                .cloned()
                .ok_or("unknown childId for this plugin")?
        };
        match op {
            "poll" => {
                let mut state = job.state.lock().unwrap_or_else(|e| e.into_inner());
                let status = if state.error.is_some() {
                    "failed"
                } else if state.result.is_some() && state.settled {
                    "done"
                } else {
                    "running"
                };
                // Bound each ABI response as well as the lifetime output budget.
                let mut deltas = Vec::new();
                let mut bytes = 0;
                while let Some(delta) = state.deltas.front() {
                    if !deltas.is_empty() && bytes + delta.len() > 64 * 1024 {
                        break;
                    }
                    bytes += delta.len();
                    deltas.push(state.deltas.pop_front().expect("front exists"));
                }
                // Terminal result is delivered only after queued deltas drain.
                let drained = state.deltas.is_empty();
                Ok(
                    json!({"childId":id, "status":if drained {status} else {"running"}, "deltas":deltas,
                    "turns":state.turns, "result":if drained && state.settled {state.result.clone()} else {None},
                    "error":if drained {state.error.clone()} else {None}}),
                )
            }
            "cancel" | "dispose" => {
                job.fail("child cancelled");
                if op == "dispose" {
                    self.jobs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(id);
                }
                Ok(
                    json!({"childId":id,"status":if op == "dispose" {"disposed"} else {"cancelled"}}),
                )
            }
            _ => Err(format!("unknown child op `{op}`")),
        }
    }

    async fn start(
        &self,
        parent: &AgentHarness,
        owner: u64,
        args: &Value,
    ) -> Result<Value, String> {
        let invocation_signal = rpi_agent::tool_execution_scope::invocation_signal();
        if invocation_signal
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err("parent tool invocation cancelled".into());
        }
        let task = args
            .get("task")
            .and_then(Value::as_str)
            .ok_or("missing child task")?;
        let role = args
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("worker")
            .trim();
        if task.trim().is_empty() || task.len() > 20_000 || role.is_empty() || role.len() > 200 {
            return Err("child task must contain 1-20000 bytes and role 1-200 bytes".into());
        }
        let turns = bounded_number(args, "maxTurns", 8, 1, 32)? as u32;
        let timeout = bounded_number(args, "timeoutSeconds", 300, 10, 1800)?;
        let tools = args
            .get("tools")
            .map(|value| {
                value
                    .as_array()
                    .ok_or("child tools must be a string array")
                    .and_then(|values| {
                        values
                            .iter()
                            .map(|v| {
                                v.as_str()
                                    .map(str::to_owned)
                                    .ok_or("child tools must be a string array")
                            })
                            .collect::<Result<Vec<_>, _>>()
                    })
            })
            .transpose()?;
        let id = parent.session().id_generator().next();
        let job = Arc::new(Job {
            owner,
            cancel: CancellationToken::new(),
            invocation_signal,
            parent_tool_call_id: rpi_agent::tool_execution_scope::invocation_tool_call_id(),
            state: Mutex::new(State::default()),
            forwarding: Mutex::new(None),
        });
        // Reserve before awaiting construction so concurrent starts cannot exceed the cap.
        let permit;
        {
            let revoked = self.revoked.lock().unwrap_or_else(|e| e.into_inner());
            if revoked.contains(&owner) {
                return Err("owning plugin unloaded".into());
            }
            let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
            if jobs.len() >= MAX_JOBS {
                return Err("child capacity reached; dispose finished children".into());
            }
            let mut active = self.active.load(Ordering::Acquire);
            loop {
                if active >= MAX_JOBS {
                    return Err("child cleanup still in progress; retry later".into());
                }
                match self.active.compare_exchange_weak(
                    active,
                    active + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(current) => active = current,
                }
            }
            permit = Permit(self.active.clone());
            jobs.insert(id.clone(), job.clone());
        }
        let sink = self.sink.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let factory = self.factory.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let request = ChildSessionRequest {
            id: id.clone(), parent_storage: parent.session().storage(),
            parent_tool_call_id: job.parent_tool_call_id.clone(),
        };
        let parent = parent.clone();
        let jobs = self.jobs.clone();
        let role = role.to_owned();
        let prompt = format!("You are a delegated {role} subagent. Complete the task in at most {turns} turns. Do not delegate, spawn, or ask another agent to do work. Work only in the current project and return a concise final report.

Task:
{task}");
        let (reply, response) = tokio::sync::oneshot::channel();
        let mut admission = AdmissionGuard(Some(job.clone()));
        let worker = job.clone();
        {
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        while tasks.try_join_next().is_some() {}
        tasks.spawn(async move {
            let _permit = permit;
            let admitted = async {
                let parent_storage = request.parent_storage.clone();
                let session = match factory {
                    Some(factory) => factory(request).await?,
                    None => memory_child_session(&request),
                };
                let storage = session.storage();
                let cancelled = || reply.is_closed() || worker.cancel.is_cancelled()
                    || !Arc::ptr_eq(&parent_storage, &parent.session().storage())
                    || worker.invocation_signal.as_ref().is_some_and(CancellationToken::is_cancelled);
                if cancelled() {
                    storage.drain().await;
                    return Err("child admission cancelled".to_owned());
                }
                let child = match parent.create_child(session, Arc::new(Emitter(worker.clone())), turns, tools).await {
                    Ok(child) => child,
                    Err(error) => { storage.drain().await; return Err(error.to_string()); }
                };
                if cancelled() {
                    let _ = child.harness.close().await;
                    storage.drain().await;
                    return Err("child admission cancelled".to_owned());
                }
                Ok((child, storage))
            };
            let (child, storage) = match std::panic::AssertUnwindSafe(admitted).catch_unwind().await {
                Ok(Ok(value)) => value,
                result => {
                    let error = match result { Ok(Err(error)) => error, _ => "child construction panicked".into() };
                    worker.fail(error.clone());
                    jobs.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            // Admission no longer needs the parent binding. Retaining it for
            // the run would keep weak runtime-action bindings alive and prevent
            // the parent's Drop from cancelling its children.
            drop(parent);
            // Sending admission is synchronous: a dropped receiver cannot cause
            // an announcement. Storage and harness exist before any child traffic.
            if reply.send(Ok(json!({"childId":id,"sessionId":id,"status":"running"}))).is_err() {
                let _ = child.harness.close().await;
                storage.drain().await;
                jobs.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                return;
            }
            let started = sink.map(|sink| worker.attach(sink, json!({
                "parentSessionId": storage.metadata().parent_session_id,
                "sessionId": id, "childId": id, "role": role,
            })));
            let execution_scope = child.execution_scope.clone();
            let cleanup_harness = child.harness.clone();
            if let Some(started) = started {
                if started.await.is_err() { worker.fail("child event announcement failed"); }
            }
            if std::panic::AssertUnwindSafe(run_child(child, worker.clone(), prompt, turns, timeout))
                .catch_unwind().await.is_err() {
                worker.fail("child runner panicked");
                let _ = cleanup_harness.close().await;
            }
            execution_scope.wait_idle().await;
            storage.drain().await;
            worker.state.lock().unwrap_or_else(|e| e.into_inner()).settled = true;
            worker.finish_events().await;
        });
        }
        let result = response.await.map_err(|_| "child admission task stopped".to_owned())?;
        // Successful admission transfers cancellation ownership to the runner.
        if result.is_ok() { admission.0.take(); }
        result
    }
}

fn bounded_number(
    args: &Value,
    key: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, String> {
    let value = match args.get(key) {
        None => default,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| format!("{key} must be an integer"))?,
    };
    if !(min..=max).contains(&value) {
        return Err(format!("{key} must be between {min} and {max}"));
    }
    Ok(value)
}

async fn run_child(child: ChildHarness, job: Arc<Job>, prompt: String, turns: u32, timeout: u64) {
    let lane = child.harness.lane("main");
    let run = lane.prompt_text(&prompt, Vec::new());
    tokio::pin!(run);
    let parent_abort = async {
        match &child.parent_signal {
            Some(signal) => signal.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };
    let invocation_abort = async {
        match &job.invocation_signal {
            Some(signal) => signal.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };
    let result = tokio::select! {
        biased;
        _ = job.cancel.cancelled() => None,
        _ = invocation_abort => {job.fail("parent tool invocation cancelled"); None},
        _ = child.scope.cancelled() => {job.fail("parent run ended"); None},
        _ = parent_abort => {job.fail("parent run cancelled"); None},
        _ = tokio::time::sleep(Duration::from_secs(timeout)) => {job.fail(format!("child timed out after {timeout} seconds")); None},
        result = &mut run => Some(result),
    };
    if let Some(result) = result {
        match result {
            Ok(result) => {
                let mut outcome = crate::extensions_actions::run_outcome_json(result.outcome);
                let mut state = job.state.lock().unwrap_or_else(|e| e.into_inner());
                let bytes = outcome
                    .get("text")
                    .and_then(Value::as_str)
                    .map_or(0, str::len);
                if state.bytes.saturating_add(bytes) > MAX_OUTPUT {
                    state
                        .error
                        .get_or_insert_with(|| "child output limit exceeded".into());
                } else if state.error.is_none() {
                    outcome["turns"] = json!(state.turns);
                    outcome["turnLimited"] = json!(state.turns >= turns);
                    state.result = Some(outcome);
                }
            }
            Err(error) => job.fail(format!("child run failed: {error}")),
        }
    } else {
        // Abort even if cancellation raced ahead of the run's first poll. A closed
        // harness rejects admission; an admitted run gets its cancellation signal.
        let _ = lane.abort().await;
        let _ = child.harness.close().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut run).await;
    }
    let _ = child.harness.close().await;
}

#[cfg(test)]
mod tests;
